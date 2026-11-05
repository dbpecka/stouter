use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use arc_swap::ArcSwap;
use tokio::net::TcpStream;
use tokio::sync::{Notify, RwLock};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::config::{Config, DynamicConfig, NodeInfo, Route, Service, ServiceHealth};
use crate::tls::{MeshStream, TlsContext};

/// A node not confirmed reachable within this window is treated as unhealthy
/// and excluded from load balancing (failing open if every node is). The
/// gossip sync loop probes every peer every 10s, so this tolerates roughly
/// three missed rounds.
pub const NODE_STALE_AFTER: Duration = Duration::from_secs(30);

/// Time allowed to open a mesh connection (TCP connect plus TLS handshake),
/// and for an accepted one to finish its handshake. Without it, a peer that
/// stopped answering (or a client that never handshakes) holds a task, and
/// often a whole background loop, indefinitely.
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// A node with no sign of life (direct contact or a rising heartbeat) for
/// this long is forgotten, so dead nodes don't linger forever.
pub const NODE_EXPIRE_AFTER: Duration = Duration::from_secs(300);

/// How long a departed (left or expired) node is remembered, so a stale
/// copy still circulating in gossip can't bring it back.
const TOMBSTONE_TTL: Duration = Duration::from_secs(1800);

/// How long a node forgotten for silence keeps being retried at its last
/// address. It may have been cut off rather than dead; when the network
/// heals, this is how the two sides find each other again even if neither
/// lists the other as a bootstrap peer.
pub const RECONNECT_FOR: Duration = Duration::from_secs(24 * 3600);

/// A relay counts as one this node uses while it holds a reverse
/// connection there, or registered one this recently (connections are
/// consumed by tunnels and replaced, so the count dips during bursts).
const RELAY_GRACE: Duration = Duration::from_secs(30);

fn unix_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// This process's incarnation: one above the previous process's (kept in
/// `{config_path}.state`) and no lower than the current Unix time in
/// seconds, which also puts it above heartbeats from versions that used the
/// wall clock directly, or from before the state file existed. Saved back
/// for the next start. If it can't be saved (a read-only directory), the
/// clock alone orders restarts, as before.
pub fn incarnation_for(config_path: &str) -> u64 {
    #[derive(serde::Serialize, serde::Deserialize)]
    struct NodeStateFile {
        incarnation: u64,
    }
    let path = format!("{config_path}.state");
    let stored = std::fs::read_to_string(&path)
        .ok()
        .and_then(|text| serde_json::from_str::<NodeStateFile>(&text).ok())
        .map_or(0, |state| state.incarnation);
    let incarnation = stored.saturating_add(1).max(unix_seconds());
    let saved = serde_json::to_vec(&NodeStateFile { incarnation })
        .map_err(std::io::Error::other)
        .and_then(|json| crate::config::write_atomic(std::path::Path::new(&path), &json));
    if let Err(e) = saved {
        tracing::info!(
            "cannot save {path} ({e}): if this node restarts with its clock set back, peers may ignore it until the clock catches up"
        );
    }
    incarnation
}

/// Whether `node` was confirmed reachable recently enough to route to.
pub fn node_is_live(node: &NodeInfo, now: std::time::Instant) -> bool {
    node.last_seen.is_some_and(|seen| now.saturating_duration_since(seen) <= NODE_STALE_AFTER)
}

/// A node that left or was forgotten, and its last heartbeat then.
struct Tombstone {
    heartbeat: u64,
    at: std::time::Instant,
    /// The node's signed "left" record, when it said so itself: relayed
    /// (see `departed_records`) so the departure reaches every node.
    record: Option<(u64, crate::config::SignedNode)>,
    /// A node forgotten for silence, as it was then: one with no address is
    /// let back in by a newer beat it signed (see `revive_with_beat`).
    forgotten: Option<NodeInfo>,
}

/// Whether an idle connection has been closed by its peer or is otherwise
/// unusable: reading it without waiting gives EOF, an error, or data nobody
/// asked for (an idle pooled connection carries none). Never blocks.
pub fn idle_stream_closed<T: tokio::io::AsyncRead + Unpin>(stream: &mut T) -> bool {
    let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
    let mut byte = [0u8; 1];
    let mut buf = tokio::io::ReadBuf::new(&mut byte);
    !std::pin::Pin::new(stream).poll_read(&mut cx, &mut buf).is_pending()
}

/// Per-key exponential backoff for reconnect loops: after each consecutive
/// failure the next attempt for that key waits twice as long, from `base`
/// up to `max`, with jitter so many callers don't retry in lockstep.
pub struct Backoff {
    base: Duration,
    max: Duration,
    keys: HashMap<String, (u32, Instant)>,
}

impl Backoff {
    pub fn new(base: Duration, max: Duration) -> Self {
        Self { base, max, keys: HashMap::new() }
    }

    /// Whether `key` may be tried at `now`.
    pub fn ready(&self, key: &str, now: Instant) -> bool {
        self.keys.get(key).is_none_or(|(_, next)| now >= *next)
    }

    /// Record a failure; returns how long until the next attempt.
    pub fn failed(&mut self, key: &str, now: Instant) -> Duration {
        let (failures, next) = self.keys.entry(key.to_string()).or_insert((0, now));
        *failures += 1;
        let wait = self.base.saturating_mul(1 << (*failures - 1).min(16)).min(self.max);
        let wait = wait.mul_f64(rand::random_range(0.75..=1.0));
        *next = now + wait;
        wait
    }

    pub fn succeeded(&mut self, key: &str) {
        self.keys.remove(key);
    }

    /// Drop state for keys `keep` rejects (e.g. peers no longer known).
    pub fn retain(&mut self, keep: impl Fn(&str) -> bool) {
        self.keys.retain(|k, _| keep(k));
    }
}

/// Pool of idle connections keyed by a string identifier.
///
/// Generic over the stream type so it can hold either plain `TcpStream`s or
/// `MeshStream`s (TLS-wrapped or not). Supports both immediate (`try_take`)
/// and blocking (`take`) retrieval. Connections the peer has closed are
/// never handed out, and [`StreamPool::prune`] drops them so they don't
/// pile up holding file descriptors.
pub struct StreamPool<T> {
    connections: Mutex<HashMap<String, Vec<T>>>,
    notifiers: Mutex<HashMap<String, Arc<Notify>>>,
}

impl<T: Send + tokio::io::AsyncRead + Unpin> StreamPool<T> {
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

            if let Some(stream) = self.try_take(key) {
                return Some(stream);
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

    /// Take a live connection without waiting, dropping closed ones on the way.
    pub fn try_take(&self, key: &str) -> Option<T> {
        let mut conns = self.connections.lock().unwrap();
        let vec = conns.get_mut(key)?;
        while let Some(mut stream) = vec.pop() {
            if !idle_stream_closed(&mut stream) {
                return Some(stream);
            }
        }
        None
    }

    /// Drop every connection the peer has closed. Returns how many.
    pub fn prune(&self) -> usize {
        let mut dropped = 0;
        let mut conns = self.connections.lock().unwrap();
        conns.retain(|_, streams| {
            let before = streams.len();
            streams.retain_mut(|s| !idle_stream_closed(s));
            dropped += before - streams.len();
            !streams.is_empty()
        });
        dropped
    }

    /// Drop every idle connection for keys `keep` rejects. Returns how many.
    pub fn retain_keys(&self, keep: impl Fn(&str) -> bool) -> usize {
        let mut dropped = 0;
        self.connections.lock().unwrap().retain(|key, streams| {
            let kept = keep(key);
            if !kept {
                dropped += streams.len();
            }
            kept
        });
        dropped
    }

    /// Number of idle connections for `key`.
    pub fn count(&self, key: &str) -> usize {
        let conns = self.connections.lock().unwrap();
        conns.get(key).map_or(0, |v| v.len())
    }
}

/// Background task: every few seconds, drop idle pooled connections their
/// peers have closed (a restarted NAT'd node leaves its old reverse
/// connections behind on every relay) and mux sessions that have ended.
pub async fn run_pool_sweeper(state: Arc<SharedState>) {
    let mut interval = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = interval.tick() => {}
            _ = state.shutdown.cancelled() => break,
        }
        let dropped = state.reverse_pool.prune() + state.tunnel_pool.prune();
        if dropped > 0 {
            tracing::debug!("dropped {dropped} closed idle connection(s)");
        }
        state.mux_pool.prune();
    }
}

/// A reverse connection registered with a relay, from
/// [`SharedState::relay_registered`]; gone when dropped.
pub struct RelayRegistration {
    state: Arc<SharedState>,
    relay: String,
}

impl Drop for RelayRegistration {
    fn drop(&mut self) {
        if let Some((live, _)) = self.state.relays.lock().unwrap().get_mut(&self.relay) {
            *live = live.saturating_sub(1);
        }
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
///
/// Counts work in progress only: tunnels being proxied, gossip and admin
/// requests, and client connections on subscriber proxies. Idle pooled
/// connections and idle mux sessions aren't counted, so a quiet node drains
/// at once.
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
        tokio::time::timeout(timeout, async {
            loop {
                // Register for the wakeup before checking the count, so a
                // drop to zero in between isn't missed (`notify_waiters`
                // only wakes waiters that already exist).
                let notified = self.zero.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                if self.count.load(Ordering::SeqCst) == 0 {
                    break;
                }
                notified.await;
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
    /// The dynamic config (as JSON) that the config file holds as far as
    /// this daemon knows: what it last wrote there or took from there. The
    /// file saying something else means an edit nobody applied, which a
    /// write must not silently overwrite (see `gossip::persist_state_with`).
    pub(crate) file_config: Mutex<Option<String>>,
    pub known_nodes: ArcSwap<HashMap<String, NodeInfo>>,
    known_nodes_write: tokio::sync::Mutex<()>,
    pub service_index: ArcSwap<HashMap<(String, String), Service>>,
    /// Routes per service name, in evaluation order. Rebuilt with
    /// `service_index` so request handling never takes the config lock.
    pub routes: ArcSwap<HashMap<String, Vec<Route>>>,
    /// Health of the services this node hosts, maintained by the local
    /// health checker and gossiped in this node's own `NodeInfo`.
    pub local_health: ArcSwap<ServiceHealth>,
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
    /// Span exporter, set at daemon start when `tracing` is configured.
    pub tracer: std::sync::OnceLock<Arc<crate::telemetry::Tracer>>,
    /// Heartbeats issued by this process (the low half of each).
    heartbeat: AtomicU64,
    /// This process's incarnation, the high half of each heartbeat. Set at
    /// daemon start from [`incarnation_for`]; the clock if never set.
    pub incarnation: std::sync::OnceLock<u64>,
    /// Nodes that left or were forgotten, by id.
    departed: Mutex<HashMap<String, Tombstone>>,
    /// Version of the last config refused for lacking a valid signature, so
    /// each is reported once rather than on every sync.
    pub(crate) rejected_config_version: AtomicU64,
    /// This node's key and certificate, to sign its own node record. Set at
    /// daemon start.
    pub signer: std::sync::OnceLock<crate::signing::Signer>,
    /// Relays (by node id) this node registered reverse connections with:
    /// how many it holds there now, and when it last registered one.
    relays: Mutex<HashMap<String, (usize, std::time::Instant)>>,
    /// Nodes forgotten for silence: last address, and when.
    forgotten: Mutex<HashMap<String, (String, std::time::Instant)>>,
    /// The record this node last gossiped about itself, reused while fresh.
    pub(crate) own_record: Mutex<Option<OwnRecord>>,
    /// Open connections per (kind, peer CN), for the per-peer limits.
    peer_slots: Mutex<HashMap<(&'static str, String), usize>>,
    /// Peer addresses known to speak delta sync (their replies say so), so
    /// syncs to them can be compact, with the (version, hash) of the config
    /// each had as of its last reply: one that's behind gets ours.
    pub(crate) delta_peers: Mutex<HashMap<String, (u64, u64)>>,
    /// This node's last beat and when it was made (NAT'd nodes only).
    pub(crate) own_beat: Mutex<Option<(crate::config::SignedBeat, std::time::Instant)>>,
    /// Set at shutdown: this node's record now says it left.
    pub leaving: std::sync::atomic::AtomicBool,
}

/// One of a peer's limited connections (see [`SharedState::try_peer_slot`]);
/// frees its place when dropped.
pub struct PeerSlot {
    state: Arc<SharedState>,
    key: (&'static str, String),
}

impl Drop for PeerSlot {
    fn drop(&mut self) {
        let mut slots = self.state.peer_slots.lock().unwrap();
        if let Some(count) = slots.get_mut(&self.key) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                slots.remove(&self.key);
            }
        }
    }
}

/// This node's last record (see `gossip::own_record`): its content, and
/// the version and signature it went out with.
pub(crate) struct OwnRecord {
    pub record: crate::config::NodeRecord,
    pub signed: Option<crate::config::SignedNode>,
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
        let routes = build_route_index(&dynamic_config);

        Arc::new(Self {
            config: Arc::new(config),
            file_config: Mutex::new(serde_json::to_string(&dynamic_config).ok()),
            dynamic_config: Arc::new(RwLock::new(dynamic_config)),
            known_nodes: ArcSwap::from_pointee(HashMap::new()),
            known_nodes_write: tokio::sync::Mutex::new(()),
            service_index: ArcSwap::from_pointee(service_index),
            routes: ArcSwap::from_pointee(routes),
            local_health: ArcSwap::from_pointee(ServiceHealth::default()),
            bootstrap_peers,
            config_path,
            reverse_pool: Arc::new(StreamPool::new()),
            tunnel_pool: Arc::new(StreamPool::new()),
            mux_pool: Arc::new(crate::mux::MuxPool::new()),
            tls: tls.map(Arc::new),
            shutdown,
            in_flight: InFlightTracker::new(),
            tracer: std::sync::OnceLock::new(),
            heartbeat: AtomicU64::new(0),
            incarnation: std::sync::OnceLock::new(),
            departed: Mutex::new(HashMap::new()),
            rejected_config_version: AtomicU64::new(0),
            signer: std::sync::OnceLock::new(),
            relays: Mutex::new(HashMap::new()),
            forgotten: Mutex::new(HashMap::new()),
            own_record: Mutex::new(None),
            peer_slots: Mutex::new(HashMap::new()),
            delta_peers: Mutex::new(HashMap::new()),
            own_beat: Mutex::new(None),
            leaving: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Wrap a newly accepted `TcpStream` with mTLS, within [`CONNECT_TIMEOUT`].
    pub async fn tls_accept(&self, stream: TcpStream) -> anyhow::Result<MeshStream> {
        match &self.tls {
            Some(ctx) => tokio::time::timeout(CONNECT_TIMEOUT, ctx.accept(stream))
                .await
                .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))?,
            None => anyhow::bail!("mTLS is required but tlsDir is not configured"),
        }
    }

    /// Wrap an outbound `TcpStream` with mTLS, within [`CONNECT_TIMEOUT`].
    pub async fn tls_connect(&self, stream: TcpStream) -> anyhow::Result<MeshStream> {
        match &self.tls {
            Some(ctx) => tokio::time::timeout(CONNECT_TIMEOUT, ctx.connect(stream))
                .await
                .map_err(|_| anyhow::anyhow!("TLS handshake timed out"))?,
            None => anyhow::bail!("mTLS is required but tlsDir is not configured"),
        }
    }

    /// Open an mTLS connection to `addr`, TCP connect and handshake within
    /// [`CONNECT_TIMEOUT`].
    ///
    /// With `expect_node`, the server's certificate must name that node:
    /// every node's certificate is signed by the same CA, so without this
    /// check any node at the address (one that took it over, or that gossip
    /// pointed us to) could receive another node's tunnels.
    pub async fn connect_mesh(&self, addr: &str, expect_node: Option<&str>) -> anyhow::Result<MeshStream> {
        use anyhow::Context as _;
        let tcp = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr))
            .await
            .map_err(|_| anyhow::anyhow!("connect to {addr} timed out"))?
            .with_context(|| format!("connect to {addr}"))?;
        crate::io::configure_stream(&tcp);
        let stream = self.tls_connect(tcp).await.with_context(|| format!("TLS connect to {addr}"))?;
        if let Some(expected) = expect_node {
            if stream.peer_cn() != Some(expected) {
                anyhow::bail!(
                    "{addr} is not node {expected}: its certificate names {}",
                    stream.peer_cn().unwrap_or("no one")
                );
            }
        }
        Ok(stream)
    }

    /// The id of the known node advertising `addr`: whose certificate a
    /// connection to `addr` should present. `None` if no node, or more than
    /// one, claims it (then only per-tunnel checks decide who it may serve).
    pub fn node_at(&self, addr: &str) -> Option<String> {
        if addr.is_empty() {
            return None;
        }
        let known = self.known_nodes.load();
        let mut claimants = known.values().filter(|n| n.addr == addr);
        let only = claimants.next()?;
        if claimants.next().is_some() {
            return None;
        }
        Some(only.id.clone())
    }

    pub async fn rebuild_service_index(&self) {
        let dc = self.dynamic_config.read().await;
        let idx = build_service_index(&dc);
        self.service_index.store(Arc::new(idx));
        self.routes.store(Arc::new(build_route_index(&dc)));
    }

    /// Whether `node_id` reports `service_name` as failing its health check.
    ///
    /// Our own services are answered from the local checker; any copy of our
    /// own `NodeInfo` relayed back through gossip may be stale.
    pub fn service_unhealthy(&self, node_id: &str, service_name: &str) -> bool {
        if node_id == self.config.node_id {
            return self.local_health.load().is_unhealthy(service_name);
        }
        self.known_nodes
            .load()
            .get(node_id)
            .is_some_and(|n| n.health.is_unhealthy(service_name))
    }

    /// Advance and return this node's own heartbeat: this process's
    /// incarnation in the high 32 bits, a counter in the low 32. Every
    /// process's incarnation is above the previous one's (it is persisted;
    /// see [`incarnation_for`]), so a restarted node's heartbeats outrank
    /// the ones it gossiped before whatever the clock did meanwhile. Also
    /// orders health reports.
    pub fn next_heartbeat(&self) -> u64 {
        let incarnation = *self.incarnation.get_or_init(unix_seconds);
        let counter = self.heartbeat.fetch_add(1, Ordering::SeqCst) + 1;
        (incarnation << 32) | (counter & 0xFFFF_FFFF)
    }

    /// Remember that `id` departed with heartbeat `heartbeat`.
    pub fn bury(&self, id: &str, heartbeat: u64) {
        self.bury_with(id, heartbeat, None);
    }

    /// Bury `node`, forgotten for silence, keeping it as it was: a node
    /// with no address can't be contacted to see it's back, and its record
    /// may not change for a long time, so only a newer beat can show it.
    pub fn bury_forgotten(&self, node: &NodeInfo) {
        self.bury(&node.id, node.heartbeat);
        if node.signed.is_some() && node.addr.is_empty() {
            if let Some(tomb) = self.departed.lock().unwrap().get_mut(&node.id) {
                tomb.forgotten = Some(node.clone());
            }
        }
    }

    /// For a beat about a node buried by [`Self::bury_forgotten`]: the node,
    /// let back in, if the beat is newer than its last and signed with the
    /// key of its record. It is alive after all, just out of touch for a
    /// while (it can't be reached, and we heard nothing of it).
    pub fn revive_with_beat(&self, beat: &crate::config::SignedBeat) -> Option<NodeInfo> {
        let mut departed = self.departed.lock().unwrap();
        let tomb = departed.get(&beat.id).filter(|t| t.at.elapsed() < TOMBSTONE_TTL)?;
        let node = tomb.forgotten.as_ref().filter(|_| beat.beat > tomb.heartbeat)?;
        let cert = &node.signed.as_ref()?.cert;
        crate::signing::verify_beat(beat, cert).ok()?;
        departed.remove(&beat.id).and_then(|t| t.forgotten)
    }

    /// [`Self::bury`], keeping the node's signed "left" record (at version
    /// `record.0`) to relay.
    pub fn bury_with(&self, id: &str, heartbeat: u64, record: Option<(u64, crate::config::SignedNode)>) {
        let mut departed = self.departed.lock().unwrap();
        departed.retain(|_, t| t.at.elapsed() < TOMBSTONE_TTL);
        let tomb = departed
            .entry(id.to_string())
            .or_insert(Tombstone { heartbeat, at: std::time::Instant::now(), record: None, forgotten: None });
        tomb.heartbeat = tomb.heartbeat.max(heartbeat);
        tomb.at = std::time::Instant::now();
        if let Some(record) = record {
            if tomb.record.as_ref().is_none_or(|(held, _)| record.0 > *held) {
                tomb.record = Some(record);
            }
        }
    }

    /// Signed "left" records of departed nodes: (id, version, record).
    pub fn departed_records(&self) -> Vec<(String, u64, crate::config::SignedNode)> {
        let departed = self.departed.lock().unwrap();
        departed
            .iter()
            .filter(|(_, t)| t.at.elapsed() < TOMBSTONE_TTL)
            .filter_map(|(id, t)| t.record.as_ref().map(|(v, r)| (id.clone(), *v, r.clone())))
            .collect()
    }

    /// Whether gossip about `id` carrying `heartbeat` is just a stale copy
    /// of a node that has since departed. A heartbeat newer than the one it
    /// departed with means it really came back.
    pub fn is_buried(&self, id: &str, heartbeat: u64) -> bool {
        let mut departed = self.departed.lock().unwrap();
        match departed.get(id) {
            Some(t) if t.at.elapsed() >= TOMBSTONE_TTL => {
                departed.remove(id);
                false
            }
            Some(t) => heartbeat <= t.heartbeat,
            None => false,
        }
    }

    /// Forget that `id` departed (it rejoined).
    pub fn unbury(&self, id: &str) {
        self.departed.lock().unwrap().remove(id);
        self.forgotten.lock().unwrap().remove(id);
    }

    /// Remember the last address of `id`, forgotten for silence, so it is
    /// retried for [`RECONNECT_FOR`]. (Not for a node that left: it said so.)
    pub fn remember_forgotten(&self, id: &str, addr: &str) {
        if !addr.is_empty() {
            self.forgotten.lock().unwrap().insert(id.to_string(), (addr.to_string(), std::time::Instant::now()));
        }
    }

    /// Last addresses of nodes forgotten within [`RECONNECT_FOR`].
    pub fn forgotten_addrs(&self) -> Vec<String> {
        let mut forgotten = self.forgotten.lock().unwrap();
        forgotten.retain(|_, (_, at)| at.elapsed() < RECONNECT_FOR);
        forgotten.values().map(|(addr, _)| addr.clone()).collect()
    }

    /// Take one of `peer`'s at most `limit` connections of `kind`, or `None`
    /// if it already holds them all.
    pub fn try_peer_slot(self: &Arc<Self>, kind: &'static str, peer: &str, limit: usize) -> Option<PeerSlot> {
        let key = (kind, peer.to_string());
        let mut slots = self.peer_slots.lock().unwrap();
        let count = slots.entry(key.clone()).or_insert(0);
        if *count >= limit {
            return None;
        }
        *count += 1;
        Some(PeerSlot { state: Arc::clone(self), key })
    }

    /// Note a reverse connection registered with `relay` (a node id); the
    /// returned guard marks it gone when dropped.
    pub fn relay_registered(self: &Arc<Self>, relay: &str) -> RelayRegistration {
        let mut relays = self.relays.lock().unwrap();
        let entry = relays.entry(relay.to_string()).or_insert((0, std::time::Instant::now()));
        entry.0 += 1;
        entry.1 = std::time::Instant::now();
        RelayRegistration { state: Arc::clone(self), relay: relay.to_string() }
    }

    /// The relays this node uses (sorted ids): ones it holds a reverse
    /// connection to, or registered one with in the last [`RELAY_GRACE`].
    pub fn current_relays(&self) -> Vec<String> {
        let mut relays = self.relays.lock().unwrap();
        relays.retain(|_, (live, at)| *live > 0 || at.elapsed() < RELAY_GRACE);
        let mut ids: Vec<String> = relays.keys().cloned().collect();
        ids.sort();
        ids
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

fn build_route_index(dc: &DynamicConfig) -> HashMap<String, Vec<Route>> {
    let mut idx: HashMap<String, Vec<Route>> = HashMap::new();
    for route in &dc.routes {
        idx.entry(route.service.clone()).or_default().push(route.clone());
    }
    idx
}

/// Helpers for tests that need real mTLS between nodes.
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;

    /// A state with a real TLS identity `id`, its CA kept in `dir` (states
    /// made with the same `dir` trust each other).
    pub fn tls_state(dir: &std::path::Path, id: &str) -> Arc<SharedState> {
        tls_state_with(dir, Config { node_id: id.into(), ..Config::default() }, String::new())
    }

    /// Like [`tls_state`], for a full config (persisted to `config_path`).
    pub fn tls_state_with(dir: &std::path::Path, cfg: Config, config_path: String) -> Arc<SharedState> {
        let material = crate::pki::load_or_create(dir.to_str().unwrap(), &cfg.node_id).unwrap();
        let tls = TlsContext::new(&material).unwrap();
        let state = SharedState::new(cfg, config_path, CancellationToken::new(), Some(tls));
        let _ = state.signer.set(crate::signing::Signer::from_material(&material).unwrap());
        state
    }

    /// An mTLS connection from `client` to `server`: (client end, server end).
    pub async fn mesh_pair(client: &SharedState, server: &SharedState) -> (MeshStream, MeshStream) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (a, b) = tokio::join!(
            async { client.tls_connect(TcpStream::connect(addr).await.unwrap()).await.unwrap() },
            async { server.tls_accept(listener.accept().await.unwrap().0).await.unwrap() },
        );
        (a, b)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test(start_paused = true)]
    async fn wait_zero_returns_when_the_last_guard_drops() {
        let tracker = InFlightTracker::new();
        assert!(tracker.wait_zero(Duration::from_secs(30)).await, "nothing in flight");

        let guards = vec![tracker.track(), tracker.track()];
        let start = Instant::now();
        let waiter = {
            let tracker = tracker.clone();
            tokio::spawn(async move { tracker.wait_zero(Duration::from_secs(30)).await })
        };
        tokio::time::sleep(Duration::from_secs(2)).await;
        drop(guards);
        assert!(waiter.await.unwrap());
        assert_eq!(start.elapsed().as_secs(), 2, "woken at the drop, not the timeout");
    }

    #[tokio::test(start_paused = true)]
    async fn wait_zero_times_out_while_work_remains() {
        let tracker = InFlightTracker::new();
        let _guard = tracker.track();
        assert!(!tracker.wait_zero(Duration::from_secs(30)).await);
        assert_eq!(tracker.current(), 1);
    }

    #[test]
    fn backoff_doubles_up_to_the_cap_and_resets_on_success() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(8));
        let now = Instant::now();
        assert!(b.ready("a", now));
        let waits: Vec<u64> = (0..6).map(|_| b.failed("a", now).as_millis() as u64).collect();
        let full = [1000, 2000, 4000, 8000, 8000, 8000];
        for (wait, full) in waits.iter().zip(full) {
            assert!((full * 3 / 4..=full).contains(wait), "{waits:?}");
        }
        assert!(!b.ready("a", now));
        assert!(b.ready("a", now + Duration::from_secs(8)));
        assert!(b.ready("b", now), "keys back off independently");
        b.succeeded("a");
        assert!(b.ready("a", now));
    }

    #[tokio::test]
    async fn pools_skip_and_prune_connections_their_peer_closed() {
        let pool: StreamPool<tokio::io::DuplexStream> = StreamPool::new();
        let (live, _live_peer) = tokio::io::duplex(64);
        let (dead, dead_peer) = tokio::io::duplex(64);
        pool.push("n".into(), live);
        pool.push("n".into(), dead);
        drop(dead_peer);

        // The dead one was pushed last (LIFO) but is skipped.
        let mut taken = pool.try_take("n").expect("the live one");
        assert!(!idle_stream_closed(&mut taken));
        assert!(pool.try_take("n").is_none(), "the dead one was dropped, not returned");

        let (dead, dead_peer) = tokio::io::duplex(64);
        pool.push("n".into(), dead);
        pool.push("n".into(), taken);
        drop(dead_peer);
        assert_eq!(pool.prune(), 1);
        assert_eq!(pool.count("n"), 1);
    }

    use super::test_support::tls_state;

    #[tokio::test]
    async fn connect_mesh_requires_the_expected_node() {
        let dir = std::env::temp_dir().join(format!("stouter_ident_{}_{}", std::process::id(), rand::random::<u64>()));
        let server = tls_state(&dir, "a");
        let client = tls_state(&dir, "b");
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            loop {
                let (tcp, _) = listener.accept().await.unwrap();
                let server = server.clone();
                tokio::spawn(async move {
                    if let Ok(mut s) = server.tls_accept(tcp).await {
                        let mut buf = [0u8; 1];
                        let _ = tokio::io::AsyncReadExt::read(&mut s, &mut buf).await;
                    }
                });
            }
        });

        let ok = client.connect_mesh(&addr, Some("a")).await.unwrap();
        assert_eq!(ok.peer_cn(), Some("a"));
        let err = client.connect_mesh(&addr, Some("c")).await.err().expect("wrong node refused").to_string();
        assert!(err.contains("certificate names a"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test(start_paused = true)]
    async fn connect_mesh_gives_up_on_a_silent_peer() {
        let dir = std::env::temp_dir().join(format!("stouter_silent_{}_{}", std::process::id(), rand::random::<u64>()));
        let client = tls_state(&dir, "b");
        // Accepts TCP but never answers the TLS handshake.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let _hold = tokio::spawn(async move {
            let (_tcp, _) = listener.accept().await.unwrap();
            std::future::pending::<()>().await;
        });
        let start = Instant::now();
        let err = client.connect_mesh(&addr, None).await.err().expect("silent peer refused");
        assert!(format!("{err:#}").contains("timed out"), "{err:#}");
        // Connect and handshake each get CONNECT_TIMEOUT. (Paused time also
        // advances while real socket I/O is in flight, so no exact value.)
        assert!(start.elapsed() <= 2 * CONNECT_TIMEOUT, "{:?}", start.elapsed());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_restart_outranks_earlier_heartbeats_even_with_the_clock_set_back() {
        let dir = std::env::temp_dir().join(format!("stouter_incarnation_{}_{}", std::process::id(), rand::random::<u64>()));
        std::fs::create_dir_all(&dir).unwrap();
        let config = dir.join("stouter.json").to_str().unwrap().to_string();
        // The previous process ran with the clock a long way ahead; now it's
        // been set back.
        let ahead = unix_seconds() + 1_000_000;
        std::fs::write(format!("{config}.state"), format!(r#"{{"incarnation":{ahead}}}"#)).unwrap();
        let last_before_restart = (ahead << 32) | 0xFFFF_FFFF;

        let state = SharedState::new(Config::default(), config.clone(), CancellationToken::new(), None);
        state.incarnation.set(incarnation_for(&config)).unwrap();
        let first = state.next_heartbeat();
        assert!(first > last_before_restart);
        assert!(state.next_heartbeat() > first);
        // Saved for the next start.
        assert_eq!(incarnation_for(&config), ahead + 2);

        // Also above the wall-clock-millisecond heartbeats of older versions.
        let now_ms = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64;
        let fresh = SharedState::new(Config::default(), String::new(), CancellationToken::new(), None);
        assert!(fresh.next_heartbeat() > now_ms);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
