#!/usr/bin/env bash
set -euo pipefail

debug() { echo "  [DEBUG] $*"; }

usage() {
  echo "Usage: ./deploy.sh [-c config_file] [-n cluster] [-r] user@host"
  echo "  -c  config file to upload"
  echo "  -n  cluster name (default: 'default'). Lets multiple stouter clusters"
  echo "      coexist on one host with separate container, config, and build dir."
  echo "  -r  restart only (skip build, just update config and restart container)"
  exit 1
}

CONFIG_FILE=""
RESTART_ONLY=false
CLUSTER="default"
while getopts "c:n:r" opt; do
  case $opt in
    c) CONFIG_FILE="$OPTARG" ;;
    n) CLUSTER="$OPTARG" ;;
    r) RESTART_ONLY=true ;;
    *) usage ;;
  esac
done
shift $((OPTIND - 1))

REMOTE_HOST="${1:?$(usage)}"
if [[ "$CLUSTER" == "default" ]]; then
  REMOTE_DIR="/srv/stouter"
  CONTAINER_NAME="stouter"
else
  REMOTE_DIR="/srv/stouter/$CLUSTER"
  CONTAINER_NAME="stouter-$CLUSTER"
fi
IMAGE_TAG="stouter:latest"

debug "REMOTE_HOST=$REMOTE_HOST"
debug "CLUSTER=$CLUSTER"
debug "REMOTE_DIR=$REMOTE_DIR"
debug "CONTAINER_NAME=$CONTAINER_NAME"
debug "CONFIG_FILE=${CONFIG_FILE:-<none>}"
debug "RESTART_ONLY=$RESTART_ONLY"

# Prompt for sudo password once and cache it for subsequent commands
read -rsp "[sudo] password for $REMOTE_HOST: " SUDO_PASS
echo
SUDO="echo '$SUDO_PASS' | sudo -S"

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Syncing source to $REMOTE_HOST:$REMOTE_DIR/build/"
  debug "Creating remote directory $REMOTE_DIR/build/"
  ssh "$REMOTE_HOST" "$SUDO mkdir -p $REMOTE_DIR/build && $SUDO chown \$(id -u):\$(id -g) $REMOTE_DIR/build"
  debug "Cleaning remote build directory"
  ssh "$REMOTE_HOST" "$SUDO rm -rf $REMOTE_DIR/build/*"
  debug "Running scp (excluding target/, .git/, .idea/, *.json)"
  scp -r $(find . -maxdepth 1 \
    ! -name '.' \
    ! -name 'target' \
    ! -name '.git' \
    ! -name '.idea' \
    ! -name '*.json' \
    -print) "$REMOTE_HOST:$REMOTE_DIR/build/"
  debug "SCP complete"
fi

if [[ -n "$CONFIG_FILE" ]]; then
  # Only upload config if it doesn't already exist on the remote
  if ssh "$REMOTE_HOST" "$SUDO test -f $REMOTE_DIR/stouter.json" 2>&1 | grep -v '^\[sudo\]'; then
    echo "==> Config already exists at $REMOTE_DIR/stouter.json, skipping upload"
  else
    echo "==> Uploading config: $CONFIG_FILE"
    debug "SCP $CONFIG_FILE -> $REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
    scp "$CONFIG_FILE" "$REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
    echo "==> Deploying stouter.json"
    debug "Copying config to $REMOTE_DIR/stouter.json"
    ssh "$REMOTE_HOST" "$SUDO cp $REMOTE_DIR/build/stouter.json $REMOTE_DIR/stouter.json" 2>&1 | grep -v '^\[sudo\]'
  fi
else
  debug "No config file specified, skipping config upload"
fi

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Building Docker image on remote"
  debug "Running: docker build -t stouter:latest in $REMOTE_DIR/build"
  ssh "$REMOTE_HOST" "cd $REMOTE_DIR/build && $SUDO docker build -t stouter:latest ." 2>&1 | grep -v '^\[sudo\]'
  debug "Docker build complete"

  echo "==> Copying stouter binary from Docker image to host"
  debug "Extracting /usr/local/bin/stouter from stouter:latest image"
  ssh "$REMOTE_HOST" "
    $SUDO docker create --name stouter-extract stouter:latest
    $SUDO docker cp stouter-extract:/usr/local/bin/stouter /usr/local/bin/stouter
    $SUDO docker rm stouter-extract
  " 2>&1 | grep -v '^\[sudo\]'
  debug "Host binary installed to /usr/local/bin/stouter"
else
  debug "Restart-only mode, skipping build"
fi

echo "==> Restarting container"
debug "Stopping and removing existing container: $CONTAINER_NAME"
ssh "$REMOTE_HOST" "
  $SUDO docker stop $CONTAINER_NAME 2>&1 || true
  $SUDO docker rm $CONTAINER_NAME 2>&1 || true
" 2>&1 | grep -v '^\[sudo\]'
debug "Starting new container: $CONTAINER_NAME (--network host, config=$REMOTE_DIR/stouter.json)"
ssh "$REMOTE_HOST" "
  $SUDO docker run -d \
    --name $CONTAINER_NAME \
    --restart unless-stopped \
    --network host \
    -v $REMOTE_DIR/stouter.json:$REMOTE_DIR/stouter.json \
    stouter:latest \
    --config $REMOTE_DIR/stouter.json
" 2>&1 | grep -v '^\[sudo\]'

echo "==> Deployed. Checking status..."
CONTAINER_STATUS=$(ssh "$REMOTE_HOST" "$SUDO docker ps --filter name=^${CONTAINER_NAME}\$ --format '{{.Status}}'" 2>&1 | grep -v '^\[sudo\]')
echo "$CONTAINER_STATUS"
debug "Container status: ${CONTAINER_STATUS:-<not running>}"
debug "Deploy finished at $(date '+%Y-%m-%d %H:%M:%S')"
