# Performance Roadmap

## Tier 1 — Biggest wins for high-connection HTTP

### ~~Stream multiplexing (yamux/h2) over tunnels~~ (DONE)
Implemented via yamux. Subscriber maintains persistent multiplexed sessions to each node. Each tunnel request opens a new yamux stream on the existing session, amortizing TCP+auth handshake cost. Falls back to legacy single-connection path when no mux session is available.

### TLS (rustls + mTLS)
Adds confidentiality (currently plaintext) and replaces HMAC auth with mutual TLS. Session resumption and 0-RTT on warm connections keeps latency low. Eliminates per-message HMAC sign/verify overhead on the hot path.

### ~~Reverse connection reuse~~ (SKIPPED)
Reverse workers already spawn tunnel handlers in background tasks and immediately reconnect, so the pool stays full during bursts. The raw TCP+registration reconnect cost is negligible. When TLS is added, reverse connections should be multiplexed via yamux over a single TLS session per relay — fold into the TLS work rather than treating as a standalone item.

### L7 health checks
Don't route to dead services. Add HTTP-level health checks (configurable path), exponential backoff on failures, and circuit breaker logic. Remove unhealthy services from the routing index so proxies never attempt a doomed tunnel.

## Tier 2 — Throughput and efficiency

### ~~splice(2) / zero-copy proxy~~ (DROPPED)
Not viable: proxy_bidirectional is generic over AsyncRead+AsyncWrite (yamux, TLS), so splice(2) can't apply. Linux-only (project targets macOS). Current 64 KiB buffer implementation is already efficient for HTTP proxy workloads.

### ~~Binary framing on hot path~~ (DONE)
Replaced double-JSON wire format (Message→JSON→SignedMessage→JSON) with MessagePack + raw 32-byte HMAC. Wire format is now `[4-byte len][32-byte HMAC][msgpack payload]`. Eliminates double serialization, hex encoding, and JSON overhead on all message paths.

### ~~Adaptive pool sizing~~ (DROPPED)
Yamux multiplexing makes the fixed tunnel pool a fallback path. Streams are opened on demand over persistent mux sessions, so pool sizing is no longer a bottleneck.

### ~~Connection coalescing~~ (DROPPED)
Already effectively implemented via yamux — all services on the same node share a single mux session. The tunnel pool fallback also keys by address, so connections are shared.

## Tier 3 — Operational maturity

### Prometheus/OpenTelemetry metrics
Export request rates, tunnel latency histograms, pool utilization (reverse + tunnel), gossip version lag, and peer reachability. Essential for capacity planning and debugging production issues.

### ~~Graceful drain~~ (DONE)
On SIGINT/SIGTERM: cancels a CancellationToken that stops all listeners and background loops, broadcasts NodeLeave to peers, then waits up to 30s for in-flight connections to drain via an RAII-guarded atomic counter before exiting.

### Multi-node service routing
When multiple nodes host the same service, only one node is selected (HashMap insert race in manage_proxies). The rest are silently dropped — no redundancy, no failover. Route across all available replicas with round-robin or least-connections, and failover when a node becomes unreachable.

### ~~Rate limiting and backpressure~~ (DROPPED)
Yamux provides built-in flow control and stream limits. Tunnel and reverse pools are already finite. No unbounded queueing exists. Revisit only if cascading failures are observed in practice.

### ~~Adaptive gossip convergence~~ (DROPPED)
The fixed 10s sync interval is adequate for small clusters. Deploys converge within one round. If faster convergence is needed, simply lower the fixed interval — adaptive logic adds complexity for marginal gain.
