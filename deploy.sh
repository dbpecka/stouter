#!/usr/bin/env bash
set -euo pipefail

usage() {
  echo "Usage: ./deploy.sh [-c config_file] [-r] user@host"
  echo "  -c  config file to upload"
  echo "  -r  restart only (skip build, just update config and restart container)"
  exit 1
}

CONFIG_FILE=""
RESTART_ONLY=false
while getopts "c:r" opt; do
  case $opt in
    c) CONFIG_FILE="$OPTARG" ;;
    r) RESTART_ONLY=true ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))

REMOTE_HOST="${1:?$(usage)}"
REMOTE_DIR="/srv/stouter"
CONTAINER_NAME="stouter"

# Prompt for sudo password once and cache it for subsequent commands
read -rsp "[sudo] password for $REMOTE_HOST: " SUDO_PASS
echo
SUDO="echo '$SUDO_PASS' | sudo -S"

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Syncing source to $REMOTE_HOST:$REMOTE_DIR/build/"
  ssh "$REMOTE_HOST" "$SUDO mkdir -p $REMOTE_DIR/build && $SUDO chown \$(id -u):\$(id -g) $REMOTE_DIR/build" 2>/dev/null
  rsync -az --delete \
    --exclude target/ \
    --exclude .git/ \
    --exclude .idea/ \
    --exclude '*.json' \
    ./ "$REMOTE_HOST:$REMOTE_DIR/build/"
fi

if [[ -n "$CONFIG_FILE" ]]; then
  echo "==> Uploading config: $CONFIG_FILE"
  ssh "$REMOTE_HOST" "$SUDO rm -rf $REMOTE_DIR/build/stouter.json $REMOTE_DIR/stouter.json" 2>/dev/null
  scp "$CONFIG_FILE" "$REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
  echo "==> Deploying stouter.json"
  ssh "$REMOTE_HOST" "$SUDO cp $REMOTE_DIR/build/stouter.json $REMOTE_DIR/stouter.json" 2>/dev/null
fi

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Building Docker image on remote"
  ssh "$REMOTE_HOST" "cd $REMOTE_DIR/build && $SUDO docker build -t stouter:latest ." 2>&1 | grep -v '^\[sudo\]'
fi

echo "==> Verifying config exists"
ssh "$REMOTE_HOST" "test -f $REMOTE_DIR/stouter.json" 2>/dev/null || { echo "Error: no config file at $REMOTE_DIR/stouter.json — use -c to provide one"; exit 1; }

echo "==> Restarting container"
ssh "$REMOTE_HOST" "
  $SUDO docker stop $CONTAINER_NAME 2>/dev/null || true
  $SUDO docker rm $CONTAINER_NAME 2>/dev/null || true
  $SUDO docker run -d \
    --name $CONTAINER_NAME \
    --restart unless-stopped \
    --network host \
    -v $REMOTE_DIR/stouter.json:$REMOTE_DIR/stouter.json \
    stouter:latest \
    --config $REMOTE_DIR/stouter.json
" 2>/dev/null

echo "==> Deployed. Checking status..."
ssh "$REMOTE_HOST" "$SUDO docker ps --filter name=$CONTAINER_NAME --format '{{.Status}}'" 2>/dev/null
