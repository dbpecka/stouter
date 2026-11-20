#!/usr/bin/env bash
# deploy-host.sh [user@host]: run deploy.sh against a real host (default:
# this one, over ssh to localhost), with a cluster name and a tlsDir outside
# the config directory. Checks that the container runs and answers, that a
# redeploy replaces the container but keeps the CA, and that it stops
# cleanly.
#
# Needs ssh to the host without a password prompt (keys), and sudo and
# Docker there. deploy.sh builds the image on the host (minutes), unless
# RESTART_ONLY=1: then stouter:latest must be there already. SUDO_PASS is
# the host's sudo password, if sudo asks for one. Without RESTART_ONLY,
# deploy.sh also installs the binary at /usr/local/bin/stouter on the host.
DIR=$(cd "$(dirname "$0")" && pwd)
REPO=$(cd "$DIR/../.." && pwd)
source "$DIR/common.sh"
HOST=${1:-$(id -un)@localhost}
CLUSTER=e2e$$
REMOTE=/srv/stouter/$CLUSTER
TLS=/srv/stouter/$CLUSTER-tls
CONTAINER=stouter-$CLUSTER
PORT=${PORT:-27401}

shq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }
# on_host SCRIPT: run SCRIPT as root on the host.
on_host() { printf '%s\n' "${SUDO_PASS:-}" | ssh -o BatchMode=yes "$HOST" "sudo -S -p '' sh -c $(shq "exec </dev/null; $1")"; }
extra_cleanup() { on_host "docker rm -f $CONTAINER; rm -rf $REMOTE $TLS" > /dev/null 2>&1 || true; }

on_host true || { fail "ssh and sudo on $HOST"; finish; }

cat > "$WORK/stouter.json" <<JSON
{"mode": "node", "bind": "127.0.0.1:$PORT", "nodeId": "deployed", "knownNodes": [], "tlsDir": "$TLS",
 "apiPort": $((PORT + 80)), "dynamicConfig": {"version": 1, "serviceGroups": []}}
JSON
# deploy [-r]: deploy.sh, building unless RESTART_ONLY (or -r) says not to.
deploy() {
  local flags=()
  [[ -n "${RESTART_ONLY:-}" || "${1:-}" == -r ]] && flags+=(-r)
  printf '%s\n' "${SUDO_PASS:-}" | "$REPO/deploy.sh" "${flags[@]}" -c "$WORK/stouter.json" -n "$CLUSTER" "$HOST" \
    > "$WORK/deploy.log" 2>&1
}
container_id() { on_host "docker inspect -f '{{.Id}}' $CONTAINER" 2>/dev/null; }
running() { on_host "docker ps --filter name=^$CONTAINER\$ --format '{{.Status}}'" | grep -q '^Up'; }
answers() {
  on_host "docker exec $CONTAINER stouter --config $REMOTE/config/stouter.json status" | grep -q 'running (node mode)'
}
ca_hash() { on_host "cat $TLS/ca.crt" | sha256sum | cut -d' ' -f1; }

if deploy; then pass "deploy.sh deploys"; else fail "deploy.sh deploys"; cat "$WORK/deploy.log"; finish; fi
check "the container runs" running
check "the daemon answers" wait_for 30 answers
check "the config is in place" on_host "test -f $REMOTE/config/stouter.json"
check "the CA is on the host, outside the container" wait_for 10 on_host "test -f $TLS/ca.crt"
ca_before=$(ca_hash)
id_before=$(container_id)

if deploy -r; then pass "a redeploy (-r)"; else fail "a redeploy (-r)"; cat "$WORK/deploy.log"; fi
check "the config already there is kept" grep -q "Config already exists" "$WORK/deploy.log"
check "the container is replaced" test "$(container_id)" != "$id_before"
check "and runs" running
check "and answers" wait_for 30 answers
check "the CA survives the new container" test "$(ca_hash)" = "$ca_before"

t0=$(date +%s)
on_host "docker stop -t 25 $CONTAINER" > /dev/null
secs=$(( $(date +%s) - t0 ))
code=$(on_host "docker inspect -f '{{.State.ExitCode}}' $CONTAINER")
echo "stopped in ${secs}s, exit code $code"
check "docker stop: exit code 0" test "$code" = 0
check "docker stop: within 10s" test "$secs" -lt 10
[[ $FAILED -eq 0 ]] || { cat "$WORK/deploy.log"; on_host "docker logs $CONTAINER" 2>&1 | tail -20; }
finish
