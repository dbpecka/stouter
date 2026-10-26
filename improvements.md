# Improvements — toward a full-featured service mesh

Features that would move stouter from a tunneling ingress/proxy toward a complete
service mesh. Grouped by area, not prioritized.

## Already in place
- mTLS on every inter-node connection (gossip, tunnels, mux, reverse pool) with a
  per-cluster CA (`src/pki.rs`, `src/tls.rs`); node certs are re-issued on every
  restart when `ca.key` is present, and `stouter cert issue` provisions new nodes.
- Config-mutation RBAC: the REST API is read-only, and mutations go through the
  gossip admin RPC, authorized against the `adminCns` certificate allowlist.
- Health-aware load balancing: nodes with stale gossip liveness are skipped, and
  a failed tunnel setup is retried against another node.
- Passive outlier ejection: a node whose tunnel setup fails repeatedly is
  temporarily removed from rotation, with exponential backoff.
- Weighted traffic splitting at the connection level (`weight` on a service;
  `0` drains a node) via smooth weighted round-robin.

## Architectural prerequisite
The data plane is L4 only: tunnels copy raw bytes and never parse HTTP. L7
routing, per-request retries/timeouts, tracing, and per-route RED metrics all
need an HTTP-aware proxy path (e.g. hyper) first. That is the next major
milestone and should be designed on its own.

## Security (identity & policy)
- Certificate rotation without a restart, and revocation (a CRL or short-lived
  certs) for removed nodes.
- SPIFFE-style workload identity so services — not just nodes — are principals.
- Authorization policies: "service A may call service B on path /x" (L7 RBAC).
- Finer-grained admin RBAC (per-group or per-operation rather than one allowlist).

## Traffic management
- L7 routing: header/path/method matching, not just SNI/host.
- Further load-balancing policies on top of weighted round-robin: P2C /
  least-connections and consistent-hash (sticky sessions).
- Retries with budgets, timeouts, and request hedging.
- Circuit breakers (max connections / pending per service); outlier ejection
  currently reacts only to tunnel-setup failures.
- Fault injection (delay/abort) for chaos testing.
- Rate limiting per route/client.
- Explicit HTTP/2 + gRPC support, WebSocket upgrade handling, HTTP/3/QUIC.

## Ingress polish
- TLS termination with SNI at the edge.
- ACME / Let's Encrypt automation for `domains`.
- HTTP redirect, rewrites, CORS, compression.

## Observability
- W3C TraceContext / OpenTelemetry propagation + span emission (metrics exist;
  tracing is the gap).
- Structured access logs with trace IDs.
- RED metrics per service/route (current metrics are mostly infrastructure-level).
- Service-topology endpoint / UI showing live nodes, services, and request flow.
- `stouter tap` — live request sampler for debugging.

## Reliability & topology
- Locality-aware routing (prefer same-zone/region before cross-WAN).
- Active health checks: hosting nodes probe their local `nodePort` and gossip
  per-service health, so subscribers skip a dead backend before trying it.
- Cross-cluster failover / federation of gossip rings.

## Config & DX
- Schema-validated, atomic config apply with version history + rollback
  (`check` exists; add `apply`/`diff`/`history`).
- Namespaces or tenants so multiple teams can share one mesh without domain
  collisions.
- kubectl-style CLI: `stouter get services`, `stouter apply -f`, `stouter logs <svc>`.
- Separate control-plane vs data-plane binaries once config grows.

## Next up
1. **Active health checks** — TCP probes of each service's backend, gossiped so
   every subscriber avoids a dead backend without paying a failed attempt.
2. **HTTP-aware data plane** — the prerequisite for L7 routing, per-request
   retries/timeouts, OpenTelemetry tracing, and per-route RED metrics.
3. **Workload identity + L7 authorization** — builds on the existing mTLS once
   the proxy understands requests.
