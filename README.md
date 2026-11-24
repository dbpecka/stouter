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
| `apiPort` | Port on `127.0.0.1` (default 5381) for the subscribe-mode REST API, or in `node` mode for `/metrics` alone. Set per-cluster when running multiple instances on one host. |
| `requireSignedConfig` | Accept a config change only if a certificate in `adminCns` signed it (see [Signed config changes](#signed-config-changes)). Default `false`. |
| `certLifetime` | On the CA node (its `tlsDir` holds `ca.key`): how long the certificates it issues last, as `30d`, `12h`, `45m` or `90s` (at least a minute). Default 10 years. Nodes renew theirs without restarting (see [Certificate renewal](#certificate-renewal)). |

The daemon won't start if one of its own settings is wrong (an invalid
`httpListen`, a `reversePoolSize` over the limit, `requireSignedConfig` without
`adminCns`, an empty `nodeId` or one with a `:` in it, which peers would take
for an address, and so on). Problems in `dynamicConfig`, such as a service listed
twice, are logged as warnings instead: that config is the cluster's, a newer
version may already fix it, and a node that refused to start over it could
never receive the fix. `stouter check` reports both kinds.

A setting stouter doesn't know (a typo such as `requireSignedConfg`, which
would otherwise leave signing off without a word, or one from a newer
version) is logged as a warning at startup and fails `stouter check`. At the
top level it is kept when the file is rewritten; inside a section
(`dynamicConfig`, a service, a route, a policy, `subscribeFilter`,
`tracing`: a service's `maxConection`, say) it is dropped then, so fix it
before the daemon rewrites the file. `check` also refuses what the daemon
can't start with or use: no `tlsDir` (or a blank one), or a `bind` (unless `outboundOnly`),
`advertiseAddr` or `knownNodes` entry without a port. A hand edit to `dynamicConfig` gets the
same checks as a change made with the CLI: one that would break the config
is refused, with a warning. An edit to any of it (a service, a policy, a
revocation) is taken without raising its version by hand: the daemon gives
it the next version (it checks the file every 10 seconds). One made while
the daemon was stopped (the file says something else than the last version
in its config history) waits until the daemon has caught up with the
cluster after starting (a first sync, or two minutes): if the cluster moved
on meanwhile, the newer config comes first and the edit is kept as
`<config>.unapplied`, to make again on top of it. Without
`requireSignedConfig`, that is; with it, a hand edit isn't taken at all.

CLI commands that change the local file (`init`, `service add` without
`--remote`, and so on) refuse a file they can't parse instead of starting
over from defaults and saving those over it. `init` sets `nodeId` to the
hostname only if it is empty, and `tlsDir` to a `tls` directory beside the
config file only if it is unset.

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

# Query daemon status (exits 1 if no daemon answers; an outboundOnly host's
# daemon listens nowhere: ask a node it syncs with, with --remote)
stouter status

# Register a service
stouter service add --group web --name api --port 3000 --domains api.example.com

# Give host1's instance of api 9x the connections of others (e.g. a 90/10 canary split)
stouter service add --group web --name api --port 3000 --node host1 --weight 9

# List all services in the cluster
stouter service list

# Issue a signed node certificate (run on the CA node)
stouter cert issue --node host2 --out /path/to/certs

# Only api-1 and api-2 may reach db from now on (see Service authorization)
stouter policy allow --service db --from api-1 --from api-2
stouter policy list

# Revoke a node's certificates: every one for a node removed for good, or
# one certificate whose key leaked (see Revoking certificates)
stouter cert revoke --node host2
stouter cert revoke --cert /path/to/leaked/node.crt
stouter cert revoked

# Copy the cluster's dynamic config into the local file (to edit it by hand)
stouter config pull --remote host1:8080

# Sign the dynamic config in the local file and push it (hand edits, with requireSignedConfig)
stouter --cert-dir /path/to/admin-certs config push --remote host1:8080

# What changed when, and putting an earlier version back (see Config history)
stouter config history
stouter config rollback --to 12
```

`config push` is signed with the CLI's certificate and taken from an admin
(`adminCns`), or, without `requireSignedConfig`, also from the daemon's own
certificate (the CLI on its host). It replaces the cluster's dynamic config
with the file's, so it
refuses a file older than the cluster's config: pushing it would undo the
changes made since. Run `config pull` (if the file's services or routes
differ, it first keeps a copy as `.bak`, or `.bak.2` and so on, never over an
earlier one), redo the edit, and push; or pass `--force`
to replace the cluster's config anyway. After a push the file holds the version
pushed, so the next edit starts from it.

#### Config history

Each daemon records every version of the dynamic config it takes, in
`<config>.history` beside its config file (a CLI changing the file with no
daemon running records too), keeping the last 50:

```sh
stouter config history                        # newest first: version, when, by whom, signed by, what changed
stouter config history --remote host2:8080    # another node's
stouter config history --version 12           # that version's config, as JSON
stouter config rollback --to 12               # put version 12's services, routes and policies back
```

```
version 14  2026-11-20 23:01:05 UTC by host1
    policy for api added: nobody
version 13  2026-11-20 22:58:41 UTC by host1
    service web/db on host1 added
```

A rollback is a new change, made the way `service` commands make theirs
(through the daemon, or in the local file): it gets the next version
number, so it reaches every node, and with `requireSignedConfig` it is
signed like any other change. There, only a version an admin signed can be
put back, checked by the CLI as it checks any config a daemon serves: a
daemon's history is its own word. Revocations are never rolled back (a
rollback can't let a revoked certificate back in): `cert unrevoke` does
that. A node's history is what it saw: a version that came and went
between two of its syncs isn't in it.

A running daemon writes each new config into its file. If the file holds an
edit it hasn't applied (a hand edit not yet picked up, one it refused, a
pulled config being edited), it saves the file as `<config>.unapplied`
(numbered like the backups)
first and logs a warning, rather than write over the edit. `config push`
mentions that file when the cluster already has what the config file now
says.

### Registering Services

Services are organized into groups. Each service entry maps a name to a local port and optional domain names. A service is known by its name everywhere (proxies, routes, the REST API), so all instances of a service belong to one group, and a domain can belong to only one service.

`stouter service add|rename|remove` and `stouter route set|clear`, local or `--remote`, refuse a change that would break the config, such as claiming another service's domain, or removing or renaming a service that still has routes (clear them first). A problem the config already had doesn't block unrelated changes.

Without `--remote`, these commands make the change through this host's daemon when it is running (`outboundOnly` ones have no listener to ask), as if `--remote` named it: editing the file behind a running daemon's back could undo a change the daemon took in at the same moment. Otherwise they edit the file, refusing if it changed while they ran. The daemon accepts these changes from its own certificate; with `requireSignedConfig` they must still be signed by an admin. `--node` defaults to this host's node, or with `--remote`, to the node at that address.

CLI commands print their results on stdout, and only those: their log lines (warnings and errors; more with `RUST_LOG=info`) go to stderr. The daemon logs to stdout from `info` up (`RUST_LOG` sets the level), in colour only on a terminal.

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

#### Certificate renewal

Every node renews its certificate once a third of its lifetime is left, and
switches to the new one without restarting: new connections present it, and
its record is re-signed with it (connections already open keep going). How
long certificates last is up to the CA node's `certLifetime` (default 10
years); set it short (`"certLifetime": "30d"`), and a leaked key stops being
good within weeks even if nobody revokes it.

- The CA node (its `tlsDir` holds `ca.key`) issues its own new certificate,
  in memory as always (a new one is issued at each start too).
- Any other node makes a new key and asks a node holding the CA's key for a
  certificate on it, over a connection made with its current certificate:
  the new one is for whoever that certificate names, so a node can only
  renew its own, and a revoked certificate can't connect to ask. The new
  `node.crt` and `node.key` are written to its `tlsDir` for the next start
  (so the directory must be writable).
- A certificate's expiry is checked when a connection is made: a
  connection opened before it expired keeps going (a node renews long
  before then), while a revoked certificate's connections are closed at
  once. A NAT'd node's beats signed with an expired certificate's key keep
  it alive no longer.
- A certificate copied into a node's `tlsDir` by hand (after `stouter cert
  issue`) is taken up the same way, without a restart, if it lasts longer
  than the one in use.
- `stouter status` shows when the daemon's certificate expires.
- Renew in time: an expired certificate can't connect to ask for another,
  and peers refuse it. A node that can't reach a CA node keeps trying, with
  warnings as expiry nears; one whose certificate expired logs an error and
  needs one issued and copied in by hand. Keep the CA node reachable, or
  put `ca.key` on more than one node.
- With every node on this version, shorten `certLifetime`; an older node
  doesn't renew, so a short-lived certificate would run out on it.
- A leaked key can renew its certificate until that certificate is revoked.
  If you can't tell which certificates the holder may have obtained, revoke
  the node's name.

TLS sessions are never resumed: a resumed session skips the certificate
checks and carries the certificate of the handshake it resumes, however
long ago that was. Every mesh connection runs a full handshake (they are
long-lived and pooled, so it costs little).

#### Revoking certificates

A certificate from the cluster CA is otherwise trusted until it expires (10
years for a node's). Revoke one when a node is removed for good or a key
leaks:

```sh
# Every certificate for a node name, issued before or after: a node removed for good
stouter cert revoke --node host2
# One certificate, by its file or fingerprint: a key that leaked. The node
# can be issued a new certificate (`cert issue`) and carry on.
stouter cert revoke --cert /path/to/node.crt
stouter cert revoke --fingerprint 3f:9a:...   # as `cert fingerprint` or `openssl x509 -fingerprint -sha256` prints it
stouter cert revoked                          # list them
stouter cert unrevoke --node host2            # undo
```

Revocations are part of the dynamic config (`revoked`: `nodes`, and `certs`
as SHA-256 fingerprints), so they reach every node by gossip like any
other change, and go through the daemon (`--remote`) or the local file like
`service` commands. Every node then refuses a revoked certificate wherever it
appears: in a TLS handshake (as client or server), in a node record or beat,
and on a config signature. It closes idle connections with it, connections
in use carry no more data, and it forgets a node whose certificate is
revoked, so gossip can't bring it back. `cert issue` warns when the name is
revoked, as the certificate would be refused everywhere.

- With `requireSignedConfig`, a revocation is an admin-signed change like any
  other, and a revoked admin's signature counts for nothing from then on. A
  config that revokes the certificate that signed it is refused (another
  admin must revoke it), and the CLI won't revoke the certificate it uses.
  The CLI refuses the certificates its own config revokes too, whether on a
  daemon it connects to or on a config it is to build a change on: a config
  forged with a revoked admin's key isn't signed over by another admin.
  Without `requireSignedConfig`, any member can change the config, so any
  member can revoke (or unrevoke) any other.
- A node keeps running with its own certificate revoked, and logs an error:
  its peers refuse it until it has a new certificate or is unrevoked.
- Every node must run a version that knows revocations: an older one ignores
  the list, and one relaying an unsigned config drops it.

### Service authorization

By default any node in the cluster can open a tunnel to any service. A
service's policy limits that to the nodes it lists (and the nodes hosting the
service, whose instances may reach one another):

```sh
stouter policy allow --service db --from api-1 --from api-2   # creates the policy: db is closed to every other node
stouter policy deny  --service db --from api-2                # api-2 no longer (db stays closed, even with nobody left)
stouter policy clear --service db                             # no policy: db open to every node again
stouter policy list
```

Policies are part of the dynamic config (`policies`: `service` and `allow`,
a list of node names), so they reach every node by gossip, and go through
the daemon (`--remote`) or the local file like `service` commands. With
`requireSignedConfig`, only an admin can change them.

- The node hosting a service enforces its policy: a caller could skip a
  check of its own. It refuses a tunnel from any other node with an error
  saying why, and counts it in `stouter_tunnel_denied_total{service}`.
- Who is asking is the certificate at the other end of the connection,
  when the caller connects directly. A NAT'd node gets its tunnels through a
  relay, whose certificate says only who the relay is; so callers sign their
  requests to NAT'd nodes, and those nodes take a request through a relay
  for a service with a policy only if it is signed by a node the policy
  lists. A relay can pass a request on but not make one up for another node,
  and a request passed on twice is refused (a caller signs its request
  afresh for each relay it tries; only allowed callers' requests are
  remembered for that, each caller with a share of the room). (A relay
  still carries the
  tunnel's bytes, as it always has: it can tamper with or cut off the
  tunnels that go through it.)
- A subscriber doesn't proxy a service its policy keeps it from, so local
  callers are refused at once; it logs which.
- The grant is to a node: every local process on an allowed node can use its
  proxy. Telling them apart needs per-service identities (not yet).
- Older nodes neither sign their requests nor check policies: a NAT'd node
  with a policy refuses their requests through a relay. Upgrade the nodes
  that call a protected service before adding its policy.

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
once. A second signal during the drain exits at once (status 1). Docker's
default stop timeout is 10s; pass `-t 30` to `docker stop` (or
`--stop-timeout 30` to `docker run`) to give long transfers the full window.

Keep `tlsDir` on a mounted volume (inside the config directory, or mounted
itself): in the container's own filesystem, the CA node's CA would be
replaced with the container, and no other node would trust the new one.
`deploy.sh` and `docker-create.sh [config dir]` mount the config's `tlsDir`
when it lies outside the config
directory. It also takes only cluster names of letters, digits, `-` and `_`
(they become remote paths), but not `build` or `config` (the default
cluster's own directories), and uploads the repository it is in, wherever
it's run from.

Before replacing the running container, `deploy.sh` checks the host's
config with the new image (`stouter check --local`: what would stop the
daemon from starting) and stops there if it fails, leaving the running
container as it is. After starting the new one it waits 5 seconds
(`DEPLOY_SETTLE_SECONDS`) and fails, printing its last log lines, if the
container isn't running or has been restarted: `docker ps` says "Up" even
of a daemon that exits at once and is restarted over and over. If the new
image can't be run to check the config at all, it stops too. Only once the
new container stays up does it install the image's binary on the host (as
`/usr/local/bin/stouter`), so a deploy that stops leaves the host's CLI
matching the daemon still running.

### Testing

`cargo test` runs the unit and in-process tests. End-to-end scripts in
`tests/e2e/` run real daemons; each prints PASS/FAIL lines and exits
non-zero on a failure (`KEEP_LOGS=1` keeps the logs of a failed run):

| Script | Checks | Needs |
|--------|--------|-------|
| `caps.sh BINARY` | Per-peer connection caps under load, and that stalled handshakes from several addresses can't lock peers out | python3 |
| `policy.sh BINARY` | Service policies: the allowed subscriber still reaches a direct and a NAT'd service (signed, through a relay); the hosting nodes refuse anyone else, and unsigned requests through a relay even from the allowed node's certificate; deny and clear | python3 |
| `renew.sh BINARY` | With `certLifetime` 90s, the CA node and another node renew without restarting and still work after their first certificates expired; the renewed one is on disk; one copied in by hand is taken up (about three minutes) | |
| `history.sh BINARY` | Changes through one daemon are recorded with what changed, on both nodes; `config rollback` puts a version back as a newer one that reaches the other node | |
| `revoke.sh BINARY` | `cert revoke --node` through one daemon reaches the others: every node forgets the node and refuses it; `cert unrevoke` lets it back | |
| `multihost.sh BINARY` | Nodes on separate hosts (network namespaces), all bound to `0.0.0.0`, find each other's real addresses and tunnel | root, iproute2, python3, curl |
| `docker-stop.sh` | `docker stop` on stouter as a container's PID 1: it leaves, drains and exits 0 within seconds | Docker, the `stouter:latest` image |
| `deploy-host.sh [user@host]` | `deploy.sh` against a real host: the container runs and answers, a redeploy keeps the CA, a config the daemon can't start with fails the deploy before the running container is touched, one it can't stay up with fails it after | ssh with keys, sudo and Docker on the host |

CI (`.github/workflows/ci.yml`) runs clippy and the tests on Linux, the
tests on macOS and Windows, a build check with Rust 1.88 (the Docker
image's), and all eight scripts, `deploy-host.sh` over ssh from the runner
to itself.

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

Every 10 seconds each node syncs with all known peers (all at once, each
with a 10s timeout), exchanging a `Sync` message. A peer keeps the connection
open for the next round when asked (older ones close it), so a cluster in a
steady state opens no new connections for gossip: in a 10-node test cluster,
540 new mTLS connections a minute before, none after. A kept connection that
has died is replaced at once. Only what changed travels:

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
  nodes) are settled the same way everywhere, so the cluster can't stay
  split: the one revoking more certificates wins (so a change made without
  having seen a revocation can't undo it), then by the `changedBy` node that
  made each, then by content. Two signed configs signed more than an hour
  apart (on either side of a partition, say) are settled by when instead:
  the later wins, as nodes refuse a config signed that long before the one
  they hold. Every node that drops a revocation this way logs a warning
  naming it. Nodes running an earlier version settle ties without either
  rule, so in a mixed cluster such a tie can stay split until the next
  change. Between one
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
  (subscribers keep 8 warm per node they tunnel to), 16 mux sessions, 1024
  idle reverse connections, and 2 sync connections kept open between rounds.
  A mux stream's first message is a tunnel request: it may be at most 16 KiB
  and must arrive within 10 seconds, and a stream counts as work in flight
  only once it has. Until its tunnel is set up (the request read, then the
  service or the NAT'd node connected), nothing reads what the peer sends on
  a stream, so it may hold a full window (256 KiB): one peer may have at most
  512 such streams, across all its sessions (128 MiB). A mux session may
  buffer at most 128 MiB unread across its streams (yamux's default is 1
  GiB). A relay refuses at once a tunnel to a node that keeps no reverse
  connections there (none registered in the last two minutes), rather than
  hold it while waiting for one.
  Connections use TCP keepalive (probes after 30s idle, every 10s, 3 tries),
  so an idle one whose peer vanished is closed within about a minute. So is
  one with data waiting to be acknowledged, after 60s of retransmitting
  (`TCP_USER_TIMEOUT` on Linux, `TCP_RXT_CONNDROPTIME` on macOS, `TCP_MAXRT`
  on Windows), where TCP would otherwise retransmit for many minutes (about
  15 on Linux). Other systems keep their own retransmission limits.
- At most 512 TLS handshakes may be in progress at once, 128 from one IPv4
  address or IPv6 /64. Until it completes its handshake a connection could
  be anyone's, so this keeps a host with no certificate from tying up every
  file descriptor. A further connection from an address at its 128 is closed
  at once; one past the 512 is let in, and the oldest handshake of the
  address with the most in progress is dropped for it. Hosts that open
  connections and send nothing then only displace one another, while a
  peer's handshake (milliseconds, from an address with few in progress)
  still gets through.
- A relay keeps nothing for a node id a tunnel request names once the
  request is done, so requests for made-up nodes leave nothing behind.
- A failed `accept` (such as no file descriptors left) is retried after a
  short, growing wait and logged, on the mesh listener, local proxies and
  `httpListen` alike: it never stops the daemon or a proxy.
- A tunnel's error message is read only up to 64 KiB and within 5 seconds:
  its length comes from the peer, and a 4 GiB one used to abort the daemon.
- Signatures in a sync are checked before the node list is locked, on
  blocking threads (as many syncs at once as there are CPUs), not the ones
  serving tunnels and admin requests. A sync with nothing to check doesn't
  wait for one, and one peer may have at most 2 syncs being checked at once
  (more are dropped; the next round sends them again), so a peer sending
  junk can't queue ahead of everyone else. Per sync, at most 256 records for nodes
  not yet known are checked (the rest wait for the next one), and only each
  node's newest beat, only for nodes with no address (the only ones that
  beat), at most 4096 of them. A peer sending junk can't stall gossip,
  tunnels or reverse registrations behind its checks.
- A config more than 2^28 versions past the one a node holds is refused, from
  a peer, a push or the config file: each change adds one, and a config at
  the last version there is (`u64::MAX`) could never be followed by another.
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
   - **Reverse pool** — an idle reverse connection the target node holds to
     this daemon (when this daemon is one of a NAT'd node's relays)
   - **Mux session** — open a new yamux stream on an existing multiplexed connection
   - **Tunnel pool** — use a pre-warmed TCP connection
   - **Cold connect** — open a fresh mTLS TCP connection

   For a NAT'd target, the last three go to one of its relays, and failing
   that (or with no answer within 5 seconds) to its next one.
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
- **Timeouts:** optional `requestTimeoutMs` per instance: the time allowed
  for response headers, counted from when the tunnel to the node is up;
  expiry returns `504`. Setting up a new tunnel may take as long again: one
  that takes longer counts as a failed connection, so the node is charged
  (towards ejection) and the request goes to another node. A NAT'd node's
  relays share that time, so one relay down doesn't fail the setup.
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
  Each export, the collector's whole answer included, has 10 seconds: a
  collector that stalls partway through doesn't hold up later exports.
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
subscriber exports for the whole cluster. Gauges about a node (this one, and
`stouter_reverse_pool_size`, `stouter_tunnel_pool_size`) stop being reported
30 seconds after it leaves or is forgotten. Outlier ejection still applies to
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

The NAT'd node replenishes its pool immediately after each use, maintaining `reversePoolSize` idle connections per relay at all times. It keeps its relays as long as they stay live, so the set (which it signs into its record) doesn't change as nodes come and go; one that stops answering for 30 seconds is replaced. New relays are picked by rendezvous hashing of the two node ids, which spreads NAT'd nodes evenly over the cluster: each node's idle reverse connections stay about the same as the cluster grows, instead of growing with it. A relay only accepts a registration for node X from a certificate for X, and subscribers send X's tunnels only through relays X lists in its signed record, so no other node can take X's traffic. Each subscriber uses the live relay of X's ranking highest for the pair (rendezvous hashing again, on the subscriber's, X's and the relay's ids), so subscribers spread over X's relays and use the reverse connections X keeps at each, and a subscriber's tunnels to many NAT'd nodes sharing relays spread over them too, instead of all going through one. If a tunnel through that relay fails (or gets no answer within 5 seconds, when its host stopped answering), the subscriber tries X's next relay in the same setup, so a relay going down doesn't fail X's tunnels for the half minute until gossip marks it stale. With a shorter time for the whole setup (an HTTP service's `requestTimeoutMs`), each relay left gets an equal share of it, so the next one still gets its turn; and a relay that didn't answer in time is tried after X's others for 20 seconds. If a reverse connection turns out to be dead, the relay tries another (up to 3) before failing the tunnel, all within 12 seconds, so the subscriber (which waits 15) hears why. An `outboundOnly` subscriber in `subscribe` mode hosts nothing, so it keeps no reverse connections. Workers back off exponentially when a relay can't be reached, and stop when it is no longer one of the node's relays.

### Multiplexing (Yamux)

To amortize TLS handshake cost, subscribe nodes maintain yamux-multiplexed sessions, and 8 warm connections, to each node their tunnels go to: the nodes hosting a service they proxy (after `subscribeFilter`), or for an `outboundOnly` one, the relay they reach it through. Multiple concurrent tunnels share a single underlying TCP connection. The mux pool is filled proactively in the background so streams are available without waiting for a new handshake. A stream counts against the session until the other end first answers on it, and yamux opens no more while 256 haven't been answered: tunnels to one slow destination (a NAT'd node with no reverse connection ready, a hung service) could hold up every other tunnel on the session. So a tunnel waits at most 1 second for a stream, then takes a pooled or new connection instead, and for the next 2 seconds tunnels skip that session. When a node no longer hosts anything proxied, its warm connections are closed, and its session once the tunnels on it finish.

### mTLS / PKI

- A single cluster CA signs all node certificates.
- All node certs share the fixed SAN `stouter-node`, which allows standard WebPki hostname verification without per-node DNS configuration.
- Servers require client certificates; clients verify server certificates. Both sides authenticate against the cluster CA.
- A node's identity is its certificate's CN, which must match its `nodeId`. A tunnel to X, whether on a mux session, a pooled connection or a new one, must reach a server whose certificate names X (or, for a NAT'd X, a relay X signed for); a relay accepts reverse registrations for X only from X; only X may announce X joining or leaving; and only X's signature makes gossip about X believed (see [Gossip Protocol](#gossip-protocol)).
- Certificates are valid from the day they are issued (backdated a day for clock skew): 20 years for a new CA, 10 years for a node certificate, never past the CA's own expiry, unless revoked (see [Revoking certificates](#revoking-certificates)).
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
  the first signed change). Nor does it build on (or pull) a verified config
  older than the admin-signed one in its own config file: the node is behind
  the cluster, or lying, and the admin's change would drop what came since.
  `config rollback --to N` puts back only a config that is, as signed,
  version N. Nor does `config pull` write an unsigned config over a file
  that holds a signed one.
- A node starts from the text an admin signed, not from edits made around
  the signature in its config file; it logs that it ignores them.
- Editing a config file by hand no longer changes anything: the node logs a
  warning. Apply the edit with
  `stouter --cert-dir <admin certs> config push [--remote host:port]`
  (start from `stouter config pull` so the edit is on top of the cluster's
  current config).
- Issue the admin certificate with `stouter cert issue --node <admin CN>` and
  list that CN in `adminCns` on every node.
- A signed config records when it was signed (`signedAt`, part of what is
  signed), and the signer's certificate is checked as of then: a config
  stays valid after its signer's certificate expires (a short
  `certLifetime` doesn't strand the cluster's config), while revocation is
  checked as of now. A config dated over an hour ahead is refused. (The
  hour allows for the signing hosts' clocks to differ.) Configs signed by
  older versions carry no date and are checked as of now, until the next
  signed change.
- An expired key that wasn't revoked can still sign, dating the config back
  into its certificate's life. So:
  - Nodes refuse a config signed more than an hour before the one they
    hold.
  - A running node takes no config whose signer's certificate has expired;
    only a node catching up in its first two minutes after starting does
    (it can't tell one from the cluster's config, signed in time).
  - A push must be signed by a certificate valid now.
  - The CLI won't build a change on, or pull, a config that the signed one
    in its own file says can't be the cluster's: older, another config at
    the same version, or newer but from an expired signer (`config pull
    --accept-expired-signer` takes one you know was signed in time).
  - Every node warns while the config in force has an expired signer, and
    `stouter status` says so: `stouter config push` signs it again (with
    nothing changed), after which that key can sign nothing nodes take.
  Revoke an admin certificate you retire: expiry alone leaves its key able
  to sign configs that nodes starting up take until the config is signed
  again.

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

`StreamPool<T>` is a generic pooling primitive backed by a `VecDeque` with async `notify` for waiters. `InFlightTracker` counts active connections and signals a drain channel on zero for graceful shutdown. Subscribers close their idle HTTP keep-alive connections to a node that leaves, and to NAT'd nodes reached through a relay that leaves, so neither waits out the 30-second drain for connections nobody is using. They do the same for a NAT'd node whose tunnels move to another of its relays (the one before stopped answering, say): a request sent on a pooled connection through a relay that vanished would otherwise wait for TCP to give up.

### Subscribe REST API

Subscribe-mode nodes expose a REST API on port `5381` by default (override with `apiPort`):

```
GET /services          → list all proxied services and their local ports
GET /services/{name}   → one service
```

Each entry has `name`, `port`, `address`, `domains` and `meta`.

This lets local processes discover which `127.0.0.1` port to connect to for each service name without static configuration.

The API also serves `GET /metrics` (Prometheus) and `GET /__health`. A
`node`-mode daemon has no REST API, but serves `GET /metrics` on the same
port (`127.0.0.1:5381`, or `apiPort`), so what nodes record (tunnels they
relay and serve, gossip) can be scraped too. If that port is taken, for
example by a subscriber on the same host, the node logs a warning and runs
without it: give one of them another `apiPort`.

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
  can call `http://127.0.0.1:15001/...` with `Host: inventory`. The service
  gets the URL's host as its `Host`, whatever the client's `Host` said.
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
