# Design: HTTP-aware data plane

Status: milestone 1 implemented, plus the resilience half of M2 (HTTP
health checks, status retries under a budget, body buffering); L7 routing and
later milestones proposed.

## Problem

Stouter's data plane only forwards raw TCP. A subscriber accepts a connection,
picks a node once, opens a tunnel, and copies bytes until either side closes.
That rules out everything a mesh does per request:

- **Load balancing is per connection.** One keep-alive client pins all of its
  requests to one backend, so weights and ejection barely affect it.
- **No request-level resilience.** There are no per-request timeouts, retries, or
  error responses. A failed backend shows up as a reset connection.
- **No observability of requests.** Metrics count tunnels, not requests, and
  there are no status codes, latencies, access logs, or trace context.
- **No L7 policy.** Routing on path or headers, and authorization on the
  calling service, both need the request to be parsed.

## Where HTTP handling lives

**On the subscriber only.** The local proxy that applications already connect
to parses HTTP. The tunnel and the hosting node stay as they are: the node
still forwards bytes to `127.0.0.1:{nodePort}`, and those bytes are now an
HTTP/1.1 connection that the subscriber opened.

Why:

- **No protocol or node changes.** Mixed-version clusters keep working, and
  nodes don't need upgrading.
- **Decisions happen where routing already happens.** The subscriber holds the
  balancer, health data and ejection state, and it is the client-side hop that
  sees the caller's request.
- **Hosting nodes stay simple.** Server-side processing on the hosting node
  (for authorization, say) can be added later where it's needed.

The cost is that each request is parsed once, on the subscriber. HTTP/1.1
parsing in hyper is cheap next to a WAN round trip.

## Opting in

Services gain `protocol`: `tcp` (default) or `http`. Because it's set per
instance in the service list, the subscriber treats a service as HTTP only when
**every** instance says `http`. A half-migrated or mistyped service falls back
to byte forwarding, which is always safe.

The protocol is read for each accepted connection rather than when the proxy
starts, so switching a service between `tcp` and `http` takes effect for new
connections without a restart.

## Milestone 1 (implemented)

Request flow for an `http` service:

1. The subscriber's local listener accepts the connection and serves HTTP/1.1
   on it with hyper.
2. For **each request**, the balancer picks an instance. Weights, health,
   ejection and `maxConnections` all apply.
3. The request is sent over a pooled upstream HTTP/1.1 connection to that
   node. Upstream connections are keep-alive connections running over
   tunnels, pooled per node. A new connection is only set up (one tunnel)
   when no idle one is available, which also spreads the cost of setting up
   tunnels across many requests.
4. The response is streamed back to the client.

### Semantics

| Concern | Behaviour |
|---|---|
| Load balancing | Per request. The pool is keyed by node, so per-request picks reuse that node's connections. |
| `maxConnections` | Caps concurrent **in-flight requests** per instance (held until the response body finishes). Over the cap → `503`. |
| Retries | To a different node, up to 3 nodes. (a) When a connection can't be established (the request was never sent): any method, not budgeted. (b) On `502/503/504`: idempotent methods only, within the retry budget; if no later attempt does better, the last real response is returned. Bodies with a declared length ≤ 64 KiB are buffered so they can be re-sent; larger or chunked bodies go once. |
| Retry budget | Per service: each request earns 0.2 retries, each status retry spends 1, up to 20 banked (and the budget starts full). Status retries therefore stay under ~20% of traffic in steady state. |
| Timeouts | Optional per-instance `requestTimeoutMs`: time allowed to receive response headers from that instance. Expiry → `504`, and it is not retried (the backend may have acted on the request). |
| Errors | No backend reachable → `502`, every instance at its limit → `503`, timeout → `504`. Each carries an `x-stouter-error` header saying which. |
| Hop-by-hop headers | Stripped in both directions (RFC 9110 §7.6.1), including any listed in `Connection`. `Host` is preserved. |
| WebSocket / upgrades | Passed through: when the backend answers `101`, both sides are upgraded and bytes are copied raw. The connection slot is held until the upgraded stream closes. |
| Trace context | W3C `traceparent` is propagated. An incoming trace keeps its trace id and flags; the proxy adds a new span id as the parent for the upstream hop. Without one, a new trace is started (sampled), so every request can be correlated across services through access logs. |
| Access log | One structured line per request (target `stouter::access`): service, method, path, status, node, attempts, duration, trace id. |
| Metrics | `stouter_http_requests_total{service,method,code}` (`code` is `2xx`…`5xx`) and `stouter_http_request_duration_seconds{service}`. |

### Out of scope for M1

- **Downstream HTTP/2 (h2c) and gRPC.** gRPC services should stay `tcp` for now.
- **TLS from the application to the local proxy.** It listens on loopback.

## Later milestones

**M2 — L7 routing and HTTP health.**
- Route rules per service: match on path prefix, method or header, and send
  to a subset of instances (selected by labels), with weights per route. Canary
  by header becomes possible. *(Proposed; needs a config-schema decision.)*
- `healthPath` health checks, probed by the hosting node through the existing
  health gossip. They report an unhealthy *status*, not just a closed port.
  *(Done.)*
- Retries on `502/503/504` for idempotent methods, limited by a retry budget,
  and small-body buffering so requests with bodies can be replayed. *(Done.)*

**M3 — Span export.** Each proxy hop emits an OpenTelemetry span over OTLP
(optional collector endpoint in config), using the trace ids M1 already
propagates. Turns the access-log correlation into real traces.

**M4 — HTTP/2 end to end.** h2c from the application, h2 over the tunnel
(one multiplexed connection per node instead of a pool), making gRPC a
first-class `http` service.

**M5 — Identity and authorization.** Per-service identities in node-issued
certs (SPIFFE-style), carried with the tunnel request. Hosting nodes then
enforce "service A may call service B on path /x". This needs request
awareness on the hosting side, which M1 deliberately left out; it gets added
there in this milestone.

## Alternatives considered

- **Parse HTTP on the hosting node.** It sees all traffic for its backend, so
  enforcement is easy there. But balancing and retries have to happen before a
  node is chosen, so the subscriber needs request awareness regardless. Doing
  it in both places in M1 would double the work for no M1 feature.
- **Detect the protocol automatically** (sniff the first bytes). This is
  convenient, but a bad guess silently changes behaviour, and server-speaks-first
  protocols stall the sniff. An explicit setting is simpler to reason about.
- **A new tunnel per request.** Simpler, but it adds a TLS-protected round trip
  to every request. Pooling over tunnels avoids that.
