# stouter

Stouter is a lightweight service mesh for distributed clusters. It handles service discovery via a gossip protocol, routes traffic through mutually authenticated (mTLS) tunnels, and supports nodes behind NAT through reverse connection pooling.

## Overview

Stouter solves three problems in distributed systems:

1. **Service discovery** — nodes advertise services and automatically learn about peers through a gossip protocol, no central registry required.
2. **Secure routing** — all inter-node traffic is encrypted and mutually authenticated using per-cluster mTLS certificates.
3. **NAT traversal** — nodes that cannot accept inbound connections (behind firewalls/NAT) register outbound reverse connections to relay nodes, which the relay uses to forward inbound requests back to the NAT'd node.

Each node runs a single binary configured by a JSON file. Depending on the mode, a node may host services, proxy remote services locally, or both.

---

## Usage

### Installation

```sh
cargo build --release
cp target/release/stouter /usr/local/bin/stouter
```

Or with Docker:

```sh
docker build -t stouter .
```

### Configuration

Create a `stouter.json` file:

```json
{
  "mode": "node",
  "bind": "0.0.0.0:8080",
  "advertiseAddr": "1.2.3.4:8080",
  "nodeId": "host1",
  "outboundOnly": false,
  "reversePoolSize": 64,
  "knownNodes": ["peer1.example.com:8080", "peer2.example.com:8080"],
  "tlsDir": "/etc/stouter/tls",
  "dynamicConfig": {
    "version": 1,
    "serviceGroups": []
  }
}
```

| Field | Description |
|---|---|
| `mode` | `node`, `subscribe`, or `nodeSubscribe` |
| `bind` | Address to listen on |
| `advertiseAddr` | Address peers use to reach this node. Without it, with a wildcard `bind` (`0.0.0.0` or `[::]`), peers use the address they reach the node at, and pass that on to nodes that hear of it through them: those use it until the node contacts them itself (they only count it as live once its own certificate answers there). Set it when that address isn't one every peer can reach. |
| `nodeId` | Unique identifier for this node |
| `outboundOnly` | Set `true` for nodes behind NAT — disables listener, uses reverse connections |
| `reversePoolSize` | Number of idle reverse connections an `outboundOnly` node keeps at each of its 3 relays (default 64, at most 1024: relays refuse more from one node) |
| `knownNodes` | Bootstrap peer addresses for initial gossip |
| `tlsDir` | Directory for `ca.crt`, `ca.key`, `node.crt`, `node.key` |
| `adminCns` | Certificate common names allowed to issue remote admin RPCs (`--remote` mutations). Empty (default) disables admin RPC |
| `apiPort` | Subscribe-mode REST API port on `127.0.0.1` (default 5381). Set per-cluster when running multiple instances on one host. |
| `requireSignedConfig` | Accept a config change only if a certificate in `adminCns` signed it (see [Signed config changes](#signed-config-changes)). Default `false`. |

The daemon won't start if one of its own settings is wrong (an invalid
`httpListen`, a `reversePoolSize` over the limit, `requireSignedConfig` without
`adminCns`, an empty `nodeId` or one with a `:` in it, which peers would take
for an address, and so on). Problems in `dynamicConfig`, such as a service listed
twice, are logged as warnings instead: that config is the cluster's, a newer
version may already fix it, and a node that refused to start over it could
never receive the fix. `stouter check` reports both kinds.

A setting stouter doesn't know (a typo such as `requireSignedConfg`, which
would otherwise leave signing off without a word, or one from a newer
version) is logged as a warning at startup and fails `stouter check`; it is
kept when the file is rewritten. A hand edit to `dynamicConfig` gets the
same checks as a change made with the CLI: one that would break the config
is refused, with a warning.

CLI commands that change the local file (`init`, `service add` without
`--remote`, and so on) refuse a file they can't parse instead of starting
over from defaults and saving those over it. `init` sets `nodeId` to the
hostname only if it is empty.

### Modes

| Mode | Description |
|---|---|
| `node` | Hosts services and relays tunnels for other nodes |
| `subscribe` | Discovers services in the cluster and exposes them as local TCP proxies |
| `nodeSubscribe` | Hosts services locally and also proxies remote services |

### CLI Commands

```sh
# Start the daemon
stouter --config /etc/stouter/stouter.json

# Validate config syntax
stouter check --config /etc/stouter/stouter.json

# Query daemon status
stouter status

# Register a service
stouter service add --group web --name api --port 3000 --domains api.example.com

# Give host1's instance of api 9x the connections of others (e.g. a 90/10 canary split)
stouter service add --group web --name api --port 3000 --node host1 --weight 9

# List all services in the cluster
stouter service list

# Issue a signed node certificate (run on the CA node)
stouter cert issue --node host2 --out /path/to/certs

# Copy the cluster's dynamic config into the local file (to edit it by hand)
stouter config pull --remote host1:8080

# Sign the dynamic config in the local file and push it (hand edits, with requireSignedConfig)
stouter --cert-dir /path/to/admin-certs config push --remote host1:8080
```

`config push` replaces the cluster's dynamic config with the file's, so it
refuses a file older than the cluster's config: pushing it would undo the
changes made since. Run `config pull` (it keeps a copy of the file as `.bak`
if its services or routes differ), redo the edit, and push; or pass `--force`
to replace the cluster's config anyway. After a push the file holds the version
pushed, so the next edit starts from it.

A running daemon writes each new config into its file. If the file holds an
edit it hasn't applied (a hand edit not yet picked up, one it refused, a
pulled config being edited), it saves the file as `<config>.unapplied`
first and logs a warning, rather than write over the edit. `config push`
mentions that file when the cluster already has what the config file now
says.

### Registering Services

Services are organized into groups. Each service entry maps a name to a local port and optional domain names. A service is known by its name everywhere (proxies, routes, the REST API), so all instances of a service belong to one group, and a domain can belong to only one service.

`stouter service add|rename|remove` and `stouter route set|clear`, local or `--remote`, refuse a change that would break the config, such as claiming another service's domain, or removing or renaming a service that still has routes (clear them first). A problem the config already had doesn't block unrelated changes.

Without `--remote`, these commands make the change through this host's daemon when it is running (`outboundOnly` ones have no listener to ask), as if `--remote` named it: editing the file behind a running daemon's back could undo a change the daemon took in at the same moment. Otherwise they edit the file, refusing if it changed while they ran. The daemon accepts these changes from its own certificate; with `requireSignedConfig` they must still be signed by an admin. `--node` defaults to this host's node, or with `--remote`, to the node at that address.

`stouter service move` copies a service from one cluster to another and then removes it from the source. It refuses to move onto an instance the destination already has (it would replace it, and a rollback would then delete it). If the source doesn't answer the remove, it checks whether the service is still there before rolling the add back, so the service isn't left in neither cluster.

```json
{
  "dynamicConfig": {
    "version": 1,
    "serviceGroups": [
      {
        "name": "web",
        "services": [
          {
            "name": "api",
            "nodeId": "host1",
            "nodePort": 3000,
            "domains": ["api.example.com"]
          }
        ]
      }
    ]
  }
}
```

When the same service name is registered on several nodes, subscribers spread
connections across them. An optional `weight` (default `1`) sets each
instance's relative share, e.g. `9` and `1` for a 90/10 canary split. A weight
of `0` drains an instance: it gets no new connections unless every instance is
drained.

An optional `maxConnections` caps how many concurrent connections each
subscriber opens to an instance (a circuit breaker). Instances at their limit
are skipped; if every instance is full, new connections are closed immediately
rather than queued, and counted in
`stouter_tunnel_errors_total{reason="circuit_open"}`. The limit is enforced by
each subscriber independently, so an instance's total load can reach
`maxConnections` × the number of subscribers.

An optional `meta` map carries key/value metadata for integrations. Stouter
keeps it through gossip and config rewrites and serves it from the subscribe
REST API, but doesn't interpret it. For example, the
[Traefik plugin](https://github.com/dbpecka/traefik-plugin-stouter) reads
`traefik.scheme` to pick the backend scheme, such as `h2c` for gRPC:

```json
{ "name": "wp-grpc", "nodeId": "min1", "nodePort": 9000,
  "domains": ["wp-grpc.pecka.cloud"], "meta": { "traefik.scheme": "h2c" } }
```

CLI: `stouter service add ... --meta traefik.scheme=h2c` (repeatable).

Subscribe nodes can filter which service groups they proxy:

```json
{
  "subscribeFilter": {
    "serviceGroups": ["web"],
    "services": []
  }
}
```

### PKI / Certificates

On first startup, stouter generates a cluster CA and node certificate automatically. For multi-node clusters, issue certificates from the CA node:

```sh
stouter cert issue --node host2 --out /etc/stouter/tls/
```

Copy the resulting files to the new node's `tlsDir` before starting it.

**Certificate loading precedence:**
1. `ca.crt` + `ca.key` present → load CA, generate fresh node cert in memory
2. `ca.crt` + `node.crt` + `node.key` present → use pre-issued cert
3. Nothing present → generate new CA (written to disk) and node cert (memory only)

Anything else (say `ca.crt` and `node.crt` copied without `node.key`) is an
error naming what's there and what's missing. It used to be taken as
"nothing present", and the new CA overwrote the cluster's `ca.crt`, cutting
the node off for good.

Only the daemon does step 3. CLI commands (with `--cert-dir`, or the local
config's `tlsDir`) refuse a directory without certificates, so a mistyped path
gives a clear error instead of a new CA that no node trusts. Local `service`
commands on a host whose daemon has never run skip telling peers; the daemon
does when it starts.

Private keys (`ca.key`, and the `node.key` written by `cert issue`) are created
readable by their owner only (`0600`); stouter warns at startup if an existing
key is readable by other users.

### Docker

```sh
docker run -d --name stouter --network host \
  -v /srv/stouter/config/:/srv/stouter/config/ \
  stouter --config /srv/stouter/config/stouter.json
```

On `SIGTERM` (what `docker stop` sends) or `SIGINT`, stouter broadcasts
`NodeLeave`, stops accepting, refuses new tunnels (subscribers retry another
node) and waits up to 30s for in-flight tunnels and requests to finish. Idle
pooled connections and mux sessions don't count, so an idle daemon exits at
once. Docker's default stop timeout is 10s; pass `-t 30` to `docker stop` (or
`--stop-timeout 30` to `docker run`) to give long transfers the full window.

Keep `tlsDir` on a mounted volume (inside the config directory, or mounted
itself): in the container's own filesystem, the CA node's CA would be
replaced with the container, and no other node would trust the new one.
`deploy.sh` mounts the config's `tlsDir` when it lies outside the config
directory. It also takes only cluster names of letters, digits, `-` and `_`
(they become remote paths), and uploads the repository it is in, wherever
it's run from.

---

## Architecture

### Components

```
┌─────────────────────────────────────────────────────────────┐
│                        stouter node                         │
│                                                             │
│  ┌──────────┐   ┌──────────┐   ┌──────────┐  ┌─────────┐  │
│  │  Gossip  │   │  Relay   │   │ Reverse  │  │ Subscribe│  │
│  │  Loop    │   │ Handler  │   │  Pool    │  │  Proxies │  │
│  └────┬─────┘   └────┬─────┘   └────┬─────┘  └────┬────┘  │
│       │              │              │              │        │
│       └──────────────┴──────────────┴──────────────┘        │
│                             │                               │
│                       ┌─────┴──────┐                        │
│                       │   Shared   │                        │
│                       │   State    │                        │
│                       └────────────┘                        │
└─────────────────────────────────────────────────────────────┘
```

### Gossip Protocol

Every 10 seconds each node connects to all known peers (all at once, each
with a 10s timeout) and exchanges a `Sync` message. Only what changed travels:

- The initiator sends its own record and a digest: the version of every node
  record it holds, and its config's version and hash.
- The peer merges that and replies with only the records the initiator lacks
  or holds older versions of, and the config only if the peer's is newer (or
  different at the same version). Its reply also says which config it has:
  if the initiator's is newer, the initiator's next sync carries it. That is
  how a change made on a node nobody can dial (an `outboundOnly` one) spreads.
- A peer running an older version sends no digest; it gets full syncs (every
  record and the config) instead, as before.

A node re-signs its record only when its address, health report or relays
change, not every round, so most rounds carry no records at all. Measured
per node: 22 KB/s before, 4 KB/s after with 10 nodes; 91 KB/s before, 9 KB/s
after with 20. It still grows with the cluster, as every node syncs with every
peer each round, but in proportion to it rather than with its square.

On receiving a config:

- If it has a higher version, the local dynamic config is replaced.
  Two *different* configs at the same version (changes made at once on two
  nodes) are settled the same way everywhere, by the `changedBy` node that made
  each and then by content, so the cluster can't stay split. Between one
  change and a copy of it relayed by an older node that dropped fields it
  doesn't know (a signature, a newer setting), the fuller copy wins, so a
  rolling upgrade doesn't erase them.
- New peer addresses are merged into the known node set.

**Signed node records.** What each node says about itself (its address,
heartbeat, health report, and for an `outboundOnly` node the relays it holds
reverse connections to) is signed with its own certificate's key and relayed
as is. Peers believe what they hear about node X only from X's signature, so
no node can point X's address at itself, claim to be X's relay, mark X's
services down or fake X's heartbeat. Once X has been seen signed, unsigned
gossip about X is ignored; with `requireSignedConfig`, unsigned gossip about
any node is.

**Liveness.** A node with an address is alive to a peer that reaches it, or
that it syncs with itself. An `outboundOnly` node can't be reached, so every
5 seconds it signs a small *beat*, a rising counter, checked against the key
in its record: that is how subscribers know it is up. It sends its beat to
every node with an address in its own syncs. Each round a node also asks two
of its peers for the beats it lacks, which is how beats reach nodes the NAT'd
node can't sync with (other NAT'd nodes). A beat is never sent to a peer that
already has it, so beats cost little even when many nodes are NAT'd.
Heartbeats and beats have the node's *incarnation* as their high half, one
higher every time the daemon starts (kept in `<config file>.state`, and never
below the current Unix time), so a restarted node always outranks what it
gossiped before, even if its clock was set back meanwhile. A node with no sign
of life for 5 minutes is forgotten. (So is an entry a relay made when an
older NAT'd node registered with it, once its registrations stop.)

**Departures.** A node shutting down signs a final record saying it left, and
sends it to its peers with `NodeLeave`. Its peers keep that record for 30
minutes and pass it on to anyone still listing the node, so the departure
reaches nodes the leave message didn't (an `outboundOnly` node, which no one
can send to, or one that was briefly cut off) within a sync round, instead of
after the 5-minute expiry. A node that left or was forgotten is not brought
back by a stale copy of its record; it rejoins with a newer one (it
restarted), by talking to its peers itself, or, for an `outboundOnly` node,
which can't be reached to see it's back, by a newer beat it signed. A
forgotten node may only have
been cut off, so its last address is retried about once a minute for 24
hours: when the network heals, the two sides find each other again even if
neither lists the other in `knownNodes`. A daemon never lists itself as a
peer. A reverse registration from a node that left (one of its workers,
still connecting as it shut down) is refused, rather than bring it back.

Message types:

| Message | Purpose |
|---|---|
| `Sync` | State exchange: records, beats and config, as a digest-based delta with peers that support it |
| `ConfigUpdate` | Notify peers of a new config version |
| `NodeJoin` / `NodeLeave` | Cluster membership changes (a node may only announce its own; a leave carries its signed final record) |
| `TunnelRequest` | Client requesting a tunnel to a named service |
| `ReverseRegistration` | NAT'd node registering an idle reverse connection |
| `MuxTunnel` | Upgrade connection to yamux multiplexing |

All gossip messages are MessagePack-encoded with a 4-byte big-endian length prefix. All connections are mTLS-authenticated.

Limits, so one peer can't exhaust a node:

- Messages are at most 8 MiB; an oversized one is refused (and warned about)
  on both ends. A config change that would make the config larger than 2 MiB
  encoded (a signed config counts its signed copy too) is refused, since it
  could never reach the rest of the cluster.
- One peer may hold at most 64 connections waiting for their first message
  (subscribers keep 8 warm per node they tunnel to), 16 mux sessions, and 1024
  idle reverse connections. Accepted connections use TCP keepalive, so ones whose peer
  vanished are closed.
- At most 512 TLS handshakes may be in progress at once, 128 from one IP
  address; further connections are closed at once. Until it completes its
  handshake a connection could be anyone's, so this keeps a host with no
  certificate from tying up every file descriptor.
- A failed `accept` (such as no file descriptors left) is retried after a
  short, growing wait and logged, on the mesh listener, local proxies and
  `httpListen` alike: it never stops the daemon or a proxy.
- A tunnel's error message is read only up to 64 KiB and within 5 seconds:
  its length comes from the peer, and a 4 GiB one used to abort the daemon.
- Signatures in a sync are checked before the node list is locked, and at
  most 256 records for nodes not yet known are checked per sync (the rest
  wait for the next one). A peer sending junk can't stall gossip or reverse
  registrations behind its checks.
- Peers that don't answer a sync are retried with backoff, from one round
  up to about 3 minutes, so a dead address costs a dial every few minutes,
  not every 10 seconds. One that syncs with us, or whose newer signed record
  reaches us (it restarted), is tried again at once.
- Without `requireSignedConfig`, any cluster member can make up nodes; at
  most 1024 nodes known only from unsigned gossip are held, and at most 512
  forgotten addresses are retried. `requireSignedConfig` closes this.

### Tunnel Routing

When a subscribe proxy needs to reach service `api` on node `host2`:

1. Look up all nodes hosting `api` in the service index, dropping nodes whose
   gossip liveness is stale and nodes whose health check reports `api` down.
2. Select a node by smooth weighted round-robin, skipping drained (weight `0`)
   and ejected nodes, and nodes at their `maxConnections` limit.
3. Acquire a connection (in priority order):
   - **Mux session** — open a new yamux stream on an existing multiplexed connection (fastest)
   - **Reverse pool** — take an idle reverse connection (for NAT'd target nodes)
   - **Tunnel pool** — use a pre-warmed TCP connection
   - **Cold connect** — open a fresh mTLS TCP connection
4. Send a `TunnelRequest{node_id, service_name, timestamp_ms}`.
5. Read a status byte: `0x00` = success.
6. Proxy bytes bidirectionally between the local client and the remote service.

Steps 3–5 must finish within 15s; a node that stops answering counts as a
failed setup rather than hanging the client. Pooled connections the peer has
closed are never handed out (and are swept every 5s), and pools to an
unreachable node are refilled with exponential backoff (1s up to 60s), not in
a tight loop.

If steps 3–5 fail, the client connection is untouched, so the proxy retries on
another node (up to 3 distinct nodes). A node whose tunnel setup fails 3 times
in a row (unreachable, or its backend refuses connections) is **ejected**: it
is left out of rotation for 10s, doubling on each repeat ejection up to 2
minutes, and a successful tunnel resets it. Every filter fails open: if it would
exclude all nodes, it is ignored. Ejections are counted in
`stouter_outlier_ejections_total{service,node_id}`.

### HTTP Services

Set `"protocol": "http"` on every instance of a service (CLI: `--protocol http`)
to make subscribers proxy it as HTTP/1.1 instead of a byte stream:

- **Per-request load balancing** over pooled keep-alive connections to each
  node, so one long-lived client connection is still spread across instances,
  and tunnels are reused instead of opened per request.
- **Timeouts:** optional `requestTimeoutMs` per instance (time allowed for
  response headers); expiry returns `504`.
- **Retries** go to a different node (up to 3 nodes in total):
  - when a connection to the chosen node can't be established (the request
    was never sent), for any method;
  - when a node answers `502`, `503` or `504`, for idempotent methods only
    (GET, HEAD, OPTIONS, PUT, DELETE, TRACE). These retries are capped by a
    per-service budget of about 20% of request volume plus a burst of 20, so
    they can't multiply load on a failing service. If every attempt fails, the
    client gets the last real backend response.

  Bodies with a declared length up to 64 KiB are buffered so they can be
  re-sent; larger or chunked bodies are streamed and sent only once.
  Retries are counted in `stouter_http_retries_total{service,reason}`, and
  ones skipped for lack of budget in `stouter_http_retry_budget_exhausted_total`.
- **Errors** from the proxy itself carry an `x-stouter-error` header:
  `502 no_backend_reachable`, `503 circuit_open` (`maxConnections` then caps
  in-flight requests), `504 timeout`.
- **WebSocket and other upgrades** are passed through.
- **Tracing:** W3C `traceparent` is propagated (with a new span id per hop),
  or started when absent. With a `tracing` section in the daemon's config,
  each request is also exported as an OpenTelemetry span:

  ```json
  "tracing": { "otlpEndpoint": "http://127.0.0.1:4318", "sampleRatio": 0.1 }
  ```

  Spans go over OTLP/HTTP (JSON) to `{otlpEndpoint}/v1/traces`, typically a
  local OpenTelemetry Collector (plain `http://` only). Sampling is
  parent-based: an incoming `traceparent` keeps its decision, and new traces
  are sampled at `sampleRatio`. The backend's own spans nest under stouter's.
- **Access log** per request (target `stouter::access`, with trace id), and
  `stouter_http_requests_total{service,method,code}` /
  `stouter_http_request_duration_seconds{service}` metrics.

A service is proxied as HTTP only if every instance says `http`, and the
setting is read for each new connection, so it can be switched without a
restart. HTTP/2 and gRPC services should stay `tcp` for now. See
[`docs/http-proxy.md`](docs/http-proxy.md) for the design and roadmap.

### L7 Routing

`http` services can be routed by path, method or header to labeled subsets of
their instances, with weights — for example canary by header, or a 90/10
version split:

```bash
stouter service add --group web --name api --port 3000 --protocol http --label version=v1
stouter service add --group web --name api --port 3000 --protocol http --label version=v2 --node host2
stouter route set --service api --routes '[
  {"name": "canary", "match": {"headers": {"x-canary": "true"}},
   "backends": [{"labels": {"version": "v2"}}]},
  {"name": "split", "backends": [{"labels": {"version": "v1"}, "weight": 9},
                                 {"labels": {"version": "v2"}, "weight": 1}]}
]'
```

The first matching route wins. Requests that match no route use every
instance. Retries stay inside the chosen subset, and a subset with no
instances returns `503 no_route_backend`. `stouter route list` and
`route clear` complete the set, all with `--remote` support. See
[`docs/http-proxy.md`](docs/http-proxy.md#milestone-2-l7-routing-implemented)
for the full semantics.

### Health Checks

Every node probes each service it hosts every 5s by opening a TCP connection to
`127.0.0.1:{nodePort}` (2s timeout). If the instance sets `healthPath` (CLI:
`--health-path /healthz`), the node also sends `GET {healthPath}` and requires a
`2xx` status, so a backend that accepts connections but is failing is caught
too. This works for `tcp` and `http` services alike. Two failures in a row mark the service
unhealthy; one success marks it healthy again. The node publishes the result in
its own gossip entry and pushes it to peers as soon as it changes, so
subscribers stop routing to a dead backend within about 10s of it going down,
before any client connection has to fail. Reports carry a version ordered like
heartbeats, so a relayed copy never overwrites a newer one and a restarted
node's first report replaces anything peers held from before.

Health shows up in `stouter status` (an `UNHEALTHY` marker per service) and in
the `stouter_service_healthy{service,node_id}` gauge (1 or 0), which every
subscriber exports for the whole cluster. Outlier ejection still applies to
failures health checks can't see, such as a node being unreachable.

### NAT Traversal via Reverse Connections

Nodes with `"outboundOnly": true` cannot accept inbound connections. Instead, they maintain a pool of outbound connections to 3 relays, picked among the live nodes that have an address, other than `subscribe`-mode ones (often client machines, which would then carry other subscribers' traffic) unless there are no others:

```
NAT'd node                  Relay node
    │──── TCP connect ──────────►│
    │──── ReverseRegistration ──►│  (held idle in relay's reverse_pool)
    │                            │
    │◄─── TunnelRequest ─────────│  (relay forwards inbound request)
    │──── status + data ────────►│
    │◄─────────── data ──────────│  (bidirectional proxy)
```

The NAT'd node replenishes its pool immediately after each use, maintaining `reversePoolSize` idle connections per relay at all times. It keeps its relays as long as they stay live, so the set (which it signs into its record) doesn't change as nodes come and go; one that stops answering for 30 seconds is replaced. New relays are picked by rendezvous hashing of the two node ids, which spreads NAT'd nodes evenly over the cluster: each node's idle reverse connections stay about the same as the cluster grows, instead of growing with it. A relay only accepts a registration for node X from a certificate for X, and subscribers send X's tunnels only through relays X lists in its signed record, so no other node can take X's traffic. Each subscriber uses the live relay ranking highest for it (rendezvous hashing again, on the subscriber's and relay's ids), so subscribers spread over X's relays and use the reverse connections X keeps at each, instead of all going through one. If a reverse connection turns out to be dead, the relay tries another (up to 3) before failing the tunnel. Workers back off exponentially when a relay can't be reached, and stop when it is no longer one of the node's relays.

### Multiplexing (Yamux)

To amortize TLS handshake cost, subscribe nodes maintain yamux-multiplexed sessions, and 8 warm connections, to each node their tunnels go to: the nodes hosting a service they proxy (after `subscribeFilter`), or for an `outboundOnly` one, the relay they reach it through. Multiple concurrent tunnels share a single underlying TCP connection. The mux pool is filled proactively in the background so streams are available without waiting for a new handshake. A stream counts against the session until the other end first answers on it, and yamux opens no more while 256 haven't been answered: tunnels to one slow destination (a NAT'd node with no reverse connection ready, a hung service) could hold up every other tunnel on the session. So a tunnel waits at most 1 second for a stream, then takes a pooled or new connection instead, and for the next 2 seconds tunnels skip that session. When a node no longer hosts anything proxied, its warm connections are closed, and its session once the tunnels on it finish.

### mTLS / PKI

- A single cluster CA signs all node certificates.
- All node certs share the fixed SAN `stouter-node`, which allows standard WebPki hostname verification without per-node DNS configuration.
- Servers require client certificates; clients verify server certificates. Both sides authenticate against the cluster CA.
- A node's identity is its certificate's CN, which must match its `nodeId`. A tunnel to X, whether on a mux session, a pooled connection or a new one, must reach a server whose certificate names X (or, for a NAT'd X, a relay X signed for); a relay accepts reverse registrations for X only from X; only X may announce X joining or leaving; and only X's signature makes gossip about X believed (see [Gossip Protocol](#gossip-protocol)).
- Certificates are valid from the day they are issued (backdated a day for clock skew): 20 years for a new CA, 10 years for a node certificate, never past the CA's own expiry.
- Tunnel requests include a millisecond timestamp. The receiving node rejects requests with clock skew greater than ±30 seconds, providing basic replay protection.

### Signed config changes

Gossip carries the cluster's config through every node, and by default any
node with a cluster certificate can replace it. `adminCns` only guards the
admin RPC. With `"requireSignedConfig": true`, a node accepts a config change
(from gossip, an admin RPC, or its own config file) only if it was signed by a
certificate from the cluster CA whose CN is in that node's `adminCns`. The
signature covers the exact JSON text of the config, which travels with it, so
any node can relay a signed config but none can alter it.

- `stouter service add|rename|remove` and `stouter route set|clear` with
  `--remote` notice the flag in the daemon's status, make the change
  themselves, sign it with the certificate from `--cert-dir`, and push it
  (re-reading and retrying if the config changed meanwhile). Without
  `--remote`, they sign the change written to the local file.
- The CLI makes a change only on a config it has verified itself: the text an
  admin signed (not what a daemon reports around the signature), signed by a
  CN in the CLI's own config's `adminCns` or by the CLI's own certificate.
  It doesn't take the daemon's word for who the admins are: a node lying
  about the config could lie about that too, and get its edits signed. List
  your fellow admins in `adminCns` in the config the CLI reads. `service
  move` checks the source cluster's config the same way, and `config pull`
  writes only a verified config (or an unsigned one, with a warning, before
  the first signed change).
- A node starts from the text an admin signed, not from edits made around
  the signature in its config file; it logs that it ignores them.
- Editing a config file by hand no longer changes anything: the node logs a
  warning. Apply the edit with
  `stouter --cert-dir <admin certs> config push [--remote host:port]`
  (start from `stouter config pull` so the edit is on top of the cluster's
  current config).
- Issue the admin certificate with `stouter cert issue --node <admin CN>` and
  list that CN in `adminCns` on every node.

Turn it on everywhere at once, once every node runs a version that supports
it. Older nodes don't relay the signature, and a node without the flag still
adopts unsigned changes. Until an admin pushes a first signed change, nodes
keep the config they have (and warn at startup that it isn't signed).
Changes with `--remote` need that first signed config to build on: `stouter
config pull` it, review it, and `stouter config push` it.

### Shared State

All async tasks share a single `Arc<State>` that contains:

| Field | Contents |
|---|---|
| `dynamic_config` | Versioned, gossip-replicated service catalog |
| `known_nodes` | Discovered peer addresses |
| `service_index` | `(node_id, service_name)` → `Service` lookup |
| `reverse_pool` | Idle reverse connections keyed by node_id |
| `tunnel_pool` | Pre-warmed TCP connections keyed by address |
| `mux_pool` | Yamux sessions keyed by address |

`StreamPool<T>` is a generic pooling primitive backed by a `VecDeque` with async `notify` for waiters. `InFlightTracker` counts active connections and signals a drain channel on zero for graceful shutdown. Subscribers close their idle HTTP keep-alive connections to a node that leaves, and to NAT'd nodes reached through a relay that leaves, so neither waits out the 30-second drain for connections nobody is using.

### Subscribe REST API

Subscribe-mode nodes expose a REST API on port `5381` by default (override with `apiPort`):

```
GET /services          → list all proxied services and their local ports
GET /services/{name}   → one service
```

Each entry has `name`, `port`, `address`, `domains` and `meta`.

This lets local processes discover which `127.0.0.1` port to connect to for each service name without static configuration.

### Calling Services from Another Node

A process on a subscribe or `nodeSubscribe` host reaches a service on another
node through a local proxy on `127.0.0.1`. By default each proxy gets a random
port, which changes on restart and is found through `/services`.

To use a fixed address instead, give the service a `localPort` (CLI:
`--local-port 15002`). Every subscriber then opens that service's proxy on
`127.0.0.1:15002`, so callers can hard-code it:

```json
{ "name": "inventory", "nodeId": "host2", "nodePort": 5000, "localPort": 15002 }
```

- Instances that set `localPort` must agree, and two services can't share one.
  It also can't be the REST API's port or `httpListen`'s. `stouter check`
  rejects all of these.
- Changing it moves the proxy to the new port on every subscriber within a
  couple of seconds, without a restart.
- If the port is taken by another process, the subscriber logs one warning and
  keeps retrying. Until the bind succeeds, the service isn't proxied and is
  missing from `/services`.

#### One address for every HTTP service (`httpListen`)

For `http` services, a subscriber can also run a single listener that routes
each request by its hostname, so callers need no ports at all:

```json
{ "mode": "nodeSubscribe", "httpListen": "127.0.0.1:15001", ... }
```

```bash
export http_proxy=http://127.0.0.1:15001 HTTP_PROXY=http://127.0.0.1:15001
curl http://inventory/items/42          # → service "inventory"
curl http://stock.example.com/items/42  # → the service whose domains include it
```

- The target is the URL's host (sent in full when a client uses a proxy) or,
  failing that, the `Host` header. A service name matches first, then any
  service's `domains`, case-insensitively. Clients that ignore proxy settings
  can call `http://127.0.0.1:15001/...` with `Host: inventory`.
- Set both `http_proxy` and `HTTP_PROXY`: curl reads only the lowercase one,
  and many other clients read the uppercase one.
- These variables affect **every** HTTP call the process makes. List hosts
  outside the mesh in `no_proxy`/`NO_PROXY`. Stouter answers an unknown host
  with `404 unknown_service` rather than forwarding it to the internet.
- HTTPS through the proxy (`CONNECT`) gets `501 connect_not_supported`, and
  a `tcp` service gets `501 service_not_http`. Use those services' own (or
  fixed `localPort`) addresses. Rejections are counted in
  `stouter_http_listener_rejected_total{reason}`.
- Requests use the same per-service proxy as the service's local port: one
  balancer, so `maxConnections`, ejection, routes, retries and tracing apply
  across both. Services on the same node (in `nodeSubscribe`) are reachable
  too, connected to directly rather than through a tunnel to itself, and
  `subscribeFilter` applies. A TCP proxy for a service hosted here and
  elsewhere likewise counts this node's own instance among its backends.
- Only valid in subscribe modes. Keep it on loopback; stouter warns at
  startup if not, since anything that can reach it can call every `http`
  service in the mesh.
