#!/usr/bin/env bash
# history.sh BINARY: config history and rollback through real daemons. A
# and B; changes made through A's daemon are recorded with what changed, on
# both nodes; a bad change is rolled back by version, and the rollback
# reaches B as a new version.
BIN=$(realpath "$1"); DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"

conf() { # conf NAME PORT KNOWN
  cat > "$WORK/$1.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:$2", "nodeId": "$1", "knownNodes": [$3], "tlsDir": "$WORK/tls",
 "apiPort": $(($2 + 80)), "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
}
conf A 27801 ""
conf B 27802 '"127.0.0.1:27801"'
start A "$WORK/A.json"
wait_for 20 test -f "$WORK/tls/ca.key" || { fail "A started"; finish; }
start B "$WORK/B.json"
check "A and B find each other" wait_for 30 log_has A "discovered new node: B"

cli() { "$BIN" --config "$WORK/A.json" "$@" 2>&1 | tee -a "$WORK/cli.log"; }
version_of() { sed -n 's/.*"version": *\([0-9]*\).*/\1/p' "$WORK/$1.json" | head -n 1; }
cli service add --group web --name api --port 3000 > /dev/null
cli service add --group web --name db --port 5432 > /dev/null
good=$(version_of A)
# A bad change: nobody may reach api any more.
cli policy allow --service api --from nobody > /dev/null
bad=$(version_of A)
check "B gets the bad change" wait_for 20 grep -q '"nobody"' "$WORK/B.json"

check "A's history lists each version with what changed" sh -c "
  '$BIN' --config '$WORK/A.json' config history > '$WORK/hist-a.txt' &&
  grep -q '^version $good ' '$WORK/hist-a.txt' && grep -q 'service web/db on A added' '$WORK/hist-a.txt' &&
  grep -q '^version $bad ' '$WORK/hist-a.txt' && grep -q 'policy for api added: nobody' '$WORK/hist-a.txt'"
check "B's history, asked remotely, has the bad change too" sh -c "
  '$BIN' --config '$WORK/A.json' config history --remote 127.0.0.1:27802 | grep -q 'policy for api added: nobody'"
check "config history --version prints that version" sh -c "
  '$BIN' --config '$WORK/A.json' config history --version $good | grep -q '\"name\": \"db\"'"

check "config rollback --to the good version" sh -c "'$BIN' --config '$WORK/A.json' config rollback --to $good >> '$WORK/cli.log' 2>&1"
check "the policy is gone on A" sh -c "! grep -q '\"nobody\"' '$WORK/A.json'"
check "and on B, as a newer version" wait_for 20 sh -c "! grep -q '\"nobody\"' '$WORK/B.json' && test \$(sed -n 's/.*\"version\": *\([0-9]*\).*/\1/p' '$WORK/B.json' | head -n 1) -gt $bad"
check "the services are still there" grep -q '"db"' "$WORK/B.json"
check "the rollback is in the history" sh -c "
  '$BIN' --config '$WORK/A.json' config history | grep -q 'policy for api removed: open to every node'"
check "rolling back to an unknown version is refused" sh -c "! '$BIN' --config '$WORK/A.json' config rollback --to 9999 2>/dev/null"

# A daemon whose stdout is closed (`stouter | logger` whose reader died)
# keeps taking changes: printing a new config, or logging, can't stop the
# loop that took it. NAT'd, so its own sync loop is its only way to hear.
cat > "$WORK/C.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:27803", "nodeId": "C", "knownNodes": ["127.0.0.1:27801"], "tlsDir": "$WORK/tls",
 "outboundOnly": true, "apiPort": 27883, "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
RUST_LOG=info "$BIN" --config "$WORK/C.json" > >(exit 0) 2> "$WORK/C.log" &
PIDS+=($!)
check "C, with its stdout closed, takes the cluster's config" wait_for 30 grep -q '"db"' "$WORK/C.json"
cli service add --group web --name cache --port 6379 > /dev/null
check "and the next change too" wait_for 30 grep -q '"cache"' "$WORK/C.json"
check "C is still running" kill -0 "${PIDS[-1]}"
# The README's way to add a node: a service added on it before it ever
# ran, while the cluster is versions ahead. Started, it catches up and makes
# the change again on top of the cluster's config, which takes it; nothing
# is set aside.
conf D 27804 '"127.0.0.1:27801"'
"$BIN" --config "$WORK/D.json" service add --group web --name dsvc --port 3900 >> "$WORK/cli.log" 2>&1
start D "$WORK/D.json"
check "a service added on a new node before it started reaches the cluster" wait_for 60 grep -q '"dsvc"' "$WORK/A.json"
check "  on top of what the cluster had" grep -q '"cache"' "$WORK/A.json"
check "  with nothing set aside" sh -c "! ls '$WORK'/D.json.unapplied* > /dev/null 2>&1"
# The same with the new node's file at the cluster's own version (as on a
# new cluster): the change isn't sent to the cluster from the file, where a
# peer would take it whole and lose every service the file lacks. The
# daemon makes it on top of the cluster's config once it runs.
v=$(version_of A)
cat > "$WORK/E.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:27805", "nodeId": "E", "knownNodes": ["127.0.0.1:27801"], "tlsDir": "$WORK/tls",
 "apiPort": 27885, "dynamicConfig": {"version": $v, "serviceGroups": []}}
JSON
"$BIN" --config "$WORK/E.json" service add --group web --name esvc --port 3901 >> "$WORK/cli.log" 2>&1
sleep 3
check "a change made with no daemon running stays in its file" sh -c "! grep -q '\"esvc\"' '$WORK/A.json' && grep -q '\"db\"' '$WORK/A.json'"
start E "$WORK/E.json"
check "  and reaches the cluster once the daemon runs" wait_for 60 grep -q '"esvc"' "$WORK/A.json"
check "  without losing what the cluster had" sh -c "grep -q '\"db\"' '$WORK/A.json' && grep -q '\"dsvc\"' '$WORK/A.json'"

# A command whose output is cut short (`| head`) ends as Unix tools do,
# by SIGPIPE, with nothing on stderr; more output than a pipe holds.
{
  printf '{"mode": "node", "bind": "127.0.0.1:27809", "nodeId": "big", "knownNodes": [], "tlsDir": "%s/big-tls",' "$WORK"
  printf ' "dynamicConfig": {"version": 1, "serviceGroups": [{"name": "g", "services": ['
  sep=""
  for i in $(seq 1 3000); do printf '%s{"name": "s%d", "nodeId": "big", "nodePort": %d}' "$sep" "$i" $((10000 + i)); sep=","; done
  printf ']}]}}\n'
} > "$WORK/big.json"
check "output cut short ends the CLI by SIGPIPE, quietly" bash -c "
  '$BIN' --config '$WORK/big.json' service list 2> '$WORK/big.err' | head -c 1 > /dev/null
  [[ \${PIPESTATUS[0]} -eq 141 ]] && ! test -s '$WORK/big.err'"

[[ $FAILED -eq 0 ]] || { cat "$WORK/cli.log"; cat "$WORK/hist-a.txt" 2>/dev/null; cat "$WORK/C.log" 2>/dev/null; }
finish
