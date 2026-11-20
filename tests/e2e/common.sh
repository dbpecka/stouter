# Shared helpers for the end-to-end tests. Source it; set BIN first.
set -euo pipefail

WORK=$(mktemp -d "${TMPDIR:-/tmp}/stouter-e2e.XXXXXX")
PIDS=()
FAILED=0

cleanup() {
  for pid in "${PIDS[@]}"; do kill "$pid" 2>/dev/null || true; done
  wait 2>/dev/null || true
  if declare -F extra_cleanup > /dev/null; then extra_cleanup; fi
  if [[ $FAILED -ne 0 && -n "${KEEP_LOGS:-}" ]]; then echo "logs kept in $WORK"; else rm -rf "$WORK"; fi
}
trap cleanup EXIT

pass() { echo "PASS: $*"; }
fail() { echo "FAIL: $*"; FAILED=1; }
check() { local what=$1; shift; if "$@"; then pass "$what"; else fail "$what"; fi; }

# start NAME CONFIG [prefix command...]: run the daemon in the background,
# logging to $WORK/NAME.log.
start() {
  local name=$1 config=$2; shift 2
  RUST_LOG=${RUST_LOG:-info} "$@" "$BIN" --config "$config" > "$WORK/$name.log" 2>&1 &
  PIDS+=($!)
}

# wait_for SECONDS COMMAND...: retry COMMAND every half second until it
# succeeds or SECONDS pass.
wait_for() {
  local deadline=$(( $(date +%s) + $1 )); shift
  until "$@" > /dev/null 2>&1; do
    (( $(date +%s) < deadline )) || return 1
    sleep 0.5
  done
}

# log_has NAME TEXT: the daemon's log mentions TEXT (colours stripped).
log_has() { sed 's/\x1b\[[0-9;]*m//g' "$WORK/$1.log" | grep -q -- "$2"; }

finish() {
  if [[ $FAILED -ne 0 ]]; then echo "some checks failed"; exit 1; fi
  echo "all checks passed"
}
