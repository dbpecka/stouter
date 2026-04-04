# Stouter TODO

## Stability fixes (low effort, high impact)

- [ ] Exponential backoff on reverse connection retries — currently a flat 2s sleep in `src/node/reverse.rs`. Cap at 60s (2s -> 4s -> 8s -> ... -> 60s) to prevent reconnect storms.
- [ ] Cap the reverse connection pool — `state.rs:push()` has no size limit. Drop new connections if `vec.len() >= pool_size`.
- [ ] Add idle timeout to `copy_bidirectional` — tunnel and proxy paths hold stale connections forever. Wrap with ~5 min idle timeout.
- [ ] Make DNS/API ports configurable — hard-coded `5380`/`5381` in `subscribe/mod.rs`. Move to config.

## Simplification

- [ ] Remove the config file polling loop — two sources of truth (file polling + gossip) creates race conditions. Go gossip-only with CLI/API for edits, treat file as bootstrap-only.
- [ ] Use `(group, name)` as service key everywhere — bare service names silently collide across groups. Either enforce unique names at validation or use `group.name` in DNS/proxy.

## Robustness (medium effort)

- [ ] Concurrent gossip broadcasts — `broadcast_message` is sequential with 5s timeout per peer. Use `join_all` to fan out in parallel.
- [ ] Add health check endpoint — `/health` on the subscribe API reporting gossip freshness, connected nodes, proxy status.
- [ ] Validate config on load — check non-empty cluster secret, valid bind address, no duplicate service names, version > 0.

## Longer-term

- [ ] TLS for inter-node traffic — plaintext with HMAC auth prevents tampering but not eavesdropping. Add `rustls` with mutual TLS.
- [ ] Structured observability — log connection counts, gossip round-trip times, proxy throughput via tracing spans.
