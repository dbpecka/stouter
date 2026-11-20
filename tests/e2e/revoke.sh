#!/usr/bin/env bash
# revoke.sh BINARY: revoking a node's certificate through the CLI reaches
# the whole cluster: every node forgets the node and refuses its
# connections, and unrevoking lets it back in. Three nodes on this host.
BIN=$(realpath "$1"); DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"

conf() { # conf NAME PORT KNOWN
  cat > "$WORK/$1.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:$2", "nodeId": "$1", "knownNodes": [$3], "tlsDir": "$WORK/tls",
 "apiPort": $(($2 + 80)), "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
}
conf A 27501 ""
conf B 27502 '"127.0.0.1:27501"'
conf C 27503 '"127.0.0.1:27501"'

start A "$WORK/A.json"
wait_for 20 test -f "$WORK/tls/ca.key" || { fail "A started"; finish; }
start B "$WORK/B.json"
start C "$WORK/C.json"
check "A knows B and C" wait_for 30 sh -c "$(declare -f log_has); WORK='$WORK'; log_has A 'discovered new node: B' && log_has A 'discovered new node: C'"
check "C knows B" wait_for 30 log_has C "discovered new node: B"

# count NAME TEXT: how many lines of NAME's log mention TEXT.
count() { sed 's/\x1b\[[0-9;]*m//g' "$WORK/$1.log" | grep -c -- "$2" || true; }

# Revoke B through A's daemon, with A's own certificate (allowed to make changes there).
if "$BIN" --config "$WORK/A.json" cert revoke --node B > "$WORK/revoke.out" 2>&1; then
  pass "cert revoke --node B"
else
  fail "cert revoke --node B"; cat "$WORK/revoke.out"
fi
check "A forgets B" wait_for 15 log_has A "node B: its certificate is revoked; forgetting it"
check "C hears of it and forgets B too" wait_for 30 log_has C "node B: its certificate is revoked; forgetting it"
check "the revocation is in C's config file" wait_for 10 grep -q '"revoked"' "$WORK/C.json"
check "cert revoked lists it" sh -c "'$BIN' --config '$WORK/C.json' cert revoked | grep -q '^node B '"
# B keeps trying A (its bootstrap peer); A must not take it back.
joined_before=$(count A "discovered new node: B")
sleep 25
check "B stays out: A doesn't rediscover it" test "$(count A 'discovered new node: B')" = "$joined_before"
check "and C doesn't either" test "$(count C 'discovered new node: B')" = 1

if "$BIN" --config "$WORK/A.json" cert unrevoke --node B > "$WORK/unrevoke.out" 2>&1; then
  pass "cert unrevoke --node B"
else
  fail "cert unrevoke --node B"; cat "$WORK/unrevoke.out"
fi
back() { test "$(count A 'discovered new node: B')" -gt "$joined_before"; }
check "B is let back in" wait_for 40 back
if [[ $FAILED -ne 0 ]]; then
  for n in A B C; do echo "--- $n"; sed 's/\x1b\[[0-9;]*m//g' "$WORK/$n.log" | tail -15; done
fi
finish
