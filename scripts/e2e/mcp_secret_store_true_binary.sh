#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/debug/kheish-daemon"
if [[ ! -x "$BIN" ]]; then
  echo "missing daemon binary: $BIN" >&2
  exit 1
fi

RUN_ID="mcp-secret-store-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE="$ROOT/tmp/e2e/$RUN_ID"
STATE_ROOT="$EVIDENCE/state"
WORKSPACE_ROOT="$EVIDENCE/workspace"
CONFIG="$EVIDENCE/mcp.toml"
LOG="$EVIDENCE/daemon.log"
MCP_FIXTURE="$EVIDENCE/mcp_bearer_fixture.py"
MCP_FIXTURE_LOG="$EVIDENCE/mcp-fixture.log"
MCP_REQUEST_LOG="$EVIDENCE/mcp-requests.jsonl"
MCP_READY="$EVIDENCE/mcp-ready.json"
SECRET_VALUE="mcp-secret-store-e2e-token"
PID=""
MCP_PID=""
mkdir -p "$STATE_ROOT" "$WORKSPACE_ROOT"

cleanup() {
  if [[ -n "$PID" ]] && kill -0 "$PID" >/dev/null 2>&1; then
    kill "$PID" >/dev/null 2>&1 || true
    wait "$PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$MCP_PID" ]] && kill -0 "$MCP_PID" >/dev/null 2>&1; then
    kill "$MCP_PID" >/dev/null 2>&1 || true
    wait "$MCP_PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

pick_port() {
  python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

if [[ -n "${KHEISH_E2E_PORT:-}" ]]; then
  PORT="${KHEISH_E2E_PORT}"
else
  PORT="$(pick_port)"
fi
if [[ -n "${KHEISH_E2E_MCP_PORT:-}" ]]; then
  MCP_PORT="${KHEISH_E2E_MCP_PORT}"
else
  MCP_PORT="$(pick_port)"
fi
BASE_URL="http://127.0.0.1:$PORT"
MCP_URL="http://127.0.0.1:$MCP_PORT/mcp"

if "$BIN" --base-url "$BASE_URL" status --output json >/dev/null 2>&1; then
  echo "refusing to reuse live daemon at $BASE_URL" >&2
  exit 1
fi

cat >"$MCP_FIXTURE" <<'PY'
import hmac
import json
import os
import pathlib
import sys
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    server_version = "KheishMcpBearerFixture/1.0"

    def log_message(self, fmt, *args):
        return

    def write_event(self, payload):
        payload["ts"] = time.time()
        with self.server.log_lock:
            with open(self.server.request_log, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(payload, sort_keys=True) + "\n")

    def send_json(self, status, payload):
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def send_empty(self, status):
        self.send_response(status)
        self.send_header("content-length", "0")
        self.end_headers()

    def auth_ok(self):
        expected = f"Bearer {self.server.expected_token}"
        actual = self.headers.get("authorization", "")
        return hmac.compare_digest(actual, expected)

    def do_GET(self):
        self.write_event({
            "http_method": "GET",
            "path": self.path,
            "auth_ok": self.auth_ok(),
        })
        self.send_empty(405)

    def do_DELETE(self):
        self.write_event({
            "http_method": "DELETE",
            "path": self.path,
            "auth_ok": self.auth_ok(),
        })
        self.send_empty(405)

    def do_POST(self):
        length = int(self.headers.get("content-length", "0"))
        raw_body = self.rfile.read(length)
        try:
            body = json.loads(raw_body.decode("utf-8") or "{}")
        except Exception:
            body = {}
        method = body.get("method")
        request_id = body.get("id")
        auth_ok = self.auth_ok()
        self.write_event({
            "http_method": "POST",
            "path": self.path,
            "auth_ok": auth_ok,
            "jsonrpc_method": method,
            "has_id": request_id is not None,
        })
        if self.path != "/mcp":
            self.send_empty(404)
            return
        if not auth_ok:
            self.send_response(401)
            self.send_header("www-authenticate", 'Bearer realm="kheish-e2e"')
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if request_id is None:
            self.send_empty(202)
            return
        if method == "initialize":
            self.send_json(200, {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {"tools": {}},
                    "serverInfo": {
                        "name": "kheish-e2e-secret-mcp",
                        "version": "1.0.0",
                    },
                    "instructions": "Secret-store bearer fixture.",
                },
            })
            return
        if method == "tools/list":
            self.send_json(200, {
                "jsonrpc": "2.0",
                "id": request_id,
                "result": {"tools": []},
            })
            return
        self.send_json(200, {
            "jsonrpc": "2.0",
            "id": request_id,
            "result": {},
        })


def main():
    port = int(sys.argv[1])
    request_log = sys.argv[2]
    ready_path = pathlib.Path(sys.argv[3])
    expected = os.environ["KHEISH_E2E_EXPECTED_BEARER"]
    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.expected_token = expected
    server.request_log = request_log
    server.log_lock = threading.Lock()
    ready_path.write_text(json.dumps({"status": "ready", "port": port}) + "\n")
    server.serve_forever()


if __name__ == "__main__":
    main()
PY

KHEISH_E2E_EXPECTED_BEARER="$SECRET_VALUE" \
  python3 "$MCP_FIXTURE" "$MCP_PORT" "$MCP_REQUEST_LOG" "$MCP_READY" \
  >"$MCP_FIXTURE_LOG" 2>&1 &
MCP_PID=$!

for _ in $(seq 1 80); do
  if [[ -s "$MCP_READY" ]]; then
    break
  fi
  if ! kill -0 "$MCP_PID" >/dev/null 2>&1; then
    echo "MCP fixture exited before becoming ready" >&2
    cat "$MCP_FIXTURE_LOG" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ ! -s "$MCP_READY" ]]; then
  echo "MCP fixture did not become ready" >&2
  exit 1
fi

cat >"$CONFIG" <<TOML
[mcp_servers.secretHttp]
url = "$MCP_URL"
bearer_token_secret_ref = "mcp.custom.secretHttp.BEARER_TOKEN"
TOML

export KHEISH_AUTH_STORE_MASTER_KEY="$("$BIN" secrets generate)"
printf '%s' "$SECRET_VALUE" | "$BIN" secrets set mcp.custom.secretHttp.BEARER_TOKEN \
  --provider generic \
  --stdin \
  --offline \
  --state-root "$STATE_ROOT" \
  --output json \
  >"$EVIDENCE/secret-set.json"

"$BIN" serve \
  --bind "127.0.0.1:$PORT" \
  --state-root "$STATE_ROOT" \
  --workspace-root "$WORKSPACE_ROOT" \
  --mcp-discovery disabled \
  --mcp-config "$CONFIG" \
  --provider openai \
  --model gpt-5.4 \
  --api-key "${OPENAI_API_KEY:-test-openai-key}" \
  >"$LOG" 2>&1 &
PID=$!

for _ in $(seq 1 80); do
  if "$BIN" --base-url "$BASE_URL" status --output json >"$EVIDENCE/status.json" 2>"$EVIDENCE/status.err"; then
    break
  fi
  if ! kill -0 "$PID" >/dev/null 2>&1; then
    echo "daemon exited before becoming ready" >&2
    cat "$LOG" >&2 || true
    exit 1
  fi
  sleep 0.25
done

"$BIN" --base-url "$BASE_URL" runtime get --output json >"$EVIDENCE/runtime.json"
"$BIN" --base-url "$BASE_URL" secrets get mcp.custom.secretHttp.BEARER_TOKEN --output json >"$EVIDENCE/secret-status.json"

jq -e '
  .mcp.servers[] |
  select(.server == "secretHttp") |
  .uses_credentials == true
  and .credential_secret_refs == ["mcp.custom.secretHttp.BEARER_TOKEN"]
  and .connected == true
' "$EVIDENCE/runtime.json" >/dev/null

jq -s -e '
  any(.[]; .http_method == "POST" and .jsonrpc_method == "initialize" and .auth_ok == true)
  and any(.[]; .http_method == "POST" and .jsonrpc_method == "tools/list" and .auth_ok == true)
  and all(.[]; .auth_ok == true)
' "$MCP_REQUEST_LOG" >/dev/null

if grep -R --binary-files=without-match "$SECRET_VALUE" "$EVIDENCE"; then
  echo "secret value leaked into evidence" >&2
  exit 1
fi

python3 - "$BASE_URL/v1/runtime/secrets/mcp.custom.secretHttp.BEARER_TOKEN" "$EVIDENCE/delete-api-body.txt" >"$EVIDENCE/delete-api-status.txt" <<'PY'
import pathlib
import sys
import urllib.error
import urllib.request

request = urllib.request.Request(sys.argv[1], method="DELETE")
try:
    with urllib.request.urlopen(request, timeout=5) as response:
        code = response.getcode()
        body = response.read()
except urllib.error.HTTPError as error:
    code = error.code
    body = error.read()
pathlib.Path(sys.argv[2]).write_bytes(body)
print(code)
PY
grep -q '^409$' "$EVIDENCE/delete-api-status.txt"
grep -q "still referenced by one or more MCP servers" "$EVIDENCE/delete-api-body.txt"

jq -n \
  --arg run_id "$RUN_ID" \
  --arg base_url "$BASE_URL" \
  --arg mcp_url "$MCP_URL" \
  --arg evidence "$EVIDENCE" \
  '{status:"passed", run_id:$run_id, base_url:$base_url, mcp_url:$mcp_url, evidence:$evidence}' \
  >"$EVIDENCE/verdict.json"

cat "$EVIDENCE/verdict.json"
