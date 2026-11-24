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

- [ ] **3. The CLI never loads the revocation list.**
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

- [ ] **4. A config at version `u64::MAX` can never be replaced.**
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

## Medium

- [ ] **5. CPU exhaustion through beat signatures.**
  `check_signatures` (`src/gossip/mod.rs:1506-1537`) verifies every beat in a
  sync: no per-node dedup, no cap, on a tokio worker (called from `merge_sync`,
  `:1060`). 20,000 beats took 1.4–2.3 s; an 8 MiB sync holds ~45,000. A few
  such connections stall gossip, tunnels and admin requests (contradicts the
  README's limits section).
  Fix: keep only the highest beat per node per sync, cap checks per sync, and
  run the checks in `spawn_blocking`.

- [ ] **6. A lying daemon can steer an admin's signature.**
  (a) The CLI builds a change on whatever signed config the daemon serves
  (`src/main.rs:1667-1676`), even one older than the local file's; its result can
  win the same-version tie-break (`src/config.rs:428-432`) and drop the latest
  change (e.g. a revocation). (b) `config rollback --to N`
  (`src/main.rs:1309-1338`) never checks that the verified config is version N.
  Fix: refuse to build on a config older than the local file's; require the
  verified version to equal N; don't let a same-version tie-break drop
  revocations.

- [ ] **7. `requestTimeoutMs` firing during tunnel setup returns 504, with no retry and no ejection.**
  `src/subscribe/http.rs:366-371` wraps the whole request, connector included;
  only connector errors reach the retry / `note_tunnel_failure` path (`:402`). A
  node whose tunnel setup hangs gets every request picked for it 504'd, and is
  never ejected: setting a timeout makes failover worse.
  Fix: start the request timeout once the tunnel is up; a timeout before that is
  a connect failure (charge the node, retry elsewhere).

- [ ] **8. Same-version hand edits are lost or ignored.**
  (a) The config poll (`src/gossip/mod.rs:595-596`) compares only services and
  routes, so a hand-added revocation or policy change at the same version is
  silently not applied. (b) An edit made while the daemon is stopped is taken
  as is at startup (`src/main.rs:458-462`, `src/state.rs:527`), never bumped, and
  settled by the same-version tie-break: it may be silently reverted with no
  `.unapplied` copy, or spread cluster-wide without a version bump.
  Fix: use `same_settings` in the poll; at startup, compare the file with the
  last history entry and bump/stamp a same-version edit like the poll does.

- [ ] **9. `deploy.sh` reports success while the new daemon crash-loops.**
  It stops and removes the running container (`deploy.sh:155-160`) before the
  new one has proven it starts, and `docker ps` right after `run -d` says "Up".
  Fix: run `stouter check` in the new image against the host's config before
  replacing the container; after starting, wait and fail on restarts.

- [ ] **10. `stouter check` passes configs the daemon refuses or misreads.**
  No `tlsDir` (what `init` writes), `bind` without a port, `knownNodes` entries
  without a port (`src/config.rs:1181-1250`, `src/main.rs:452-454`). Unknown
  keys are only caught at the top level (`src/config.rs:1080`): a misspelled
  nested setting (`dynamicConfig.polices`, a service's `maxConection`,
  `subscribeFilter.serviceGroup`, `tracing.sampleRation`) is silently ignored
  and dropped when the file is rewritten, contrary to README:76-81.
  Fix: move those checks into `validate_local`; have `init` write a `tlsDir`;
  catch and keep unknown keys in the nested structs too (or correct the README).

- [ ] **11. The CLI's log lines go to stdout, with colour codes.**
  `src/main.rs:420-425`. Breaks machine-readable output (e.g. the JSON from
  `config history --version N`), and pollutes `status` and `cert issue`.
  Fix: log to stderr; default CLI subcommands to `warn`; no colour off a terminal.

## Low

- [ ] **12. Refused callers fill the replay cache.** `src/authz.rs:77` records a
  signed request's nonce before the policy check (`:91`); a refused member can
  fill the 100,000-entry cache and lock out allowed callers through relays.
  Fix: record the nonce only once the caller is allowed; cap entries per signer.
- [ ] **13. One signed nonce is reused across relay failover.** `src/subscribe/proxy.rs:448-457`
  signs once and reuses the request on every route (`:487-505`); if relay 1
  delivered it but answered slowly, relay 2's attempt is refused as a replay.
  Fix: sign a fresh request per route.
- [ ] **14. A route with no backends crashes every request it matches.**
  `src/subscribe/http.rs:530` indexes `route.backends[0]`; gossip adopts such
  routes (`src/state.rs:918` indexes them unchecked). Fix: skip routes with no
  backends when indexing, or answer 503.
- [ ] **15. A dead reverse connection at a subscriber that is the NAT'd node's relay fails the tunnel.**
  `src/subscribe/proxy.rs:466-467`: no per-attempt timeout or fallback; waits 15 s
  without trying the node's other relays. Fix: ~5 s per attempt, try another
  reverse connection, then fall through to `tunnel_routes`.
- [ ] **16. Connecting to the local service has no timeout.** `src/node/tunnel.rs:163`:
  a backend with a full accept queue holds the tunnel task ~2 min (and delays a
  drain). Fix: time out after a few seconds, under the subscriber's 15 s.
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
- [ ] **22. A second SIGTERM/SIGINT during the drain is ignored.** `src/main.rs:491-497`,
  `1477-1505`. Fix: exit at once on a second signal.
- [ ] **23. A huge `certLifetime` crashes `check` and the daemon.** `src/pki.rs:40-44`
  (`time::Duration::days(n)` overflows). Fix: bound `n` before building it.
- [ ] **24. `deploy.sh`'s binary extraction can install a stale binary.**
  `deploy.sh:143-147`: fixed container name `stouter-extract`, no error checks.
  Fix: a unique name, `docker rm -f` it first, `set -e` in the remote script.
- [ ] **25. `deploy.sh` with no arguments under macOS bash 3.2** dies with
  "ARGS[@]: unbound variable" instead of printing usage (`deploy.sh:26`).
  Fix: `set -- ${ARGS[@]+"${ARGS[@]}"}`.
- [ ] **26. No-op and empty input accepted.** `route clear` on an unknown service
  succeeds and bumps the version (`src/config.rs:762-776`); `--domains ""` or
  `a,,b` stores an empty domain, which then clashes (`src/main.rs:269`).
- [ ] **27. `tests/e2e/renew.sh` never checks that the CA node still answers after
  its first certificate expires** (lines 35-37). Add `check "A answers past its
  first certificate's expiry" status A`.
