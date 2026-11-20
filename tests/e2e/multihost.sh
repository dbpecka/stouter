#!/usr/bin/env bash
# multihost.sh BINARY: four "hosts" (network namespaces on one bridge, each
# with its own IP), all bound to 0.0.0.0:8080 with no advertiseAddr, the
# default. X and the subscriber S know only A. S must still reach X: it
# learns X's real address through A. Needs root, iproute2, python3, curl.
BIN=$(realpath "$1"); DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"
[[ $EUID -eq 0 ]] || { echo "needs root (network namespaces)"; exit 1; }

NAMES=(a x t s)
extra_cleanup() {
  for n in "${NAMES[@]}"; do
    ip netns pids "st-$n" 2>/dev/null | xargs -r kill 2>/dev/null || true
    ip netns del "st-$n" 2>/dev/null || true
    ip link del "st-$n-h" 2>/dev/null || true
  done
  ip link del st-br0 2>/dev/null || true
  iptables -D FORWARD -i st-br0 -o st-br0 -j ACCEPT 2>/dev/null || true
}
extra_cleanup
ip link add st-br0 type bridge && ip link set st-br0 up
# With Docker running (as on CI runners), forwarding defaults to DROP and
# bridged traffic goes through it: let the test bridge's own through.
if command -v iptables > /dev/null; then iptables -I FORWARD -i st-br0 -o st-br0 -j ACCEPT; fi
i=0
for n in "${NAMES[@]}"; do
  i=$((i + 1))
  ip netns add "st-$n"
  ip link add "st-$n-h" type veth peer name "st-$n-n"
  ip link set "st-$n-h" master st-br0 up
  ip link set "st-$n-n" netns "st-$n"
  ip -n "st-$n" link set "st-$n-n" name eth0
  ip -n "st-$n" addr add "10.99.0.$i/24" dev eth0
  ip -n "st-$n" link set eth0 up
  ip -n "st-$n" link set lo up
done
in_ns() { local n=$1; shift; ip netns exec "st-$n" "$@"; }

DC='{"version": 1, "serviceGroups": [{"name": "g", "services": [
  {"name": "xsvc", "nodeId": "X", "nodePort": 9000, "localPort": 15000}]}]}'
conf() { # conf NAME MODE KNOWN
  echo "{\"mode\": \"$2\", \"bind\": \"0.0.0.0:8080\", \"nodeId\": \"$1\", \"knownNodes\": [$3],
    \"tlsDir\": \"$WORK/tls\", \"dynamicConfig\": $DC}" > "$WORK/$1.json"
}
conf A node ""
conf X node '"10.99.0.1:8080"'
conf T node '"10.99.0.1:8080"'
conf S subscribe '"10.99.0.1:8080"'

# Directly, not through a function: `$!` must be the server itself.
ip netns exec st-x python3 "$DIR/echo.py" 9000 x > "$WORK/echo.log" 2>&1 &
PIDS+=($!)
start A "$WORK/A.json" ip netns exec st-a
wait_for 20 test -f "$WORK/tls/ca.key"
start X "$WORK/X.json" ip netns exec st-x
start T "$WORK/T.json" ip netns exec st-t
start S "$WORK/S.json" ip netns exec st-s

health() { in_ns s curl -s 127.0.0.1:5381/__health; }
x_live_at() { health | python3 -c "
import json, sys
peers = json.load(sys.stdin)['nodes']['peers']
sys.exit(0 if any(p['id'] == 'X' and p['addr'] == '$1' and p['live'] for p in peers) else 1)"; }
tunnel() { in_ns s python3 -c "
import socket
s = socket.create_connection(('127.0.0.1', 15000), timeout=5)
s.sendall(b'hi'); s.shutdown(socket.SHUT_WR)
print(s.recv(100).decode())"; }

check "S learns X's routable address through A, and finds it live" wait_for 40 x_live_at 10.99.0.2:8080
tunnel_ok() { tunnel | grep -q '^x:hi$'; }
no_wildcards() { ! health | grep -q '"addr":"0.0.0.0'; }
check "S tunnels to X's service" wait_for 20 tunnel_ok
check "T knows X at its routable address" wait_for 20 log_has T "X @ 10.99.0.2:8080\|address updated: .* -> 10.99.0.2:8080"
check "S holds no unroutable 0.0.0.0 address" no_wildcards
finish
