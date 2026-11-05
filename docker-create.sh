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
CONFIG_DIR="${CONFIG_DIR%/}"
CONFIG="$CONFIG_DIR/stouter.json"
[[ -f "$CONFIG" ]] || { echo "error: no config at $CONFIG" >&2; exit 1; }

mounts=(-v "$CONFIG_DIR:$CONFIG_DIR")
tls_dir=$(sed -n 's/.*"tlsDir"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p' "$CONFIG" | head -n 1)
case "$tls_dir" in
  '') ;;
  "$CONFIG_DIR"/*) ;;
  /*) mkdir -p "$tls_dir" && mounts+=(-v "$tls_dir:$tls_dir") ;;
  *) echo "warning: tlsDir '$tls_dir' is relative, so it is inside the container and lost with it" >&2 ;;
esac

# --stop-timeout 30: stouter's full drain on `docker stop` (Docker's default is 10s).
docker run -d \
    --name stouter \
    --restart unless-stopped \
    --stop-timeout 30 \
    --network host \
    "${mounts[@]}" \
    stouter --config "$CONFIG"
