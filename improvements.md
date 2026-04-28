# Improvements — toward a full-featured service mesh

Features that would move stouter from a tunneling ingress/proxy toward a complete
service mesh. Grouped by area, not prioritized.

## Security (identity & policy)
- mTLS with rustls + per-node certs rotated automatically (already on the roadmap).
- SPIFFE-style workload identity so services — not just nodes — are principals.
- Authorization policies: "service A may call service B on path /x" (L7 RBAC).
- Config-mutation RBAC on the REST API (currently unauthenticated HTTP mutations).

## Traffic management
- L7 routing: header/path/method matching, not just SNI/host.
- Traffic splitting by weight (canary, blue/green) — extend the round-robin load
  balancer in `src/subscribe/mod.rs` to weighted + P2C + consistent-hash (sticky
  sessions).
- Retries with budgets, timeouts, and request hedging.
- Circuit breakers + outlier ejection (pairs with the planned L7 health checks).
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
- Active L7 health checks (roadmap) feeding the routing index.
- Cross-cluster failover / federation of gossip rings.

## Config & DX
- Schema-validated, atomic config apply with version history + rollback
  (`check` exists; add `apply`/`diff`/`history`).
- Namespaces or tenants so multiple teams can share one mesh without domain
  collisions.
- kubectl-style CLI: `stouter get services`, `stouter apply -f`, `stouter logs <svc>`.
- Separate control-plane vs data-plane binaries once config grows.

## Top three to start
1. **mTLS + workload identity** — confidentiality, authenticated peers, and a
   foundation for every policy feature above.
2. **L7 routing with weighted splitting + retries/timeouts** — the traffic
   features that distinguish a mesh from a reverse proxy.
3. **OpenTelemetry tracing** — observability that actually explains request
   flow across the cluster.
