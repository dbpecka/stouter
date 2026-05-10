#!/usr/bin/env bash
# Imports all Stouter Elastic Stack artifacts into a running Kibana instance.
#
# Required environment variables:
#   KIBANA_URL       Base URL of Kibana, e.g. https://kibana.example.com:5601
#   KIBANA_USER      Username for HTTP Basic auth
#   KIBANA_PASSWORD  Password for HTTP Basic auth
#
# Optional:
#   KIBANA_SPACE     Kibana space to import into (default: "default")
#
# Usage:
#   KIBANA_URL=https://kibana:5601 KIBANA_USER=elastic KIBANA_PASSWORD=changeme ./import.sh

set -euo pipefail

: "${KIBANA_URL:?KIBANA_URL must be set (e.g. https://kibana.example.com:5601)}"
: "${KIBANA_USER:?KIBANA_USER must be set}"
: "${KIBANA_PASSWORD:?KIBANA_PASSWORD must be set}"
KIBANA_SPACE="${KIBANA_SPACE:-default}"

command -v curl >/dev/null 2>&1 || { echo "ERROR: curl is required"; exit 1; }
command -v jq   >/dev/null 2>&1 || { echo "ERROR: jq is required"; exit 1; }

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BASIC_AUTH="$(printf '%s:%s' "$KIBANA_USER" "$KIBANA_PASSWORD" | base64 | tr -d '\n')"

if [[ "$KIBANA_SPACE" == "default" ]]; then
  SPACE_PREFIX=""
else
  SPACE_PREFIX="/s/${KIBANA_SPACE}"
fi

kibana_req() {
  local method="$1"; shift
  local path="$1"; shift
  curl -sS \
    -H "kbn-xsrf: true" \
    -H "Authorization: Basic $BASIC_AUTH" \
    -X "$method" \
    "${KIBANA_URL}${SPACE_PREFIX}${path}" \
    "$@"
}

# ── Saved objects (data views, visualizations, dashboards) ──────────────────

echo "==> Importing saved objects..."
result=$(kibana_req POST "/api/saved_objects/_import?overwrite=true" \
  -F "file=@${SCRIPT_DIR}/saved-objects.ndjson")

if echo "$result" | jq -e '.success == true' >/dev/null 2>&1; then
  count=$(echo "$result" | jq -r '.successCount')
  echo "    OK — imported ${count} objects"
elif echo "$result" | jq -e 'has("errors")' >/dev/null 2>&1; then
  errors=$(echo "$result" | jq -r '.errors | length')
  echo "    WARN — completed with ${errors} error(s):"
  echo "$result" | jq -r '.errors[] | "      [\(.type)/\(.id)] \(.error.message)"'
else
  echo "    ERROR — unexpected response from Kibana:"
  echo "$result" | sed 's/^/      /'
  exit 1
fi
echo ""

# ── Alert rules ──────────────────────────────────────────────────────────────

echo "==> Creating alert rules..."
any_error=0
for rule_file in "${SCRIPT_DIR}/alert-rules/"*.json; do
  name=$(jq -r .name "$rule_file")
  printf "    %-50s " "${name} ..."

  response=$(kibana_req POST "/api/alerting/rule" \
    -H "Content-Type: application/json" \
    -d "@${rule_file}" \
    -w "\n%{http_code}")

  http_code=$(printf '%s' "$response" | tail -1)
  body=$(printf '%s' "$response" | head -n -1)

  if [[ "$http_code" =~ ^2 ]]; then
    rule_id=$(printf '%s' "$body" | jq -r '.id // "unknown"')
    echo "OK (id: ${rule_id})"
  else
    msg=$(printf '%s' "$body" | jq -r '.message // .error // "HTTP '"$http_code"'"' 2>/dev/null || echo "HTTP $http_code")
    echo "ERROR — ${msg}"
    any_error=1
  fi
done

echo ""
if [[ $any_error -eq 0 ]]; then
  echo "==> Done."
else
  echo "==> Done (with errors — check output above)."
fi
echo "    Dashboards: ${KIBANA_URL}${SPACE_PREFIX}/app/dashboards"
