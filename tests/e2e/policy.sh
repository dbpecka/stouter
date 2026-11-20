#!/usr/bin/env bash
# policy.sh BINARY: service authorization end to end, on this host.
# A (relay), X (hosts xsvc) and Y (NAT'd: outboundOnly, hosts ysvc, reached
# through a relay), and subscriber S1. Once only S1 may reach the services:
# S1 still tunnels to both (to Y with signed requests through a relay), and
# the hosting nodes refuse anyone else, whatever a client does: a node
# that isn't allowed connecting directly, and unsigned requests through a
# relay, even from S1's own certificate. Needs python3.
BIN=$(realpath "$1"); DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"

DC='{"version": 1, "serviceGroups": [{"name": "g", "services": [
  {"name": "xsvc", "nodeId": "X", "nodePort": 27710, "localPort": 27720},
  {"name": "ysvc", "nodeId": "Y", "nodePort": 27711, "localPort": 27721}]}]}'
conf() { # conf NAME MODE PORT KNOWN [EXTRA]
  echo "{\"mode\": \"$2\", \"bind\": \"127.0.0.1:$3\", \"nodeId\": \"$1\", \"knownNodes\": [$4],
    \"tlsDir\": \"$WORK/tls\", \"apiPort\": $(($3 + 80)), \"dynamicConfig\": $DC ${5:-}}" > "$WORK/$1.json"
}
conf A node 27701 ""
conf X node 27702 '"127.0.0.1:27701"'
conf Y node 27703 '"127.0.0.1:27701"' ', "outboundOnly": true'
conf S1 subscribe 27704 '"127.0.0.1:27701"'

python3 "$DIR/echo.py" 27710 x > "$WORK/echo-x.log" 2>&1 &
PIDS+=($!)
python3 "$DIR/echo.py" 27711 y > "$WORK/echo-y.log" 2>&1 &
PIDS+=($!)
start A "$WORK/A.json"
wait_for 20 test -f "$WORK/tls/ca.key" || { fail "A started"; finish; }
start X "$WORK/X.json"
start Y "$WORK/Y.json"
start S1 "$WORK/S1.json"
for n in S1 S2; do "$BIN" --config "$WORK/A.json" cert issue --node $n --out "$WORK/$n" > /dev/null; done

# through PORT: S1's local proxy answers with TAG (the service's reply).
through() { python3 -c "
import socket
s = socket.create_connection(('127.0.0.1', $1), timeout=10)
s.sendall(b'hi'); s.shutdown(socket.SHUT_WR)
print(s.recv(100).decode())" 2>/dev/null | grep -q "^$2:hi$"; }
# raw CERT HOST_PORT NODE SERVICE: a tunnel opened by hand (see tunnel.py).
raw() { python3 "$DIR/tunnel.py" 127.0.0.1 "$2" "$WORK/$1" "$3" "$4"; }
opens() { raw "$@" | grep -q '^OK'; }
refused_for() { local why=$1; shift; raw "$@" | tee -a "$WORK/raw.log" | grep -q "^REFUSED.*$why"; }
policy() { "$BIN" --config "$WORK/A.json" policy "$@" >> "$WORK/policy.log" 2>&1; }
in_config() { grep -q "$2" "$WORK/$1.json"; }

check "S1 reaches xsvc (direct)" wait_for 60 through 27720 x
check "S1 reaches ysvc (NAT'd, through a relay)" wait_for 60 through 27721 y
check "with no policy, S2 opens a tunnel to xsvc too" opens S2 27702 X xsvc

check "policy allow: only S1 may reach xsvc" policy allow --service xsvc --from S1
check "policy allow: only S1 may reach ysvc" policy allow --service ysvc --from S1
check "the policies reach X and Y" wait_for 30 sh -c "grep -q policies '$WORK/X.json' && grep -q policies '$WORK/Y.json'"
check "S1 still reaches xsvc" wait_for 20 through 27720 x
check "S1 still reaches ysvc: its requests through the relay are signed" wait_for 20 through 27721 y
check "S1 opens a tunnel to X by hand: its certificate says who it is" opens S1 27702 X xsvc
check "X refuses S2, connecting directly" refused_for "'S2' may not reach service 'xsvc'" S2 27702 X xsvc
check "Y refuses S2's unsigned request through relay A" refused_for "isn't signed" S2 27701 Y ysvc
check "Y refuses even S1's certificate, unsigned through the relay" refused_for "isn't signed" S1 27701 Y ysvc
check "policy list shows them" sh -c "'$BIN' --config '$WORK/X.json' policy list | grep -q \"'xsvc' may be reached by S1\""

check "policy deny: S1 may no longer reach xsvc" policy deny --service xsvc --from S1
check "S1 stops proxying xsvc" wait_for 20 log_has S1 "not proxying 'xsvc'"
check "and X refuses S1 by hand" wait_for 20 refused_for "'S1' may not reach service 'xsvc'" S1 27702 X xsvc
check "policy clear: xsvc is open again" policy clear --service xsvc
check "S2 opens a tunnel to xsvc again" wait_for 20 opens S2 27702 X xsvc
check "and S1 proxies it again" wait_for 20 through 27720 x
if [[ $FAILED -ne 0 ]]; then
  echo "--- policy.log"; cat "$WORK/policy.log"
  echo "--- raw.log"; cat "$WORK/raw.log" 2>/dev/null
  for n in A X Y S1; do echo "--- $n"; sed 's/\x1b\[[0-9;]*m//g' "$WORK/$n.log" | tail -12; done
fi
finish
