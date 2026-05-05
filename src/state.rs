use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::TcpStream;
use tokio::sync::{Notify, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, DynamicConfig, NodeInfo, Service};
use crate::tls::{MeshStream, TlsContext};

/// Pool of idle connections keyed by a string identifier.
///
/// Generic over the stream type so it can hold either plain `TcpStream`s or
/// `MeshStream`s (TLS-wrapped or not). Supports both immediate (`try_take`)
/// and blocking (`take`) retrieval.
pub struct StreamPool<T> {
    connections: Mutex<HashMap<String, Vec<T>>>,
    notifiers: Mutex<HashMap<String, Arc<Notify>>>,
}

impl<T: Send> StreamPool<T> {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            notifiers: Mutex::new(HashMap::new()),
        }
    }

    /// Add an idle connection for `key` and wake one waiter.
    pub fn push(&self, key: String, stream: T) {
        {
            let mut conns = self.connections.lock().unwrap();
            conns.entry(key.clone()).or_default().push(stream);
        }
        let notify = {
            let notifiers = self.notifiers.lock().unwrap();
            notifiers.get(&key).cloned()
        };
        if let Some(n) = notify {
            n.notify_one();
        }
    }

    /// Take a connection, waiting up to `timeout` for one to become available.
    pub async fn take(&self, key: &str, timeout: Duration) -> Option<T> {
        let deadline = Instant::now() + timeout;

        loop {
            let notify = {
                let mut notifiers = self.notifiers.lock().unwrap();
                notifiers
                    .entry(key.to_string())
                    .or_insert_with(|| Arc::new(Notify::new()))
                    .clone()
            };
            let notified = notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();

            {
                let mut conns = self.connections.lock().unwrap();
                if let Some(vec) = conns.get_mut(key) {
                    if let Some(stream) = vec.pop() {
                        return Some(stream);
                    }
                }
            }

            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }

            match tokio::time::timeout(remaining, notified).await {
                Ok(()) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Take a connection without waiting.
    pub fn try_take(&self, key: &str) -> Option<T> {
        let mut conns = self.connections.lock().unwrap();
        let vec = conns.get_mut(key)?;
        vec.pop()
    }

    /// Number of idle connections for `key`.
    pub fn count(&self, key: &str) -> usize {
        let conns = self.connections.lock().unwrap();
        conns.get(key).map_or(0, |v| v.len())
    }
}

/// RAII guard that decrements the in-flight counter on drop.
pub struct InFlightGuard(Arc<InFlightTracker>);

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let prev = self.0.count.fetch_sub(1, Ordering::SeqCst);
        if prev == 1 {
            self.0.zero.notify_waiters();
        }
    }
}

/// Tracks the number of in-flight connections for graceful drain.
pub struct InFlightTracker {
    count: AtomicUsize,
    zero: Notify,
}

impl InFlightTracker {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            count: AtomicUsize::new(0),
            zero: Notify::new(),
        })
    }

    pub fn track(self: &Arc<Self>) -> InFlightGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(Arc::clone(self))
    }

    pub fn current(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }

    pub async fn wait_zero(&self, timeout: Duration) -> bool {
        if self.count.load(Ordering::SeqCst) == 0 {
            return true;
        }
        tokio::time::timeout(timeout, async {
            loop {
                self.zero.notified().await;
                if self.count.load(Ordering::SeqCst) == 0 {
                    break;
                }
            }
        })
        .await
        .is_ok()
    }
}

/// Cluster state shared across async tasks within a single daemon process.
pub struct SharedState {
    pub config: Arc<Config>,
    pub dynamic_config: Arc<RwLock<DynamicConfig>>,
    pub known_nodes: ArcSwap<HashMap<String, NodeInfo>>,
    known_nodes_write: tokio::sync::Mutex<()>,
    pub service_index: ArcSwap<HashMap<(String, String), Service>>,
    pub bootstrap_peers: Vec<String>,
    pub config_path: String,
    /// Idle reverse connections from NAT'd nodes, keyed by node_id.
    pub reverse_pool: Arc<StreamPool<MeshStream>>,
    /// Pre-established connections to nodes, keyed by connect address.
    pub tunnel_pool: Arc<StreamPool<MeshStream>>,
    pub mux_pool: Arc<crate::mux::MuxPool>,
    /// TLS context when `tls_dir` is configured; `None` → plain TCP.
    pub tls: Option<Arc<TlsContext>>,
    pub shutdown: CancellationToken,
    pub in_flight: Arc<InFlightTracker>,
}

impl SharedState {
    pub fn new(
        config: Config,
        config_path: String,
        shutdown: CancellationToken,
        tls: Option<TlsContext>,
    ) -> Arc<Self> {
        let dynamic_config = config.dynamic_config.clone();
        let bootstrap_peers = config.known_nodes.clone();
        let service_index = build_service_index(&dynamic_config);

        Arc::new(Self {
            config: Arc::new(config),
            dynamic_config: Arc::new(RwLock::new(dynamic_config)),
            known_nodes: ArcSwap::from_pointee(HashMap::new()),
            known_nodes_write: tokio::sync::Mutex::new(()),
            service_index: ArcSwap::from_pointee(service_index),
            bootstrap_peers,
            config_path,
            reverse_pool: Arc::new(StreamPool::new()),
            tunnel_pool: Arc::new(StreamPool::new()),
            mux_pool: Arc::new(crate::mux::MuxPool::new()),
            tls: tls.map(Arc::new),
            shutdown,
            in_flight: InFlightTracker::new(),
        })
    }

    /// Wrap a newly accepted `TcpStream` with mTLS.
    pub async fn tls_accept(&self, stream: TcpStream) -> anyhow::Result<MeshStream> {
        match &self.tls {
            Some(ctx) => ctx.accept(stream).await,
            None => anyhow::bail!("mTLS is required but tlsDir is not configured"),
        }
    }

    /// Wrap an outbound `TcpStream` with mTLS.
    pub async fn tls_connect(&self, stream: TcpStream) -> anyhow::Result<MeshStream> {
        match &self.tls {
            Some(ctx) => ctx.connect(stream).await,
            None => anyhow::bail!("mTLS is required but tlsDir is not configured"),
        }
    }

    pub async fn rebuild_service_index(&self) {
        let dc = self.dynamic_config.read().await;
        let idx = build_service_index(&dc);
        self.service_index.store(Arc::new(idx));
    }

    pub async fn update_known_nodes<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&mut HashMap<String, NodeInfo>) -> R,
    {
        let _guard = self.known_nodes_write.lock().await;
        let mut map = (*self.known_nodes.load_full()).clone();
        let result = f(&mut map);
        self.known_nodes.store(Arc::new(map));
        result
    }
}

fn build_service_index(dc: &DynamicConfig) -> HashMap<(String, String), Service> {
    let mut idx = HashMap::new();
    for group in &dc.service_groups {
        for svc in &group.services {
            idx.insert((svc.node_id.clone(), svc.name.clone()), svc.clone());
        }
    }
    idx
}
