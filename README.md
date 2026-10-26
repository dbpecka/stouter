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
| `advertiseAddr` | Address peers use to reach this node |
| `nodeId` | Unique identifier for this node |
| `outboundOnly` | Set `true` for nodes behind NAT — disables listener, uses reverse connections |
| `reversePoolSize` | Number of idle reverse connections to maintain per relay peer (default 64) |
| `knownNodes` | Bootstrap peer addresses for initial gossip |
| `tlsDir` | Directory for `ca.crt`, `ca.key`, `node.crt`, `node.key` |
| `adminCns` | Certificate common names allowed to issue remote admin RPCs (`--remote` mutations). Empty (default) disables admin RPC |
| `apiPort` | Subscribe-mode REST API port on `127.0.0.1` (default 5381). Set per-cluster when running multiple instances on one host. |

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
```

### Registering Services

Services are organized into groups. Each service entry maps a name to a local port and optional domain names:

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

### Docker

```sh
docker run -d --name stouter --network host \
  -v /srv/stouter/config/:/srv/stouter/config/ \
  stouter --config /srv/stouter/config/stouter.json
```

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

Every 10 seconds each node connects to all known peers and exchanges a `Sync` message containing its current config version and known node list. On receiving a sync:

- If the peer has a higher config version, the local dynamic config is replaced.
- New peer addresses are merged into the known node set.

Message types:

| Message | Purpose |
|---|---|
| `Sync` | Full state exchange (config + known nodes) |
| `ConfigUpdate` | Notify peers of a new config version |
| `NodeJoin` / `NodeLeave` | Cluster membership changes |
| `TunnelRequest` | Client requesting a tunnel to a named service |
| `ReverseRegistration` | NAT'd node registering an idle reverse connection |
| `MuxTunnel` | Upgrade connection to yamux multiplexing |

All gossip messages are MessagePack-encoded with a 4-byte big-endian length prefix. All connections are mTLS-authenticated.

### Tunnel Routing

When a subscribe proxy needs to reach service `api` on node `host2`:

1. Look up all nodes hosting `api` in the service index, dropping nodes whose
   gossip liveness is stale.
2. Select a node by smooth weighted round-robin, skipping drained (weight `0`)
   and ejected nodes.
3. Acquire a connection (in priority order):
   - **Mux session** — open a new yamux stream on an existing multiplexed connection (fastest)
   - **Reverse pool** — take an idle reverse connection (for NAT'd target nodes)
   - **Tunnel pool** — use a pre-warmed TCP connection
   - **Cold connect** — open a fresh mTLS TCP connection
4. Send a `TunnelRequest{node_id, service_name, timestamp_ms}`.
5. Read a status byte: `0x00` = success.
6. Proxy bytes bidirectionally between the local client and the remote service.

If steps 3–5 fail, the client connection is untouched, so the proxy retries on
another node (up to 3 distinct nodes). A node whose tunnel setup fails 3 times
in a row (unreachable, or its backend refuses connections) is **ejected**: it
is left out of rotation for 10s, doubling on each repeat ejection up to 2
minutes, and a successful tunnel resets it. Every filter fails open: if it would
exclude all nodes, it is ignored. Ejections are counted in
`stouter_outlier_ejections_total{service,node_id}`.

### NAT Traversal via Reverse Connections

Nodes with `"outboundOnly": true` cannot accept inbound connections. Instead, they maintain a pool of outbound connections to every reachable relay peer:

```
NAT'd node                  Relay node
    │──── TCP connect ──────────►│
    │──── ReverseRegistration ──►│  (held idle in relay's reverse_pool)
    │                            │
    │◄─── TunnelRequest ─────────│  (relay forwards inbound request)
    │──── status + data ────────►│
    │◄─────────── data ──────────│  (bidirectional proxy)
```

The NAT'd node replenishes its pool immediately after each use, maintaining `reversePoolSize` idle connections per relay peer at all times.

### Multiplexing (Yamux)

To amortize TLS handshake cost, subscribe nodes maintain yamux-multiplexed sessions to each peer. Multiple concurrent tunnels share a single underlying TCP connection. The mux pool is filled proactively in the background so streams are available without waiting for a new handshake.

### mTLS / PKI

- A single cluster CA signs all node certificates.
- All node certs share the fixed SAN `stouter-node`, which allows standard WebPki hostname verification without per-node DNS configuration.
- Servers require client certificates; clients verify server certificates. Both sides authenticate against the cluster CA.
- Tunnel requests include a millisecond timestamp. The receiving node rejects requests with clock skew greater than ±30 seconds, providing basic replay protection.

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

`StreamPool<T>` is a generic pooling primitive backed by a `VecDeque` with async `notify` for waiters. `InFlightTracker` counts active connections and signals a drain channel on zero for graceful shutdown.

### Subscribe REST API

Subscribe-mode nodes expose a REST API on port `5381` by default (override with `apiPort`):

```
GET /services          → list all proxied services and their local ports
```

This lets local processes discover which `127.0.0.1` port to connect to for each service name without static configuration.
