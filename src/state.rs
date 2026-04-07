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

/// Pool of idle TCP connections keyed by a string identifier.
///
/// Supports both immediate (`try_take`) and blocking (`take`) retrieval.
/// When the pool is empty, callers can wait (with timeout) for a connection
/// to become available instead of failing immediately.
pub struct StreamPool {
    connections: Mutex<HashMap<String, Vec<TcpStream>>>,
    notifiers: Mutex<HashMap<String, Arc<Notify>>>,
}

impl StreamPool {
    pub fn new() -> Self {
        Self {
            connections: Mutex::new(HashMap::new()),
            notifiers: Mutex::new(HashMap::new()),
        }
    }

    /// Add an idle connection for `key` and wake one waiter.
    pub fn push(&self, key: String, stream: TcpStream) {
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
    ///
    /// Uses a register-before-check pattern to avoid races: interest is
    /// registered on the [`Notify`] *before* inspecting the pool, so a
    /// [`push`] that fires between the check and the await is never lost.
    pub async fn take(&self, key: &str, timeout: Duration) -> Option<TcpStream> {
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
    pub fn try_take(&self, key: &str) -> Option<TcpStream> {
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

    /// Increment the counter and return a guard that decrements on drop.
    pub fn track(self: &Arc<Self>) -> InFlightGuard {
        self.count.fetch_add(1, Ordering::SeqCst);
        InFlightGuard(Arc::clone(self))
    }

    /// Wait until in-flight count reaches zero, or timeout expires.
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
    /// Member nodes discovered via gossip, keyed by node_id.
    /// Uses ArcSwap for lock-free reads on the hot path.
    pub known_nodes: ArcSwap<HashMap<String, NodeInfo>>,
    /// Serializes writes to `known_nodes` so concurrent updates don't lose changes.
    known_nodes_write: tokio::sync::Mutex<()>,
    /// O(1) service lookup by (node_id, service_name). Rebuilt when dynamic_config changes.
    /// Uses ArcSwap for lock-free reads on the hot path.
    pub service_index: ArcSwap<HashMap<(String, String), Service>>,
    /// Bootstrap peer addresses from config, used only for initiating connections.
    /// These are never treated as member identities.
    pub bootstrap_peers: Vec<String>,
    /// Path to the on-disk config file, used to persist dynamic config updates.
    pub config_path: String,
    /// Pool of idle reverse connections from NAT'd nodes, keyed by node_id.
    /// Used by relay nodes to dispatch tunnel requests to unreachable peers.
    pub reverse_pool: Arc<StreamPool>,
    /// Pool of pre-established tunnel connections to nodes, keyed by connect address.
    /// Eliminates TCP handshake latency on the proxy hot path.
    pub tunnel_pool: Arc<StreamPool>,
    /// Pool of yamux multiplexed sessions to nodes, keyed by connect address.
    /// Allows opening many logical streams over a single TCP connection.
    pub mux_pool: Arc<crate::mux::MuxPool>,
    /// Cancellation token for graceful shutdown.
    pub shutdown: CancellationToken,
    /// Tracks in-flight connections for graceful drain.
    pub in_flight: Arc<InFlightTracker>,
}

impl SharedState {
    /// Construct a new `SharedState` and wrap it in an `Arc`.
    ///
    /// The `dynamic_config` is seeded from `config.dynamic_config`.
    /// Bootstrap peer addresses are stored separately; the member list
    /// (`known_nodes`) starts empty and is populated by gossip sync.
    pub fn new(config: Config, config_path: String, shutdown: CancellationToken) -> Arc<Self> {
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
            shutdown,
            in_flight: InFlightTracker::new(),
        })
    }

    /// Rebuild the service index from the current dynamic_config.
    /// Call this after every dynamic_config update.
    pub async fn rebuild_service_index(&self) {
        let dc = self.dynamic_config.read().await;
        let idx = build_service_index(&dc);
        self.service_index.store(Arc::new(idx));
    }

    /// Atomically update `known_nodes` via a clone-modify-swap pattern.
    /// The write mutex ensures concurrent updates don't lose changes.
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

/// Build a HashMap from (node_id, service_name) to Service.
fn build_service_index(dc: &DynamicConfig) -> HashMap<(String, String), Service> {
    let mut idx = HashMap::new();
    for group in &dc.service_groups {
        for svc in &group.services {
            idx.insert((svc.node_id.clone(), svc.name.clone()), svc.clone());
        }
    }
    idx
}
