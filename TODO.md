# TODO: findings from the review of main at 99d1c9a

Four independent reviews (identity and security; gossip, config and history;
data plane; CLI, operations and docs), each finding checked against the code.
Severity is for a cluster where any member may be compromised; "unsigned
cluster" means one without `requireSignedConfig`.

## High

- [x] **1. Any cluster member can make a node buffer gigabytes (OOM) through mux streams.**
  `src/mux.rs:195` uses `yamux::Config::default()`; `src/mux.rs:235` reads each
  stream's first message with only the 8 MiB message cap and no timeout. A peer
  opens up to 16 sessions × 256 streams, sends most of a large first message on
  each and stalls; the node keeps it all (a test grew a node by 517 MiB from 128
  streams). Such streams also count as in-flight work, holding up a drain, and
  escape the "64 connections waiting for their first message" cap.
  Fix: read a mux stream's first message with a small cap (a TunnelRequest is a
  few KiB) and a timeout; count streams awaiting their first message against
  the peer's waiting-connection slots; bound streams and the receive window per
  session.
  Fixed: a mux stream's first message is capped at 16 KiB and 10 s, waiting streams share the peer's 64 waiting slots, streams count as in flight only once their request arrives, and a session buffers at most 128 MiB. (yamux's stream limit stays at its default: reaching it closes the whole session.)

- [x] **2. A signed config stops being accepted once the certificate that signed it expires.**
  `src/signing.rs:101-105` checks the signer's certificate via
  `TlsContext::verify_cluster_cert` (`src/tls.rs:521-526`) at the current time.
  Admin certificates from `cert issue` (`src/main.rs:1211`) get `certLifetime`
  and are never renewed. With `certLifetime: 30d` (as the README suggests), 30
  days later new, restarted or lagging nodes refuse the cluster's current
  config (`adopt_if_newer` → `admit`), and remote changes, `config pull` and
  rollback fail ("not verified"). Same for configs signed with a node
  certificate after it renews.
  Fix: check the signer's certificate as of the signing time, carried in the
  signed text, and refuse configs signed earlier than the one held; keep
  checking revocation as of now.
  Fixed: signed configs carry `signedAt`; the signer's certificate is checked as of then (revocation as of now); configs dated over an hour ahead, or over an hour before the held one, are refused; `stouter status` shows the signer and their certificate's expiry.

- [x] **3. The CLI never loads the revocation list.**
  Every CLI `TlsContext` (`src/main.rs` 1087, 1172, 1333, 1652, 1780, 1986; via
  `build_cli_tls` / `TlsContext::new`) starts with an empty list; only the daemon
  sets one (`src/state.rs:516`, `641`). A config forged with a revoked admin's key
  and served to another admin's CLI (by a compromised node, or anyone on the
  path holding the revoked certificate) passes `trusted_config`
  (`src/main.rs:1713`); the admin's change on top of it is signed and pushed,
  un-revoking the attacker cluster-wide.
  Fix: load the local config's `revoked` into every CLI TLS context before
  connecting or verifying; refuse a served config that drops a revocation the
  local config holds unless an admin signed the removal.
  Fixed: every CLI command loads the local config's revocations into its TLS context, so it refuses a daemon presenting a revoked certificate and a config signed with one. (A served config older than the local one, dropping a revocation an admin did sign, is #6.)

- [x] **4. A config at version `u64::MAX` can never be replaced.**
  `DynamicConfig::supersedes` (`src/config.rs:421-433`) has no upper bound on
  versions; every edit does `self.version += 1` (`src/config.rs` 594, 653, 683,
  702, 714, 742, 756, 772, 801, 842; `src/gossip/mod.rs` 458, 601, 631;
  `src/main.rs:1129`). Once adopted, every edit overflows (panic in debug, wrap
  to 0 in release, which peers ignore), pushes are refused, and a second
  `u64::MAX` config loses the tie-break. Any member can do it in an unsigned
  cluster; with `requireSignedConfig`, a not-yet-revoked leaked admin key.
  Recovery means hand-editing every node's file.
  Fix: refuse a config whose version jumps more than a bounded amount past the
  one held; use `checked_add` and turn overflow into an error.
  Fixed: a config more than 2^28 versions past the one held is refused (gossip, push, config file); every edit takes its version with `checked_add`, refusing a change at `u64::MAX` instead of wrapping or panicking.

## Medium

- [x] **5. CPU exhaustion through beat signatures.**
  `check_signatures` (`src/gossip/mod.rs:1506-1537`) verifies every beat in a
  sync: no per-node dedup, no cap, on a tokio worker (called from `merge_sync`,
  `:1060`). 20,000 beats took 1.4–2.3 s; an 8 MiB sync holds ~45,000. A few
  such connections stall gossip, tunnels and admin requests (contradicts the
  README's limits section).
  Fix: keep only the highest beat per node per sync, cap checks per sync, and
  run the checks in `spawn_blocking`.
  Fixed: only each node's newest beat is checked, only for nodes with no address (the only ones that beat) or forgotten ones, at most 4096 per sync, on blocking threads (as many syncs at once as there are CPUs).

- [x] **6. A lying daemon can steer an admin's signature.**
  (a) The CLI builds a change on whatever signed config the daemon serves
  (`src/main.rs:1667-1676`), even one older than the local file's; its result can
  win the same-version tie-break (`src/config.rs:428-432`) and drop the latest
  change (e.g. a revocation). (b) `config rollback --to N`
  (`src/main.rs:1309-1338`) never checks that the verified config is version N.
  Fix: refuse to build on a config older than the local file's; require the
  verified version to equal N; don't let a same-version tie-break drop
  revocations.
  Fixed: (a) the CLI refuses to build on, or pull, a verified config older than the admin-signed one in its own config file; (b) `config rollback --to N` refuses a config that isn't, as signed, version N; (c) the same-version tie-break compares the number of revocations first, so the config revoking more wins on every node. (Two configs each revoking something different still settle by `changedBy`.)

- [x] **7. `requestTimeoutMs` firing during tunnel setup returns 504, with no retry and no ejection.**
  `src/subscribe/http.rs:366-371` wraps the whole request, connector included;
  only connector errors reach the retry / `note_tunnel_failure` path (`:402`). A
  node whose tunnel setup hangs gets every request picked for it 504'd, and is
  never ejected: setting a timeout makes failover worse.
  Fix: start the request timeout once the tunnel is up; a timeout before that is
  a connect failure (charge the node, retry elsewhere).
  Fixed: `requestTimeoutMs` counts from when the tunnel is up. Setting one up may take as long again; a setup taking longer fails as a connect error, so the node is charged and the request retried on another (502 if none is left), not answered 504.

- [x] **8. Same-version hand edits are lost or ignored.**
  (a) The config poll (`src/gossip/mod.rs:595-596`) compares only services and
  routes, so a hand-added revocation or policy change at the same version is
  silently not applied. (b) An edit made while the daemon is stopped is taken
  as is at startup (`src/main.rs:458-462`, `src/state.rs:527`), never bumped, and
  settled by the same-version tie-break: it may be silently reverted with no
  `.unapplied` copy, or spread cluster-wide without a version bump.
  Fix: use `same_settings` in the poll; at startup, compare the file with the
  last history entry and bump/stamp a same-version edit like the poll does.
  Fixed: the poll compares the file with `same_settings` (a hand-added revocation or policy is taken and bumped); at startup, a file at the version the daemon last ran (its history's newest entry) but saying something else gets the next version and this node as author.

- [x] **9. `deploy.sh` reports success while the new daemon crash-loops.**
  It stops and removes the running container (`deploy.sh:155-160`) before the
  new one has proven it starts, and `docker ps` right after `run -d` says "Up".
  Fix: run `stouter check` in the new image against the host's config before
  replacing the container; after starting, wait and fail on restarts.
  Fixed: `deploy.sh` runs `stouter check --local` (new: only what stops the daemon from starting) with the new image before touching the running container, and stops if it fails; after starting, it waits 5 s (`DEPLOY_SETTLE_SECONDS`) and fails, with the container's last log lines, if it isn't running or has restarted. `deploy-host.sh` covers both.

- [x] **10. `stouter check` passes configs the daemon refuses or misreads.**
  No `tlsDir` (what `init` writes), `bind` without a port, `knownNodes` entries
  without a port (`src/config.rs:1181-1250`, `src/main.rs:452-454`). Unknown
  keys are only caught at the top level (`src/config.rs:1080`): a misspelled
  nested setting (`dynamicConfig.polices`, a service's `maxConection`,
  `subscribeFilter.serviceGroup`, `tracing.sampleRation`) is silently ignored
  and dropped when the file is rewritten, contrary to README:76-81.
  Fix: move those checks into `validate_local`; have `init` write a `tlsDir`;
  catch and keep unknown keys in the nested structs too (or correct the README).
  Fixed: `validate_local` (daemon startup and `check`) requires `tlsDir`, and a port on `bind` (unless `outboundOnly`), `advertiseAddr` and each `knownNodes` entry; `init` writes a `tlsDir` (`tls` beside the config). Unknown keys inside sections are found (against each type's derived field names) and fail `check`, and are warned about at startup and when a hand edit is taken; they are still dropped on rewrite, as the README now says.

- [x] **11. The CLI's log lines go to stdout, with colour codes.**
  `src/main.rs:420-425`. Breaks machine-readable output (e.g. the JSON from
  `config history --version N`), and pollutes `status` and `cert issue`.
  Fix: log to stderr; default CLI subcommands to `warn`; no colour off a terminal.
  Fixed: CLI commands log to stderr, warnings and errors only by default (`RUST_LOG` for more); the daemon logs to stdout as before; colour only on a terminal.

## Low

- [x] **12. Refused callers fill the replay cache.** `src/authz.rs:77` records a
  signed request's nonce before the policy check (`:91`); a refused member can
  fill the 100,000-entry cache and lock out allowed callers through relays.
  Fix: record the nonce only once the caller is allowed; cap entries per signer.
  Fixed: a signed request's nonce is noted only once the caller is allowed, and one signer may hold at most a tenth of the cache.
- [x] **13. One signed nonce is reused across relay failover.** `src/subscribe/proxy.rs:448-457`
  signs once and reuses the request on every route (`:487-505`); if relay 1
  delivered it but answered slowly, relay 2's attempt is refused as a replay.
  Fix: sign a fresh request per route.
  Fixed: the request is signed afresh for each route tried.
- [x] **14. A route with no backends crashes every request it matches.**
  `src/subscribe/http.rs:530` indexes `route.backends[0]`; gossip adopts such
  routes (`src/state.rs:918` indexes them unchecked). Fix: skip routes with no
  backends when indexing, or answer 503.
  Fixed: routes with no backend to send to are left out of the route index (with a warning), and route selection can't index an empty list.
- [ ] **15. A dead reverse connection at a subscriber that is the NAT'd node's relay fails the tunnel.**
  `src/subscribe/proxy.rs:466-467`: no per-attempt timeout or fallback; waits 15 s
  without trying the node's other relays. Fix: ~5 s per attempt, try another
  reverse connection, then fall through to `tunnel_routes`.
- [x] **16. Connecting to the local service has no timeout.** `src/node/tunnel.rs:163`:
  a backend with a full accept queue holds the tunnel task ~2 min (and delays a
  drain). Fix: time out after a few seconds, under the subscriber's 15 s.
  Fixed: the connection to the local service times out after 5 s.
- [ ] **17. A CLI command can race the daemon's renewal save.** `load_existing`
  runs `finish_renewal` (`src/pki.rs:122-126`) on the daemon's `tlsDir` for every
  local CLI command; between `save_renewed`'s two renames it deletes the staged
  key, leaving the new certificate beside the old key (the node fails at its
  next start). Fix: only the daemon finishes a cut-short renewal (at startup),
  or take a lock file.
- [ ] **18. History keeps both configs at one version; rollback picks the loser.**
  `src/history.rs:73` dedups only against the last entry; `config rollback --to N`
  (`src/main.rs:1312`) and `history --version N` (`:1278`) take the first entry at N.
  Fix: replace the entry when recording a version already there.
- [ ] **19. `service add` with a missing or mistyped `--config` creates a new config.**
  `src/main.rs:654` uses `load_or_default`. Fix: `Config::load`, as rename/remove do.
- [ ] **20. `status` exits 0 with no daemon, and says "no daemon" on `outboundOnly` hosts.**
  `src/main.rs:556-561`, `590`. Fix: non-zero exit; for `outboundOnly`, say there
  is no listener to ask and suggest `--remote`.
- [ ] **21. `config push` always fails against a daemon with empty `adminCns`.**
  `src/main.rs:1137` always signs; `accept_push` (`src/gossip/mod.rs:453`) wants the
  signer in `adminCns`, though other admin ops accept the node's own name
  (`:343`). Fix: accept the daemon's own name when signing isn't required, or
  document that push needs `adminCns`.
- [x] **22. A second SIGTERM/SIGINT during the drain is ignored.** `src/main.rs:491-497`,
  `1477-1505`. Fix: exit at once on a second signal.
  Fixed: a second signal during the drain exits at once (status 1).
- [x] **23. A huge `certLifetime` crashes `check` and the daemon.** `src/pki.rs:40-44`
  (`time::Duration::days(n)` overflows). Fix: bound `n` before building it.
  Fixed: the lifetime is multiplied out with a check, and refused when too long.
- [x] **24. `deploy.sh`'s binary extraction can install a stale binary.**
  `deploy.sh:143-147`: fixed container name `stouter-extract`, no error checks.
  Fix: a unique name, `docker rm -f` it first, `set -e` in the remote script.
  Fixed: a container name of its own, removed on exit, `set -e`, and the binary moved into place only once fully copied.
- [ ] **25. `deploy.sh` with no arguments under macOS bash 3.2** dies with
  "ARGS[@]: unbound variable" instead of printing usage (`deploy.sh:26`).
  Fix: `set -- ${ARGS[@]+"${ARGS[@]}"}`.
- [ ] **26. No-op and empty input accepted.** `route clear` on an unknown service
  succeeds and bumps the version (`src/config.rs:762-776`); `--domains ""` or
  `a,,b` stores an empty domain, which then clashes (`src/main.rs:269`).
- [ ] **27. `tests/e2e/renew.sh` never checks that the CA node still answers after
  its first certificate expires** (lines 35-37). Add `check "A answers past its
  first certificate's expiry" status A`.

# Second review, of main at 8568087

Four independent reviews again (identity and security; gossip, config and
history; data plane; CLI, operations and docs), each finding checked
against the code. Numbered on from the list above.

## High

- [x] **28. One peer could still make a node hold 1–2 GiB through mux streams.**
  The fix for #1 capped each session at 128 MiB, but a peer may open 16
  sessions, and a stream let go of its waiting place once its request was
  read: a request for a made-up NAT'd node then waited 10 s at the relay,
  its window filling unread (measured: +1023 MiB with 16 sessions).
  Fixed: a peer's mux streams count against a budget of 512 across all its sessions until their tunnel is connected (128 MiB; measured +139 MiB with 16 sessions), and a relay refuses at once a tunnel to a node that keeps no reverse connections there.

- [x] **29. Two configs at one version signed over an hour apart split the cluster for good.**
  The tie-break picked one (more revocations, then changedBy), but nodes
  holding the other refused it as signed too early.
  Fixed: such ties go to the later-signed config (the one every node accepts); closer ones as before; a node dropping a revocation in a tie warns.

## Medium

- [x] **30. An expired, unrevoked admin key could still change the config.**
  Dating a config back into its certificate's life: nodes took it in a
  quiet cluster (nothing signed since the expiry) and new nodes always; an
  honest admin's CLI, served one, built a change on it and signed it over.
  Fixed: running nodes take no config from an expired signer (only a node catching up in its first two minutes does); pushes need a signer valid now; the CLI refuses served configs its own signed config contradicts (older, another at the same version, signed too early, or newer from an expired signer, unless `config pull --accept-expired-signer`); nodes warn while the config in force has an expired signer, and `config push` re-signs it. Left: a node starting up can still take one (it can't tell it from the cluster's genuine config); revoke retired admin certificates.

- [x] **31. Stamping an edit made while the daemon was stopped went wrong three ways.**
  It could take the version of a change the cluster made meanwhile (the
  tie-break then dropped one silently); on a node behind the cluster the
  first sync overwrote it with no `.unapplied` copy; and it skipped the
  checks, so an edit the running daemon refused spread at its next restart.
  Fixed: the daemon starts on the config it last ran, and its poll takes the file's edit once caught up (first sync, no peers, or two minutes), checked, as a new version; if the cluster moved on, the edit is kept as `.unapplied`.

- [x] **32. A `requestTimeoutMs` of 5 s or less stopped relay failover.**
  Tunnel setup was bounded by it, but the first relay got 5 s before the
  next was tried, so a dead relay failed every request and got the NAT'd
  node ejected.
  Fixed: a NAT'd node's relays share the setup's budget (min(5 s, an equal share of what's left) each), and a relay that didn't answer in time is tried last for 20 s.

- [x] **33. One member could slow every node's gossip merges.**
  All syncs waited on one signature-check semaphore, with no limit per
  peer (an honest merge took 3.8 s behind 200 junk syncs, debug build).
  Fixed: a sync with nothing to check skips the queue, and a peer may have at most 2 syncs being checked at once.

- [ ] **34. A burst of tunnels to a 1-CPU node may reset healthy streams (unconfirmed).**
  `src/mux.rs`: the session accept loop doesn't yield; on a single tokio
  worker a burst could exceed the stream budget before any stream runs,
  and each reset is charged to the node by the subscriber. A reviewer saw
  13–34 resets per 100 tunnels; not reproduced on a 4-CPU host. (#28 made
  the budget 512 and separate from warm connections, which should help.)

## Low

- [x] **35. `config pull` writes an unsigned served config over an admin-signed local file**,
  replacing the revocation list the CLI trusts. Fix: refuse an unsigned
  config when the file holds a verified signed one.
  Fixed: `config pull` refuses an unsigned served config when the file holds a verified signed one.
- [x] **36. `"tlsDir": ""` passes `check` and `check --local`;** the daemon then makes a new
  CA in the container's working directory on every redeploy. Fix: treat a
  blank tlsDir as unset; warn in deploy.sh and docker-create.sh.
  Fixed: a blank tlsDir counts as unset (refused by `check`, `check --local` and the daemon); deploy.sh and docker-create.sh warn.
- [x] **37. In `deploy.sh`, a cluster named `build` (or `config`) lives inside the default
  cluster's directories;** deploying the default cluster wipes its config
  and CA. Fix: reserve the names, or put named clusters under `clusters/`.
  Fixed: deploy.sh refuses the cluster names `build` and `config`.
- [ ] **38. The relative version bound strands a new node** (version 0) if the cluster's
  version is above 2^28 (say, a Unix timestamp set by hand). Fix: exempt
  the empty version-0 config, or bound versions absolutely.
- [ ] **39. Certificate expiry is checked only at the handshake:** long-lived connections
  and beats from an expired certificate keep working.
- [ ] **40. `requestTimeoutMs` can be exceeded up to twice over** when a request ends up
  on a pooled connection that came free while its own tunnel was being set
  up (the background setup keeps extending its deadline).
- [x] **41. The HTTP pool's 60 s idle timeout is never enforced** (no `pool_timer`), so
  idle tunnels linger until the node's 300 s timeout.
  Fixed: the proxy's and the telemetry exporter's clients have a pool timer, so idle connections are closed on time.
- [x] **42. At startup, an edit with its version raised by hand keeps the old signature**
  (`status` and history name an admin who never signed it).
  Fixed: #31 routes offline edits through the config poll, which now clears the signature and `signedAt` of any edit it takes and makes this node its author (the time tie-break of #29 applies only to configs that are signed).
- [x] **43. A hand edit that doesn't parse is ignored at runtime with only a debug log.**
  Fixed: warned about once per distinct error.
- [x] **44. `deploy.sh` installs the new host binary before the config check** that may
  abort the deploy.
  Fixed: the binary is installed only after the new container stays up.
- [x] **45. If `deploy.sh`'s image probe fails for any reason, it skips the config check**
  instead of aborting.
  Fixed: a probe that fails to run aborts the deploy; only an image that runs and lacks `--local` is warned about and skipped.
- [x] **46. The CLI panics (exit 101) on a closed pipe** (`stouter config history | head`).
  Fixed: CLI commands restore SIGPIPE's default, so they stop quietly (status 141) as Unix tools do; the daemon keeps ignoring it.
- [ ] **47. A huge `signedAt` overflows** formatting a refusal before any signature is
  checked (panic in debug builds). Partly fixed with #30 (`saturating_mul`
  in `verify`); check the other call sites.
- [ ] **48. `service move` and `--remote` with another cluster's `--cert-dir`** apply this
  cluster's revocations to the other one (fails safe, confusing error).
