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
- [x] **15. A dead reverse connection at a subscriber that is the NAT'd node's relay fails the tunnel.**
  `src/subscribe/proxy.rs:466-467`: no per-attempt timeout or fallback; waits 15 s
  without trying the node's other relays. Fix: ~5 s per attempt, try another
  reverse connection, then fall through to `tunnel_routes`.
  Fixed: up to two reverse connections get at most 3 s each (with a request signed afresh), then the node's relays.
- [x] **16. Connecting to the local service has no timeout.** `src/node/tunnel.rs:163`:
  a backend with a full accept queue holds the tunnel task ~2 min (and delays a
  drain). Fix: time out after a few seconds, under the subscriber's 15 s.
  Fixed: the connection to the local service times out after 5 s.
- [x] **17. A CLI command can race the daemon's renewal save.** `load_existing`
  runs `finish_renewal` (`src/pki.rs:122-126`) on the daemon's `tlsDir` for every
  local CLI command; between `save_renewed`'s two renames it deletes the staged
  key, leaving the new certificate beside the old key (the node fails at its
  next start). Fix: only the daemon finishes a cut-short renewal (at startup),
  or take a lock file.
  Fixed: CLI loads only read tlsDir; only the daemon (at startup) finishes a cut-short renewal.
- [x] **18. History keeps both configs at one version; rollback picks the loser.**
  `src/history.rs:73` dedups only against the last entry; `config rollback --to N`
  (`src/main.rs:1312`) and `history --version N` (`:1278`) take the first entry at N.
  Fix: replace the entry when recording a version already there.
  Fixed: recording a version already in the history replaces its entry; lookups by version take the latest.
- [x] **19. `service add` with a missing or mistyped `--config` creates a new config.**
  `src/main.rs:654` uses `load_or_default`. Fix: `Config::load`, as rename/remove do.
  Fixed: `service add` without --remote uses `Config::load`.
- [x] **20. `status` exits 0 with no daemon, and says "no daemon" on `outboundOnly` hosts.**
  `src/main.rs:556-561`, `590`. Fix: non-zero exit; for `outboundOnly`, say there
  is no listener to ask and suggest `--remote`.
  Fixed: exits 1 when no daemon answers (or answers oddly); on an outboundOnly host says there's no listener to ask and points to --remote.
- [x] **21. `config push` always fails against a daemon with empty `adminCns`.**
  `src/main.rs:1137` always signs; `accept_push` (`src/gossip/mod.rs:453`) wants the
  signer in `adminCns`, though other admin ops accept the node's own name
  (`:343`). Fix: accept the daemon's own name when signing isn't required, or
  document that push needs `adminCns`.
  Fixed: without requireSignedConfig a push is also taken from the node's own certificate, as other admin requests are.
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
- [x] **25. `deploy.sh` with no arguments under macOS bash 3.2** dies with
  "ARGS[@]: unbound variable" instead of printing usage (`deploy.sh:26`).
  Fix: `set -- ${ARGS[@]+"${ARGS[@]}"}`.
  Fixed: the expansion is guarded, and a missing host prints the usage (checked under bash 3.2).
- [x] **26. No-op and empty input accepted.** `route clear` on an unknown service
  succeeds and bumps the version (`src/config.rs:762-776`); `--domains ""` or
  `a,,b` stores an empty domain, which then clashes (`src/main.rs:269`).
  Fixed: `route clear` with no routes to clear is refused; the CLI drops empty domains, and an empty domain in a config is a problem.
- [x] **27. `tests/e2e/renew.sh` never checks that the CA node still answers after
  its first certificate expires** (lines 35-37). Add `check "A answers past its
  first certificate's expiry" status A`.

# Second review, of main at 8568087

Four independent reviews again (identity and security; gossip, config and
history; data plane; CLI, operations and docs), each finding checked
against the code. Numbered on from the list above.

## High
  Fixed: `renew.sh` checks that A answers past its first certificate's expiry.

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

- [x] **34. A burst of tunnels to a 1-CPU node may reset healthy streams (unconfirmed).**
  `src/mux.rs`: the session accept loop doesn't yield; on a single tokio
  worker a burst could exceed the stream budget before any stream runs,
  and each reset is charged to the node by the subscriber. A reviewer saw
  13–34 resets per 100 tunnels; not reproduced on a 4-CPU host. (#28 made
  the budget 512 and separate from warm connections, which should help.)
  Fixed (defensively; still not reproduced): the accept loop yields every 64 streams, so streams accepted run before the next burst is taken. A test bursts 1,500 tunnels over 8 sessions on one worker thread: no stream is reset.

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
- [x] **38. The relative version bound strands a new node** (version 0) if the cluster's
  version is above 2^28 (say, a Unix timestamp set by hand). Fix: exempt
  the empty version-0 config, or bound versions absolutely.
  Fixed: a node holding version 0 takes any version up to 2^62 (the step bound applies once it holds one).
- [x] **39. Certificate expiry is checked only at the handshake:** long-lived connections
  and beats from an expired certificate keep working.
  Fixed (beats): a beat checked against a certificate that has expired counts for nothing. Connections opened before an expiry keep going, by design (nodes renew long before; revocation closes them): now documented.
- [x] **40. `requestTimeoutMs` can be exceeded up to twice over** when a request ends up
  on a pooled connection that came free while its own tunnel was being set
  up (the background setup keeps extending its deadline).
  Fixed: a setup the client finishes in a task of its own, for the pool, lets go of the request at once, so the request is timed from then. A test (paused time) checks the request's deadline; it failed before.
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
  Fixed: CLI commands restore SIGPIPE's default, so they stop quietly (status 141) as Unix tools do; the daemon keeps ignoring it. (Reworked in #53: only a failed print ends the CLI by SIGPIPE.)
- [x] **47. A huge `signedAt` overflows** formatting a refusal before any signature is
  checked (panic in debug builds). Partly fixed with #30 (`saturating_mul`
  in `verify`); check the other call sites.
  Fixed: times from certificates and signatures are formatted as seconds (`history::format_secs`), with no multiply.
- [x] **48. `service move` and `--remote` with another cluster's `--cert-dir`** apply this
  cluster's revocations to the other one (fails safe, confusing error).
  Fixed: the local config's revocations are applied only with certificates from the local tlsDir's CA (or when that can't be told).

# Third review, of main at db67147 (before going live)

Five independent reviews (gossip and config; identity and security; CLI and
operations; data plane; concurrency, panics and resources), each finding
reproduced or checked against the code. Numbered on from the lists above.

## High

- [x] **49. Histogram samples pile up in memory unless something scrapes `/metrics`.**
  `metrics::install` used `install_recorder`, whose caller must run the
  exporter's upkeep; nothing did, so every request, tunnel setup and sync
  timing stayed in memory until a scrape (about 16 bytes a sample: 1.3 GiB a
  day at 1,000 requests/s).
  Fixed: the gauge loop runs the recorder's upkeep every 5 seconds.

- [x] **50. A peer's mux streams left the #28 budget once connected,** so any member
  could make any node hold gigabytes: registering reverse connections for
  itself at the node, then having the node relay thousands of tunnels back
  to itself, never reading them (600 connected, none counted). Plain
  relayed tunnels had no cap at all.
  Fixed: every tunnel a peer has open through a node counts against it for its whole life (at most 4096, plain connections and mux streams alike, served or relayed); a tunnel relayed back to its caller is refused; proxy buffers are allocated once data flows.

## Medium

- [x] **51. A CLI change made while the daemon was stopped was lost if the cluster had moved on.**
  The CLI recorded its change in the history as the version the daemon
  last ran, so at startup the file matched it, the change was in force at
  once and the first sync overwrote it, with no `.unapplied` copy (the
  README's way to add a node: `service add` on a new node, then start it).
  Signed clusters too.
  Fixed: the CLI records nothing in the history (on a node that never ran, the config it changed, as the starting point), so the daemon takes the change once caught up, or keeps it as `.unapplied`; signed changes in the file wait the same way.

- [x] **52. `cert revoke --remote` didn't protect the admin who ran it.**
  The CLI refuses configs signed with certificates its own file revokes,
  but no `--remote` change wrote the file: a node holding the revoked key
  could serve a forged config, and the admin's next change was signed on
  top of it, undoing the revocation cluster-wide.
  Fixed: after a signed change, the CLI's file holds the config it signed (as after `config push`), unless the file holds edits not yet pushed (then it says so) or another cluster's config. README: other admins pull after a revocation, and drop the revoked CN from `adminCns`.

- [x] **53. The CLI died silently (status 141) when a daemon refused its certificate,**
  a regression from #46: TLS writes get no `MSG_NOSIGNAL`, so with SIGPIPE
  at its default the write after the daemon's close killed the process.
  When it survived, `change_target` took any failure (a refused
  certificate, a 2 s timeout) for "no daemon" and edited the file behind
  the running daemon.
  Fixed: SIGPIPE stays ignored, and only a failed print on stdout or stderr ends the CLI by SIGPIPE; the CLI edits the file only when nothing accepts a connection at this host's daemon's address (refused, or not accepted within 3 s: macOS drops some), anything else is an error (`--file-only` edits the file anyway).

- [x] **54. A closed stdout or stderr silently stopped the daemon's loops.**
  The config dump used `println!`, which panics on a closed pipe; a failed
  log write was reported with `eprintln!`, which panics too; and a panic
  ended the loop for good while the process looked healthy (an
  `outboundOnly` node stopped gossiping).
  Fixed: the dump ignores write errors, the log drops lines it can't write, and a background loop that stops makes the daemon drain and exit with status 1, to be restarted.

- [x] **55. A higher version signed over an hour earlier splits a signed cluster** after a
  partition heals (side B refuses side A's v12 as signed too early; side A
  ignores B's older v11), until the next change. The warning blames clocks.
  Fix: apply `signed_too_early` only when the signer's certificate has
  expired by now (`src/signing.rs:243`).
  Fixed: only a config from an expired signer is refused as signed too early; the same-version tie-break puts an expired signer's config last (as running nodes refuse it) instead of comparing signing times.

- [x] **56. A node with no `knownNodes` counts as caught up at once,** so its offline
  edit is taken before any peer syncs, then lost with no `.unapplied` copy
  (`src/state.rs` `caught_up`). Fix: drop the "no peers" shortcut, or keep
  the pre-edit text in `file_config` until the edit survives a sync.
  Fixed: knowing no peers no longer counts as caught up; such a node waits for a peer's sync, or two minutes. (Left: a sync from a peer that is itself behind still counts.)

- [x] **57. With `requireSignedConfig`, any member can hold the config write lock** with
  large configs carrying junk signatures: `adopt_if_newer` decodes and
  verifies under `dynamic_config.write()` (a reader waited about 0.5 s with 8
  senders). Fix: refuse oversized signed text first; verify outside the
  lock with a per-peer slot; remember the last refused signature.
  Fixed: a peer's signed config is checked outside the config lock (on a blocking thread, one per peer at a time); one larger than a config may be is refused unread, and one refused in the last ten minutes isn't checked again. A reader waited under 50 ms with 8 senders (194 ms before).
- [x] **58. `ca.key` is never checked against `ca.crt`** (`src/pki.rs` `load_ca`). A node
  deployed before its certificates are copied in makes its own CA; copying
  the cluster's in afterwards leaves a mismatched key, accepted silently:
  the node runs isolated with nothing logged at warn. Fix: refuse a
  mismatched key; warn when a node with `knownNodes` makes a new CA; log
  repeated handshake failures at warn.
  Fixed: a `ca.key` that isn't `ca.crt`'s key is refused (so is a `node.key` that isn't `node.crt`'s), saying to remove it; a node making a new CA while its config lists peers warns; a node whose every sync has failed for a minute warns (every ten minutes).

- [x] **59. The README's `cert issue --out /etc/stouter/tls/` writes the new node's key
  into the CA node's own tlsDir** (its example tlsDir), beside `ca.key`;
  "copy the resulting files" then copies `ca.key` along, making the new node
  a CA too (able to issue certificates for any name). (First reported as the
  CA node taking the other node's certificate: it doesn't, as its own lives
  in memory, made from `ca.key` at each start.) Fix: refuse to write beside
  a `ca.key` or over another node's certificate; fix the example.
  Fixed: `cert issue` refuses an `--out` holding `ca.key`, or another node's certificate (the same node's is replaced, as before); the README issues into `host2-tls` and says to copy all but `ca.key`; the "no CA" error says to run it on the CA node instead of starting a daemon (which makes a second CA).

## Low

- [x] **60. A relay can make a NAT'd node hold up to 8 MiB per reverse connection,**
  with no deadline (`src/node/reverse.rs` reads the relay's first message
  with the 8 MiB cap); the plain-connection dispatcher likewise. The
  attacker sends every byte held. Fix: cap a reverse connection's message
  at the tunnel-request size; a deadline once a length prefix arrives.
  Fixed: a reverse connection's message is capped at 16 KiB and must arrive within 10 s once begun; any connection's first message must follow its length within 30 s.
- [x] **61. A relay refuses a burst of tunnels after a quiet spell:** `relayed_here` is
  refreshed only on registration, so once idle connections registered over
  two minutes ago are used up, the next tunnels are refused at once while
  replacements register. Fix: refresh it when a reverse connection is taken.
  Fixed: taking a reverse connection counts as relaying lately, so the burst's later tunnels wait for the replacements.
- [x] **62. `config rollback` and `service move` accept history entries forged with an expired admin key.**
  Fix: refuse a fetched entry whose signer has expired unless accepted
  explicitly, or it matches the local history.
  Fixed: a daemon's history entry (rollback) or source config (`service move`) signed by an expired certificate is refused, unless this host's history has the entry, or `--accept-expired-signer`.
- [x] **63. deploy.sh's pre-check ignores tlsDir:** a missing `node.key` passes `check
  --local` and the new container crash-loops. Fix: mount tlsDir in the
  check, and have `check --local` check the certificate set (and #58).
  Fixed: `check --local` checks the certificates as the daemon would load them (read only) and warns when it would make a new CA though peers are listed; deploy.sh mounts tlsDir (read only) for the check.
- [x] **64. The nonce cache caps a caller at about 143 tunnels/s per node** to policy
  services (10,000 per signer per 70 s), and rescans the cache on every call
  at the cap. Fix: scale the cap, prune incrementally.
  Fixed: the cache keeps each signer's nonces in expiry order and lets go of expired ones as it goes (no rescans); a caller may have 50,000 (some 800 new tunnels a second to one node), 500,000 in all.
- [x] **65. The same-version tie-break can cycle between three configs** signed 40
  minutes apart (`src/config.rs` `supersedes`); it settles in a live
  cluster. Fix: a total order (#55 removes most of the need).
  Fixed with #55: the tie-break compares each config's own properties (signer expired, revocations, changedBy, content), a total order.
- [x] **66. The #35 check is skipped on a daemon's word** (`config pull` with a daemon
  saying `require_signed_config: false`). Fix: treat the cluster as signed
  when the local file holds a verified signed config.
  Fixed: `config pull` treats the cluster as signed when the file holds a verified signed config, whatever the daemon says.
- [x] **67. Relay retries resend the caller's nonce,** so the NAT'd node refuses the retry
  as a replay and the real cause is lost. Fix: retry only when the first
  send failed.
  Fixed: a signed request is sent down another reverse connection only if it couldn't be sent, or the connection closed before answering; after a timeout the relay reports it, and the subscriber tries the next relay with a request signed afresh.
- [x] **68. `requestTimeoutMs: 0` is accepted,** then fails every request and ejects
  every node. Fix: a minimum in `config_problems`.
  Fixed: below 10 ms is a config problem (changes adding it are refused).
- [x] **69. Nonces expire by the monotonic clock, freshness by the wall clock,** so a
  clock stepped back allows one replay (code reading only).
  Fixed: nonces are kept until their request's timestamp no longer passes the freshness check, by the wall clock.
- [x] **70. Smaller items:** deploy.sh's `-r` help says it updates the config (it never
  replaces one); `cert issue` without a CA says "start the daemon once"
  (which makes a second CA); the README says empty `adminCns` disables
  admin RPC (the node's own certificate still works); deploy.sh uploads
  untracked files from the repo directory, keys included; a `service add`
  that changes nothing bumps the version; `config push` writes over a newer
  config a local daemon persisted; the daemon does blocking file I/O under
  the config lock.
  Fixed: deploy.sh uploads only the files git tracks (keys never), and its `-r` help says an existing config is never replaced; `cert issue` without a CA says to run it on the CA node (#59). The `adminCns` wording is right now; a `service add` that changes nothing is refused (and a refused change exits with the same status through a daemon or in the file); `config push` leaves a newer config a local daemon saved meanwhile. The daemon's file I/O under the config lock runs off the async workers (`block_in_place`), and what serves traffic, `status` and metrics read the config without its lock (a snapshot taken at each change): a hung disk now holds up config changes and gossip, not requests. A test hangs the config file (a FIFO) mid-save.
- [x] **71. The idle reverse-connection cap can be passed by one** (found running
  `caps.sh`: 1025 kept, once in four runs): `handle_reverse_registration`
  counts the pool, awaits the known-nodes update, then pushes, so
  registrations arriving together all pass at 1023. Fix: check and push
  under one lock (a capped `StreamPool::push`).
  Fixed: the registration is counted again as it goes into the pool, under the pool's lock.

# Fourth review, of the changes db67147..ca25a93

Three independent reviews of what the three batches above changed (data
plane and authz; gossip, config and signing; CLI, PKI and operations),
each finding reproduced. Numbered on.

## Medium

- [x] **72. One oversized request through a relay cost a NAT'd node three reverse connections** (a #60 regression):
  the node drops a reverse connection whose message is over 16 KiB, and
  the relay, taking that as a dead connection, tried the next two; each
  drop sent a worker into backoff. About one request a second kept the node
  unreachable.
  Fixed: the relay refuses a request over 16 KiB (encoded as it would send it) before taking a connection.

- [x] **73. A member could undo a revocation once the winning config's signer expired** (a #55 regression):
  the same-version tie-break ranked configs from expired signers last,
  judged now, so the held winner was demoted and a replayed loser took over.
  Fixed: only an incoming config from an expired signer is ranked last (it never wins); the held one isn't demoted.

- [x] **74. A small clock step back re-admitted a replayed request** (a #69 regression):
  a nonce let go 31 s after its request's timestamp, then the clock stepped
  back, and the request passed the freshness check again.
  Fixed: the cache keeps the wall clock's highest reading and refuses requests too old by it.

- [x] **75. Changes made while the daemon was stopped (#51) were set aside wrongly, or waited:**
  - one the CLI had broadcast, which the cluster took and built on, was set aside as `.unapplied` at the next start (likewise the config `keep_pushed` wrote);
  - the README's way to add a node (`service add` before its first start) never took effect when the cluster was ahead;
  - a new cluster's first node waited two minutes for it;
  - a config dropped unchecked (busy check slot) counted as a sync, so an offline signed edit could be taken and lost.
  Fixed: an edit is compared with the newer config piece by piece (service instances, routes and policy per service, revocations): already in it, nothing is kept; not in it and touching nothing the cluster changed, it is made again on top and taken as the next version (unsigned clusters); only a clash is set aside. A node's first start with no `knownNodes` takes the edit at once. An unchecked config doesn't count as a sync. history.sh checks the add-a-node flow.

## Low

- [x] **76. deploy.sh outside a git checkout uploaded keys in subdirectories** (`host2-tls/`).
  Fixed: it uploads only what the image is built from.
- [x] **77. `check --local` missed a mismatched SEC1/PKCS#1 `node.key`, and refused a directory mid-renewal.**
  Fixed: it builds the TLS context as the daemon does, and finishes a cut-short renewal on a copy.
- [x] **78. Any other program on the config's `bind` blocked every file change from the CLI,** saying a daemon was running.
  Fixed: "something is listening", and `--file-only` to change the file anyway.
- [x] **79. Edge cases of the third batch:**
  - proxy buffers grew to 64 KiB on the first byte and stayed: now they grow only while reads fill them, and shrink back;
  - no retry after a timeout cost a NAT'd node with one relay its tunnels: the relay retries again, and reports the timeout if the retry is refused as a replay;
  - the 4096 cap is now `maxTunnelsPerPeer`, and a refused mux stream is told why;
  - `keep_pushed` keeps a node's file only if the cluster pushed to knows the node (lists it, remembers it leaving, or has services on it, as the file does: two clusters may share a CA); otherwise it says so;
  - a `requestTimeoutMs` under 10 ms already in a config is taken as 10;
  - the "no sync has worked" warning counts syncs peers start, and not a stretch with no peers.
- [x] **80. Stale docs:** #53 and `change_target`'s comment; two doc comments over the wrong function.

## Older, found on the way

- [x] **81. A node that missed an unrevoke never converges:** v6 revokes admin
  X, v7 unrevokes, v8 is signed by X. A node still on v6, offered only v8,
  refuses it by its own (stale) revocations until another admin signs.
  Fix: check the signer against the revocations in force up to the incoming
  config, not only this node's.
  Fixed (another way: a config's own revocation list can't be trusted before its signer is): when a peer's config is refused because this node has its signer revoked, and that config's list no longer revokes it, the node asks that peer for its config history and takes the versions it missed in order, each checked as usual (at most 20, once a minute per peer). The unrevoke is among them, so v8 is then taken. A test runs the v6/v7/v8 case.

# Fifth review, of main at 2019846 (before going live)

Three independent reviews (data plane and authz; cluster state; CLI, PKI
and operations), each finding reproduced by a failing test or a script,
then checked again here. Numbered on. #93's policy-before-services ordering
is fixed with #85.

## High

- [x] **82. A CLI change made with no daemon running is broadcast from the file, and replaces the cluster's config.**
  `src/main.rs` (`service add` around 802, `rename`, `remove`; `broadcast_config`
  for route set, cert revoke/unrevoke, rollback, policy). The CLI edits the
  file, then sends it as a `ConfigUpdate` to `knownNodes` with this node's
  certificate, on a node that never ran too. Peers take it whole: a higher
  version wins outright, an equal one by the tie-break. Following the
  README's add-a-node flow (cert issue, copy, `service add` before the first
  start), host1's `api` was removed cluster-wide ("version 2 by host2 ...
  service web/api on host1 removed"). Contrary to README ("skip telling
  peers"). Fix: don't broadcast from the file-edit path; the daemon takes
  the edit once caught up.
  Fixed: no change made in the local file is sent to peers (`service`, `route`, `cert revoke`/`unrevoke`, `config rollback`, `policy`); the CLI says the daemon takes it, or to use `--remote`. history.sh adds a new node whose file is at the cluster's version: its change stays in its file until its daemon runs, then reaches the cluster with nothing lost (both checks failed before).
- [x] **83. A config file at an older version is rebased as an edit, undoing every change since** (unsigned clusters).
  `persist_to_file` rebases with base = what the daemon last wrote, so a file
  saved from an old editor buffer, pulled from a node that is behind, or
  restored from a `.bak` counts as an edit that undoes everything in
  between. Rebased onto the next config and taken as a new version: in the
  test, a service added since vanished and a revocation was lifted on
  every node, with only info logs and no `.unapplied` copy. The poll loop
  itself already refuses an older file for this reason. Fix: rebase only
  from the version the file says it was edited from (the history's entry
  at that version), else set it aside; the same at startup.
  Fixed: a file at an older version than the daemon last wrote isn't rebased: if it is a version this node ran (per its history), it is written over; otherwise it is set aside as `.unapplied`. This covers startup too (the daemon starts on its history's last entry). Not rebased from the history entry at the file's version either: a CLI `--file-only` edit made on an old file carries version+1, so that base could be wrong. A test runs the reviewer's case and a backup put back.

## Medium

- [x] **84. A relay can make a NAT'd node hold unbounded tunnels:** tunnels that
  come through a reverse connection take no `maxTunnelsPerPeer` slot, and the
  reverse worker registers again at once. A test: 207 tunnels and backend
  connections from one relay against a cap of 8, in 2.6 s. Fix: count them
  against the relay (with a larger cap), or the signer, and bound tunnels in
  flight per relay.
  Fixed: a tunnel arriving on a reverse connection counts against the relay, up to 4 times `maxTunnelsPerPeer`; past that the NAT'd node refuses it and that reverse worker backs off before registering again. A test: tunnels from one relay stop at the limit.
- [x] **85. A policy change doesn't reach tunnels already open:** `policy deny`
  stops only new tunnels. A subscriber's pooled HTTP keep-alives (and open TCP
  and mux tunnels) keep reaching the service: 5 of 5 requests still answered
  after the deny. Revocation, by contrast, fails open streams at once. Fix:
  on a policy change, close tunnels whose caller is no longer allowed
  (hosting node), and reset the HTTP pool for such services (subscriber).
  Fixed: the hosting node registers each tunnel it serves with its service and caller (the peer's certificate, or a verified signature; through a relay to a service with no policy, a caller not known). On every config change it closes those the policies no longer allow. The subscriber's pooled connection then dies and its next tunnel is refused. A test: 0 of 5 requests answered after the deny.
- [x] **86. The daemon and `check --local` take a `node.crt` that isn't for this `nodeId`, or isn't from the `ca.crt` beside it**
  (`pki::load_issued` checks only that the key matches). The node runs cut off
  from its peers with nothing logged above info, and deploy.sh reports
  success. `cert issue` also takes `--node ''` and `--node x:1`. Fix: refuse a
  CN other than `nodeId`, and a certificate the CA didn't issue; validate
  the name in `cert issue`.
  Fixed: the daemon and `check --local` refuse a `node.crt` whose CN isn't `nodeId`, or that the `ca.crt` beside it didn't issue (checked as of the certificate's start, so an expired one is left to renewal). `cert issue` refuses an empty name, a `host:port` and whitespace.
- [x] **87. `service add` on an existing instance replaces it whole:** the README's
  canary (`--weight 9`) and drain (`--weight 0`) examples drop the instance's
  domains, `protocol: http`, health path, labels and meta. One instance
  falling back to tcp makes the whole service plain TCP. Fix: keep what isn't
  given on the command line (or a `service set`).
  Fixed: on an instance already there, only the options given change (`--domains ""` still clears the domains); a new instance gets the defaults. With `--remote`, the instance is read from that daemon first. README says so.
- [x] **88. A history request reads the history file on an async worker:** up to
  16 MiB, outside `block_in_place`, with no per-peer cap. On a hung disk a few
  requests stop every worker (a test, 2 workers: 0 of 8 other tasks ran in
  3 s, in some runs), against #70's promise. Also `unknown_nested_settings_in`
  under the config lock, and renew's check of a copied-in `node.crt`.
  Fixed: a history request is read with `spawn_blocking`, at most 2 at a time per peer; `unknown_nested_settings_in` and renew's check of a copied-in `node.crt` go through `block_in_place`. The reviewer's test passes every time (it failed 1 run in 3).

## Low

- [x] **89. `httpListen` doesn't check the subscriber's own policy:** a request for
  a service this node may not reach goes to up to 3 hosting nodes, each
  refusing it and each charged a tunnel failure (ejected on this
  subscriber); the client gets 502, not the README's refusal at once.
  Fixed: the HTTP proxy (behind `httpListen` and each service's port) answers `403 denied_by_policy` before choosing a node, and a TCP proxy whose policy changed refuses the connection before its next reconcile; no hosting node is asked or charged. A test: 403 with none of 3 nodes contacted (502 before).
- [x] **90. A missed unrevoke is caught up from the next peer only after 10 minutes:**
  the refused-config cache answers re-offers before the catch-up check, so a
  first peer that can't help (history starting later, unreachable) blocks the
  others until the entry expires. The README says once a minute per peer.
  Fixed: the catch-up check runs before the signature check and its cache, so each peer offering such a config is asked (at most once a minute). The unrevoke test now has an unreachable first peer; it timed out before.
- [x] **91. With `requireSignedConfig`, the daemon starts on an unsigned hand edit**
  (`startup_config` falls back to the file as is), though the README says a
  hand edit isn't taken. Fix: start on the history's signed entry when the
  file doesn't verify.
  Fixed: a file that doesn't verify gives way to the newest history entry that does, with a warning; with none (a cluster before its first signed change) the file is kept, as before.
- [x] **92. A relative `tlsDir` is resolved against the working directory,** not the
  config file's: a daemon started elsewhere makes a new CA there (warned
  only when peers are listed).
  Fixed: `Config::tls_path` takes a relative `tlsDir` from the config file's directory, for the daemon, renewal and every CLI command; the file keeps it as written. deploy.sh and docker-create.sh no longer warn of a relative one (it is in the mounted config directory) unless it climbs out with `..`.
- [x] **93. Small items:** a ~600-deep nested first message overflows a worker's
  stack in debug builds (release copes to rmp's 1024; set a lower decode
  depth); `index_config` stores the service index before the policies;
  deploy.sh and docker-create.sh mount the config directory twice when
  `tlsDir` is that directory (Docker 29.9 accepts it); stray line breaks in
  the README's relay sentence.
  Fixed: messages are decoded at most 64 levels deep (a test overflowed the stack without it); the index ordering with #85; the scripts take `tlsDir` equal to the config directory as inside it; the README's line breaks.

Unconfirmed (code reading): a partial length prefix holds a connection slot
(bounded per peer); subscribers keep warm connections to nodes hosting only
services they're denied; `revocation_lifted_by` and the self-revocation
check compare fingerprints unnormalised; `keep_pushed` with no `tlsDir`;
shutdown may outlast `docker stop -t 30` with slow peers.

# Sixth review, of main at f7eda1e

Four independent reviews (data plane; cluster state; identity and
security; CLI, operations and docs). Each finding was reproduced by a
failing test or script, and every repro was run again here. Numbered on.

## High

- [ ] **94. A 513th tunnel on one mux session resets the 512 already open on it.**
  `src/mux.rs` keeps yamux's default `max_num_streams` (512), and
  `MuxPool::open_stream` doesn't count streams. A subscriber sends every
  tunnel to a node over one session (TCP connections and pooled HTTP
  keep-alives, which have no per-host cap). yamux answers the 513th with
  `TooManyStreams` and drops every stream on the connection. A test: 512 of
  512 open tunnels broke. Fix: count open streams per session and fall back
  to a pooled or cold connection (or another session) well below the limit.
- [ ] **95. An offline edit is lost without a trace if its first sync is with a peer behind the cluster.**
  `caught_up` (state.rs) is any merged sync (#56 left this). The edit is
  rebased onto the lagging peer's config and taken as the next version. The
  cluster's newer config then replaces it outright, and `persist_to_file`
  writes over the file with no `.unapplied` copy and no warning. The same
  happens to any change made on a node that is behind (admin RPC, poll).
  A test: the edit's service vanished from memory and file. Fix: when a
  newer config drops a change this node made, rebase that change onto it (or
  set it aside as `.unapplied`); don't count a sync with a peer below the
  highest version seen as caught up.

## Medium

- [ ] **96. `service add --remote` signs instance fields a lying daemon put around the signature.**
  #87 reads the existing instance from the daemon's unverified status
  (`fetch_remote_dynamic_config`) and pushes the whole `Service` to be
  signed on the verified config. A compromised node can add a domain or
  `localPort` to an instance; an admin's harmless `--weight` through it signs
  them cluster-wide. Fix: build the instance from the verified config (or an
  admin op that carries only the flags given).
- [ ] **97. `config push` undoes a concurrent cluster change when a CLI edit raised the file's version.**
  `stale_push` refuses only a cluster version above the file's, and the push
  sends `base_version = reported.version`. A file pulled at v2, edited by the
  CLI to v3, pushed after another admin made v3: pushed as v4, the other
  change gone, exit 0. Fix: keep the version a file's edits are based on and
  refuse (or Conflict) when the cluster's config differs from it.
- [ ] **98. A policy deny that lands while a tunnel is being set up never closes it.**
  #85's `close_refused_tunnels` sees only registered tunnels; a tunnel is
  registered after authorization, the local connect (up to 5 s) and the
  success byte (node/tunnel.rs, mux.rs). Nothing checks again at
  registration. A test: denied mid-setup, the tunnel still reaches the
  service (4 of 4 runs). Fix: register before authorizing, or check the
  policy again once registered.
- [ ] **99. `keep_pushed` writes another cluster's config into a CLI file without `tlsDir`** (`service move`).
  The same-cluster check runs only for files with a `tlsDir`; an admin
  workstation config uses `--cert-dir`. After a move between clusters that
  share a CA and admin, the source cluster's file holds the destination's
  config, and every later change to the source is refused as older. Fix: don't
  update the file for the other cluster in `service move`, or check the
  cluster another way without a `tlsDir`.
- [ ] **100. Renewal asks only 5 random peers, so the CA node may never be asked.**
  `renew.rs` asks `MAX_ASKED` = 5 shuffled peers; nodes don't know which one
  holds `ca.key`; a failed attempt waits a full check interval. With a 12-24h
  `certLifetime` and a dozen or more nodes, a node can miss the CA on every
  attempt and cut itself off. A test: the CA node wasn't asked in 10 of 20
  attempts (12 nodes). Fix: gossip which nodes issue (or remember the last
  issuer) and ask them first; retry a failure on a short backoff.
- [ ] **101. A replacement certificate copied into `tlsDir` is ignored if it expires earlier than the one in use.**
  `take_up_copied_cert` switches only to a later `not_after`. The README's
  leaked-key recovery (revoke, issue, copy in) fails silently when the new
  certificate has a shorter `certLifetime`: the node keeps its revoked one
  until restarted. Fix: take any valid certificate that differs, tell old
  from new by `not_before`, always leave a revoked one, warn when skipping.
- [ ] **102. Every sync and sync reply deep-clones and hashes the whole config, even when it isn't sent.**
  `build_compact_sync` / `build_sync_reply` clone a config of up to 2 MiB
  under its read lock, then serialize it to hash it 1-3 times. Release: 9 ms
  per sync pair with a 1.8 MB config (about 0.5 s of CPU per round at 50
  peers). Fix: hash once at `index_config`, build from `config_now`, clone
  only to send.

## Low

- [ ] **103. Shutdown can take 35-40 s, past the `docker stop -t 30` the scripts and README use:**
  leave broadcast (up to 5 s), then the 30 s drain, then the span flush (up
  to 5 s), in a row. A test: 35 s. Fix: one deadline under the stop timeout.
- [ ] **104. Subscribers keep warm connections and a mux session to nodes hosting only services they're denied:**
  `proxied_nodes` applies `subscribeFilter` but not the policy.
- [ ] **105. Each request and TCP connection scans the whole service index and deep-clones every instance's `NodeInfo`:**
  37 us per request in release with 2000 index entries. Fix: a per-service
  index of `Arc`s; `retain_nodes` with a set.
- [ ] **106. A merged sync that changes nothing still copies the known-nodes map** (O(N^2) per round): store only when changed.
- [ ] **107. With `requireSignedConfig`, every config poll verifies the signed file under the config write lock on an async worker:**
  8 ms (release, 1.8 MB) each 10 s that every reader waits behind. Fix:
  skip an unchanged file; verify outside the lock.
- [ ] **108. #91's fallback is undone by `edited_while_stopped`:** with history
  [v3 signed, v4 unsigned], `startup_config` picks v3, then
  `edited_while_stopped` starts the daemon on unverified v4. Fix: use the
  newest entry that verifies there too.
- [ ] **109. Fingerprints compared as written:** `signing::verify`'s
  revokes-its-own-signer check, `revocation_lifted_by` (asks peers for
  history for nothing), and `revoke`/`unrevoke` (a colon or uppercase entry
  can't be lifted; revoke adds a duplicate). No evasion: `Revocations::set`
  normalises. Fix: normalise `revoked.certs` on decode, or compare normalised.
- [ ] **110. A refused change exits 1 in the file but 2 (or 3) through the daemon,** against #70:
  `save_local_change` bails with anyhow instead of `refused_change`.
- [ ] **111. README says CLI changes with no daemon running are recorded in the history:** since #51 they
  aren't (nor can `config rollback --to` reach them); the History help text likewise.

Refuted: a partial length prefix holds only the peer's bounded idle slot;
half-close over a mesh tunnel works.

Unconfirmed (code reading): an idempotent request isn't retried after a
keep-alive reset (502); a status retry holds the earlier node's slot; the
HTTP ports have no header-read timeout or connection cap; `init` with a
wildcard bind announces `0.0.0.0` in `NodeJoin`; deploy scripts match
`tlsDir` textually (`config/../tls`); Dockerfile builds without `--locked`
and runs as root; every cluster shares the `stouter:latest` tag;
`--label`/`--meta` replace the whole map; `keep_pushed` treats `tlsDir: ""`
as set; a read-only config directory makes every start "first start
alone"; history catch-up is triggered by an unverified config (bounded).
