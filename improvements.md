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
- Circuit breaking: per-instance `maxConnections`, enforced by each
  subscriber as a hard cap (excess connections are rejected, not queued).
- Active TCP health checks: each node probes its own services' `nodePort` and
  gossips a versioned health report, so subscribers skip a dead backend
  before any client connection reaches it.

- HTTP-aware proxying (opt-in `protocol: http`, milestone 1 of
  `docs/http-proxy.md`): per-request balancing over pooled tunnels, timeouts,
  connect-failure retries, upgrade passthrough, W3C trace propagation, access
  logs, and per-service request metrics. Plus retries on 502/503/504 for
  idempotent methods under a retry budget, and buffering of small bodies so
  they can be replayed.
- HTTP health checks: optional `healthPath` per instance, requiring a 2xx.
- OpenTelemetry span export (OTLP/HTTP JSON) for `http` services, with
  parent-based sampling and spans the backend's own spans nest under.
- L7 routing for `http` services: instance `labels` and ordered `routes`
  (path prefix / method / header matches → weighted label subsets), managed
  with `stouter route set|clear|list`.

## Security (identity & policy)
- Certificate rotation without a restart, and revocation (a CRL or short-lived
  certs) for removed nodes.
- SPIFFE-style workload identity so services — not just nodes — are principals.
- Authorization policies: "service A may call service B on path /x" (L7 RBAC).
- Finer-grained admin RBAC (per-group or per-operation rather than one allowlist).

## Traffic management
- Route actions beyond subset selection: path rewrites, header injection,
  regex and presence-only header matches.
- Further load-balancing policies on top of weighted round-robin: P2C /
  least-connections and consistent-hash (sticky sessions).
- Request hedging, and counting 5xx responses toward outlier ejection
  (today only tunnel failures eject a node).
- Cluster-wide connection limits (today each subscriber enforces
  `maxConnections` on its own) and a bounded pending queue instead of
  immediate rejection.
- Fault injection (delay/abort) for chaos testing.
- Rate limiting per route/client.
- HTTP/2 (h2c downstream, h2 over tunnels) and gRPC as `http` services
  (M4); HTTP/3/QUIC.

## Ingress polish
- TLS termination with SNI at the edge.
- ACME / Let's Encrypt automation for `domains`.
- HTTP redirect, rewrites, CORS, compression.

## Observability
- `https://` OTLP endpoints and OTLP metrics export; spans for tunnel
  setup on hosting nodes.
- Faster graceful shutdown: long-lived peer mux sessions count as
  in-flight, so a drain always waits the full 30s timeout.
- Per-route metrics once route rules exist (per-service request metrics exist
  for `http` services).
- A metrics endpoint for pure `node`-mode daemons (only subscribers serve
  `/metrics` today, so tunnel metrics recorded on nodes can't be scraped).
- Service-topology endpoint / UI showing live nodes, services, and request flow.
- `stouter tap` — live request sampler for debugging.

## Reliability & topology
- Locality-aware routing (prefer same-zone/region before cross-WAN).
- Cross-cluster failover / federation of gossip rings.

## Config & DX
- Schema-validated, atomic config apply with version history + rollback
  (`check` exists; add `apply`/`diff`/`history`).
- Namespaces or tenants so multiple teams can share one mesh without domain
  collisions.
- kubectl-style CLI: `stouter get services`, `stouter apply -f`, `stouter logs <svc>`.
- Separate control-plane vs data-plane binaries once config grows.

## Next up
1. **M5 — Workload identity + L7 authorization** — per-service identities in
   certs, enforced on hosting nodes.
2. **A `/metrics` endpoint for `node`-mode daemons** — small, but tunnel
   metrics recorded on nodes can't be scraped today.
3. **Graceful shutdown that doesn't wait on idle mux sessions.**
