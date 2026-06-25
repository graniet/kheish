#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${KHEISH_BIN:-"$ROOT/target/debug/kheish-daemon"}"

if [[ -f "$ROOT/.env" ]]; then
  set -a
  source "$ROOT/.env"
  set +a
fi
unset KHEISH_AUTH_STORE_MASTER_KEY
unset KHEISH_AUTH_STORE_MASTER_KEY_FILE

OPENAI_API_KEY="${OPENAI_API_KEY:-${KHEISH_OPENAI_API_KEY:-}}"
export OPENAI_API_KEY
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY or KHEISH_OPENAI_API_KEY is required for ops_runbook_live_smoke.sh" >&2
  exit 2
fi

MODEL="${KHEISH_OPS_LIVE_MODEL:-gpt-5.4}"
CANARY_TIMEOUT_MS="${KHEISH_OPS_CANARY_TIMEOUT_MS:-120000}"

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPS_LIVE_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPS_LIVE_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/kheish-ops-live.XXXXXX")"
SECRET_DIR="$(mktemp -d "$WORK_BASE/kheish-ops-live-secrets.XXXXXX")"
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
OLD_ADMIN_TOKEN_FILE="$SECRET_DIR/old-admin-token"
NEXT_ADMIN_TOKEN_SOURCE="$SECRET_DIR/admin-token.next.source"
WORKSPACE_SENTINEL_REL="ops-workspace/live-sentinel.txt"
DAEMON_PID=""
PROXY_PID=""

cleanup() {
  if [[ -n "${PROXY_PID:-}" ]] && kill -0 "$PROXY_PID" 2>/dev/null; then
    kill "$PROXY_PID" 2>/dev/null || true
    wait "$PROXY_PID" 2>/dev/null || true
  fi
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

start_streaming_proxy() {
  local target="$1"
  local port="$2"
  local log_file="$3"
  PROXY_PID=""
  python3 - "$target" "$port" <<'PY' >"$log_file" 2>&1 &
import http.server
import sys
import urllib.error
import urllib.request

target = sys.argv[1].rstrip("/")
port = int(sys.argv[2])
hop_by_hop = {
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailers",
    "transfer-encoding",
    "upgrade",
    "content-length",
}

class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, fmt, *args):
        return

    def do_GET(self):
        self.forward()

    def do_POST(self):
        self.forward()

    def do_PUT(self):
        self.forward()

    def do_DELETE(self):
        self.forward()

    def do_OPTIONS(self):
        self.forward()

    def forward(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length) if length else None
        request = urllib.request.Request(
            target + self.path,
            data=body,
            method=self.command,
        )
        for name, value in self.headers.items():
            if name.lower() not in hop_by_hop:
                request.add_header(name, value)
        try:
            response = urllib.request.urlopen(request, timeout=3600)
            self.send_response(response.status)
            for name, value in response.headers.items():
                if name.lower() not in hop_by_hop:
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            content_type = response.headers.get("Content-Type", "")
            chunk_size = 1 if content_type.startswith("text/event-stream") else 4096
            while True:
                chunk = response.read(chunk_size)
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                except BrokenPipeError:
                    break
        except urllib.error.HTTPError as error:
            payload = error.read()
            self.send_response(error.code)
            for name, value in error.headers.items():
                if name.lower() not in hop_by_hop:
                    self.send_header(name, value)
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            self.wfile.write(payload)

server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
server.serve_forever()
PY
  PROXY_PID="$!"
  wait_http_ok "http://127.0.0.1:$port/readyz"
}

stop_proxy() {
  if [[ -n "${PROXY_PID:-}" ]] && kill -0 "$PROXY_PID" 2>/dev/null; then
    kill "$PROXY_PID"
    wait "$PROXY_PID" 2>/dev/null || true
  fi
  PROXY_PID=""
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

json_get() {
  local file="$1"
  local path="$2"
  python3 - "$file" "$path" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    value = json.load(handle)
for part in sys.argv[2].split("."):
    value = value[part]
print(value)
PY
}

json_get_optional() {
  local file="$1"
  local path="$2"
  python3 - "$file" "$path" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    value = json.load(handle)
try:
    for part in sys.argv[2].split("."):
        value = value[part]
except Exception:
    raise SystemExit(0)
if value is not None:
    print(value)
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

json_query() {
  local file="$1"
  local expression="$2"
  python3 - "$file" "$expression" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    data = json.load(handle)
value = eval(sys.argv[2], {"data": data, "len": len}, {})
if isinstance(value, (dict, list)):
    print(json.dumps(value, sort_keys=True))
elif value is not None:
    print(value)
PY
}

http_status() {
  local url="$1"
  local token_file="$2"
  python3 - "$url" "$token_file" <<'PY'
import sys
import urllib.error
import urllib.request

with open(sys.argv[2], "r", encoding="utf-8") as handle:
    token = handle.read().strip()
request = urllib.request.Request(
    sys.argv[1],
    headers={"Authorization": f"Bearer {token}"},
)
try:
    with urllib.request.urlopen(request, timeout=5) as response:
        print(response.status)
except urllib.error.HTTPError as error:
    print(error.code)
except Exception:
    print("000")
PY
}

wait_for_http_status() {
  local url="$1"
  local token_file="$2"
  local expected="$3"
  local deadline=$((SECONDS + 30))
  local status=""
  while (( SECONDS < deadline )); do
    status="$(http_status "$url" "$token_file")"
    if [[ "$status" == "$expected" ]]; then
      return 0
    fi
    sleep 0.2
  done
  echo "timed out waiting for $url to return $expected; last status=$status" >&2
  return 1
}

wait_connector_health() {
  local base_url="$1"
  local token="$2"
  python3 - "$base_url" "$token" <<'PY'
import json
import sys
import time
import urllib.error
import urllib.request

base_url = sys.argv[1].rstrip("/")
token = sys.argv[2]
deadline = time.time() + 30
last = None
while time.time() < deadline:
    request = urllib.request.Request(
        f"{base_url}/health",
        headers={"Authorization": f"Bearer {token}"},
    )
    try:
        with urllib.request.urlopen(request, timeout=2) as response:
            payload = json.loads(response.read().decode("utf-8"))
            if response.status == 200 and payload.get("status") == "ready":
                raise SystemExit(0)
            last = {"status": response.status, "payload": payload}
    except Exception as exc:
        last = str(exc)
    time.sleep(0.2)
raise SystemExit(f"timed out waiting for connector health: {last}")
PY
}

wait_for_connector_active_lease() {
  local out_file="$1"
  local base_url="$2"
  local subject_id="$3"
  local previous_lease="${4:-}"
  local tmp_file="$out_file.tmp"
  local deadline=$((SECONDS + 30))
  while (( SECONDS < deadline )); do
    if capture_json "$tmp_file" "$base_url" runtime auth subject "$subject_id"; then
      if python3 - "$tmp_file" "$previous_lease" <<'PY'
import json
import sys

with open(sys.argv[1], "r", encoding="utf-8") as handle:
    data = json.load(handle)
previous = sys.argv[2]
lease_ids = data.get("active_connector_lease_ids") or []
if len(lease_ids) == 1 and lease_ids[0] and (not previous or lease_ids[0] != previous):
    raise SystemExit(0)
raise SystemExit(1)
PY
      then
        mv "$tmp_file" "$out_file"
        return 0
      fi
    fi
    sleep 0.2
  done
  echo "timed out waiting for active connector lease on $subject_id" >&2
  [[ -s "$tmp_file" ]] && cat "$tmp_file" >&2 || true
  return 1
}

make_webhook_routes_json() {
  local secret="$1"
  python3 - "$secret" <<'PY'
import json
import sys

secret = sys.argv[1]
json.dump(
    {
        "demo": {
            "secret": secret,
            "prompt_template": "Webhook payload:\n{payload_json}",
            "session_mode": "per_delivery",
            "allowed_events": ["ops-accepted"],
        }
    },
    sys.stdout,
    separators=(",", ":"),
    sort_keys=True,
)
PY
}

make_connector_payload() {
  local base_url="$1"
  local shared_token_ref="$2"
  local capture_path="$3"
  local generation="$4"
  python3 - "$base_url" "$shared_token_ref" "$capture_path" "$generation" "$ROOT/connectors/python/run_connector.py" <<'PY'
import json
import sys

base_url, shared_token_ref, capture_path, generation, launcher = sys.argv[1:]
json.dump(
    {
        "platform": "webhook",
        "mode": "child_process",
        "base_url": base_url,
        "shared_token": {"secret_ref": shared_token_ref},
        "include_self_output": True,
        "session_policy": {"create_if_missing": True},
        "child_process": {
            "command": "python3",
            "args": [launcher, "webhook"],
            "env": {
                "KHEISH_EXTERNAL_CONNECTOR_DELIVERY_CAPTURE_PATH": capture_path,
                "OPS_CONNECTOR_ROTATION_GENERATION": generation,
            },
            "credential_slots": {
                "WEBHOOK_ROUTES_JSON": "sidecars.webhook.routes",
            },
        },
    },
    sys.stdout,
    separators=(",", ":"),
    sort_keys=True,
)
PY
}

post_signed_webhook() {
  local out_file="$1"
  local base_url="$2"
  local secret="$3"
  local delivery_id="$4"
  local expected_status="$5"
  python3 - "$out_file" "$base_url" "$secret" "$delivery_id" "$expected_status" <<'PY'
import hmac
import json
import sys
import time
import urllib.error
import urllib.request

out_file, base_url, secret, delivery_id, expected = sys.argv[1:]
expected = int(expected)
deadline = time.time() + 30
last = None
while time.time() < deadline:
    body = json.dumps(
        {"event_type": "ignored", "title": delivery_id},
        separators=(",", ":"),
        sort_keys=True,
    ).encode("utf-8")
    signature = "sha256=" + hmac.new(secret.encode("utf-8"), body, "sha256").hexdigest()
    request = urllib.request.Request(
        f"{base_url.rstrip('/')}/webhooks/demo",
        data=body,
        method="POST",
        headers={
            "Content-Type": "application/json",
            "X-GitHub-Delivery": delivery_id,
            "X-GitHub-Event": "ignored",
            "X-Hub-Signature-256": signature,
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            status = response.status
            response_body = response.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as error:
        status = error.code
        response_body = error.read().decode("utf-8", "replace")
    except Exception as exc:
        last = {"error": str(exc)}
        time.sleep(0.2)
        continue
    try:
        decoded = json.loads(response_body)
    except json.JSONDecodeError:
        decoded = response_body
    last = {"status": status, "body": decoded}
    if status == expected:
        with open(out_file, "w", encoding="utf-8") as handle:
            json.dump(last, handle, sort_keys=True)
        raise SystemExit(0)
    time.sleep(0.2)
with open(out_file, "w", encoding="utf-8") as handle:
    json.dump(last, handle, sort_keys=True)
raise SystemExit(f"timed out waiting for webhook HTTP {expected}; last={last}")
PY
}

probe_sse_url() {
  local url="$1"
  local token_file="$2"
  python3 - "$url" "$token_file" <<'PY'
import http.client
import socket
import sys
import time
import urllib.parse

url = urllib.parse.urlparse(sys.argv[1])
path = url.path + (("?" + url.query) if url.query else "")
with open(sys.argv[2], "r", encoding="utf-8") as handle:
    token = handle.read().strip()
conn = http.client.HTTPConnection(url.hostname, url.port or 80, timeout=20)
conn.request(
    "GET",
    path,
    headers={
        "Authorization": f"Bearer {token}",
        "Accept": "text/event-stream",
    },
)
response = conn.getresponse()
content_type = response.getheader("content-type", "")
if response.status != 200:
    raise SystemExit(f"SSE probe returned HTTP {response.status} for {path}")
if not content_type.startswith("text/event-stream"):
    raise SystemExit(f"SSE probe returned content-type {content_type!r} for {path}")
if conn.sock is not None:
    conn.sock.settimeout(1)
deadline = time.time() + 20
buffer = b""
while time.time() < deadline:
    try:
        chunk = response.read(1)
    except socket.timeout:
        continue
    if not chunk:
        break
    buffer += chunk
    normalized = buffer.replace(b"\r\n", b"\n")
    if b"\n\n" in normalized:
        event = normalized.split(b"\n\n", 1)[0].decode("utf-8", "replace")
        if "event:" not in event:
            raise SystemExit(f"SSE frame has no event field for {path}: {event!r}")
        if "id:" not in event:
            raise SystemExit(f"SSE replay frame has no id field for {path}: {event!r}")
        raise SystemExit(0)
raise SystemExit(f"SSE probe timed out before an id-bearing frame for {path}")
PY
}

assert_runbook_command_coverage() {
  python3 "$ROOT/scripts/e2e/verify_runbook_commands.py" \
    "$ROOT/docs/operators/production-runbooks.mdx" >/dev/null
}

assert_no_secret_leaks() {
  local secret
  for secret in \
    "$OPENAI_API_KEY" \
    "${WEBHOOK_SHARED_TOKEN:-}" \
    "${WEBHOOK_SECRET_V1:-}" \
    "${WEBHOOK_SECRET_V2:-}"
  do
    [[ -z "$secret" ]] && continue
    if grep -R --binary-files=without-match -F "$secret" "$TMP" >/dev/null 2>&1; then
      echo "raw secret leaked into ops live smoke artifacts under $TMP" >&2
      return 1
    fi
  done
  while IFS='=' read -r name value; do
    case "$name" in
      *API_KEY*|*TOKEN*|*SECRET*|*PASSWORD*|*MASTER_KEY*) ;;
      *) continue ;;
    esac
    [[ ${#value} -lt 8 ]] && continue
    if grep -R --binary-files=without-match -F "$value" "$TMP" >/dev/null 2>&1; then
      echo "environment secret $name leaked into ops live smoke artifacts under $TMP" >&2
      return 1
    fi
  done < <(env)
  if find "$TMP" -type f \( \
    -name admin-token -o \
    -name readonly-token -o \
    -name old-admin-token -o \
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
printf 'admin-token-live-%s-%s\n' "$$" "$RANDOM" >"$ADMIN_TOKEN_FILE"
printf 'readonly-token-live-%s-%s\n' "$$" "$RANDOM" >"$READONLY_TOKEN_FILE"
chmod 600 "$MASTER_KEY_FILE" "$ADMIN_TOKEN_FILE" "$READONLY_TOKEN_FILE"
mkdir -p "$WORKSPACE/$(dirname "$WORKSPACE_SENTINEL_REL")"
printf 'workspace-live-backup-smoke:%s\n' "$TMP" >"$WORKSPACE/$WORKSPACE_SENTINEL_REL"
WORKSPACE_SENTINEL_SHA="$(sha256_file "$WORKSPACE/$WORKSPACE_SENTINEL_REL")"

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
    --from-env OPENAI_API_KEY >/dev/null

PORT="$(free_port)"
BASE_URL="http://127.0.0.1:$PORT"
start_daemon "$STATE" "$WORKSPACE" "$PORT" "$TMP/daemon.log"
PROXY_PORT="$(free_port)"
PROXY_BASE_URL="http://127.0.0.1:$PROXY_PORT"
start_streaming_proxy "$BASE_URL" "$PROXY_PORT" "$TMP/proxy.log"

assert_runbook_command_coverage
capture_json "$TMP/status.json" "$PROXY_BASE_URL" status
json_expect "$TMP/status.json" "data.get('health', {}).get('ok') is True" "status health ok"
json_expect "$TMP/status.json" "data.get('storage', {}).get('ok') is True" "storage ok"
json_expect "$TMP/status.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0" \
  "provider readiness ok"
json_expect "$TMP/status.json" \
  "data.get('delivery', {}).get('unresolved_dead_lettered', 0) == 0" \
  "delivery DLQ clean"
capture_json "$TMP/runtime.json" "$PROXY_BASE_URL" runtime get
capture_json "$TMP/doctor.json" "$PROXY_BASE_URL" doctor
capture_json "$TMP/doctor-cors.json" "$PROXY_BASE_URL" doctor --cors-origin http://localhost:5173
capture_json "$TMP/doctor-routes-auth.json" "$PROXY_BASE_URL" doctor routes --check-auth
capture_json "$TMP/doctor-routes-references.json" "$PROXY_BASE_URL" doctor routes --check-references
capture_json "$TMP/doctor-routes-canary.json" "$PROXY_BASE_URL" \
  doctor routes --route openai --check-auth --canary --canary-timeout-ms "$CANARY_TIMEOUT_MS"
json_expect "$TMP/doctor-routes-canary.json" \
  "any(canary.get('status') == 'passed' for canary in data.get('canaries', []))" \
  "doctor routes canary passed"

SESSION_ID="ops-live-$$"
capture_json "$TMP/session-create.json" "$PROXY_BASE_URL" sessions create "$SESSION_ID"
capture_json "$TMP/run-submit.json" "$PROXY_BASE_URL" \
  sessions input "$SESSION_ID" \
    "Reply exactly OPS_RUNBOOK_LIVE_OK and nothing else." \
    --provider openai \
    --model "$MODEL" \
    --max-output-tokens 64 \
    --tool-choice none
RUN_ID="$(json_get "$TMP/run-submit.json" run_id)"
capture_json "$TMP/run-wait.json" "$PROXY_BASE_URL" runs wait "$RUN_ID" --poll-interval-ms 500
json_expect "$TMP/run-wait.json" "data.get('status') == 'completed'" "live run completed"
json_expect "$TMP/run-wait.json" \
  "any('OPS_RUNBOOK_LIVE_OK' in output.get('content', '') for output in data.get('outputs', []))" \
  "live run output marker persisted"

probe_sse_url "$PROXY_BASE_URL/v1/events/stream?cursor=0" "$ADMIN_TOKEN_FILE"
probe_sse_url "$PROXY_BASE_URL/v1/sessions/$SESSION_ID/stream?cursor=0" "$ADMIN_TOKEN_FILE"
probe_sse_url "$PROXY_BASE_URL/v1/runs/$RUN_ID/stream?cursor=0" "$ADMIN_TOKEN_FILE"

capture_json "$TMP/runtime-permission.json" "$PROXY_BASE_URL" runtime set-permission-mode dont-ask
json_expect "$TMP/runtime-permission.json" \
  "data.get('permission_mode') == 'dontAsk'" \
  "incident containment switched permission mode to dontAsk"
capture_json "$TMP/runtime-debug-off.json" "$PROXY_BASE_URL" runtime set-debug-level off
json_expect "$TMP/runtime-debug-off.json" \
  "data.get('debug_level') == 'off'" \
  "incident containment disabled debug capture"
capture_json "$TMP/runs-list.json" "$PROXY_BASE_URL" runs list --session-id "$SESSION_ID"
capture_json "$TMP/run-get.json" "$PROXY_BASE_URL" runs get "$RUN_ID"
capture_json "$TMP/session-events.json" "$PROXY_BASE_URL" sessions events "$SESSION_ID"
capture_json "$TMP/tasks-list.json" "$PROXY_BASE_URL" tasks list "$SESSION_ID"
capture_json "$TMP/deliveries-list.json" "$PROXY_BASE_URL" deliveries list --run-id "$RUN_ID"
capture_json "$TMP/external-actions.json" "$PROXY_BASE_URL" runs external-actions "$RUN_ID"
json_expect "$TMP/external-actions.json" \
  "len(data) >= 2 and any(record.get('phase') == 'request' for record in data) and any(record.get('phase') == 'response' for record in data) and all(record.get('signature_alg') == 'ed25519' and record.get('key_id') and record.get('signature') and record.get('record_hash') for record in data)" \
  "signed external action records persisted"
capture_json "$TMP/session-interrupt.json" "$PROXY_BASE_URL" sessions interrupt "$SESSION_ID"
capture_json "$TMP/run-cancel-terminal.json" "$PROXY_BASE_URL" runs cancel "$RUN_ID"

AGENT_ID="$(json_get_optional "$TMP/run-wait.json" agent_id)"
if [[ -n "$AGENT_ID" ]]; then
  SUBJECT_ID="agent:$AGENT_ID"
else
  SUBJECT_ID="session:$SESSION_ID"
fi
capture_json "$TMP/revoke-subject.json" "$PROXY_BASE_URL" runtime auth revoke-subject "$SUBJECT_ID"
json_expect "$TMP/revoke-subject.json" \
  "data.get('revoked') is True and not data.get('active_connector_lease_ids', []) and not data.get('active_route_lease_ids', []) and not data.get('active_mcp_lease_ids', [])" \
  "broker subject revoked and active leases cleared"
capture_json "$TMP/revoke-slot.json" "$PROXY_BASE_URL" runtime auth revoke-slot openai.prod
json_expect "$TMP/revoke-slot.json" \
  "data.get('slot_id') == 'openai.prod' and data.get('revoked_leases', -1) >= 0" \
  "route slot revocation returned stable shape"

CONNECTOR_NAME="ops-webhook"
CONNECTOR_SUBJECT="connector:$CONNECTOR_NAME"
CONNECTOR_ROUTES_SLOT="sidecars.webhook.routes"
CONNECTOR_SHARED_SLOT="sidecars.webhook.shared"
WEBHOOK_SHARED_TOKEN="ops-webhook-shared-$$-$RANDOM"
WEBHOOK_SECRET_V1="ops-webhook-route-v1-$$-$RANDOM"
WEBHOOK_SECRET_V2="ops-webhook-route-v2-$$-$RANDOM"
WEBHOOK_PORT="$(free_port)"
WEBHOOK_BASE_URL="http://127.0.0.1:$WEBHOOK_PORT"
WEBHOOK_CAPTURE="$TMP/ops-webhook-deliveries.jsonl"
WEBHOOK_ROUTES_V1="$(make_webhook_routes_json "$WEBHOOK_SECRET_V1")"
WEBHOOK_ROUTES_V2="$(make_webhook_routes_json "$WEBHOOK_SECRET_V2")"

capture_json "$TMP/connector-shared-secret-set.json" "$PROXY_BASE_URL" \
  secrets set "$CONNECTOR_SHARED_SLOT" --provider generic --value "$WEBHOOK_SHARED_TOKEN"
capture_json "$TMP/connector-routes-secret-v1.json" "$PROXY_BASE_URL" \
  secrets set "$CONNECTOR_ROUTES_SLOT" --provider generic --value "$WEBHOOK_ROUTES_V1"
CONNECTOR_PAYLOAD_V1="$(make_connector_payload "$WEBHOOK_BASE_URL" "$CONNECTOR_SHARED_SLOT" "$WEBHOOK_CAPTURE" "v1")"
capture_json "$TMP/connector-put-v1.json" "$PROXY_BASE_URL" \
  connectors put-external "$CONNECTOR_NAME" --json "$CONNECTOR_PAYLOAD_V1"
wait_connector_health "$WEBHOOK_BASE_URL" "$WEBHOOK_SHARED_TOKEN"
wait_for_connector_active_lease "$TMP/connector-subject-v1.json" \
  "$PROXY_BASE_URL" "$CONNECTOR_SUBJECT"
CONNECTOR_LEASE_V1="$(json_query "$TMP/connector-subject-v1.json" "data['active_connector_lease_ids'][0]")"
post_signed_webhook "$TMP/connector-webhook-v1.json" \
  "$WEBHOOK_BASE_URL" "$WEBHOOK_SECRET_V1" "ops-webhook-v1" 200
json_expect "$TMP/connector-webhook-v1.json" \
  "data.get('status') == 200 and data.get('body', {}).get('status') == 'ignored'" \
  "connector accepted v1 credential-backed route secret without creating a provider run"

capture_json "$TMP/connector-routes-secret-v2.json" "$PROXY_BASE_URL" \
  secrets set "$CONNECTOR_ROUTES_SLOT" --provider generic --value "$WEBHOOK_ROUTES_V2"
capture_json "$TMP/connector-lease-v1-after-secret-rotation.json" "$PROXY_BASE_URL" \
  runtime auth lease "$CONNECTOR_LEASE_V1"
json_expect "$TMP/connector-lease-v1-after-secret-rotation.json" \
  "data.get('revoked') is True and data.get('active') is False and 'token_digest' not in data.get('lease', {})" \
  "connector secret rotation revoked old lease and hid token digest"
CONNECTOR_PAYLOAD_V2="$(make_connector_payload "$WEBHOOK_BASE_URL" "$CONNECTOR_SHARED_SLOT" "$WEBHOOK_CAPTURE" "v2")"
capture_json "$TMP/connector-put-v2.json" "$PROXY_BASE_URL" \
  connectors put-external "$CONNECTOR_NAME" --json "$CONNECTOR_PAYLOAD_V2"
wait_for_connector_active_lease "$TMP/connector-subject-v2.json" \
  "$PROXY_BASE_URL" "$CONNECTOR_SUBJECT" "$CONNECTOR_LEASE_V1"
CONNECTOR_LEASE_V2="$(json_query "$TMP/connector-subject-v2.json" "data['active_connector_lease_ids'][0]")"
post_signed_webhook "$TMP/connector-webhook-old-secret-after-rotation.json" \
  "$WEBHOOK_BASE_URL" "$WEBHOOK_SECRET_V1" "ops-webhook-old-after-rotation" 401
json_expect "$TMP/connector-webhook-old-secret-after-rotation.json" \
  "data.get('status') == 401 and data.get('body', {}).get('error') == 'invalid signature'" \
  "rotated connector sidecar rejects old webhook secret"
post_signed_webhook "$TMP/connector-webhook-v2.json" \
  "$WEBHOOK_BASE_URL" "$WEBHOOK_SECRET_V2" "ops-webhook-v2" 200
json_expect "$TMP/connector-webhook-v2.json" \
  "data.get('status') == 200 and data.get('body', {}).get('status') == 'ignored'" \
  "rotated connector sidecar accepts new webhook secret"
capture_json "$TMP/connector-revoke-slot.json" "$PROXY_BASE_URL" \
  runtime auth revoke-slot "$CONNECTOR_ROUTES_SLOT"
json_expect "$TMP/connector-revoke-slot.json" \
  "data.get('slot_id') == 'sidecars.webhook.routes' and data.get('revoked_leases', 0) >= 1" \
  "connector route slot revoke returned active lease count"
capture_json "$TMP/connector-lease-v2-after-revoke-slot.json" "$PROXY_BASE_URL" \
  runtime auth lease "$CONNECTOR_LEASE_V2"
json_expect "$TMP/connector-lease-v2-after-revoke-slot.json" \
  "data.get('revoked') is True and data.get('active') is False and 'token_digest' not in data.get('lease', {})" \
  "explicit connector slot revoke revoked new lease and hid token digest"
capture_json "$TMP/connector-delete.json" "$PROXY_BASE_URL" \
  connectors delete external "$CONNECTOR_NAME"
capture_json "$TMP/connector-routes-secret-delete.json" "$PROXY_BASE_URL" \
  secrets delete "$CONNECTOR_ROUTES_SLOT"
capture_json "$TMP/connector-shared-secret-delete.json" "$PROXY_BASE_URL" \
  secrets delete "$CONNECTOR_SHARED_SLOT"

cp "$ADMIN_TOKEN_FILE" "$OLD_ADMIN_TOKEN_FILE"
printf 'admin-token-live-rotated-%s-%s\n' "$$" "$RANDOM" >"$NEXT_ADMIN_TOKEN_SOURCE"
install -m 600 "$NEXT_ADMIN_TOKEN_SOURCE" "$ADMIN_TOKEN_FILE.next"
mv "$ADMIN_TOKEN_FILE.next" "$ADMIN_TOKEN_FILE"
wait_for_http_status "$PROXY_BASE_URL/v1/status" "$OLD_ADMIN_TOKEN_FILE" 401
wait_for_http_status "$PROXY_BASE_URL/v1/status" "$ADMIN_TOKEN_FILE" 200
capture_json "$TMP/status-after-token-rotation.json" "$PROXY_BASE_URL" status
json_expect "$TMP/status-after-token-rotation.json" "data.get('health', {}).get('ok') is True" "post-token-rotation status health ok"
json_expect "$TMP/status-after-token-rotation.json" "data.get('storage', {}).get('ok') is True" "post-token-rotation storage ok"

stop_proxy
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
RESTORED_PROXY_PORT="$(free_port)"
RESTORED_PROXY_BASE_URL="http://127.0.0.1:$RESTORED_PROXY_PORT"
start_streaming_proxy "$RESTORED_BASE_URL" "$RESTORED_PROXY_PORT" "$TMP/restored-proxy.log"

capture_json "$TMP/restored-status.json" "$RESTORED_PROXY_BASE_URL" status
json_expect "$TMP/restored-status.json" "data.get('health', {}).get('ok') is True" "restored status health ok"
json_expect "$TMP/restored-status.json" "data.get('storage', {}).get('ok') is True" "restored storage ok"
json_expect "$TMP/restored-status.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0" \
  "restored provider readiness ok"
capture_json "$TMP/restored-runtime.json" "$RESTORED_PROXY_BASE_URL" runtime get
capture_json "$TMP/restored-routes-auth.json" "$RESTORED_PROXY_BASE_URL" doctor routes --check-auth
capture_json "$TMP/restored-run-get.json" "$RESTORED_PROXY_BASE_URL" runs get "$RUN_ID"
json_expect "$TMP/restored-run-get.json" "data.get('status') == 'completed'" "restored run completed"
json_expect "$TMP/restored-run-get.json" \
  "any('OPS_RUNBOOK_LIVE_OK' in output.get('content', '') for output in data.get('outputs', []))" \
  "restored run output marker persisted"
capture_json "$TMP/restored-external-actions.json" "$RESTORED_PROXY_BASE_URL" runs external-actions "$RUN_ID"
json_expect "$TMP/restored-external-actions.json" \
  "len(data) >= 2 and any(record.get('phase') == 'request' for record in data) and any(record.get('phase') == 'response' for record in data) and all(record.get('signature_alg') == 'ed25519' and record.get('key_id') and record.get('signature') and record.get('record_hash') for record in data)" \
  "restored signed external action records persisted"
if ! cmp -s "$TMP/external-actions.json" "$TMP/restored-external-actions.json"; then
  echo "restored external-action records differ from pre-backup records" >&2
  exit 1
fi
[[ "$(sha256_file "$RESTORED_STATE/audit-signing.key")" == "$AUDIT_SIGNING_SHA" ]]
[[ "$(sha256_file "$RESTORED_WORKSPACE/$WORKSPACE_SENTINEL_REL")" == "$WORKSPACE_SENTINEL_SHA" ]]

assert_no_secret_leaks

printf 'ops live runbook smoke passed: %s\n' "$TMP"
