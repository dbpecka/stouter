#!/usr/bin/env bash
# renew.sh BINARY: certificates renew without restarts. A is the CA node,
# with "certLifetime": "90s"; B has a certificate issued by A (90s too) in
# its own tlsDir. Both renew when a third is left, without restarting, and
# still work after their first certificates expired; B's renewed one is on
# disk for its next start; one copied into B's tlsDir by hand is taken up.
# Takes about three minutes.
BIN=$(realpath "$1"); DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"

conf() { # conf NAME PORT KNOWN TLS
  cat > "$WORK/$1.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:$2", "nodeId": "$1", "knownNodes": [$3], "tlsDir": "$4",
 "certLifetime": "90s", "apiPort": $(($2 + 80)), "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
}
conf A 27901 "" "$WORK/a-tls"
conf B 27902 '"127.0.0.1:27901"' "$WORK/b-tls"
start A "$WORK/A.json"
wait_for 20 test -f "$WORK/a-tls/ca.key" || { fail "A started"; finish; }
"$BIN" --config "$WORK/A.json" cert issue --node B --out "$WORK/b-tls" > /dev/null
first=$(sha256sum "$WORK/b-tls/node.crt" | cut -d' ' -f1)
start B "$WORK/B.json"
check "A and B find each other" wait_for 30 log_has A "discovered new node: B"
started=$(date +%s)

check "A (the CA node) renews its own certificate" wait_for 90 log_has A "renewed this node's certificate"
check "A issues B a renewed one" wait_for 90 log_has A "issued a renewed certificate for 'B'"
check "B switches to it" wait_for 10 log_has B "renewed this node's certificate"
check "B's renewed certificate is saved in its tlsDir" test "$(sha256sum "$WORK/b-tls/node.crt" | cut -d' ' -f1)" != "$first"

# Past the first certificates' expiry (90s after they were issued).
sleep $(( started + 100 - $(date +%s) > 0 ? started + 100 - $(date +%s) : 0 ))
status() { "$BIN" --config "$WORK/$1.json" status ${2:+--remote "$2"} 2>/dev/null | grep -q "running (node mode)"; }
check "past its first certificate's expiry, B still answers (with its renewed one on disk)" status B
check "and A reaches B over a new connection" status A 127.0.0.1:27902
check "A (the CA node) answers past its first certificate's expiry too" status A
check "B keeps syncing with A" sh -c "! sed 's/\x1b\[[0-9;]*m//g' '$WORK/A.log' | grep -q 'forgetting node B\|node B.*expired'"
check "status shows the certificate's expiry" sh -c "'$BIN' --config '$WORK/B.json' status | grep -q 'Certificate valid until'"

# A certificate copied in by hand (as after `cert issue` on the CA node).
"$BIN" --config "$WORK/A.json" cert issue --node B --out "$WORK/b-tls" > /dev/null
check "a certificate copied into B's tlsDir is taken up without a restart" wait_for 30 log_has B "now using the certificate copied into"
check "B never restarted" test "$(grep -c 'Node listening' "$WORK/B.log")" = 1
if [[ $FAILED -ne 0 ]]; then
  for n in A B; do echo "--- $n"; sed 's/\x1b\[[0-9;]*m//g' "$WORK/$n.log" | grep -iv "debug" | tail -15; done
fi
finish
