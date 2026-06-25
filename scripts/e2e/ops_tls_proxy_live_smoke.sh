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
  echo "OPENAI_API_KEY or KHEISH_OPENAI_API_KEY is required for ops_tls_proxy_live_smoke.sh" >&2
  exit 2
fi

MODEL="${KHEISH_OPS_TLS_LIVE_MODEL:-gpt-5.4}"

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPS_TLS_LIVE_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPS_TLS_LIVE_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/kheish-ops-tls-live.XXXXXX")"
SECRET_DIR="$(mktemp -d "$WORK_BASE/kheish-ops-tls-live-secrets.XXXXXX")"
STATE="$TMP/state"
WORKSPACE="$TMP/workspace"
ROUTES="$TMP/routes.toml"
MASTER_KEY_FILE="$SECRET_DIR/auth-store-master-key"
ADMIN_TOKEN_FILE="$SECRET_DIR/admin-token"
READONLY_TOKEN_FILE="$SECRET_DIR/readonly-token"
TLS_CERT="$TMP/tls-cert.pem"
TLS_KEY="$SECRET_DIR/tls-key.pem"
OPENSSL_CONFIG="$SECRET_DIR/openssl-san.cnf"
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

wait_https_ok() {
  local url="$1"
  local cert="$2"
  python3 - "$url" "$cert" <<'PY'
import ssl
import sys
import time
import urllib.request

url = sys.argv[1]
context = ssl.create_default_context(cafile=sys.argv[2])
deadline = time.time() + 30
last = None
while time.time() < deadline:
    try:
        with urllib.request.urlopen(url, context=context, timeout=2) as response:
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

start_tls_proxy() {
  local target="$1"
  local port="$2"
  local cert="$3"
  local key="$4"
  local log_file="$5"
  PROXY_PID=""
  python3 - "$target" "$port" "$cert" "$key" <<'PY' >"$log_file" 2>&1 &
import http.client
import http.server
import ssl
import sys
import urllib.parse

target = urllib.parse.urlparse(sys.argv[1])
port = int(sys.argv[2])
cert = sys.argv[3]
key = sys.argv[4]
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

    def do_OPTIONS(self):
        self.forward()

    def forward(self):
        length = int(self.headers.get("Content-Length", "0") or "0")
        body = self.rfile.read(length) if length else None
        headers = {
            name: value
            for name, value in self.headers.items()
            if name.lower() not in hop_by_hop
        }
        headers["Host"] = target.netloc
        conn = http.client.HTTPConnection(target.hostname, target.port or 80, timeout=3600)
        try:
            conn.request(self.command, self.path, body=body, headers=headers)
            response = conn.getresponse()
            self.send_response(response.status)
            for name, value in response.headers.items():
                if name.lower() not in hop_by_hop:
                    self.send_header(name, value)
            self.send_header("Connection", "close")
            self.end_headers()
            content_type = response.getheader("Content-Type", "")
            chunk_size = 1 if content_type.startswith("text/event-stream") else 65536
            while True:
                chunk = response.read(chunk_size)
                if not chunk:
                    break
                try:
                    self.wfile.write(chunk)
                    self.wfile.flush()
                except BrokenPipeError:
                    break
        except Exception as exc:
            payload = f"tls proxy upstream error: {exc}".encode()
            if not self.wfile.closed:
                self.send_response(502)
                self.send_header("Content-Type", "text/plain")
                self.send_header("Content-Length", str(len(payload)))
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(payload)
        finally:
            conn.close()


server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
context.load_cert_chain(certfile=cert, keyfile=key)
server.socket = context.wrap_socket(server.socket, server_side=True)
server.serve_forever()
PY
  PROXY_PID="$!"
  wait_https_ok "https://127.0.0.1:$port/readyz" "$cert"
}

https_json() {
  local out_file="$1"
  local method="$2"
  local path="$3"
  local body_file="${4:-}"
  python3 - "$TLS_BASE_URL" "$TLS_CERT" "$ADMIN_TOKEN_FILE" "$method" "$path" "$out_file" "$body_file" <<'PY'
import pathlib
import ssl
import sys
import urllib.error
import urllib.request

base_url, cert_file, token_file, method, path, out_file, body_file = sys.argv[1:8]
token = pathlib.Path(token_file).read_text(encoding="utf-8").strip()
payload = pathlib.Path(body_file).read_bytes() if body_file else None
headers = {
    "Authorization": f"Bearer {token}",
    "Accept": "application/json",
}
if payload is not None:
    headers["Content-Type"] = "application/json"
request = urllib.request.Request(base_url + path, data=payload, method=method, headers=headers)
context = ssl.create_default_context(cafile=cert_file)
try:
    with urllib.request.urlopen(request, context=context, timeout=120) as response:
        body = response.read()
        pathlib.Path(out_file).write_bytes(body)
        if not (200 <= response.status < 300):
            raise SystemExit(f"{method} {path} returned HTTP {response.status}: {body[:200]!r}")
except urllib.error.HTTPError as error:
    body = error.read()
    pathlib.Path(out_file).write_bytes(body)
    raise SystemExit(f"{method} {path} returned HTTP {error.code}: {body[:500]!r}")
PY
}

https_status_no_token() {
  local path="$1"
  python3 - "$TLS_BASE_URL" "$TLS_CERT" "$path" <<'PY'
import ssl
import sys
import urllib.error
import urllib.request

base_url, cert_file, path = sys.argv[1:4]
request = urllib.request.Request(base_url + path, headers={"Accept": "application/json"})
context = ssl.create_default_context(cafile=cert_file)
try:
    with urllib.request.urlopen(request, context=context, timeout=10) as response:
        print(response.status)
except urllib.error.HTTPError as error:
    print(error.code)
PY
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

wait_run_tls() {
  local run_id="$1"
  local out_file="$2"
  python3 - "$TLS_BASE_URL" "$TLS_CERT" "$ADMIN_TOKEN_FILE" "$run_id" "$out_file" <<'PY'
import json
import pathlib
import ssl
import sys
import time
import urllib.request

base_url, cert_file, token_file, run_id, out_file = sys.argv[1:6]
token = pathlib.Path(token_file).read_text(encoding="utf-8").strip()
context = ssl.create_default_context(cafile=cert_file)
deadline = time.time() + 180
last = None
while time.time() < deadline:
    request = urllib.request.Request(
        f"{base_url}/v1/runs/{run_id}",
        headers={"Authorization": f"Bearer {token}", "Accept": "application/json"},
    )
    with urllib.request.urlopen(request, context=context, timeout=30) as response:
        data = json.loads(response.read().decode("utf-8"))
    last = data
    status = data.get("status")
    if status in {"completed", "failed", "interrupted", "cancelled"}:
        pathlib.Path(out_file).write_text(json.dumps(data, indent=2), encoding="utf-8")
        if status == "completed":
            raise SystemExit(0)
        raise SystemExit(f"run {run_id} reached terminal status {status}")
    time.sleep(1)
pathlib.Path(out_file).write_text(json.dumps(last, indent=2), encoding="utf-8")
raise SystemExit(f"timed out waiting for run {run_id}; last status={last.get('status') if last else None}")
PY
}

probe_tls_sse_url() {
  local path="$1"
  local last_event_id="${2:-}"
  local minimum_event_id="${3:-}"
  python3 - "$TLS_BASE_URL" "$TLS_CERT" "$ADMIN_TOKEN_FILE" "$path" "$last_event_id" "$minimum_event_id" <<'PY'
import http.client
import socket
import ssl
import sys
import time
import urllib.parse

base_url, cert_file, token_file, path, last_event_id, minimum_event_id = sys.argv[1:7]
url = urllib.parse.urlparse(base_url)
with open(token_file, "r", encoding="utf-8") as handle:
    token = handle.read().strip()
headers = {
    "Authorization": f"Bearer {token}",
    "Accept": "text/event-stream",
}
if last_event_id:
    headers["Last-Event-ID"] = last_event_id
conn = http.client.HTTPSConnection(
    url.hostname,
    url.port or 443,
    timeout=20,
    context=ssl.create_default_context(cafile=cert_file),
)
conn.request("GET", path, headers=headers)
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
        event_id = None
        for line in event.splitlines():
            if line.startswith("id:"):
                event_id = line.split(":", 1)[1].strip()
                break
        if not event_id:
            raise SystemExit(f"SSE replay frame has no id field for {path}: {event!r}")
        if minimum_event_id and int(event_id) <= int(minimum_event_id):
            raise SystemExit(
                f"SSE replay returned stale id {event_id} for {path}; expected > {minimum_event_id}"
            )
        print(event_id)
        raise SystemExit(0)
raise SystemExit(f"SSE probe timed out before an id-bearing frame for {path}")
PY
}

assert_no_secret_leaks() {
  if grep -R --binary-files=without-match -F "$OPENAI_API_KEY" "$TMP" >/dev/null 2>&1; then
    echo "OPENAI_API_KEY leaked into TLS proxy smoke artifacts under $TMP" >&2
    return 1
  fi
  while IFS='=' read -r name value; do
    case "$name" in
      *API_KEY*|*TOKEN*|*SECRET*|*PASSWORD*|*MASTER_KEY*) ;;
      *) continue ;;
    esac
    [[ ${#value} -lt 8 ]] && continue
    if grep -R --binary-files=without-match -F "$value" "$TMP" >/dev/null 2>&1; then
      echo "environment secret $name leaked into TLS proxy smoke artifacts under $TMP" >&2
      return 1
    fi
  done < <(env)
  if find "$TMP" -type f \( \
    -name admin-token -o \
    -name readonly-token -o \
    -name auth-store-master-key -o \
    -name tls-key.pem \
  \) -print -quit | grep -q .; then
    echo "local token, master-key, or TLS private key remained in evidence dir $TMP" >&2
    return 1
  fi
}

mkdir -p "$STATE" "$WORKSPACE"
"$BIN" secrets generate >"$MASTER_KEY_FILE"
printf 'admin-token-tls-%s-%s\n' "$$" "$RANDOM" >"$ADMIN_TOKEN_FILE"
printf 'readonly-token-tls-%s-%s\n' "$$" "$RANDOM" >"$READONLY_TOKEN_FILE"
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

DAEMON_PORT="$(free_port)"
DAEMON_BASE_URL="http://127.0.0.1:$DAEMON_PORT"
start_daemon "$DAEMON_PORT" "$TMP/daemon.log"

cat >"$OPENSSL_CONFIG" <<'EOF'
[req]
distinguished_name = dn
prompt = no

[dn]
CN = localhost

[ext]
subjectAltName = DNS:localhost,IP:127.0.0.1
EOF

openssl req \
  -x509 \
  -newkey rsa:2048 \
  -nodes \
  -keyout "$TLS_KEY" \
  -out "$TLS_CERT" \
  -days 1 \
  -config "$OPENSSL_CONFIG" \
  -extensions ext \
  >"$TMP/openssl.stdout" 2>"$TMP/openssl.stderr"
chmod 600 "$TLS_KEY"

TLS_PORT="$(free_port)"
TLS_BASE_URL="https://127.0.0.1:$TLS_PORT"
start_tls_proxy "$DAEMON_BASE_URL" "$TLS_PORT" "$TLS_CERT" "$TLS_KEY" "$TMP/tls-proxy.log"

[[ "$(https_status_no_token "/v1/status")" == "401" ]]
https_json "$TMP/status.json" GET "/v1/status"
json_expect "$TMP/status.json" "data.get('health', {}).get('ok') is True" "TLS status health ok"
json_expect "$TMP/status.json" "data.get('storage', {}).get('ok') is True" "TLS storage ok"
json_expect "$TMP/status.json" \
  "data.get('provider_readiness', {}).get('active_route_ready') is True and data.get('provider_readiness', {}).get('error_route_count') == 0" \
  "TLS provider readiness ok"

SESSION_ID="ops-tls-live-$$"
python3 - "$SESSION_ID" >"$TMP/session-create-body.json" <<'PY'
import json
import sys

json.dump(
    {
        "session_id": sys.argv[1],
        "thread_id": None,
        "persona_id": None,
        "capability_scope": None,
        "credential_scope": None,
    },
    sys.stdout,
)
PY
https_json "$TMP/session-create.json" POST "/v1/sessions" "$TMP/session-create-body.json"

python3 - "$MODEL" >"$TMP/run-submit-body.json" <<'PY'
import json
import sys

model = sys.argv[1]
json.dump(
    {
        "provider": "openai",
        "source_plugin": None,
        "source_kind": None,
        "actor_id": None,
        "content": "Reply exactly TLS_PROXY_LIVE_OK and nothing else.",
        "input_items": [],
        "attachments": [],
        "generation": {
            "model": model,
            "tool_choice": {"type": "none"},
            "max_output_tokens": 64,
        },
        "completion_requirements": None,
        "metadata": None,
        "binding_keys": [],
        "reply_targets": [],
        "reply_plugin": None,
        "reply_address": None,
    },
    sys.stdout,
)
PY
https_json "$TMP/run-submit.json" POST "/v1/sessions/$SESSION_ID/runs" "$TMP/run-submit-body.json"
RUN_ID="$(json_get "$TMP/run-submit.json" run_id)"
wait_run_tls "$RUN_ID" "$TMP/run-wait.json"
json_expect "$TMP/run-wait.json" \
  "any('TLS_PROXY_LIVE_OK' in output.get('content', '') for output in data.get('outputs', []))" \
  "TLS live run output marker persisted"

GLOBAL_SSE_ID="$(probe_tls_sse_url "/v1/events/stream?cursor=0")"
probe_tls_sse_url "/v1/sessions/$SESSION_ID/stream?cursor=0" >/dev/null
probe_tls_sse_url "/v1/runs/$RUN_ID/stream?cursor=0" >/dev/null

FOLLOWUP_SESSION_ID="ops-tls-live-followup-$$"
python3 - "$FOLLOWUP_SESSION_ID" >"$TMP/followup-session-create-body.json" <<'PY'
import json
import sys

json.dump(
    {
        "session_id": sys.argv[1],
        "thread_id": None,
        "persona_id": None,
        "capability_scope": None,
        "credential_scope": None,
    },
    sys.stdout,
)
PY
https_json "$TMP/followup-session-create.json" POST "/v1/sessions" "$TMP/followup-session-create-body.json"
probe_tls_sse_url "/v1/events/stream" "$GLOBAL_SSE_ID" "$GLOBAL_SSE_ID" >/dev/null

assert_no_secret_leaks

printf 'ops TLS proxy live smoke passed: %s\n' "$TMP"
