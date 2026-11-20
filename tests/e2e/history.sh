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
[[ $FAILED -eq 0 ]] || { cat "$WORK/cli.log"; cat "$WORK/hist-a.txt" 2>/dev/null; }
finish
