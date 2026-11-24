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
use super::http::HttpProxies;
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

/// Run a local TCP proxy for a service, routing across all available nodes.
///
/// Binds on `127.0.0.1:{port}` (a random port when `None`), sends the bound
/// port, or the bind error, back through `port_tx`,
/// then accepts connections and forwards each one through an authenticated
/// tunnel to a node hosting the service. Nodes are selected by smooth
/// weighted round-robin, nodes whose tunnel setup keeps failing are ejected
/// from rotation for a while, and nodes at their `maxConnections` limit are
/// skipped; when every node is full the connection is closed at once
/// (see [`Balancer`]).
pub async fn run_proxy(
    service_name: String,
    state: Arc<SharedState>,
    proxies: Arc<HttpProxies>,
    port: Option<u16>,
    port_tx: oneshot::Sender<std::result::Result<u16, String>>,
) -> Result<()> {
    let listener = match TcpListener::bind(("127.0.0.1", port.unwrap_or(0))).await {
        Ok(l) => l,
        Err(e) => {
            let _ = port_tx.send(Err(format!("bind 127.0.0.1:{}: {e}", port.unwrap_or(0))));
            return Ok(());
        }
    };

    let local_port = listener.local_addr()?.port();
    let _ = port_tx.send(Ok(local_port));

    info!("Proxy for {} on 127.0.0.1:{}", service_name, local_port);

    // Shared with the httpListen dispatcher, so both entry points use one
    // balancer (and one set of connection limits and ejections).
    let http = proxies.get(&service_name);
    let balancer = http.balancer();

    let what = format!("proxy for {service_name}");
    let mut backoff = crate::io::AcceptBackoff::default();
    loop {
        // Stop accepting at shutdown; connections already accepted are
        // tracked as in-flight and drained. A failed accept (such as no file
        // descriptors left) is waited out, so the proxy keeps its port.
        let (client, _) = tokio::select! {
            accepted = listener.accept() => match accepted {
                Ok(accepted) => {
                    backoff.succeeded(&what);
                    accepted
                }
                Err(e) => {
                    if backoff.failed(&what, &e, &state.shutdown).await {
                        continue;
                    }
                    return Ok(());
                }
            },
            _ = state.shutdown.cancelled() => return Ok(()),
        };
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
        let in_flight = state.in_flight.track();
        tokio::spawn(async move {
            let _in_flight = in_flight;
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
    let all = service_backends(&service_name, &state);
    balancer.retain_nodes(&all);
    let backends = prefer_healthy(all, &service_name, &state);
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
#[cfg(test)]
fn find_service_nodes(service_name: &str, state: &SharedState) -> Vec<Backend> {
    prefer_healthy(service_backends(service_name, state), service_name, state)
}

/// Every known node hosting `service_name`, sorted by node id so balancer
/// state maps to a stable node across connections. Unfiltered.
pub(super) fn service_backends(service_name: &str, state: &SharedState) -> Vec<Backend> {
    let idx = state.service_index.load();
    let kn = state.known_nodes.load();

    let mut backends: Vec<Backend> = idx
        .iter()
        .filter(|((_, svc), _)| svc == service_name)
        .filter_map(|((node_id, _), svc)| {
            // A node never lists itself among its peers, but an instance on
            // this node (`nodeSubscribe`) is a backend too, reached directly.
            let node = kn.get(node_id).cloned().or_else(|| own_node(state).filter(|own| own.id == *node_id))?;
            Some(Backend {
                node,
                weight: svc.weight,
                max_connections: svc.max_connections,
                request_timeout: svc.request_timeout_ms.map(Duration::from_millis),
                labels: svc.labels.clone(),
            })
        })
        .collect();

    backends.sort_by(|a, b| a.node.id.cmp(&b.node.id));
    backends
}

/// This node, as a backend for services it hosts itself: only a
/// `nodeSubscribe` node both hosts services and proxies them. It is live
/// (it's us); its services' health comes from its own checker.
pub(super) fn own_node(state: &SharedState) -> Option<NodeInfo> {
    matches!(state.config.mode, crate::config::Mode::NodeSubscribe).then(|| NodeInfo {
        id: state.config.node_id.clone(),
        last_seen: Some(Instant::now()),
        heard_at: Some(Instant::now()),
        ..Default::default()
    })
}

/// Drop nodes whose gossip liveness marker is stale, then nodes whose health
/// check reports the service down.
///
/// Each filter fails open: if it would drop every node (e.g. a cluster-wide
/// gossip hiccup, or a health check misconfigured everywhere), it is skipped
/// so per-connection retry can still find a working backend.
pub(super) fn prefer_healthy(backends: Vec<Backend>, service_name: &str, state: &SharedState) -> Vec<Backend> {
    let now = Instant::now();
    let live = prefer(backends, |b| crate::state::node_is_live(&b.node, now));
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
        let msg = crate::node::tunnel::read_tunnel_error(stream).await?;
        bail!("node rejected tunnel: {msg}");
    }

    Ok(())
}

/// An established tunnel to a remote node, ready for proxying.
pub(super) enum Tunnel {
    /// A stream on a pooled yamux session, and a handle keeping that
    /// session running while the stream is in use.
    Mux(Compat<yamux::Stream>, #[allow(dead_code)] crate::mux::MuxHandle),
    /// A dedicated mTLS connection (boxed: a TLS stream is large).
    Mesh(Box<MeshStream>),
    /// A service on this node itself (`nodeSubscribe`), reached directly.
    Local(TcpStream),
}

impl AsyncRead for Tunnel {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Tunnel::Mux(s, _) => std::pin::Pin::new(s).poll_read(cx, buf),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_read(cx, buf),
            Tunnel::Local(s) => std::pin::Pin::new(s).poll_read(cx, buf),
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
            Tunnel::Mux(s, _) => std::pin::Pin::new(s).poll_write(cx, buf),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_write(cx, buf),
            Tunnel::Local(s) => std::pin::Pin::new(s).poll_write(cx, buf),
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Tunnel::Mux(s, _) => std::pin::Pin::new(s).poll_flush(cx),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_flush(cx),
            Tunnel::Local(s) => std::pin::Pin::new(s).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Tunnel::Mux(s, _) => std::pin::Pin::new(s).poll_shutdown(cx),
            Tunnel::Mesh(s) => std::pin::Pin::new(s.as_mut()).poll_shutdown(cx),
            Tunnel::Local(s) => std::pin::Pin::new(s).poll_shutdown(cx),
        }
    }
}

/// Time allowed to set up a tunnel, from picking a connection to the node's
/// answer. A node that stopped responding otherwise hangs the client for as
/// long as TCP takes to give up (many minutes), and is never ejected. Above
/// the relay's own wait for a reverse connection, so a relayed setup can
/// report why it failed.
const TUNNEL_SETUP_TIMEOUT: Duration = Duration::from_secs(15);

pub(super) async fn establish_tunnel(
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
) -> Result<Tunnel> {
    establish_tunnel_within(node, service_name, state, TUNNEL_SETUP_TIMEOUT).await
}

/// [`establish_tunnel`], within `budget` (at most [`TUNNEL_SETUP_TIMEOUT`]):
/// a NAT'd node's relays share it (see [`RELAY_ROUTE_TIMEOUT`]), so a short
/// budget still leaves the next relay its turn.
pub(super) async fn establish_tunnel_within(
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
    budget: Duration,
) -> Result<Tunnel> {
    let budget = budget.min(TUNNEL_SETUP_TIMEOUT);
    let deadline = tokio::time::Instant::now() + budget;
    tokio::time::timeout(budget, establish_tunnel_untimed(node, service_name, state, deadline))
        .await
        .map_err(|_| anyhow::anyhow!("tunnel setup to node {} timed out after {} ms", node.id, budget.as_millis()))?
}

/// Where to connect for a tunnel to `node`, and the node whose certificate
/// must answer there: the first of [`tunnel_routes`]. `None`: no address and
/// no usable relay, so only a reverse connection the node holds to us can
/// reach it.
pub(crate) fn tunnel_route(node: &NodeInfo, state: &SharedState) -> Option<(String, Option<String>)> {
    tunnel_routes(node, state).into_iter().next()
}

/// Every way to reach `node` for a tunnel, best first: where to connect, and
/// the node whose certificate must answer there.
///
/// A node with an address is reached there, and only that node may answer:
/// whoever else gossip says is at the address, a tunnel for `node` never
/// goes to them. A NAT'd node that signs its record is reached only through
/// the relays it lists there, live ones first, each ranked by rendezvous
/// hashing on this subscriber, the node and the relay. Each subscriber sticks
/// to one relay per node, while across subscribers and nodes the tunnels,
/// and the reverse connections each node keeps at its relays, are used
/// evenly; the others are tried if it fails. An older NAT'd node, known from
/// unsigned gossip, through its gossiped relay.
pub(crate) fn tunnel_routes(node: &NodeInfo, state: &SharedState) -> Vec<(String, Option<String>)> {
    if !node.addr.is_empty() {
        return vec![(node.addr.clone(), Some(node.id.clone()))];
    }
    if node.signed.is_some() {
        let known = state.known_nodes.load();
        let now = Instant::now();
        let mut relays: Vec<&NodeInfo> = node
            .relays
            .iter()
            .filter_map(|id| known.get(id))
            .filter(|relay| !relay.addr.is_empty())
            .collect();
        let chooser = format!("{}\0{}", state.config.node_id, node.id);
        relays.sort_by_key(|r| {
            (
                std::cmp::Reverse(crate::state::node_is_live(r, now)),
                // One that just failed to answer in time goes after the
                // others for a while (gossip takes half a minute to mark a
                // dead one stale).
                state.relay_lagging(&r.id),
                std::cmp::Reverse(crate::node::reverse::rendezvous_score(&chooser, &r.id)),
            )
        });
        return relays.iter().map(|r| (r.addr.clone(), Some(r.id.clone()))).collect();
    }
    node.relay.iter().map(|relay| (relay.clone(), state.node_at(relay))).collect()
}

/// For a NAT'd node with several relays: the most time to try each but the
/// last before moving on to the next, within the one tunnel setup (less
/// when the setup's budget is short: each relay left gets an equal share).
/// A relay that went down, or whose host stopped answering, costs this
/// much, not the client's tunnel; and it is tried last for a while (see
/// `SharedState::relay_lagging`).
const RELAY_ROUTE_TIMEOUT: Duration = Duration::from_secs(5);

async fn establish_tunnel_untimed(
    node: &NodeInfo,
    service_name: String,
    state: &Arc<SharedState>,
    deadline: tokio::time::Instant,
) -> Result<Tunnel> {
    // Our own instance (a `nodeSubscribe` node): no tunnel needed.
    if node.id == state.config.node_id {
        let connect_start = Instant::now();
        let stream = crate::node::tunnel::connect_local_service(&service_name, state).await.map_err(anyhow::Error::msg)?;
        metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "local")
            .record(connect_start.elapsed().as_secs_f64());
        return Ok(Tunnel::Local(stream));
    }

    let timestamp_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system time before Unix epoch")?
        .as_millis() as u64;

    // A NAT'd node is reached through a relay or a reverse connection,
    // where the certificate it sees is the relay's: sign the request, so it
    // knows who asks (see `crate::authz`). Directly, our certificate says.
    // Signed afresh for each route tried: a request one relay delivered, but
    // answered too slowly, would otherwise be refused through the next as a
    // replay of its nonce.
    let request = || Message::TunnelRequest {
        node_id: node.id.clone(),
        service_name: service_name.clone(),
        timestamp_ms,
        caller: if node.addr.is_empty() {
            state.signer.get().and_then(|signer| {
                crate::signing::sign_tunnel_request(signer, &node.id, &service_name, timestamp_ms)
                    .map_err(|e| tracing::warn!("can't sign the tunnel request to {}: {e:#}", node.id))
                    .ok()
            })
        } else {
            None
        },
    };
    let msg = request();
    let connect_start = Instant::now();

    // A reverse connection the node holds to us: it registered it with its
    // own certificate, so it is the right node.
    if let Some(mut stream) = state.reverse_pool.try_take(&node.id) {
        send_and_check_tunnel(&mut stream, &msg).await?;
        metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "reverse")
            .record(connect_start.elapsed().as_secs_f64());
        return Ok(Tunnel::Mesh(Box::new(stream)));
    }

    let routes = tunnel_routes(node, state);
    if routes.is_empty() {
        // No address and no usable relay: wait for the node to give us a
        // reverse connection.
        let mut stream = state
            .reverse_pool
            .take(&node.id, REVERSE_POOL_TIMEOUT)
            .await
            .with_context(|| format!("node {} not reachable: no address, relay or reverse connection", node.id))?;
        send_and_check_tunnel(&mut stream, &msg).await?;
        metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "reverse")
            .record(connect_start.elapsed().as_secs_f64());
        return Ok(Tunnel::Mesh(Box::new(stream)));
    }
    let mut last_err = None;
    for (i, (addr, expect)) in routes.iter().enumerate() {
        let fresh;
        let msg = if i == 0 {
            &msg
        } else {
            fresh = request();
            &fresh
        };
        let attempt = tunnel_via(addr, expect.as_deref(), msg, state, connect_start);
        let result = if i + 1 < routes.len() {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            let slice = RELAY_ROUTE_TIMEOUT.min(left / (routes.len() - i) as u32);
            match tokio::time::timeout(slice, attempt).await {
                Ok(result) => result,
                Err(_) => {
                    if let Some(relay) = expect {
                        state.note_relay_lagging(relay);
                    }
                    Err(anyhow::anyhow!("no answer through {addr} within {} ms", slice.as_millis()))
                }
            }
        } else {
            attempt.await
        };
        match result {
            Ok(tunnel) => return Ok(tunnel),
            Err(e) => {
                if i + 1 < routes.len() {
                    tracing::debug!("tunnel to node {} through {addr} failed, trying its next relay: {e:#}", node.id);
                }
                last_err = Some(e);
            }
        }
    }
    Err(last_err.expect("at least one route"))
}

/// A tunnel through `addr`, where `expect` (if given) must answer: on a mux
/// session, else a warm pooled connection, else a new one.
async fn tunnel_via(
    addr: &str,
    expect: Option<&str>,
    msg: &Message,
    state: &Arc<SharedState>,
    connect_start: Instant,
) -> Result<Tunnel> {
    // A stream on a mux session, if it is with the node we must reach.
    if let Some((yamux_stream, session)) = state.mux_pool.open_stream(addr, expect).await {
        let mut compat = yamux_stream.compat();
        send_and_check_tunnel(&mut compat, msg).await?;
        metrics::histogram!("stouter_tunnel_connect_duration_seconds", "method" => "mux")
            .record(connect_start.elapsed().as_secs_f64());
        return Ok(Tunnel::Mux(compat, session));
    }

    // A warm pooled connection to that node, else a new one.
    let pooled = state
        .tunnel_pool
        .try_take(addr)
        .filter(|stream| expect.is_none() || stream.peer_cn() == expect);
    let (method, mut node_stream) = match pooled {
        Some(stream) => ("pool", stream),
        None => ("cold", state.connect_mesh(addr, expect).await?),
    };
    send_and_check_tunnel(&mut node_stream, msg).await?;
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
                labels: Default::default(),
                meta: Default::default(),
                local_port: None,
            })
            .collect();
        let cfg = Config {
            node_id: "sub".into(),
            dynamic_config: DynamicConfig {
                version: 1,
                routes: vec![],
                service_groups: vec![ServiceGroup { name: "web".into(), services }],
                ..Default::default()
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
                            ..Default::default()
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

    #[tokio::test]
    async fn tunnels_go_to_the_node_itself_or_a_relay_it_signed() {
        let state = state_with(&[("r1", true, &[]), ("m", true, &[])]).await;
        let signed = Some(crate::config::SignedNode {
            record_json: String::new(),
            cert: String::new(),
            signature: String::new(),
        });
        // A direct node: its own address, and only it may answer there.
        let direct = NodeInfo { id: "x".into(), addr: "x:1".into(), ..Default::default() };
        assert_eq!(tunnel_route(&direct, &state), Some(("x:1".into(), Some("x".into()))));

        // A signing NAT'd node: only through a relay it listed, whatever
        // relay gossip claims for it.
        let natd = NodeInfo {
            id: "n".into(),
            relays: vec!["r1".into()],
            relay: Some("m:1".into()),
            signed: signed.clone(),
            ..Default::default()
        };
        assert_eq!(tunnel_route(&natd, &state), Some(("r1:1".into(), Some("r1".into()))));
        let unlisted = NodeInfo { relays: vec!["nobody".into()], ..natd };
        assert_eq!(tunnel_route(&unlisted, &state), None, "no usable relay: reverse connections only");

        // An older, unsigned NAT'd node: its gossiped relay, as before.
        let old = NodeInfo { id: "o".into(), relay: Some("m:1".into()), ..Default::default() };
        assert_eq!(tunnel_route(&old, &state), Some(("m:1".into(), Some("m".into()))));
    }

    #[tokio::test]
    async fn a_node_subscribe_node_reaches_its_own_instances_directly() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = backend.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (mut conn, _) = backend.accept().await.unwrap();
            conn.write_all(b"from me").await.unwrap();
        });
        let dc: DynamicConfig = serde_json::from_value(serde_json::json!({
            "version": 1,
            "serviceGroups": [{"name": "g", "services": [{"name": "web", "nodeId": "me", "nodePort": port}]}],
        }))
        .unwrap();
        let cfg = Config { node_id: "me".into(), mode: crate::config::Mode::NodeSubscribe, dynamic_config: dc, ..Config::default() };
        let state = SharedState::new(cfg.clone(), String::new(), CancellationToken::new(), None);

        // Our own instance is a backend (live), though we're not a peer of ours.
        let backends = service_backends("web", &state);
        assert_eq!(ids(&prefer_healthy(backends.clone(), "web", &state)), ["me"]);
        // And reached straight at its port, no tunnel.
        let mut tunnel = establish_tunnel(&backends[0].node, "web".into(), &state).await.unwrap();
        assert!(matches!(tunnel, Tunnel::Local(_)));
        let mut got = String::new();
        tunnel.read_to_string(&mut got).await.unwrap();
        assert_eq!(got, "from me");

        // A plain subscriber hosts nothing itself: no such backend.
        let plain = SharedState::new(Config { mode: crate::config::Mode::Subscribe, ..cfg }, String::new(), CancellationToken::new(), None);
        assert!(service_backends("web", &plain).is_empty());
    }

    #[tokio::test]
    async fn subscribers_spread_a_natd_nodes_tunnels_over_its_relays() {
        let natd = NodeInfo {
            id: "n".into(),
            relays: vec!["r1".into(), "r2".into(), "r3".into()],
            signed: Some(crate::config::SignedNode { record_json: String::new(), cert: String::new(), signature: String::new() }),
            ..Default::default()
        };
        let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for sub in 0..60 {
            let cfg = Config { node_id: format!("sub-{sub}"), ..Config::default() };
            let state = SharedState::new(cfg, String::new(), CancellationToken::new(), None);
            state
                .update_known_nodes(|kn| {
                    for r in &natd.relays {
                        let relay = NodeInfo { id: r.clone(), addr: format!("{r}:1"), last_seen: Some(Instant::now()), ..Default::default() };
                        kn.insert(r.clone(), relay);
                    }
                })
                .await;
            let (addr, expect) = tunnel_route(&natd, &state).unwrap();
            // The same choice every time, so its warm connections stay put.
            assert_eq!(tunnel_route(&natd, &state), Some((addr.clone(), expect.clone())));
            *used.entry(addr.clone()).or_default() += 1;

            // Once that relay stops being live, another is used.
            let id = expect.unwrap();
            state.update_known_nodes(|kn| kn.get_mut(&id).unwrap().last_seen = None).await;
            assert_ne!(tunnel_route(&natd, &state).unwrap().0, addr);
        }
        // 60 subscribers over 3 relays: about 20 each, not all on one.
        assert_eq!(used.len(), 3, "{used:?}");
        assert!(used.values().all(|&n| (8..=32).contains(&n)), "{used:?}");
    }
    fn natd_with_relays(id: &str, relays: &[&str]) -> NodeInfo {
        NodeInfo {
            id: id.into(),
            relays: relays.iter().map(|r| r.to_string()).collect(),
            signed: Some(crate::config::SignedNode { record_json: String::new(), cert: String::new(), signature: String::new() }),
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn one_subscriber_spreads_natd_nodes_over_their_shared_relays() {
        let state = state_with(&[("r1", true, &[]), ("r2", true, &[]), ("r3", true, &[])]).await;
        let mut used: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        for n in 0..30 {
            let natd = natd_with_relays(&format!("n{n}"), &["r1", "r2", "r3"]);
            let routes = tunnel_routes(&natd, &state);
            assert_eq!(routes.len(), 3, "every relay, to fall back on");
            *used.entry(routes[0].0.clone()).or_default() += 1;
        }
        assert_eq!(used.len(), 3, "not all through one relay: {used:?}");
        assert!(used.values().all(|&n| (4..=18).contains(&n)), "{used:?}");

        // A relay that isn't live goes last.
        let natd = natd_with_relays("n0", &["r1", "r2", "r3"]);
        let first = tunnel_routes(&natd, &state)[0].1.clone().unwrap();
        state.update_known_nodes(|kn| kn.get_mut(&first).unwrap().last_seen = None).await;
        let routes = tunnel_routes(&natd, &state);
        assert_eq!(routes.last().unwrap().1.as_deref(), Some(first.as_str()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_tunnel_falls_over_to_the_next_relay() {
        use crate::state::test_support::tls_state;
        let dir = std::env::temp_dir().join(format!("stouter_failover_{}_{}", std::process::id(), rand::random::<u64>()));
        let sub = tls_state(&dir, "sub");
        let natd = natd_with_relays("x", &["r1", "r2"]);
        let relay = |id: &str| NodeInfo { id: id.into(), addr: format!("{id}:1"), last_seen: Some(Instant::now()), ..Default::default() };
        sub.update_known_nodes(|kn| {
            kn.insert("r1".into(), relay("r1"));
            kn.insert("r2".into(), relay("r2"));
        })
        .await;
        let (first, second) = match &tunnel_routes(&natd, &sub)[..] {
            [a, b] => (a.1.clone().unwrap(), b.1.clone().unwrap()),
            other => panic!("{other:?}"),
        };

        // The first relay is down (nothing listens); the second is up, and
        // answers the tunnel request (as it would once X does).
        let dead = TcpListener::bind("127.0.0.1:0").await.unwrap().local_addr().unwrap().to_string();
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up.local_addr().unwrap().to_string();
        let second_state = tls_state(&dir, &second);
        tokio::spawn(async move {
            let (tcp, _) = up.accept().await.unwrap();
            let mut stream = second_state.tls_accept(tcp).await.unwrap();
            let Ok(Message::TunnelRequest { node_id, .. }) = gossip::recv_message(&mut stream).await else { return };
            assert_eq!(node_id, "x");
            use tokio::io::AsyncWriteExt;
            stream.write_all(&[0]).await.unwrap();
            stream.write_all(b"via the second relay").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        sub.update_known_nodes(|kn| {
            kn.get_mut(&first).unwrap().addr = dead.clone();
            kn.get_mut(&second).unwrap().addr = up_addr.clone();
        })
        .await;

        let started = Instant::now();
        let mut tunnel = establish_tunnel(&natd, "api".into(), &sub).await.expect("through the second relay");
        assert!(started.elapsed() < RELAY_ROUTE_TIMEOUT, "a refused relay costs no wait");
        let mut got = String::new();
        use tokio::io::AsyncReadExt;
        tunnel.read_to_string(&mut got).await.unwrap();
        assert_eq!(got, "via the second relay");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A NAT'd node's first relay accepts connections but never answers.
    /// Within a 1 s budget (a short requestTimeoutMs), the setup still gets
    /// to the next relay in time, and the first is tried last for a while.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_short_budget_leaves_time_for_the_next_relay() {
        use crate::state::test_support::tls_state;
        let dir = std::env::temp_dir().join(format!("stouter_budget_{}_{}", std::process::id(), rand::random::<u64>()));
        let sub = tls_state(&dir, "sub");
        let natd = natd_with_relays("x", &["r1", "r2"]);
        let relay = |id: &str| NodeInfo { id: id.into(), addr: format!("{id}:1"), last_seen: Some(Instant::now()), ..Default::default() };
        sub.update_known_nodes(|kn| {
            kn.insert("r1".into(), relay("r1"));
            kn.insert("r2".into(), relay("r2"));
        })
        .await;
        let (first, second) = match &tunnel_routes(&natd, &sub)[..] {
            [a, b] => (a.1.clone().unwrap(), b.1.clone().unwrap()),
            other => panic!("{other:?}"),
        };
        // The first takes the tunnel request and says nothing more.
        let (nonces_tx, mut nonces) = tokio::sync::mpsc::unbounded_channel();
        let hung = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hung_addr = hung.local_addr().unwrap().to_string();
        let first_state = tls_state(&dir, &first);
        let first_nonces = nonces_tx.clone();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((tcp, _)) = hung.accept().await {
                let Ok(mut stream) = first_state.tls_accept(tcp).await else { continue };
                if let Ok(Message::TunnelRequest { caller, .. }) = gossip::recv_message(&mut stream).await {
                    let _ = first_nonces.send(caller.map(|c| c.nonce));
                }
                held.push(stream);
            }
        });
        let up = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let up_addr = up.local_addr().unwrap().to_string();
        let second_state = tls_state(&dir, &second);
        tokio::spawn(async move {
            let (tcp, _) = up.accept().await.unwrap();
            let mut stream = second_state.tls_accept(tcp).await.unwrap();
            let Ok(Message::TunnelRequest { caller, .. }) = gossip::recv_message(&mut stream).await else { return };
            let _ = nonces_tx.send(caller.map(|c| c.nonce));
            use tokio::io::AsyncWriteExt;
            stream.write_all(&[0]).await.unwrap();
            stream.write_all(b"via the second relay").await.unwrap();
            stream.shutdown().await.unwrap();
        });
        sub.update_known_nodes(|kn| {
            kn.get_mut(&first).unwrap().addr = hung_addr.clone();
            kn.get_mut(&second).unwrap().addr = up_addr.clone();
        })
        .await;

        let started = Instant::now();
        let mut tunnel = establish_tunnel_within(&natd, "api".into(), &sub, Duration::from_secs(1))
            .await
            .expect("through the second relay, in time");
        assert!(started.elapsed() < Duration::from_secs(1), "{:?}", started.elapsed());
        let mut got = String::new();
        use tokio::io::AsyncReadExt;
        tunnel.read_to_string(&mut got).await.unwrap();
        assert_eq!(got, "via the second relay");
        assert_eq!(tunnel_routes(&natd, &sub)[0].1.as_deref(), Some(second.as_str()), "the hung relay goes last");
        // Each relay got a request signed afresh: had the first passed its
        // request on, the node would refuse the second's as a replay.
        let (a, b) = (nonces.recv().await.unwrap(), nonces.recv().await.unwrap());
        assert!(a.is_some() && b.is_some() && a != b, "{a:?} {b:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
