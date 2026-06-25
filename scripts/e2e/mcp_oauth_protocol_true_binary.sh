#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/debug/kheish-daemon"
if [[ ! -x "$BIN" ]]; then
  echo "missing daemon binary: $BIN" >&2
  echo "run: cargo build -p kheish-daemon" >&2
  exit 2
fi

RUN_ID="mcp-oauth-protocol-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE="$ROOT/tmp/e2e/$RUN_ID"
STATE_ROOT="$EVIDENCE/state"
WORKSPACE_ROOT="$EVIDENCE/workspace"
CONFIG="$EVIDENCE/mcp.oauth.toml"
DAEMON_LOG="$EVIDENCE/daemon.log"
OAUTH_LOG="$EVIDENCE/oauth-fixture.log"
OAUTH_EVENTS="$EVIDENCE/oauth-events.jsonl"
OAUTH_READY="$EVIDENCE/oauth-ready.json"
LOGIN_STDERR="$EVIDENCE/login-stderr.json"
LOGIN_STDOUT="$EVIDENCE/login-status.json"
PID=""
OAUTH_PID=""

mkdir -p "$STATE_ROOT" "$WORKSPACE_ROOT"

cleanup() {
  if [[ -n "$PID" ]] && kill -0 "$PID" >/dev/null 2>&1; then
    kill "$PID" >/dev/null 2>&1 || true
    wait "$PID" >/dev/null 2>&1 || true
  fi
  if [[ -n "$OAUTH_PID" ]] && kill -0 "$OAUTH_PID" >/dev/null 2>&1; then
    kill "$OAUTH_PID" >/dev/null 2>&1 || true
    wait "$OAUTH_PID" >/dev/null 2>&1 || true
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
  PORT="$KHEISH_E2E_PORT"
else
  PORT="$(pick_port)"
fi
if [[ -n "${KHEISH_E2E_MCP_OAUTH_PORT:-}" ]]; then
  MCP_PORT="$KHEISH_E2E_MCP_OAUTH_PORT"
else
  MCP_PORT="$(pick_port)"
fi

BASE_URL="http://127.0.0.1:$PORT"
MCP_URL="http://127.0.0.1:$MCP_PORT/mcp"

if "$BIN" --base-url "$BASE_URL" status --output json >/dev/null 2>&1; then
  echo "refusing to reuse live daemon at $BASE_URL" >&2
  exit 2
fi

python3 - "$MCP_PORT" "$OAUTH_EVENTS" "$OAUTH_READY" <<'PY' >"$OAUTH_LOG" 2>&1 &
import base64
import hashlib
import json
import pathlib
import sys
import threading
import time
import urllib.parse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT = int(sys.argv[1])
EVENTS = sys.argv[2]
READY = pathlib.Path(sys.argv[3])
ORIGIN = f"http://127.0.0.1:{PORT}"
MCP_URL = f"{ORIGIN}/mcp"
ISSUER = f"{ORIGIN}/oauth"


class Handler(BaseHTTPRequestHandler):
    server_version = "KheishMcpOAuthFixture/1.0"

    def log_message(self, fmt, *args):
        return

    def event(self, payload):
        payload["ts"] = time.time()
        with self.server.lock:
            with open(EVENTS, "a", encoding="utf-8") as handle:
                handle.write(json.dumps(payload, sort_keys=True) + "\n")

    def send_json(self, status, payload):
        body = json.dumps(payload, sort_keys=True).encode("utf-8")
        self.send_response(status)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        parsed = urllib.parse.urlparse(self.path)
        query = urllib.parse.parse_qs(parsed.query)
        self.event({
            "method": "GET",
            "path": parsed.path,
            "has_authorization": bool(self.headers.get("authorization")),
        })
        if parsed.path == "/mcp":
            self.send_response(401)
            self.send_header(
                "www-authenticate",
                f'Bearer resource_metadata="{ORIGIN}/.well-known/oauth-protected-resource"',
            )
            self.send_header("content-length", "0")
            self.end_headers()
            return
        if parsed.path == "/.well-known/oauth-protected-resource":
            self.send_json(200, {
                "resource": MCP_URL,
                "authorization_servers": [ISSUER],
                "scopes_supported": ["read"],
            })
            return
        if parsed.path in (
            "/.well-known/oauth-authorization-server/oauth",
            "/.well-known/openid-configuration/oauth",
            "/oauth/.well-known/openid-configuration",
        ):
            self.send_json(200, {
                "issuer": ISSUER,
                "authorization_endpoint": f"{ISSUER}/authorize",
                "token_endpoint": f"{ISSUER}/token",
                "registration_endpoint": f"{ISSUER}/register",
                "scopes_supported": ["read"],
                "code_challenge_methods_supported": ["S256"],
            })
            return
        if parsed.path == "/oauth/authorize":
            client_id = (query.get("client_id") or [""])[0]
            response_type = (query.get("response_type") or [""])[0]
            redirect_uri = (query.get("redirect_uri") or [""])[0]
            state = (query.get("state") or [""])[0]
            resource = (query.get("resource") or [""])[0]
            scope = (query.get("scope") or [""])[0]
            code_challenge = (query.get("code_challenge") or [""])[0]
            challenge_method = (query.get("code_challenge_method") or [""])[0]
            valid = (
                client_id == self.server.registered_client_id
                and response_type == "code"
                and redirect_uri == self.server.registered_redirect_uri
                and resource == MCP_URL
                and scope == "read"
                and challenge_method == "S256"
                and bool(code_challenge)
                and bool(state)
            )
            self.event({
                "method": "GET",
                "path": parsed.path,
                "authorization_request_valid": valid,
                "client_id": client_id,
                "resource": resource,
                "scope": scope,
            })
            if not valid:
                self.send_json(400, {"error": "invalid_authorization_request"})
                return
            self.server.auth_codes["fixture-code-1"] = {
                "code_challenge": code_challenge,
                "redirect_uri": redirect_uri,
                "client_id": client_id,
            }
            location = f"{redirect_uri}?code=fixture-code-1&state={urllib.parse.quote(state)}"
            self.send_response(302)
            self.send_header("location", location)
            self.send_header("content-length", "0")
            self.end_headers()
            return
        self.send_response(404)
        self.send_header("content-length", "0")
        self.end_headers()

    def do_POST(self):
        parsed = urllib.parse.urlparse(self.path)
        length = int(self.headers.get("content-length", "0"))
        raw = self.rfile.read(length).decode("utf-8")
        content_type = self.headers.get("content-type", "")
        if "application/json" in content_type:
            try:
                body = json.loads(raw or "{}")
            except Exception:
                body = {}
        else:
            body = {key: values[0] for key, values in urllib.parse.parse_qs(raw).items()}
        self.event({
            "method": "POST",
            "path": parsed.path,
            "grant_type": body.get("grant_type"),
            "has_code_verifier": bool(body.get("code_verifier")),
            "has_refresh_token": bool(body.get("refresh_token")),
            "resource": body.get("resource"),
        })
        if parsed.path == "/oauth/register":
            redirect_uris = body.get("redirect_uris") or []
            grant_types = set(body.get("grant_types") or [])
            response_types = set(body.get("response_types") or [])
            valid = (
                body.get("client_name") == "Kheish"
                and len(redirect_uris) == 1
                and str(redirect_uris[0]).startswith("http://127.0.0.1:")
                and {"authorization_code", "refresh_token"}.issubset(grant_types)
                and "code" in response_types
                and body.get("token_endpoint_auth_method") == "none"
            )
            self.event({
                "method": "POST",
                "path": parsed.path,
                "dcr_valid": valid,
                "redirect_uri": redirect_uris[0] if redirect_uris else "",
            })
            if not valid:
                self.send_json(400, {"error": "invalid_client_metadata"})
                return
            self.server.registered_client_id = "fixture-client"
            self.server.registered_redirect_uri = redirect_uris[0]
            self.send_json(201, {"client_id": "fixture-client"})
            return
        if parsed.path == "/oauth/token":
            grant_type = body.get("grant_type")
            if body.get("resource") != MCP_URL:
                self.send_json(400, {"error": "invalid_target"})
                return
            if grant_type == "authorization_code":
                code = body.get("code")
                verifier = body.get("code_verifier") or ""
                challenge = base64.urlsafe_b64encode(
                    hashlib.sha256(verifier.encode("utf-8")).digest()
                ).decode("ascii").rstrip("=")
                code_record = self.server.auth_codes.get(code)
                valid = (
                    code_record is not None
                    and body.get("client_id") == code_record["client_id"]
                    and body.get("redirect_uri") == code_record["redirect_uri"]
                    and challenge == code_record["code_challenge"]
                )
                self.event({
                    "method": "POST",
                    "path": parsed.path,
                    "grant_type": grant_type,
                    "pkce_verified": valid,
                    "client_id": body.get("client_id"),
                    "resource": body.get("resource"),
                })
                if not valid:
                    self.send_json(400, {"error": "invalid_grant"})
                    return
                self.send_json(200, {
                    "access_token": "access-token-1",
                    "refresh_token": "refresh-token-1",
                    "expires_in": 3600,
                    "scope": "read",
                    "token_type": "Bearer",
                })
                return
            if grant_type == "refresh_token":
                if body.get("refresh_token") != "refresh-token-1":
                    self.send_json(400, {"error": "invalid_grant"})
                    return
                self.send_json(200, {
                    "access_token": "access-token-2",
                    "refresh_token": "refresh-token-2",
                    "expires_in": 3600,
                    "scope": "read",
                    "token_type": "Bearer",
                })
                return
        self.send_response(404)
        self.send_header("content-length", "0")
        self.end_headers()


server = ThreadingHTTPServer(("127.0.0.1", PORT), Handler)
server.lock = threading.Lock()
server.registered_client_id = ""
server.registered_redirect_uri = ""
server.auth_codes = {}
READY.write_text(json.dumps({"status": "ready", "port": PORT}) + "\n")
server.serve_forever()
PY
OAUTH_PID="$!"

for _ in $(seq 1 80); do
  if [[ -s "$OAUTH_READY" ]]; then
    break
  fi
  if ! kill -0 "$OAUTH_PID" >/dev/null 2>&1; then
    echo "OAuth fixture exited before becoming ready" >&2
    cat "$OAUTH_LOG" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ ! -s "$OAUTH_READY" ]]; then
  echo "OAuth fixture did not become ready" >&2
  exit 1
fi

export KHEISH_AUTH_STORE_MASTER_KEY="$("$BIN" secrets generate)"

start_daemon() {
  local config_path="${1:-}"
  local command=(
    "$BIN" serve
    --bind "127.0.0.1:$PORT"
    --state-root "$STATE_ROOT"
    --workspace-root "$WORKSPACE_ROOT"
    --mcp-discovery disabled
  )
  if [[ -n "${1:-}" ]]; then
    command+=(--mcp-config "$config_path")
  fi
  command+=(
    --provider openai
    --model gpt-5.4
    --api-key "${OPENAI_API_KEY:-test-openai-key}"
  )
  "${command[@]}" >"$DAEMON_LOG" 2>&1 &
  PID="$!"
  for _ in $(seq 1 100); do
    if "$BIN" --base-url "$BASE_URL" status --output json >"$EVIDENCE/status.json" 2>"$EVIDENCE/status.err"; then
      return 0
    fi
    if ! kill -0 "$PID" >/dev/null 2>&1; then
      echo "daemon exited during startup" >&2
      cat "$DAEMON_LOG" >&2 || true
      exit 1
    fi
    sleep 0.2
  done
  echo "daemon did not become ready at $BASE_URL" >&2
  cat "$DAEMON_LOG" >&2 || true
  exit 1
}

stop_daemon() {
  if [[ -n "$PID" ]] && kill -0 "$PID" >/dev/null 2>&1; then
    kill "$PID" >/dev/null 2>&1 || true
    wait "$PID" >/dev/null 2>&1 || true
  fi
  PID=""
}

start_daemon ""

"$BIN" --base-url "$BASE_URL" mcp oauth login oauthHttp \
  --url "$MCP_URL" \
  --no-open \
  --allow-http-for-loopback \
  --scopes read \
  --timeout-sec 60 \
  --output json \
  >"$LOGIN_STDOUT" 2>"$LOGIN_STDERR" &
LOGIN_PID="$!"

AUTH_URL=""
for _ in $(seq 1 100); do
  if [[ -s "$LOGIN_STDERR" ]]; then
    AUTH_URL="$(python3 - "$LOGIN_STDERR" <<'PY' 2>/dev/null || true
import json, sys
try:
    print(json.loads(open(sys.argv[1], encoding="utf-8").read())["authorization_url"])
except Exception:
    pass
PY
)"
    if [[ -n "$AUTH_URL" ]]; then
      break
    fi
  fi
  if ! kill -0 "$LOGIN_PID" >/dev/null 2>&1; then
    echo "OAuth login command exited before printing authorization URL" >&2
    cat "$LOGIN_STDERR" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ -z "$AUTH_URL" ]]; then
  echo "OAuth login command did not print authorization URL" >&2
  cat "$LOGIN_STDERR" >&2 || true
  exit 1
fi

python3 - "$AUTH_URL" "$EVIDENCE/browser-callback.txt" <<'PY'
import pathlib
import sys
import urllib.request
body = urllib.request.urlopen(sys.argv[1], timeout=10).read().decode("utf-8")
pathlib.Path(sys.argv[2]).write_text(body + "\n")
PY

wait "$LOGIN_PID"

"$BIN" --base-url "$BASE_URL" mcp oauth login oauthBadState \
  --url "$MCP_URL" \
  --no-open \
  --allow-http-for-loopback \
  --scopes read \
  --timeout-sec 60 \
  --output json \
  >"$EVIDENCE/bad-state-login-stdout.json" 2>"$EVIDENCE/bad-state-login-stderr.json" &
BAD_STATE_LOGIN_PID="$!"

BAD_STATE_REDIRECT_URI=""
for _ in $(seq 1 100); do
  if [[ -s "$EVIDENCE/bad-state-login-stderr.json" ]]; then
    BAD_STATE_REDIRECT_URI="$(python3 - "$EVIDENCE/bad-state-login-stderr.json" <<'PY' 2>/dev/null || true
import json, sys
try:
    print(json.loads(open(sys.argv[1], encoding="utf-8").read())["redirect_uri"])
except Exception:
    pass
PY
)"
    if [[ -n "$BAD_STATE_REDIRECT_URI" ]]; then
      break
    fi
  fi
  if ! kill -0 "$BAD_STATE_LOGIN_PID" >/dev/null 2>&1; then
    echo "bad-state OAuth login command exited before printing redirect URI" >&2
    cat "$EVIDENCE/bad-state-login-stderr.json" >&2 || true
    exit 1
  fi
  sleep 0.1
done
if [[ -z "$BAD_STATE_REDIRECT_URI" ]]; then
  echo "bad-state OAuth login command did not print redirect URI" >&2
  cat "$EVIDENCE/bad-state-login-stderr.json" >&2 || true
  exit 1
fi

set +e
python3 - "$BAD_STATE_REDIRECT_URI" "$EVIDENCE/bad-state-callback.txt" <<'PY'
import pathlib
import sys
import urllib.request
url = f"{sys.argv[1]}?code=fixture-code-bad-state&state=wrong-state"
body = urllib.request.urlopen(url, timeout=10).read().decode("utf-8")
pathlib.Path(sys.argv[2]).write_text(body + "\n")
PY
wait "$BAD_STATE_LOGIN_PID"
BAD_STATE_STATUS="$?"
set -e
if [[ "$BAD_STATE_STATUS" == "0" ]]; then
  echo "bad-state OAuth login unexpectedly succeeded" >&2
  exit 1
fi

"$BIN" --base-url "$BASE_URL" mcp oauth status oauthHttp --output json \
  >"$EVIDENCE/oauth-status-before-refresh.json"
"$BIN" --base-url "$BASE_URL" runtime auth accounts list --output json \
  >"$EVIDENCE/oauth-accounts-before-refresh.json"
"$BIN" --base-url "$BASE_URL" mcp oauth refresh oauthHttp --output json \
  >"$EVIDENCE/oauth-refresh.json"
"$BIN" --base-url "$BASE_URL" runtime auth accounts get mcp.oauth.oauthHttp --output json \
  >"$EVIDENCE/oauth-status-after-refresh.json"

stop_daemon

cat >"$CONFIG" <<TOML
[mcp_servers.oauthHttp]
url = "$MCP_URL"
oauth_slot_ref = "mcp.oauth.oauthHttp"
oauth_resource = "$MCP_URL"
oauth_scopes = ["read"]
TOML

start_daemon "$CONFIG"
"$BIN" --base-url "$BASE_URL" runtime get --output json >"$EVIDENCE/runtime-with-oauth-mcp.json"

python3 - "$EVIDENCE" "$OAUTH_EVENTS" "$MCP_URL" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
events_path = pathlib.Path(sys.argv[2])
expected_resource = sys.argv[3]
errors = []

login = json.loads((root / "login-status.json").read_text())
accounts_before = json.loads((root / "oauth-accounts-before-refresh.json").read_text())
refresh = json.loads((root / "oauth-refresh.json").read_text())
status_after = json.loads((root / "oauth-status-after-refresh.json").read_text())
runtime = json.loads((root / "runtime-with-oauth-mcp.json").read_text())
events = [
    json.loads(line)
    for line in events_path.read_text().splitlines()
    if line.strip()
]

if login.get("provider") != "mcp_oauth":
    errors.append("login did not create an mcp_oauth account")
if login.get("mode") != "oauth_account":
    errors.append("login account mode is not oauth_account")
if login.get("details", {}).get("resource") != expected_resource:
    errors.append("login account resource does not match the MCP resource URL")
if any(account.get("slot_id") == "mcp.oauth.oauthBadState" for account in accounts_before):
    errors.append("bad-state login stored an OAuth account")
if refresh.get("details", {}).get("last_refresh_outcome") != "success":
    errors.append("forced refresh did not report last_refresh_outcome=success")
if status_after.get("details", {}).get("last_refresh_at_ms") is None:
    errors.append("refreshed account does not expose last_refresh_at_ms")
servers = (runtime.get("mcp") or {}).get("servers") or []
oauth_server = next((server for server in servers if server.get("server") == "oauthHttp"), None)
if oauth_server is None:
    errors.append("oauthHttp MCP server is missing from runtime snapshot")
else:
    if oauth_server.get("connected") is not False:
        errors.append("oauthHttp should be fail-closed/disconnected at boot")
    if oauth_server.get("error") != "oauth_requires_scoped_runtime_initialization":
        errors.append("oauthHttp did not report oauth_requires_scoped_runtime_initialization")
    if "mcp.oauth.oauthHttp" not in (oauth_server.get("credential_secret_refs") or []):
        errors.append("oauthHttp did not expose the OAuth slot ref")
if not any(event.get("path") == "/oauth/register" and event.get("dcr_valid") for event in events):
    errors.append("valid dynamic client registration was not exercised")
if not any(event.get("path") == "/oauth/authorize" and event.get("authorization_request_valid") for event in events):
    errors.append("valid authorization request was not exercised")
if not any(event.get("grant_type") == "authorization_code" and event.get("pkce_verified") for event in events):
    errors.append("authorization_code exchange with verified PKCE was not exercised")
if not any(event.get("grant_type") == "refresh_token" and event.get("has_refresh_token") for event in events):
    errors.append("refresh_token grant was not exercised")
bad_state_body = (root / "bad-state-callback.txt").read_text()
if "failed" not in bad_state_body.lower():
    errors.append("bad-state callback did not fail visibly")
if any(event.get("has_authorization") for event in events if event.get("path") == "/mcp"):
    errors.append("daemon sent an Authorization header to MCP resource during boot")

verdict = {
    "scenario": "mcp_oauth_protocol_true_binary",
    "status": "failed" if errors else "passed",
    "errors": errors,
    "events": len(events),
    "evidence_root": str(root),
}
(root / "verdict-before-logout.json").write_text(json.dumps(verdict, indent=2) + "\n")
if errors:
    for error in errors:
        print(f"FAIL: {error}", file=sys.stderr)
    sys.exit(1)
print(json.dumps(verdict, indent=2))
PY

stop_daemon
start_daemon ""

"$BIN" --base-url "$BASE_URL" mcp oauth logout oauthHttp --output json \
  >"$EVIDENCE/oauth-logout.json"
"$BIN" --base-url "$BASE_URL" runtime auth accounts list --output json \
  >"$EVIDENCE/oauth-accounts-after-logout.json"

python3 - "$EVIDENCE" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
errors = []
accounts = json.loads((root / "oauth-accounts-after-logout.json").read_text())
logout = json.loads((root / "oauth-logout.json").read_text())
if logout.get("accepted") is not True:
    errors.append("logout did not delete the OAuth account")
if any(account.get("slot_id") == "mcp.oauth.oauthHttp" for account in accounts):
    errors.append("OAuth account still appears after logout")
for marker in ("access-token-1", "access-token-2", "refresh-token-1", "refresh-token-2"):
    for path in root.rglob("*"):
        if path.is_file() and marker in path.read_text(errors="ignore"):
            errors.append(f"token marker leaked into evidence file {path.name}")
            break
previous = json.loads((root / "verdict-before-logout.json").read_text())
errors = previous.get("errors", []) + errors
verdict = {
    "scenario": "mcp_oauth_protocol_true_binary",
    "status": "failed" if errors else "passed",
    "errors": errors,
    "evidence_root": str(root),
}
(root / "verdict.json").write_text(json.dumps(verdict, indent=2) + "\n")
if errors:
    for error in errors:
        print(f"FAIL: {error}", file=sys.stderr)
    sys.exit(1)
print(json.dumps(verdict, indent=2))
PY

echo "evidence: $EVIDENCE"
