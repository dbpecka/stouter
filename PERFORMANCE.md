# Performance Roadmap

## Tier 1 — Biggest wins for high-connection HTTP

### ~~Stream multiplexing (yamux/h2) over tunnels~~ (DONE)
Implemented via yamux. Subscriber maintains persistent multiplexed sessions to each node. Each tunnel request opens a new yamux stream on the existing session, amortizing TCP+auth handshake cost. Falls back to legacy single-connection path when no mux session is available.

### TLS (rustls + mTLS)
Adds confidentiality (currently plaintext) and replaces HMAC auth with mutual TLS. Session resumption and 0-RTT on warm connections keeps latency low. Eliminates per-message HMAC sign/verify overhead on the hot path.

### Reverse connection reuse
Reverse workers currently reconnect after each tunnel completes. Instead, return completed reverse connections to the pool for reuse, or multiplex them. Halves reverse pool pressure and eliminates reconnect latency.

### L7 health checks
Don't route to dead services. Add HTTP-level health checks (configurable path), exponential backoff on failures, and circuit breaker logic. Remove unhealthy services from the routing index so proxies never attempt a doomed tunnel.

## Tier 2 — Throughput and efficiency

### splice(2) / zero-copy proxy
The proxy path currently copies through 64KB user-space buffers. On Linux, use splice(2) (via tokio-splice or raw syscall) to move data directly between sockets in kernel space. 20-40% throughput gain on sustained transfers.

### Binary framing on hot path
Every message round-trips through serde_json. For tunnel requests and relay forwarding (the hot path), switch to a compact binary format (bincode, MessagePack, or hand-rolled header). Keep JSON for gossip where debuggability matters more than speed.

### Adaptive pool sizing
Fixed 8 warm connections per node doesn't match bursty HTTP traffic. Size pools proportional to recent request rate (exponential moving average) with high/low watermarks and hysteresis. Replace the fixed 10ms sleep in fill_tunnel_pool with a semaphore-bounded connect loop.

### Connection coalescing
Multiple services on the same node get independent tunnel connections. A single multiplexed tunnel per node reduces file descriptor pressure and pool management overhead significantly.

## Tier 3 — Operational maturity

### Prometheus/OpenTelemetry metrics
Export request rates, tunnel latency histograms, pool utilization (reverse + tunnel), gossip version lag, and peer reachability. Essential for capacity planning and debugging production issues.

### Graceful drain
On shutdown, broadcast NodeLeave, stop accepting new tunnels, drain in-flight connections with a configurable timeout, then exit. Enables zero-downtime deploys and rolling restarts.

### Load balancing across replicas
When multiple nodes host the same service, distribute connections via round-robin or least-connections. Currently the subscribe proxy picks a single node per service — no redundancy or spread.

### Rate limiting and backpressure
Semaphore-bound concurrent tunnels per node. When the limit is hit, return a backpressure signal to subscribers rather than queueing unbounded. Prevents cascade failures under overload.

### Adaptive gossip convergence
Reduce sync interval to 1-2s when cluster membership is changing (detected via version bumps in recent syncs). Relax back to 10s when stable. Gives fast convergence during deploys without constant overhead in steady state.
