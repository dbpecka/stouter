#!/usr/bin/env bash
set -euo pipefail

VERBOSE=false
debug() { [[ "$VERBOSE" == true ]] && echo "  [DEBUG] $*"; return 0; }

usage() {
  echo "Usage: ./deploy.sh [-c config_file] [-n cluster] [-r] [-v] user@host"
  echo "  -c  config file to upload"
  echo "  -n  cluster name (default: 'default'). Lets multiple stouter clusters"
  echo "      coexist on one host with separate container, config, and build dir."
  echo "  -r  restart only (skip build, just update config and restart container)"
  echo "  -v, --verbose  print debug logs"
  exit 1
}

# Translate long options to short equivalents for getopts
ARGS=()
for arg in "$@"; do
  case "$arg" in
    --verbose) ARGS+=("-v") ;;
    --help)    ARGS+=("-h") ;;
    *)         ARGS+=("$arg") ;;
  esac
done
set -- "${ARGS[@]}"

CONFIG_FILE=""
RESTART_ONLY=false
CLUSTER="default"
while getopts "c:n:rvh" opt; do
  case $opt in
    c) CONFIG_FILE="$OPTARG" ;;
    n) CLUSTER="$OPTARG" ;;
    r) RESTART_ONLY=true ;;
    v) VERBOSE=true ;;
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

# Prompt for the sudo password once. It reaches sudo on stdin, never on a
# command line, where `ps` (on either host) and shell history would show it.
read -rsp "[sudo] password for $REMOTE_HOST: " SUDO_PASS
echo

# Quote a string for a POSIX shell.
shq() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

# Run a shell script as root on the remote host. The script runs with stdin
# closed, so nothing in it can read the password left there.
remote_sudo() {
  printf '%s\n' "$SUDO_PASS" | ssh "$REMOTE_HOST" "sudo -S -p '' sh -c $(shq "exec </dev/null; $1")"
}

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Syncing source to $REMOTE_HOST:$REMOTE_DIR/build/"
  debug "Creating remote directory $REMOTE_DIR/build/"
  # sudo sets SUDO_UID/SUDO_GID to the ssh user's ids.
  remote_sudo "mkdir -p $REMOTE_DIR/build && chown \"\$SUDO_UID:\$SUDO_GID\" $REMOTE_DIR/build"
  debug "Cleaning remote build directory"
  remote_sudo "rm -rf $REMOTE_DIR/build/*"
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
  if remote_sudo "test -f $REMOTE_DIR/config/stouter.json" >/dev/null 2>&1; then
    echo "==> Config already exists at $REMOTE_DIR/config/stouter.json, skipping upload"
  else
    echo "==> Uploading config: $CONFIG_FILE"
    debug "Creating remote config directory $REMOTE_DIR/config"
    remote_sudo "mkdir -p $REMOTE_DIR/config"
    debug "SCP $CONFIG_FILE -> $REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
    scp "$CONFIG_FILE" "$REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
    echo "==> Deploying stouter.json"
    debug "Copying config to $REMOTE_DIR/config/stouter.json"
    remote_sudo "cp $REMOTE_DIR/build/stouter.json $REMOTE_DIR/config/stouter.json"
  fi
else
  debug "No config file specified, skipping config upload"
fi

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Building Docker image on remote"
  debug "Running: docker build -t stouter:latest in $REMOTE_DIR/build"
  remote_sudo "cd $REMOTE_DIR/build && docker build -t stouter:latest ."
  debug "Docker build complete"

  echo "==> Copying stouter binary from Docker image to host"
  debug "Extracting /usr/local/bin/stouter from stouter:latest image"
  remote_sudo "
    docker create --name stouter-extract stouter:latest
    docker cp stouter-extract:/usr/local/bin/stouter /usr/local/bin/stouter
    docker rm stouter-extract
  "
  debug "Host binary installed to /usr/local/bin/stouter"
else
  debug "Restart-only mode, skipping build"
fi

echo "==> Restarting container"
debug "Removing existing container if present: $CONTAINER_NAME"
# Give in-flight connections stouter's full 30s drain (Docker's default is 10s).
remote_sudo "
  if docker inspect $CONTAINER_NAME >/dev/null 2>&1; then
    docker stop -t 30 $CONTAINER_NAME >/dev/null
    docker rm $CONTAINER_NAME >/dev/null
  fi
" || true
debug "Starting new container: $CONTAINER_NAME (--network host, config=$REMOTE_DIR/config/stouter.json)"
remote_sudo "
  docker run -d \
    --name $CONTAINER_NAME \
    --restart unless-stopped \
    --stop-timeout 30 \
    --network host \
    -v $REMOTE_DIR/config:$REMOTE_DIR/config \
    stouter:latest \
    --config $REMOTE_DIR/config/stouter.json
"

echo "==> Deployed. Checking status..."
CONTAINER_STATUS=$(remote_sudo "docker ps --filter name=^${CONTAINER_NAME}\$ --format '{{.Status}}'")
echo "$CONTAINER_STATUS"
debug "Container status: ${CONTAINER_STATUS:-<not running>}"
debug "Deploy finished at $(date '+%Y-%m-%d %H:%M:%S')"
