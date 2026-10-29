# Design: HTTP-aware data plane

Status: milestones 1–3 implemented (M2: L7 routing, HTTP health checks,
status retries under a budget, body buffering; M3: OpenTelemetry span
export); M4–M5 proposed.

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

## Milestone 2: L7 routing (implemented)

### Schema

Instances carry free-form `labels`. Routes live at the top level of
`dynamicConfig` (so they're gossiped and versioned with the rest of the
config), each naming the service it applies to:

```json
{
  "dynamicConfig": {
    "serviceGroups": [{ "name": "web", "services": [
      { "name": "api", "nodeId": "n1", "nodePort": 3000, "protocol": "http", "labels": { "version": "v1" } },
      { "name": "api", "nodeId": "n2", "nodePort": 3000, "protocol": "http", "labels": { "version": "v2" } }
    ]}],
    "routes": [
      { "name": "canary", "service": "api",
        "match": { "headers": { "x-canary": "true" } },
        "backends": [{ "labels": { "version": "v2" } }] },
      { "name": "split", "service": "api",
        "backends": [{ "labels": { "version": "v1" }, "weight": 9 },
                     { "labels": { "version": "v2" }, "weight": 1 }] }
    ]
  }
}
```

Why this shape:

- **Labels on instances, not named subsets.** Whoever adds an instance tags it,
  and routes select by label. There's no separate subset registry to keep in
  sync.
- **Routes at the top level, keyed by service.** Instances of one service can
  sit in different groups, and subscribers already address services by bare
  name, so a route belongs to the service rather than to a group.
- **An ordered list, first match wins.** This is easy to predict, and it's how
  most proxies (Envoy, nginx `location`) behave.

### Semantics

| Concern | Behaviour |
|---|---|
| Evaluation | A service's routes are tried in order; the first whose `match` fits wins. No match → every instance, as without routes. |
| `match` | `pathPrefix` (on segment boundaries: `/v2` matches `/v2` and `/v2/x`, not `/v20`), `method` (exact), `headers` (exact values, case-insensitive names). All present conditions must hold; an empty `match` matches everything. |
| `backends` | Weighted instance subsets. Each request picks one subset by weight (random), then the balancer picks an instance inside it as usual. An empty `labels` selects every instance. |
| Retries | Stay inside the chosen subset: a canary request is never retried onto stable instances. |
| Health | Health filtering fails open **within** the subset, just as it does service-wide. A subset whose instances are all unhealthy still gets its share, which matches Envoy's panic mode. Drain an instance with weight 0, or remove it from the route. |
| Empty subset | If no instance carries the labels → `503` with `x-stouter-error: no_route_backend`. |
| Observability | The route name (or `#index`, or `default`) appears in the access log as `route=` and as the `route` label on `stouter_http_requests_total`. |
| Validation | A routed service must exist and be `http` on every instance; backends must be non-empty with a non-zero total weight; `pathPrefix` must start with `/`, `method` must be uppercase, header names must be valid. This is checked by `stouter check`, the CLI and the admin RPC alike. |

### Managing routes

```bash
stouter service add --group web --name api --port 3000 --protocol http --label version=v2
stouter route set   --service api --routes @routes.json   # replaces all of api's routes
stouter route clear --service api
stouter route list
```

`set` and `clear` take `--remote host:port` for the admin RPC, like `service`.
The `service` field may be left out of routes passed to `route set`.

### Mixed versions

Older nodes ignore `routes` and `labels`, so they route by plain balancing.
An older node that *mutates* the config (for example with `service add`)
writes it back without the fields it doesn't know. That means `routes`,
`labels`, and every other field added since (`weight`, `maxConnections`,
`protocol`, `requestTimeoutMs`, `healthPath`). The new version then
propagates cluster-wide with those fields gone. Upgrade every node that runs
admin mutations before relying on any of them.

## Milestone 3: span export (implemented)

Configured per daemon in the static config (not gossiped, since each daemon
usually sends to its own local collector):

```json
"tracing": { "otlpEndpoint": "http://127.0.0.1:4318", "sampleRatio": 0.1, "serviceName": "stouter" }
```

| Concern | Behaviour |
|---|---|
| What's exported | One `CLIENT` span per request to an `http` service, from the subscriber. TCP services and hosting nodes emit none (there's no trace context at the byte level). |
| Span ids | The span's id is the one sent upstream in `traceparent`, so the backend's own server span nests directly under it. Its parent is the caller's span from the incoming `traceparent`, or none for a new trace. |
| Sampling | Parent-based. An incoming `traceparent` keeps its sampled flag. A new trace is sampled with probability `sampleRatio` (default 1.0), and the decision is propagated in the flags (`01`/`00`) so downstream services agree. Unsampled requests cost nothing extra. |
| Contents | Name `{METHOD} {service}`. Attributes follow the OTel HTTP conventions (`http.request.method`, `url.path`, `http.response.status_code`, `server.address` = node, `error.type`), plus `stouter.service`, `stouter.route`, `stouter.attempts`. Each failed attempt is an `attempt_failed` event naming the node and the reason. Status is `ERROR` for 4xx/5xx and proxy errors (client-span convention). Resource: `service.name`, `service.instance.id` = node id. |
| Duration | Until response headers arrive, matching the request-duration metric. A streamed body doesn't keep a span open. |
| Transport | OTLP/HTTP with the spec's JSON encoding, `POST {otlpEndpoint}/v1/traces`, in batches of up to 512 spans or every 5s. Plain `http://` only: to reach a TLS or SaaS backend, run an OpenTelemetry Collector locally and let it forward. |
| Back-pressure | Spans go into a bounded queue (4096) and are never awaited by requests. When the queue is full, or an export fails, spans are dropped and counted in `stouter_tracing_spans_dropped_total{reason}`. An export outage logs one warning, then one "recovered" line. Exported spans are counted in `stouter_tracing_spans_exported_total`. |
| Shutdown | After draining connections, queued spans are flushed (up to 5s). |

**Why not the `opentelemetry` crates?** The SDK plus an OTLP exporter brings
in tonic/prost or reqwest, a large dependency tree for a single span type.
OTLP's JSON encoding is a stable part of the spec, and the exporter is about
200 lines on the hyper/serde already in the tree. If stouter later needs
more (metrics/logs over OTLP, gRPC transport, TLS), switching to the SDK is
a contained change to `src/telemetry.rs`.

## Later milestones

**M2 follow-ups.** Path rewriting and header injection on routes; regex and
presence-only header matches; cross-cluster `service move` carrying the
service's routes.

**M3 follow-ups.** `https://` collector endpoints (needs a root-certificate
source); OTLP metrics export; spans for tunnel setup on hosting nodes.

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
