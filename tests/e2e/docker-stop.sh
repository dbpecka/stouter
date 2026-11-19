#!/usr/bin/env bash
# docker-stop.sh: `docker stop` on stouter running as a container's PID 1.
# It must take the SIGTERM, tell its peers it is leaving, drain, and exit 0
# within seconds, not ignore the signal until Docker kills it at the
# timeout (exit 137) with no word to its peers.
# Needs Docker and the image (IMAGE, default stouter:latest; build it with
# `docker build -t stouter:latest .`).
DIR=$(cd "$(dirname "$0")" && pwd)
source "$DIR/common.sh"
IMAGE=${IMAGE:-stouter:latest}
NAME=stouter-e2e-$$
extra_cleanup() { docker rm -f "$NAME-a" "$NAME-b" > /dev/null 2>&1 || true; }

conf() { # conf NAME PORT KNOWN
  cat > "$WORK/$1.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:$2", "nodeId": "$1", "knownNodes": [$3], "tlsDir": "$WORK/tls",
 "apiPort": $(($2 + 80)), "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
}
conf A 27301 ""
conf B 27302 '"127.0.0.1:27301"'

run() { # run NAME: the daemon in a container, as this user (so the test can clean up what it writes)
  docker run -d --name "$NAME-$(echo "$1" | tr A-Z a-z)" --network host --user "$(id -u):$(id -g)" \
    -e RUST_LOG=info -v "$WORK:$WORK" "$IMAGE" --config "$WORK/$1.json" > /dev/null
}
logs() { docker logs "$NAME-$1" 2>&1 | sed 's/\x1b\[[0-9;]*m//g'; }
logs_have() { logs "$1" | grep -q -- "$2"; }

run A
wait_for 30 test -f "$WORK/tls/ca.key" || { fail "A started"; logs a; finish; }
run B
check "A and B find each other" wait_for 30 logs_have a "discovered new node: B"

# stop NAME: stop the container, with more time than it should need.
stop() {
  local t0; t0=$(date +%s)
  docker stop -t 25 "$NAME-$1" > /dev/null
  STOP_SECS=$(( $(date +%s) - t0 ))
  STOP_CODE=$(docker inspect -f '{{.State.ExitCode}}' "$NAME-$1")
}
stop b
echo "B stopped in ${STOP_SECS}s, exit code $STOP_CODE"
check "B exits 0 on docker stop" test "$STOP_CODE" = 0
check "B stops within 10s, not at Docker's timeout" test "$STOP_SECS" -lt 10
check "B shuts down gracefully" logs_have b "shutting down gracefully"
check "B drains" logs_have b "all connections drained"
check "A hears that B left" wait_for 10 logs_have a "node left: B"

stop a
check "A exits 0 on docker stop too" test "$STOP_CODE" = 0
check "A stops within 10s" test "$STOP_SECS" -lt 10
if [[ $FAILED -ne 0 ]]; then
  echo "--- A"; logs a | tail -20
  echo "--- B"; logs b | tail -20
fi
finish
