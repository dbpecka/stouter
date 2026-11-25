#!/bin/bash
# docker-create.sh [config_dir]: start the stouter container on this host,
# with the config in config_dir (default /srv/stouter/config).
#
# The config directory is mounted into the container, and so is the
# config's tlsDir if it lies elsewhere: in the container's own filesystem
# the CA node's CA (and every node's certificates) would be lost with the
# container, and a new container would make a new CA no other node trusts.
set -euo pipefail

CONFIG_DIR="${1:-/srv/stouter/config}"
[[ -f "$CONFIG_DIR/stouter.json" ]] || { echo "error: no config at $CONFIG_DIR/stouter.json" >&2; exit 1; }
# Absolute: Docker mounts only absolute paths, and the container sees the
# config at the same path.
CONFIG_DIR=$(cd "$CONFIG_DIR" && pwd)
CONFIG="$CONFIG_DIR/stouter.json"

mounts=(-v "$CONFIG_DIR:$CONFIG_DIR")
tls_dir=$(sed -n 's/.*"tlsDir"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$CONFIG" | head -n 1)
# A relative tlsDir is the config directory's (the daemon takes it from
# there), so it is mounted with it, unless it climbs out.
case "$tls_dir" in
  '') echo "warning: $CONFIG sets no tlsDir: the daemon won't start (\`stouter check --local\` says why)" >&2 ;;
  "$CONFIG_DIR" | "$CONFIG_DIR"/*) ;;
  /*) mkdir -p "$tls_dir" && mounts+=(-v "$tls_dir:$tls_dir") ;;
  .. | ../* | */.. | */../*) echo "warning: tlsDir '$tls_dir' leaves $CONFIG_DIR, so it is inside the container and lost with it" >&2 ;;
esac

# --stop-timeout 30: stouter's full drain on `docker stop` (Docker's default is 10s).
docker run -d \
    --name stouter \
    --restart unless-stopped \
    --stop-timeout 30 \
    --network host \
    "${mounts[@]}" \
    stouter --config "$CONFIG"
