#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${KHEISH_BIN:-"$ROOT/target/debug/kheish-daemon"}"

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPS_SMOKE_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPS_SMOKE_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/kheish-ops-smoke.XXXXXX")"
SECRET_DIR="$(mktemp -d "$WORK_BASE/kheish-ops-smoke-secrets.XXXXXX")"
STATE="$TMP/state"
RESTORED_STATE="$TMP/restored-state"
WORKSPACE="$TMP/workspace"
RESTORED_WORKSPACE="$TMP/restored-workspace"
BACKUP="$SECRET_DIR/state-backup.tar.gz"
WORKSPACE_BACKUP="$SECRET_DIR/workspace-backup.tar.gz"
ROUTES="$TMP/routes.toml"
MASTER_KEY_FILE="$SECRET_DIR/auth-store-master-key"
ADMIN_TOKEN_FILE="$SECRET_DIR/admin-token"
READONLY_TOKEN_FILE="$SECRET_DIR/readonly-token"
INITIAL_ROUTE_SECRET="sk-ops-smoke-initial"
ROTATED_ROUTE_SECRET="sk-ops-smoke-rotated"
WORKSPACE_SENTINEL_REL="ops-workspace/sentinel.txt"
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
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

wait_http_ok() {
  local url="$1"
  python3 - "$url" <<'PY'
import sys
import time
import urllib.error
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
  local state_root="$1"
  local workspace_root="$2"
  local port="$3"
  local log_file="$4"
  DAEMON_PID=""
  KHEISH_AUTH_STORE_MASTER_KEY_FILE="$MASTER_KEY_FILE" \
    "$BIN" serve \
      --bind "127.0.0.1:$port" \
      --state-root "$state_root" \
      --workspace-root "$workspace_root" \
      --routes-file "$ROUTES" \
      --mcp-discovery disabled \
      --http-auth-mode bearer \
      --http-admin-token-file "$ADMIN_TOKEN_FILE" \
      --http-readonly-token-file "$READONLY_TOKEN_FILE" \
      >"$log_file" 2>&1 &
  DAEMON_PID="$!"
  wait_http_ok "http://127.0.0.1:$port/readyz"
}

stop_daemon() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID"
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  DAEMON_PID=""
}

cli() {
  local base_url="$1"
  shift
  "$BIN" \
    --base-url "$base_url" \
    --token-file "$ADMIN_TOKEN_FILE" \
    --output json \
    "$@" >/dev/null
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

sha256_file() {
  python3 - "$1" <<'PY'
import hashlib
import sys

digest = hashlib.sha256()
with open(sys.argv[1], "rb") as handle:
    for chunk in iter(lambda: handle.read(1024 * 1024), b""):
        digest.update(chunk)
print(digest.hexdigest())
PY
}

assert_no_route_secret_leaks() {
  local secret
  for secret in "$INITIAL_ROUTE_SECRET" "$ROTATED_ROUTE_SECRET"; do
    if grep -R --binary-files=without-match -F "$secret" "$TMP" >/dev/null 2>&1; then
      echo "route secret leaked into ops backup/restore smoke artifacts under $TMP" >&2
      return 1
    fi
  done
  if find "$TMP" -type f \( \
    -name admin-token -o \
    -name readonly-token -o \
    -name auth-store-master-key -o \
    -name state-backup.tar.gz -o \
    -name workspace-backup.tar.gz \
  \) -print -quit | grep -q .; then
    echo "local token, master-key, or backup artifact remained in evidence dir $TMP" >&2
    return 1
  fi
}

assert_tar_lacks_member() {
  local archive="$1"
  local member="$2"
  if tar -tzf "$archive" | sed 's#^\./##' | grep -Fx "$member" >/dev/null; then
    echo "$archive unexpectedly contains workspace member $member" >&2
    return 1
  fi
}

mkdir -p "$STATE" "$RESTORED_STATE" "$WORKSPACE" "$RESTORED_WORKSPACE"
"$BIN" secrets generate >"$MASTER_KEY_FILE"
printf 'admin-token-%s\n' "$RANDOM" >"$ADMIN_TOKEN_FILE"
printf 'readonly-token-%s\n' "$RANDOM" >"$READONLY_TOKEN_FILE"
chmod 600 "$MASTER_KEY_FILE" "$ADMIN_TOKEN_FILE" "$READONLY_TOKEN_FILE"
mkdir -p "$WORKSPACE/$(dirname "$WORKSPACE_SENTINEL_REL")"
printf 'workspace-backup-smoke:%s\n' "$TMP" >"$WORKSPACE/$WORKSPACE_SENTINEL_REL"
WORKSPACE_SENTINEL_SHA="$(sha256_file "$WORKSPACE/$WORKSPACE_SENTINEL_REL")"
python3 "$ROOT/scripts/e2e/verify_runbook_commands.py" \
  "$ROOT/docs/operators/production-runbooks.mdx" >/dev/null

cat >"$ROUTES" <<'TOML'
version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "gpt-5.4"
auth_ref = "openai.prod"
TOML

KHEISH_AUTH_STORE_MASTER_KEY_FILE="$MASTER_KEY_FILE" \
  "$BIN" secrets set openai.prod \
    --offline \
    --state-root "$STATE" \
    --provider openai \
    --value "$INITIAL_ROUTE_SECRET" \
    --output json >/dev/null

PORT="$(free_port)"
BASE_URL="http://127.0.0.1:$PORT"
start_daemon "$STATE" "$WORKSPACE" "$PORT" "$TMP/daemon.log"

capture_json "$TMP/status.json" "$BASE_URL" status
json_expect "$TMP/status.json" "data.get('health', {}).get('ok') is True" "initial status health ok"
json_expect "$TMP/status.json" "data.get('storage', {}).get('ok') is True" "initial storage ok"
json_expect "$TMP/status.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0" \
  "initial provider readiness ok"
json_expect "$TMP/status.json" \
  "data.get('delivery', {}).get('unresolved_dead_lettered', 0) == 0" \
  "initial delivery DLQ clean"
capture_json "$TMP/runtime.json" "$BASE_URL" runtime get
capture_json "$TMP/doctor.json" "$BASE_URL" doctor
capture_json "$TMP/doctor-routes-auth.json" "$BASE_URL" doctor routes --routes-file "$ROUTES" --default-route openai --check-auth
capture_json "$TMP/doctor-routes-references.json" "$BASE_URL" doctor routes --check-references
cli "$BASE_URL" secrets list
cli "$BASE_URL" secrets get openai.prod

cli "$BASE_URL" secrets set openai.prod --provider openai --value "$ROTATED_ROUTE_SECRET"
capture_json "$TMP/revoke-slot.json" "$BASE_URL" runtime auth revoke-slot openai.prod
json_expect "$TMP/revoke-slot.json" \
  "data.get('slot_id') == 'openai.prod' and data.get('revoked_leases', -1) >= 0" \
  "route slot revocation returned stable shape"
capture_json "$TMP/status-after-route-secret-rotation.json" "$BASE_URL" status
json_expect "$TMP/status-after-route-secret-rotation.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0" \
  "provider readiness ok after route secret rotation"
capture_json "$TMP/doctor-routes-after-rotation.json" "$BASE_URL" doctor routes --check-auth

stop_daemon
[[ -s "$STATE/audit-signing.key" ]]
AUDIT_SIGNING_SHA="$(sha256_file "$STATE/audit-signing.key")"
tar -C "$STATE" -czf "$BACKUP" .
assert_tar_lacks_member "$BACKUP" "$WORKSPACE_SENTINEL_REL"
tar -C "$WORKSPACE" -czf "$WORKSPACE_BACKUP" .
tar -C "$RESTORED_STATE" -xzf "$BACKUP"
tar -C "$RESTORED_WORKSPACE" -xzf "$WORKSPACE_BACKUP"

RESTORED_PORT="$(free_port)"
RESTORED_BASE_URL="http://127.0.0.1:$RESTORED_PORT"
start_daemon "$RESTORED_STATE" "$RESTORED_WORKSPACE" "$RESTORED_PORT" "$TMP/restored-daemon.log"

capture_json "$TMP/restored-status.json" "$RESTORED_BASE_URL" status
json_expect "$TMP/restored-status.json" "data.get('health', {}).get('ok') is True" "restored status health ok"
json_expect "$TMP/restored-status.json" "data.get('storage', {}).get('ok') is True" "restored storage ok"
json_expect "$TMP/restored-status.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0" \
  "restored provider readiness ok"
capture_json "$TMP/restored-runtime.json" "$RESTORED_BASE_URL" runtime get
capture_json "$TMP/restored-routes-auth.json" "$RESTORED_BASE_URL" doctor routes --routes-file "$ROUTES" --default-route openai --check-auth
capture_json "$TMP/restored-routes-references.json" "$RESTORED_BASE_URL" doctor routes --check-references
cli "$RESTORED_BASE_URL" secrets get openai.prod
[[ "$(sha256_file "$RESTORED_STATE/audit-signing.key")" == "$AUDIT_SIGNING_SHA" ]]
[[ "$(sha256_file "$RESTORED_WORKSPACE/$WORKSPACE_SENTINEL_REL")" == "$WORKSPACE_SENTINEL_SHA" ]]
assert_no_route_secret_leaks

printf 'ops backup/restore smoke passed: %s\n' "$TMP"
