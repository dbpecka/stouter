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

# The cluster name becomes part of remote paths (and of `rm -rf`) and the
# container's name: letters, digits, '-' and '_' only. "eu west" would
# otherwise have the remote shell delete all of /srv/stouter/eu.
if [[ ! "$CLUSTER" =~ ^[A-Za-z0-9][A-Za-z0-9_-]*$ ]]; then
  echo "error: cluster name '$CLUSTER' may contain only letters, digits, '-' and '_'" >&2
  exit 1
fi

# The config file is named relative to where we were run from; everything
# else (the source to upload) is the repository this script is in, wherever
# it is run from.
if [[ -n "$CONFIG_FILE" ]]; then
  [[ -f "$CONFIG_FILE" ]] || { echo "error: config file '$CONFIG_FILE' not found" >&2; exit 1; }
  CONFIG_FILE="$(cd "$(dirname "$CONFIG_FILE")" && pwd)/$(basename "$CONFIG_FILE")"
fi
cd "$(dirname "${BASH_SOURCE[0]}")"
[[ -f Cargo.toml && -f Dockerfile ]] || { echo "error: $(pwd) isn't the stouter repository" >&2; exit 1; }

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

# The remote directory, quoted for the remote shell.
RD=$(shq "$REMOTE_DIR")

# Run a shell script as root on the remote host. The script runs with stdin
# closed, so nothing in it can read the password left there.
remote_sudo() {
  printf '%s\n' "$SUDO_PASS" | ssh "$REMOTE_HOST" "sudo -S -p '' sh -c $(shq "exec </dev/null; $1")"
}

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Syncing source to $REMOTE_HOST:$REMOTE_DIR/build/"
  debug "Creating remote directory $REMOTE_DIR/build/"
  # sudo sets SUDO_UID/SUDO_GID to the ssh user's ids.
  remote_sudo "mkdir -p $RD/build && chown \"\$SUDO_UID:\$SUDO_GID\" $RD/build"
  debug "Cleaning remote build directory"
  remote_sudo "rm -rf $RD/build/*"
  debug "Running scp (excluding target/, .git/, .idea/, *.json)"
  mapfile -t SOURCES < <(find . -maxdepth 1 \
    ! -name '.' \
    ! -name 'target' \
    ! -name '.git' \
    ! -name '.idea' \
    ! -name '*.json' \
    -print)
  scp -r "${SOURCES[@]}" "$REMOTE_HOST:$REMOTE_DIR/build/"
  debug "SCP complete"
fi

if [[ -n "$CONFIG_FILE" ]]; then
  # Only upload config if it doesn't already exist on the remote
  if remote_sudo "test -f $RD/config/stouter.json" >/dev/null 2>&1; then
    echo "==> Config already exists at $REMOTE_DIR/config/stouter.json, skipping upload"
  else
    echo "==> Uploading config: $CONFIG_FILE"
    debug "Creating remote config directory $REMOTE_DIR/config"
    remote_sudo "mkdir -p $RD/config"
    debug "SCP $CONFIG_FILE -> $REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
    scp "$CONFIG_FILE" "$REMOTE_HOST:$REMOTE_DIR/build/stouter.json"
    echo "==> Deploying stouter.json"
    debug "Copying config to $REMOTE_DIR/config/stouter.json"
    remote_sudo "cp $RD/build/stouter.json $RD/config/stouter.json"
  fi
else
  debug "No config file specified, skipping config upload"
fi

if [[ "$RESTART_ONLY" == false ]]; then
  echo "==> Building Docker image on remote"
  debug "Running: docker build -t stouter:latest in $REMOTE_DIR/build"
  remote_sudo "cd $RD/build && docker build -t stouter:latest ."
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
# The config directory is mounted into the container, and so is the
# config's tlsDir if it lies elsewhere: otherwise the CA node's CA (and
# every node's certificates) would live only in the container, and the next
# deploy, replacing the container, would make a new CA no other node trusts.
remote_sudo "
  D=$RD
  set -- -v \"\$D/config:\$D/config\"
  TLS_DIR=\$(sed -n 's/.*\"tlsDir\"[[:space:]]*:[[:space:]]*\"\\([^\"]*\\)\".*/\\1/p' \"\$D/config/stouter.json\" | head -n 1)
  case \"\$TLS_DIR\" in
    '') ;;
    \"\$D/config\"/*) ;;
    /*) mkdir -p \"\$TLS_DIR\" && set -- \"\$@\" -v \"\$TLS_DIR:\$TLS_DIR\" ;;
    *) echo \"warning: tlsDir '\$TLS_DIR' is relative, so it is inside the container and lost when it is replaced\" >&2 ;;
  esac
  docker run -d \
    --name $CONTAINER_NAME \
    --restart unless-stopped \
    --stop-timeout 30 \
    --network host \
    \"\$@\" \
    stouter:latest \
    --config \"\$D/config/stouter.json\"
"

echo "==> Deployed. Checking status..."
CONTAINER_STATUS=$(remote_sudo "docker ps --filter name=^${CONTAINER_NAME}\$ --format '{{.Status}}'")
echo "$CONTAINER_STATUS"
debug "Container status: ${CONTAINER_STATUS:-<not running>}"
debug "Deploy finished at $(date '+%Y-%m-%d %H:%M:%S')"
