#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${KHEISH_BIN:-"$ROOT/target/debug/kheish-daemon"}"
MODEL="${KHEISH_ROUTES_CANARY_MODEL:-gpt-5.4}"
SUCCESS_TIMEOUT_MS="${KHEISH_ROUTES_CANARY_SUCCESS_TIMEOUT_MS:-120000}"
FAIL_TIMEOUT_MS="${KHEISH_ROUTES_CANARY_FAIL_TIMEOUT_MS:-30000}"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "$ROOT/.env"
  set +a
fi

if [[ -z "${OPENAI_API_KEY:-}" && -n "${KHEISH_OPENAI_API_KEY:-}" ]]; then
  OPENAI_API_KEY="$KHEISH_OPENAI_API_KEY"
  export OPENAI_API_KEY
fi

if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY or KHEISH_OPENAI_API_KEY is required for the live routes canary matrix." >&2
  exit 2
fi

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_ROUTES_CANARY_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_ROUTES_CANARY_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/routes-canary-live-matrix.XXXXXX")"
STATE="$TMP/state"
WORKSPACE="$TMP/workspace"
ROUTES="$TMP/routes.toml"
MASTER_KEY_FILE="$TMP/auth-store-master-key"
DAEMON_PID=""
BASE_URL=""

cleanup() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

stop_daemon() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
  DAEMON_PID=""
}

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

kheish() {
  "$BIN" --base-url "$BASE_URL" --output json "$@"
}

with_master_key_file() {
  env -u KHEISH_AUTH_STORE_MASTER_KEY \
    KHEISH_AUTH_STORE_MASTER_KEY_FILE="$MASTER_KEY_FILE" \
    "$@"
}

start_daemon() {
  local default_route="$1"
  local log_file="$2"
  DAEMON_PID=""
  env -u KHEISH_AUTH_STORE_MASTER_KEY \
    KHEISH_AUTH_STORE_MASTER_KEY_FILE="$MASTER_KEY_FILE" \
    "$BIN" serve \
      --bind "127.0.0.1:$PORT" \
      --state-root "$STATE" \
      --workspace-root "$WORKSPACE" \
      --routes-file "$ROUTES" \
      --default-route "$default_route" \
      --mcp-discovery disabled \
      >"$log_file" 2>&1 &
  DAEMON_PID="$!"
  wait_http_ok "$BASE_URL/readyz"
}

capture_success() {
  local name="$1"
  shift
  "$@" >"$TMP/$name.json" 2>"$TMP/$name.stderr"
}

capture_failure() {
  local name="$1"
  shift
  set +e
  "$@" >"$TMP/$name.json" 2>"$TMP/$name.stderr"
  local status=$?
  set -e
  if [[ "$status" -eq 0 ]]; then
    echo "expected command to fail: $*" >&2
    exit 1
  fi
  python3 -m json.tool "$TMP/$name.json" >/dev/null
}

mkdir -p "$STATE" "$WORKSPACE"
"$BIN" secrets generate >"$MASTER_KEY_FILE"
chmod 600 "$MASTER_KEY_FILE"

with_master_key_file "$BIN" secrets set openai.prod \
  --offline \
  --state-root "$STATE" \
  --provider openai \
  --from-env OPENAI_API_KEY \
  --output json >"$TMP/secret-set.json"

DEAD_PORT="$(free_port)"
cat >"$ROUTES" <<TOML
version = 1
default_route = "openai-ok"

[routes.openai-ok]
driver = "openai"
default_model = "$MODEL"
auth_ref = "openai.prod"

[routes.openai-spare]
driver = "openai"
default_model = "$MODEL"
auth_ref = "openai.prod"

[routes.openai-invalid-model]
driver = "openai"
default_model = "kheish-invalid-live-model-20260503"
model_support = "any"
auth_ref = "openai.prod"

[routes.openai-dead-base-url]
driver = "openai"
default_model = "$MODEL"
auth_ref = "openai.prod"
base_url = "http://127.0.0.1:$DEAD_PORT/v1/responses"
TOML

PORT="$(free_port)"
BASE_URL="http://127.0.0.1:$PORT"
start_daemon openai-ok "$TMP/daemon.log"

capture_success auth-check kheish doctor routes --check-auth
capture_success canary-ok kheish doctor routes --route openai-ok --canary --canary-timeout-ms "$SUCCESS_TIMEOUT_MS"
capture_failure canary-invalid-model kheish doctor routes --route openai-invalid-model --canary --canary-timeout-ms "$FAIL_TIMEOUT_MS"
capture_failure canary-dead-base-url kheish doctor routes --route openai-dead-base-url --canary --canary-timeout-ms "$FAIL_TIMEOUT_MS"

REFERENCE_SESSION="route-reference-live"
NEXT_AT="$(
  python3 - <<'PY'
from datetime import datetime, timedelta, timezone
print((datetime.now(timezone.utc) + timedelta(minutes=30)).isoformat())
PY
)"
capture_success session-create kheish sessions create "$REFERENCE_SESSION"
capture_success session-route kheish sessions set-route "$REFERENCE_SESSION" --provider openai-ok --model "$MODEL"
capture_success schedule-create kheish schedules create route-reference-live-schedule "$REFERENCE_SESSION" "do not dispatch" --at "$NEXT_AT" --provider openai-ok --model "$MODEL"

stop_daemon
cat >"$ROUTES" <<TOML
version = 1
default_route = "openai-spare"

[routes.openai-spare]
driver = "openai"
default_model = "$MODEL"
auth_ref = "openai.prod"
TOML

start_daemon openai-spare "$TMP/daemon-restarted.log"
capture_failure route-references kheish doctor routes --check-references

python3 - "$TMP" <<'PY'
import json
import os
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
secret = os.environ["OPENAI_API_KEY"]

def load(name):
    with (root / f"{name}.json").open() as handle:
        return json.load(handle)

auth = load("auth-check")
assert auth["ok"] is True, auth

ok = load("canary-ok")
assert ok["ok"] is True, ok
assert ok["canaries"][0]["route_id"] == "openai-ok", ok
assert ok["canaries"][0]["status"] == "passed", ok
assert ok["canaries"][0].get("run_id"), ok

invalid = load("canary-invalid-model")
assert invalid["ok"] is False, invalid
assert invalid["canaries"][0]["route_id"] == "openai-invalid-model", invalid
assert invalid["canaries"][0]["status"] == "failed", invalid
assert any(
    item.get("code") == "route_canary_failed"
    and item.get("route_id") == "openai-invalid-model"
    for item in invalid.get("diagnostics", [])
), invalid

dead = load("canary-dead-base-url")
assert dead["ok"] is False, dead
assert dead["canaries"][0]["route_id"] == "openai-dead-base-url", dead
assert dead["canaries"][0]["status"] in {"failed", "timeout"}, dead
assert any(
    item.get("code") in {"route_canary_failed", "route_canary_timeout"}
    and item.get("route_id") == "openai-dead-base-url"
    for item in dead.get("diagnostics", [])
), dead

references = load("route-references")
assert references["ok"] is False, references
assert references["reference_checked"] is True, references
codes = {
    (item.get("code"), item.get("route_id"))
    for item in references.get("diagnostics", [])
}
assert ("stale_session_route_policy", "openai-ok") in codes, references
assert ("stale_schedule_route", "openai-ok") in codes, references

for name in [
    "auth-check",
    "canary-ok",
    "canary-invalid-model",
    "canary-dead-base-url",
    "route-references",
]:
    rendered = json.dumps(load(name), sort_keys=True)
    assert secret not in rendered, f"{name} leaked OPENAI_API_KEY"
PY

if grep -R --fixed-strings --quiet "$OPENAI_API_KEY" "$TMP"; then
  echo "routes canary live matrix leaked OPENAI_API_KEY into evidence: $TMP" >&2
  exit 1
fi

printf 'routes canary live matrix passed: %s\n' "$TMP"
