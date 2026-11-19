#!/usr/bin/env bash
# caps.sh BINARY: per-peer connection caps hold under load, and the node
# keeps answering. Needs python3.
BIN=$(realpath "$1"); DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"

PORT=${PORT:-27101}
cat > "$WORK/n.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:$PORT", "nodeId": "n1", "knownNodes": [], "tlsDir": "$WORK/tls",
 "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
start n1 "$WORK/n.json"
wait_for 20 test -f "$WORK/tls/ca.key" || { fail "node started"; finish; }
"$BIN" --config "$WORK/n.json" cert issue --node peer1 --out "$WORK/peer1" > /dev/null
"$BIN" --config "$WORK/n.json" cert issue --node peer2 --out "$WORK/peer2" > /dev/null
wait_for 20 log_has n1 "Node listening"

python3 "$DIR/caps.py" 127.0.0.1 "$PORT" "$WORK/peer1" "$WORK/peer2" || FAILED=1
check "the node is still running" kill -0 "${PIDS[0]}"
check "and answers a status request" sh -c "'$BIN' --config '$WORK/n.json' status | grep -q 'running (node mode)'"
finish
