#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${KHEISH_BIN:-"$ROOT/target/debug/kheish-daemon"}"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi
unset KHEISH_AUTH_STORE_MASTER_KEY
unset KHEISH_AUTH_STORE_MASTER_KEY_FILE

OPENAI_API_KEY="${OPENAI_API_KEY:-${KHEISH_OPENAI_API_KEY:-}}"
export OPENAI_API_KEY
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY or KHEISH_OPENAI_API_KEY is required for ops_slo_probe_smoke.sh" >&2
  exit 2
fi

MODEL="${KHEISH_OPS_SLO_MODEL:-gpt-5.4}"

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPS_SLO_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPS_SLO_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/kheish-ops-slo.XXXXXX")"
SECRET_DIR="$(mktemp -d "$WORK_BASE/kheish-ops-slo-secrets.XXXXXX")"
STATE="$TMP/state"
WORKSPACE="$TMP/workspace"
ROUTES="$TMP/routes.toml"
MASTER_KEY_FILE="$SECRET_DIR/auth-store-master-key"
ADMIN_TOKEN_FILE="$SECRET_DIR/admin-token"
READONLY_TOKEN_FILE="$SECRET_DIR/readonly-token"
DAEMON_PID=""

cleanup() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  rm -rf "$SECRET_DIR"
}
trap cleanup EXIT

free_port() {
  python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

wait_http_ok() {
  local url="$1"
  python3 - "$url" <<'PY'
import sys
import time
import urllib.request

url = sys.argv[1]
deadline = time.time() + 30
last = None
while time.time() < deadline:
    try:
        with urllib.request.urlopen(url, timeout=2) as response:
            if 200 <= response.status < 300:
                raise SystemExit(0)
            last = f"status={response.status}"
    except Exception as exc:
        last = str(exc)
    time.sleep(0.2)
raise SystemExit(f"timed out waiting for {url}: {last}")
PY
}

start_daemon() {
  local port="$1"
  local log_file="$2"
  DAEMON_PID=""
  KHEISH_AUTH_STORE_MASTER_KEY_FILE="$MASTER_KEY_FILE" \
    "$BIN" serve \
      --bind "127.0.0.1:$port" \
      --state-root "$STATE" \
      --workspace-root "$WORKSPACE" \
      --routes-file "$ROUTES" \
      --mcp-discovery disabled \
      --http-auth-mode bearer \
      --http-admin-token-file "$ADMIN_TOKEN_FILE" \
      --http-readonly-token-file "$READONLY_TOKEN_FILE" \
      >"$log_file" 2>&1 &
  DAEMON_PID="$!"
  wait_http_ok "http://127.0.0.1:$port/readyz"
}

cli_json() {
  local base_url="$1"
  shift
  "$BIN" \
    --base-url "$base_url" \
    --token-file "$ADMIN_TOKEN_FILE" \
    --output json \
    "$@"
}

capture_json() {
  local out_file="$1"
  local base_url="$2"
  shift 2
  cli_json "$base_url" "$@" >"$out_file"
}

json_expect() {
  local file="$1"
  local expression="$2"
  local description="$3"
  python3 - "$file" "$expression" "$description" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    data = json.load(handle)
if not eval(sys.argv[2], {"data": data, "any": any, "all": all, "len": len}, {}):
    raise SystemExit(f"assertion failed: {sys.argv[3]}")
PY
}

assert_no_secret_leaks() {
  if grep -R --binary-files=without-match -F "$OPENAI_API_KEY" "$TMP" >/dev/null 2>&1; then
    echo "OPENAI_API_KEY leaked into SLO probe artifacts under $TMP" >&2
    return 1
  fi
  while IFS='=' read -r name value; do
    case "$name" in
      *API_KEY*|*TOKEN*|*SECRET*|*PASSWORD*|*MASTER_KEY*) ;;
      *) continue ;;
    esac
    [[ ${#value} -lt 8 ]] && continue
    if grep -R --binary-files=without-match -F "$value" "$TMP" >/dev/null 2>&1; then
      echo "environment secret $name leaked into SLO probe artifacts under $TMP" >&2
      return 1
    fi
  done < <(env)
  if find "$TMP" -type f \( \
    -name admin-token -o \
    -name readonly-token -o \
    -name auth-store-master-key \
  \) -print -quit | grep -q .; then
    echo "local token or master-key artifact remained in evidence dir $TMP" >&2
    return 1
  fi
}

mkdir -p "$STATE" "$WORKSPACE"
"$BIN" secrets generate >"$MASTER_KEY_FILE"
printf 'admin-token-slo-%s-%s\n' "$$" "$RANDOM" >"$ADMIN_TOKEN_FILE"
printf 'readonly-token-slo-%s-%s\n' "$$" "$RANDOM" >"$READONLY_TOKEN_FILE"
chmod 600 "$MASTER_KEY_FILE" "$ADMIN_TOKEN_FILE" "$READONLY_TOKEN_FILE"

cat >"$ROUTES" <<TOML
version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "$MODEL"
auth_ref = "openai.prod"
TOML

KHEISH_AUTH_STORE_MASTER_KEY_FILE="$MASTER_KEY_FILE" \
  "$BIN" --output json secrets set openai.prod \
    --offline \
    --state-root "$STATE" \
    --provider openai \
    --from-env OPENAI_API_KEY >"$TMP/secret-set.json"

PORT="$(free_port)"
BASE_URL="http://127.0.0.1:$PORT"
start_daemon "$PORT" "$TMP/daemon.log"

wait_http_ok "$BASE_URL/readyz"
capture_json "$TMP/status.json" "$BASE_URL" status
capture_json "$TMP/doctor.json" "$BASE_URL" doctor
capture_json "$TMP/doctor-routes-auth.json" "$BASE_URL" doctor routes --check-auth

json_expect "$TMP/status.json" "data.get('health', {}).get('ok') is True" "SLO health path ok"
json_expect "$TMP/status.json" "data.get('storage', {}).get('ok') is True" "SLO storage path ok"
json_expect "$TMP/status.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0 and data.get('provider_readiness', {}).get('route_count', 0) >= 1" \
  "SLO provider readiness path ok"
json_expect "$TMP/status.json" \
  "data.get('runs', {}).get('waiting_for_approval') == 0 and data.get('runs', {}).get('waiting_for_user_question') == 0 and data.get('runs', {}).get('running') == 0" \
  "SLO run backlog paths ok"
json_expect "$TMP/status.json" \
  "data.get('delivery', {}).get('unresolved_dead_lettered', 0) == 0" \
  "SLO delivery DLQ path ok"
json_expect "$TMP/status.json" \
  "data.get('runtime', {}).get('debug_level') != 'full'" \
  "SLO debug level path ok"
json_expect "$TMP/status.json" \
  "data.get('control_plane', {}).get('auth_enabled') is True and data.get('control_plane', {}).get('bind_is_loopback') is True" \
  "SLO control-plane posture paths ok"
json_expect "$TMP/doctor-routes-auth.json" \
  "data.get('ok') is True and not data.get('diagnostics', [])" \
  "SLO route auth diagnostic clean"

assert_no_secret_leaks

printf 'ops SLO probe smoke passed: %s\n' "$TMP"
