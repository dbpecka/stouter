use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio_util::compat::{Compat, FuturesAsyncReadCompatExt};
use tracing::info;

use crate::tls::MeshStream;

use super::balancer::{Backend, Balancer, Pick};
use super::http::HttpProxy;
use crate::config::{NodeInfo, Protocol};
use crate::gossip;
use crate::gossip::messages::Message;
use crate::state::SharedState;

/// Timeout for waiting on a reverse pool connection when the node has no
/// direct or relay address (NAT-only).
const REVERSE_POOL_TIMEOUT: Duration = Duration::from_secs(5);

/// Maximum number of distinct nodes to try for a single client connection
/// before giving up. Bounds retry amplification during a hosting outage.
pub(super) const MAX_TUNNEL_ATTEMPTS: usize = 3;

/// A node not confirmed reachable within this window is treated as unhealthy
/// and excluded from load balancing. The gossip sync loop probes every peer
/// every 10s, so this tolerates roughly three missed cycles before ejection.
const NODE_STALE_AFTER: Duration = Duration::from_secs(30);

/// Run a local TCP proxy for a service, routing across all available nodes.
///
/// Binds on `127.0.0.1:0`, sends the assigned port back through `port_tx`,
/// then accepts connections and forwards each one through an authenticated
/// tunnel to a node hosting the service. Nodes are selected by smooth
/// weighted round-robin, nodes whose tunnel setup keeps failing are ejected
/// from rotation for a while, and nodes at their `maxConnections` limit are
/// skipped; when every node is full the connection is closed at once
/// (see [`Balancer`]).
pub async fn run_proxy(
    service_name: String,
    state: Arc<SharedState>,
    port_tx: oneshot::Sender<u16>,
) -> Result<()> {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind proxy listener")?;

    let local_port = listener.local_addr()?.port();
    let _ = port_tx.send(local_port);

    info!("Proxy for {} on 127.0.0.1:{}", service_name, local_port);

    let balancer = Arc::new(Balancer::new());
    let http = HttpProxy::new(service_name.clone(), state.clone(), balancer.clone());

    loop {
        let (client, _) = listener.accept().await.context("proxy accept")?;
        client.set_nodelay(true).ok();

        // Checked per connection so a protocol change in the config applies
        // to new connections without restarting the proxy.
        if service_is_http(&service_name, &state) {
            tokio::spawn(http.clone().serve_connection(client));
            continue;
        }

        let service_name = service_name.clone();
        let state = state.clone();
        let balancer = balancer.clone();
        tokio::spawn(async move {
            serve_client(client, service_name, state, balancer).await;
        });
    }
}

/// Forward one client connection, trying up to [`MAX_TUNNEL_ATTEMPTS`]
/// distinct nodes until a tunnel is established.
async fn serve_client(
    client: TcpStream,
    service_name: String,
    state: Arc<SharedState>,
    balancer: Arc<Balancer>,
) {
    let backends = find_service_nodes(&service_name, &state);
    if backends.is_empty() {
        metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "no_nodes").increment(1);
        tracing::warn!("no nodes available for service {service_name}");
        return;
    }

    let max_attempts = backends.len().min(MAX_TUNNEL_ATTEMPTS);
    let start = Instant::now();
    let mut tried: Vec<String> = Vec::with_capacity(max_attempts);
    let mut last_err: Option<anyhow::Error> = None;

    // The client stream is not touched until a tunnel is up, so a failed
    // setup can safely be retried against another node.
    while tried.len() < max_attempts {
        // `_slot` holds this connection's place under the node's connection
        // limit until the iteration ends: at once on failure, or after
        // proxying on success.
        let (idx, _slot) = match balancer.pick(&backends, &tried, Instant::now()) {
            Pick::Chosen { index, slot } => (index, slot),
            Pick::AllTried => break,
            Pick::AtCapacity if last_err.is_none() => {
                metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "circuit_open").increment(1);
                tracing::debug!("rejecting connection to {service_name}: every backend is at its connection limit");
                return;
            }
            Pick::AtCapacity => break,
        };
        let node = &backends[idx].node;
        tried.push(node.id.clone());

        metrics::counter!("stouter_tunnel_requests_total", "service" => service_name.clone(), "node_id" => node.id.clone()).increment(1);

        let tunnel = match establish_tunnel(node, service_name.clone(), &state).await {
            Ok(tunnel) => tunnel,
            Err(e) => {
                tracing::debug!("tunnel to node {} failed, trying another node: {e}", node.id);
                note_tunnel_failure(&balancer, &service_name, &node.id, &e);
                last_err = Some(e);
                continue;
            }
        };
        balancer.record_success(&node.id);

        let result = crate::io::proxy_bidirectional(client, tunnel).await;
        match result {
            Ok(()) => {
                metrics::histogram!("stouter_proxy_duration_seconds", "service" => service_name.clone())
                    .record(start.elapsed().as_secs_f64());
            }
            Err(e) => {
                metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "connection_error").increment(1);
                tracing::warn!("proxy connection error to node {}: {e}", node.id);
            }
        }
        return;
    }

    metrics::counter!("stouter_tunnel_errors_total", "service" => service_name.clone(), "reason" => "node_unreachable").increment(1);
    match last_err {
        Some(e) => tracing::warn!(
            "all {} tunnel attempt(s) failed for service {service_name}: {e}",
            tried.len()
        ),
        None => tracing::warn!("no tunnel attempts made for service {service_name}"),
    }
}

/// Record a failed tunnel setup with the balancer, logging and counting an
/// ejection if it caused one.
pub(super) fn note_tunnel_failure(
    balancer: &Balancer,
    service_name: &str,
    node_id: &str,
    err: &dyn std::fmt::Display,
) {
    if let Some(duration) = balancer.record_failure(node_id, Instant::now()) {
        metrics::counter!("stouter_outlier_ejections_total", "service" => service_name.to_string(), "node_id" => node_id.to_string()).increment(1);
        tracing::warn!(
            "ejecting node {node_id} from service {service_name} for {}s after repeated tunnel failures: {err}",
            duration.as_secs()
        );
    }
}

/// Whether `service_name` should be proxied as HTTP: every instance must say
/// so, so a half-migrated service falls back to byte forwarding.
pub(super) fn service_is_http(service_name: &str, state: &SharedState) -> bool {
    let idx = state.service_index.load();
    let mut instances = idx.iter().filter(|((_, svc), _)| svc == service_name).peekable();
    instances.peek().is_some() && instances.all(|(_, svc)| svc.protocol == Protocol::Http)
}

/// Find the nodes hosting a given service, ordered and health-filtered.
///
/// Scans the service index for entries matching `service_name`, resolves each
/// node_id to its current `NodeInfo`, sorts by node id so balancer state maps
/// to a stable node across connections, then drops nodes whose gossip
/// liveness marker is stale and nodes whose health check reports the service
/// down.
///
/// Each filter fails open: if it would drop every node (e.g. a cluster-wide
/// gossip hiccup, or a health check misconfigured everywhere), it is skipped
/// so per-connection retry can still find a working backend.
pub(super) fn find_service_nodes(service_name: &str, state: &SharedState) -> Vec<Backend> {
    let idx = state.service_index.load();
    let kn = state.known_nodes.load();

    let mut backends: Vec<Backend> = idx
        .iter()
        .filter(|((_, svc), _)| svc == service_name)
        .filter_map(|((node_id, _), svc)| {
            kn.get(node_id).map(|node| Backend {
                node: node.clone(),
                weight: svc.weight,
                max_connections: svc.max_connections,
                request_timeout: svc.request_timeout_ms.map(Duration::from_millis),
            })
        })
        .collect();

    backends.sort_by(|a, b| a.node.id.cmp(&b.node.id));

    let now = Instant::now();
    let live = prefer(backends, |b| {
        b.node
            .last_seen
            .is_some_and(|seen| now.duration_since(seen) <= NODE_STALE_AFTER)
    });
    prefer(live, |b| !state.service_unhealthy(&b.node.id, service_name))
}

/// The items of `items` satisfying `keep`, or all of them if none do.
fn prefer<T>(items: Vec<T>, keep: impl Fn(&T) -> bool) -> Vec<T> {
    if items.iter().any(&keep) {
        items.into_iter().filter(|i| keep(i)).collect()
    } else {
        items
    }
}

/// Send a tunnel request on a stream, read the status byte, and bail on error.
async fn send_and_check_tunnel<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    msg: &Message,
) -> Result<()> {
    gossip::send_message(stream, msg)
        .await
        .context("send tunnel request")?;

    let mut status = [0u8; 1];
    stream
        .read_exact(&mut status)
        .await
        .context("read tunnel status byte")?;

    if status[0] != 0x00 {
        let mut len_buf = [0u8; 4];
        stream
            .read_exact(&mut len_buf)
            .await
            .context("read tunnel error length")?;
        let err_len = u32::from_be_bytes(len_buf) as usize;
        let mut err_bytes = vec![0u8; err_len];
        stream
            .read_exact(&mut err_bytes)
            .await
            .context("read tunnel error message")?;
        let msg = String::from_utf8_lossy(&err_bytes);
        bail!("node rejected tunnel: {msg}");
    }

    Ok(())
}

/// An established tunnel to a remote node, ready for proxying.
pub(super) enum Tunnel {
    /// A stream on a pooled yamux session.
    Mux(Compat<yamux::Stream>),
    /// A dedicated mTLS connection (boxed: a TLS stream is large).
    Mesh(Box<MeshStream>),
}

/// Establish an authenticated tunnel to `service_name` on `node`.
///
/// Connection acquisition priority:
/// 1. Mux pool (open stream on existing yamux session)
/// 2. Reverse pool (instant check — free path to NAT'd nodes)
/// 3. Reverse pool with wait (for NAT-only nodes with no address)
/// 4. Cold TCP connect as last resort
impl AsyncRead for Tunnel {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Tunnel::Mux(s) => std::pin::Pin::new(s).poll_read(cx, buf),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
        }
    }
}

impl AsyncWrite for Tunnel {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Tunnel::Mux(s) => std::pin::Pin::new(s).poll_write(cx, buf),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Tunnel::Mux(s) => std::pin::Pin::new(s).poll_flush(cx),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Tunnel::Mux(s) => std::pin::Pin::new(s).poll_shutdown(cx),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
        }
    }
}

pub(super) async fn establish_tunnel(
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
) -> Result<Tunnel> {
    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    let msg = Message::TunnelRequest {
        node_id: node.id.clone(),
        service_name,
        timestamp_ms,
    };

    let connect_addr = node.relay.as_deref().unwrap_or(&node.addr);
    let connect_start = Instant::now();

    // --- Try mux pool first (multiplexed yamux stream) ---
    if !connect_addr.is_empty() {
        if let Some(yamux_stream) = state.mux_pool.open_stream(connect_addr).await {
            let mut compat = yamux_stream.compat();
            send_and_check_tunnel(&mut compat, &msg).await?;
            metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "mux")
                .record(connect_start.elapsed().as_secs_f64());
            return Ok(Tunnel::Mux(compat));
        }
    }

    // --- Fallback: legacy single-connection path ---
    let method;
    let mut node_stream: MeshStream = if let Some(stream) = state.reverse_pool.try_take(&node.id) {
        method = "reverse";
        stream
    } else if let Some(stream) = state.tunnel_pool.try_take(connect_addr) {
        method = "pool";
        stream
    } else if connect_addr.is_empty() {
        method = "reverse";
        state
            .reverse_pool
            .take(&node.id, REVERSE_POOL_TIMEOUT)
            .await
            .with_context(|| {
                format!("node {} not reachable: no address and no reverse connections", node.id)
            })?
    } else {
        method = "cold";
        let tcp = TcpStream::connect(connect_addr)
            .await
            .with_context(|| format!("connect to {connect_addr}"))?;
        tcp.set_nodelay(true).ok();
        state
            .tls_connect(tcp)
            .await
            .with_context(|| format!("TLS connect to {connect_addr}"))?
    };

    send_and_check_tunnel(&mut node_stream, &msg).await?;
    metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => method)
        .record(connect_start.elapsed().as_secs_f64());

    Ok(Tunnel::Mesh(Box::new(node_stream)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DynamicConfig, Service, ServiceGroup, ServiceHealth};
    use tokio_util::sync::CancellationToken;

    async fn state_with(nodes: &[(&str, bool, &[&str])]) -> Arc<SharedState> {
        let services = nodes
            .iter()
            .map(|(id, _, _)| Service {
                name: "api".into(),
                node_id: id.to_string(),
                node_port: 3000,
                domains: vec![],
                weight: 1,
                max_connections: None,
                protocol: crate::config::Protocol::Tcp,
                request_timeout_ms: None,
                health_path: None,
            })
            .collect();
        let cfg = Config {
            node_id: "sub".into(),
            dynamic_config: DynamicConfig {
                version: 1,
                service_groups: vec![ServiceGroup { name: "web".into(), services }],
            },
            ..Config::default()
        };
        let state = SharedState::new(cfg, String::new(), CancellationToken::new(), None);
        state
            .update_known_nodes(|kn| {
                for (id, live, unhealthy) in nodes {
                    kn.insert(
                        id.to_string(),
                        NodeInfo {
                            id: id.to_string(),
                            addr: format!("{id}:1"),
                            relay: None,
                            last_seen: live.then(Instant::now),
                            health: ServiceHealth {
                                version: 1,
                                unhealthy: unhealthy.iter().map(|s| s.to_string()).collect(),
                            },
                        },
                    );
                }
            })
            .await;
        state
    }

    fn ids(backends: &[Backend]) -> Vec<&str> {
        backends.iter().map(|b| b.node.id.as_str()).collect()
    }

    #[tokio::test]
    async fn skips_nodes_reporting_service_unhealthy() {
        let state = state_with(&[("a", true, &["api"]), ("b", true, &[]), ("c", true, &["other"])]).await;
        assert_eq!(ids(&find_service_nodes("api", &state)), ["b", "c"]);
    }

    #[tokio::test]
    async fn all_unhealthy_fails_open() {
        let state = state_with(&[("a", true, &["api"]), ("b", true, &["api"])]).await;
        assert_eq!(ids(&find_service_nodes("api", &state)), ["a", "b"]);
    }

    #[tokio::test]
    async fn stale_nodes_dropped_before_health_filter() {
        // b is healthy but stale; a is live but unhealthy. Liveness wins, then
        // the health filter fails open over the live set.
        let state = state_with(&[("a", true, &["api"]), ("b", false, &[])]).await;
        assert_eq!(ids(&find_service_nodes("api", &state)), ["a"]);
    }

    #[tokio::test]
    async fn own_services_use_local_health_not_relayed_copy() {
        let state = state_with(&[("sub", true, &[]), ("b", true, &[])]).await;
        state.local_health.store(Arc::new(ServiceHealth {
            version: 1,
            unhealthy: vec!["api".into()],
        }));
        assert_eq!(ids(&find_service_nodes("api", &state)), ["b"]);
    }
}
