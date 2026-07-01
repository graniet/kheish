#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/debug/kheish-daemon"
RUN_ID="stack-as-code-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE="$ROOT/tmp/e2e/$RUN_ID"
STATE_ROOT="$EVIDENCE/state"
WORKSPACE_ROOT="$EVIDENCE/workspace"
LOG="$EVIDENCE/daemon.log"
PID=""

mkdir -p "$STATE_ROOT" "$WORKSPACE_ROOT"
cd "$ROOT"

cleanup() {
  if [[ -n "$PID" ]] && kill -0 "$PID" >/dev/null 2>&1; then
    kill "$PID" >/dev/null 2>&1 || true
    wait "$PID" >/dev/null 2>&1 || true
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

if [[ "${KHEISH_E2E_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon 2>&1 | tee "$EVIDENCE/build.log"
fi

if [[ ! -x "$BIN" ]]; then
  echo "missing daemon binary: $BIN" >&2
  exit 1
fi

PORT="${KHEISH_E2E_PORT:-$(pick_port)}"
BASE_URL="http://127.0.0.1:$PORT"

if "$BIN" --base-url "$BASE_URL" status --output json >/dev/null 2>&1; then
  echo "refusing to reuse live daemon at $BASE_URL" >&2
  exit 1
fi

export KHEISH_AUTH_STORE_MASTER_KEY="${KHEISH_AUTH_STORE_MASTER_KEY:-0123456789abcdef0123456789abcdef}"
export KHEISH_STACK_E2E_MANAGED_SECRET="managed-secret-e2e-value"

"$BIN" serve \
  --bind "127.0.0.1:$PORT" \
  --state-root "$STATE_ROOT" \
  --workspace-root "$WORKSPACE_ROOT" \
  --provider openai \
  --model gpt-5.4 \
  --api-key "sk-e2e-no-network" \
  --http-auth-mode none \
  >"$LOG" 2>&1 &
PID="$!"

for _ in {1..80}; do
  if "$BIN" --base-url "$BASE_URL" --output json status >"$EVIDENCE/status.json" 2>"$EVIDENCE/status.err"; then
    break
  fi
  sleep 0.25
done

"$BIN" --base-url "$BASE_URL" --output json status >"$EVIDENCE/status-ready.json"

"$BIN" --base-url "$BASE_URL" --output json secrets set \
  mcp.linear.LINEAR_API_KEY \
  --provider generic \
  --value "linear-e2e-token" \
  >"$EVIDENCE/linear-secret.json"

"$BIN" --base-url "$BASE_URL" --output json stack init \
  --output-file "$EVIDENCE/generic/Kheishfile.yaml" \
  --name stack-as-code-e2e \
  >"$EVIDENCE/init.out"
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/generic/Kheishfile.yaml" \
  >"$EVIDENCE/generic-validate.json"

cat >"$EVIDENCE/fail-open-scope.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: fail-open-scope
spec:
  personas:
    - persona_id: fail-open-persona
      display_name: Fail Open Persona
      soul: Must not validate.
      capability_scope:
        mcp_server_allow: ["linear"]
        mcp_tool_allow: ["mcp__linear__list_issues"]
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/fail-open-scope.yaml" \
  >"$EVIDENCE/fail-open-scope.json" 2>"$EVIDENCE/fail-open-scope.err"
FAIL_OPEN_STATUS=$?
set -e
if [[ "$FAIL_OPEN_STATUS" -eq 0 ]]; then
  echo "fail-open scope fixture unexpectedly validated" >&2
  exit 1
fi

cat >"$EVIDENCE/partial-deny-scope.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: partial-deny-scope
spec:
  personas:
    - persona_id: partial-deny-persona
      display_name: Partial Deny Persona
      soul: Must not validate.
      capability_scope:
        skill_deny: ["bash"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/partial-deny-scope.yaml" \
  >"$EVIDENCE/partial-deny-scope.json" 2>"$EVIDENCE/partial-deny-scope.err"
PARTIAL_DENY_STATUS=$?
set -e
if [[ "$PARTIAL_DENY_STATUS" -eq 0 ]]; then
  echo "partial deny scope fixture unexpectedly validated" >&2
  exit 1
fi

cat >"$EVIDENCE/fail-open-connector.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: fail-open-connector
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/fail-open-connector.yaml" \
  >"$EVIDENCE/fail-open-connector.json" 2>"$EVIDENCE/fail-open-connector.err"
FAIL_OPEN_CONNECTOR_STATUS=$?
set -e
if [[ "$FAIL_OPEN_CONNECTOR_STATUS" -eq 0 ]]; then
  echo "fail-open connector session_policy fixture unexpectedly validated" >&2
  exit 1
fi

cat >"$EVIDENCE/unknown-connector-field.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: unknown-connector-field
spec:
  sessions:
    - session_id: ingress-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  connectors:
    - kind: http
      name: ingress
      spec:
        fixed_session_id: ingress-session
        require_hmac_signatre: true
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/unknown-connector-field.yaml" \
  >"$EVIDENCE/unknown-connector-field.json" 2>"$EVIDENCE/unknown-connector-field.err"
UNKNOWN_CONNECTOR_STATUS=$?
set -e
if [[ "$UNKNOWN_CONNECTOR_STATUS" -eq 0 ]]; then
  echo "unknown connector field fixture unexpectedly validated" >&2
  exit 1
fi

cat >"$EVIDENCE/inline-connector-secret.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: inline-connector-secret
spec:
  sessions:
    - session_id: ingress-session
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  connectors:
    - kind: http
      name: ingress
      spec:
        fixed_session_id: ingress-session
        bearer_token:
          value: not-drift-safe
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/inline-connector-secret.yaml" \
  >"$EVIDENCE/inline-connector-secret.json" 2>"$EVIDENCE/inline-connector-secret.err"
INLINE_CONNECTOR_SECRET_STATUS=$?
set -e
if [[ "$INLINE_CONNECTOR_SECRET_STATUS" -eq 0 ]]; then
  echo "inline connector secret fixture unexpectedly validated" >&2
  exit 1
fi

cat >"$EVIDENCE/external-session-persona.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: external-session-persona
spec:
  sessions:
    - session_id: ingress-session
      persona_id: external-persona
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/external-session-persona.yaml" \
  >"$EVIDENCE/external-session-persona.json" 2>"$EVIDENCE/external-session-persona.err"
EXTERNAL_SESSION_PERSONA_STATUS=$?
set -e
if [[ "$EXTERNAL_SESSION_PERSONA_STATUS" -eq 0 ]]; then
  echo "external session persona fixture unexpectedly validated" >&2
  exit 1
fi
set +e
"$BIN" --base-url "$BASE_URL" --output json stack import \
  --file "$EVIDENCE/external-session-persona.yaml" \
  --resource session/ingress-session \
  >"$EVIDENCE/external-session-persona-import.json" \
  2>"$EVIDENCE/external-session-persona-import.err"
EXTERNAL_SESSION_PERSONA_IMPORT_STATUS=$?
set -e
if [[ "$EXTERNAL_SESSION_PERSONA_IMPORT_STATUS" -eq 0 ]]; then
  echo "external session persona import unexpectedly succeeded" >&2
  exit 1
fi
if ! grep -q "422 Unprocessable Entity (stack_import_blocked)" "$EVIDENCE/external-session-persona-import.err"; then
  echo "external session persona import did not return stack_import_blocked" >&2
  exit 1
fi

cat >"$EVIDENCE/external-connector-persona.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: external-connector-persona
spec:
  connectors:
    - kind: http
      name: ingress
      spec:
        allow_unauthenticated_ingress: true
        session_policy:
          create_if_missing: true
          persona_id: external-persona
          capability_scope:
            skill_deny: ["*"]
            mcp_server_deny: ["*"]
            mcp_tool_deny: ["*"]
          credential_scope:
            route_deny: ["*"]
            connector_deny: ["*"]
            connector_credential_deny: ["*"]
            mcp_server_deny: ["*"]
YAML

set +e
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/external-connector-persona.yaml" \
  >"$EVIDENCE/external-connector-persona.json" 2>"$EVIDENCE/external-connector-persona.err"
EXTERNAL_CONNECTOR_PERSONA_STATUS=$?
set -e
if [[ "$EXTERNAL_CONNECTOR_PERSONA_STATUS" -eq 0 ]]; then
  echo "external connector persona fixture unexpectedly validated" >&2
  exit 1
fi

run_stack() {
  local name="$1"
  local file="$2"
  local prefix="$EVIDENCE/$name"
  mkdir -p "$prefix"
  "$BIN" --base-url "$BASE_URL" --output json stack validate \
    --file "$file" \
    >"$prefix/validate.json"
  "$BIN" --base-url "$BASE_URL" --output json stack plan \
    --file "$file" \
    >"$prefix/plan.json"
  "$BIN" --base-url "$BASE_URL" --output json stack apply \
    --file "$file" \
    >"$prefix/apply.json"
  "$BIN" --base-url "$BASE_URL" --output json stack verify \
    --file "$file" \
    >"$prefix/verify.json"
  "$BIN" --base-url "$BASE_URL" --output json stack apply \
    --file "$file" \
    >"$prefix/apply-second.json"
  "$BIN" --base-url "$BASE_URL" --output json stack verify \
    --file "$file" \
    >"$prefix/verify-second.json"
  "$BIN" --base-url "$BASE_URL" --output json stack diff \
    --file "$file" \
    >"$prefix/diff.json"
}

cat >"$EVIDENCE/runtime-stack.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: runtime-stack
spec:
  runtime:
    permission_mode: plan
YAML

mkdir -p "$EVIDENCE/runtime-stack"
"$BIN" --base-url "$BASE_URL" --output json stack validate \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/validate.json"
set +e
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/apply-before-import.json" 2>"$EVIDENCE/runtime-stack/apply-before-import.err"
RUNTIME_APPLY_BEFORE_IMPORT_STATUS=$?
set -e
if [[ "$RUNTIME_APPLY_BEFORE_IMPORT_STATUS" -eq 0 ]]; then
  echo "runtime stack apply before import unexpectedly succeeded" >&2
  exit 1
fi
"$BIN" --base-url "$BASE_URL" --output json stack import \
  --file "$EVIDENCE/runtime-stack.yaml" \
  --resource runtime/settings \
  >"$EVIDENCE/runtime-stack/import.json"
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/apply.json"
"$BIN" --base-url "$BASE_URL" --output json stack verify \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/verify.json"
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/apply-second.json"
"$BIN" --base-url "$BASE_URL" --output json stack diff \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/diff.json"
"$BIN" --base-url "$BASE_URL" --output json runtime get \
  >"$EVIDENCE/runtime-stack/runtime.json"

run_stack "generic" "$EVIDENCE/generic/Kheishfile.yaml"
run_stack "linear-triage-daily" "$ROOT/examples/stacks/linear-triage-daily/Kheishfile.yaml"
run_stack "playbook-review" "$ROOT/examples/stacks/playbook-review/Kheishfile.yaml"

mkdir -p "$EVIDENCE/managed-secret-retain"
cat >"$EVIDENCE/managed-secret-retain/Kheishfile.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: managed-secret-retain
spec:
  requires:
    secrets:
      - ref: stack.e2e.MANAGED_SECRET
        provider: generic
        value_env: KHEISH_STACK_E2E_MANAGED_SECRET
YAML
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/managed-secret-retain/Kheishfile.yaml" \
  --allow-secret-env \
  >"$EVIDENCE/managed-secret-retain/apply.json"
"$BIN" --base-url "$BASE_URL" --output json stack down \
  --file "$EVIDENCE/managed-secret-retain/Kheishfile.yaml" \
  --yes \
  >"$EVIDENCE/managed-secret-retain/down.json"
"$BIN" --base-url "$BASE_URL" --output json secrets get \
  stack.e2e.MANAGED_SECRET \
  >"$EVIDENCE/managed-secret-retain/secret-after-down.json"
cat >"$EVIDENCE/managed-secret-retain/Kheishfile-pruned.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: managed-secret-retain
spec: {}
YAML
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/managed-secret-retain/Kheishfile-pruned.yaml" \
  --prune \
  >"$EVIDENCE/managed-secret-retain/prune.json"
"$BIN" --base-url "$BASE_URL" --output json secrets get \
  stack.e2e.MANAGED_SECRET \
  >"$EVIDENCE/managed-secret-retain/secret-after-prune.json"
python3 - "$EVIDENCE/managed-secret-retain/down.json" "$EVIDENCE/managed-secret-retain/prune.json" <<'PY'
import json
import pathlib
import sys

down = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
prune = json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
if not any(action.get("resource_type") == "secret" and action.get("operation") == "blocked" for action in down.get("actions", [])):
    raise SystemExit("managed secret down did not report a blocked secret action")
plan_actions = prune.get("plan", {}).get("actions", [])
if not any(action.get("resource_type") == "secret" and action.get("operation") == "blocked" for action in plan_actions):
    raise SystemExit("managed secret prune did not retain the blocked secret in the plan")
if any(action.get("resource_type") == "secret" for action in prune.get("applied", [])):
    raise SystemExit("managed secret prune reported applying a secret deletion")
PY

mkdir -p "$EVIDENCE/partial-down"
cat >"$EVIDENCE/partial-down/Kheishfile.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: partial-down
spec:
  connectors:
    - kind: http
      name: partial-down-webhook
      spec:
        actor_id: partial-down
        fixed_session_id: partial-down
        allow_unauthenticated_ingress: true
        require_hmac_signature: false
        require_idempotency_key: false
        allow_payload_reply_targets: false
  personas:
    - persona_id: partial-down-persona
      display_name: Partial Down
      soul: This persona is intentionally retained by stack down.
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
  sessions:
    - session_id: partial-down
      persona_id: partial-down-persona
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
YAML
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/partial-down/Kheishfile.yaml" \
  >"$EVIDENCE/partial-down/apply.json"
"$BIN" --base-url "$BASE_URL" --output json stack down \
  --file "$EVIDENCE/partial-down/Kheishfile.yaml" \
  --yes \
  >"$EVIDENCE/partial-down/down.json"
set +e
"$BIN" --base-url "$BASE_URL" --output json connectors get \
  http partial-down-webhook \
  >"$EVIDENCE/partial-down/connector-after-down.json" \
  2>"$EVIDENCE/partial-down/connector-after-down.err"
PARTIAL_DOWN_CONNECTOR_GET_STATUS=$?
set -e
if [[ "$PARTIAL_DOWN_CONNECTOR_GET_STATUS" -eq 0 ]]; then
  echo "partial down did not delete the deletable connector" >&2
  exit 1
fi
python3 - "$BASE_URL" "$EVIDENCE/partial-down/down.json" "$EVIDENCE/partial-down/ledger-after-down.json" <<'PY'
import json
import pathlib
import sys
import urllib.parse
import urllib.request

base_url, down_path, ledger_path = sys.argv[1:4]
down = json.loads(pathlib.Path(down_path).read_text(encoding="utf-8"))
actions = down.get("actions", [])
if not any(action.get("resource_type") == "connector" and action.get("operation") == "delete" for action in actions):
    raise SystemExit("partial down did not plan connector deletion")
if not any(action.get("resource_type") == "persona" and action.get("operation") == "blocked" for action in actions):
    raise SystemExit("partial down did not retain blocked persona action")
ownership_id = urllib.parse.quote("partial-down", safe="")
with urllib.request.urlopen(f"{base_url}/v1/stacks/{ownership_id}/ledger", timeout=10) as response:
    payload = response.read()
pathlib.Path(ledger_path).write_bytes(payload)
ledger = json.loads(payload.decode("utf-8"))
resources = ledger.get("stack", {}).get("resources", {})
if "connector/http/partial-down-webhook" in resources:
    raise SystemExit("partial down left deleted connector in ledger")
if "persona/partial-down-persona" not in resources:
    raise SystemExit("partial down removed blocked persona from ledger")
PY

cat >"$EVIDENCE/prune-blocked-original.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: prune-blocked
spec:
  personas:
    - persona_id: prune-blocked-persona
      display_name: Prune Blocked Persona
      soul: This persona cannot be hard-deleted by prune.
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
YAML
cat >"$EVIDENCE/prune-blocked-pruned.yaml" <<'YAML'
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: prune-blocked
spec: {}
YAML
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/prune-blocked-original.yaml" \
  >"$EVIDENCE/prune-blocked-apply.json"
"$BIN" --base-url "$BASE_URL" --output json stack apply \
  --file "$EVIDENCE/prune-blocked-pruned.yaml" \
  --prune \
  >"$EVIDENCE/prune-blocked-prune.json"
python3 - "$BASE_URL" "$EVIDENCE/prune-blocked-prune.json" "$EVIDENCE/prune-blocked-ledger.json" <<'PY'
import json
import pathlib
import sys
import urllib.parse
import urllib.request

base_url, prune_path, ledger_path = sys.argv[1:4]
prune = json.loads(pathlib.Path(prune_path).read_text(encoding="utf-8"))
plan_actions = prune.get("plan", {}).get("actions", [])
if not any(action.get("resource_type") == "persona" and action.get("operation") == "blocked" for action in plan_actions):
    raise SystemExit("blocked persona prune did not retain the blocked action in the plan")
ownership_id = urllib.parse.quote("prune-blocked", safe="")
with urllib.request.urlopen(f"{base_url}/v1/stacks/{ownership_id}/ledger", timeout=10) as response:
    payload = response.read()
pathlib.Path(ledger_path).write_bytes(payload)
ledger = json.loads(payload.decode("utf-8"))
if "persona/prune-blocked-persona" not in ledger.get("stack", {}).get("resources", {}):
    raise SystemExit("blocked persona prune removed the persona from ledger")
PY

python3 - "$BASE_URL" "$ROOT/examples/stacks/playbook-review/Kheishfile.yaml" "$EVIDENCE/playbook-review/api-plan.json" "$EVIDENCE/playbook-review/api-ledger.json" "$EVIDENCE/raw-api-apply.json" "$EVIDENCE/raw-api-verify.json" "$EVIDENCE/raw-down-partial.json" "$EVIDENCE/raw-prune-partial.json" <<'PY'
import json
import pathlib
import sys
import urllib.parse
import urllib.request
import urllib.error

base_url, manifest_path, plan_path, ledger_path, raw_apply_path, raw_verify_path, raw_down_path, raw_prune_path = sys.argv[1:9]
manifest = pathlib.Path(manifest_path)
resolved_manifest = manifest.read_text(encoding="utf-8")
playbook_file = manifest.parent / "playbook.yaml"
if playbook_file.exists():
    embedded = "    - manifest:\n" + "".join(
        f"        {line}" for line in playbook_file.read_text(encoding="utf-8").splitlines(True)
    )
    resolved_manifest = resolved_manifest.replace("    - manifest_file: playbook.yaml\n", embedded)
body = json.dumps({
    "manifest": resolved_manifest,
    "strict_scopes": True,
    "only_changes": True,
}).encode("utf-8")
request = urllib.request.Request(
    f"{base_url}/v1/stacks/plan",
    data=body,
    headers={"content-type": "application/json"},
    method="POST",
)
with urllib.request.urlopen(request, timeout=10) as response:
    pathlib.Path(plan_path).write_bytes(response.read())

ownership_id = urllib.parse.quote("playbook-review", safe="")
with urllib.request.urlopen(f"{base_url}/v1/stacks/{ownership_id}/ledger", timeout=10) as response:
    pathlib.Path(ledger_path).write_bytes(response.read())

raw_manifest = """
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: raw-api-stack
spec:
  sessions:
    - session_id: raw-api-stack
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
  verification:
    - name: raw-session
      type: session_exists
      session_id: raw-api-stack
"""
raw_body = json.dumps({"manifest": raw_manifest, "strict_scopes": True}).encode("utf-8")
for endpoint, path in [
    ("/v1/stacks/apply", raw_apply_path),
    ("/v1/stacks/verify", raw_verify_path),
]:
    request = urllib.request.Request(
        f"{base_url}{endpoint}",
        data=raw_body,
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        pathlib.Path(path).write_bytes(response.read())

def post_json(endpoint, body, path):
    request = urllib.request.Request(
        f"{base_url}{endpoint}",
        data=json.dumps(body).encode("utf-8"),
        headers={"content-type": "application/json"},
        method="POST",
    )
    with urllib.request.urlopen(request, timeout=10) as response:
        pathlib.Path(path).write_bytes(response.read())

raw_partial_manifest = """
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: raw-partial-down
spec:
  connectors:
    - kind: http
      name: raw-partial-down-webhook
      spec:
        actor_id: raw-partial-down
        fixed_session_id: raw-partial-down
        allow_unauthenticated_ingress: true
        require_hmac_signature: false
        require_idempotency_key: false
        allow_payload_reply_targets: false
  personas:
    - persona_id: raw-partial-down-persona
      display_name: Raw Partial Down
      soul: Retained by raw HTTP down.
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
  sessions:
    - session_id: raw-partial-down
      persona_id: raw-partial-down-persona
      capability_scope:
        skill_deny: ["*"]
        mcp_server_deny: ["*"]
        mcp_tool_deny: ["*"]
      credential_scope:
        route_deny: ["*"]
        connector_deny: ["*"]
        connector_credential_deny: ["*"]
        mcp_server_deny: ["*"]
"""
post_json(
    "/v1/stacks/apply",
    {"manifest": raw_partial_manifest, "strict_scopes": True},
    str(pathlib.Path(raw_down_path).with_suffix(".apply.json")),
)
post_json(
    "/v1/stacks/down",
    {"manifest": raw_partial_manifest, "strict_scopes": True, "yes": True},
    raw_down_path,
)
raw_down = json.loads(pathlib.Path(raw_down_path).read_text(encoding="utf-8"))
if not any(action.get("resource_type") == "connector" and action.get("operation") == "delete" for action in raw_down.get("actions", [])):
    raise AssertionError("raw HTTP down did not include connector delete action")
if not any(action.get("resource_type") == "persona" and action.get("operation") == "blocked" for action in raw_down.get("actions", [])):
    raise AssertionError("raw HTTP down did not include blocked persona action")

post_json(
    "/v1/stacks/apply",
    {
        "manifest": """
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: prune-blocked
spec: {}
""",
        "strict_scopes": True,
        "prune": True,
    },
    raw_prune_path,
)
raw_prune = json.loads(pathlib.Path(raw_prune_path).read_text(encoding="utf-8"))
if not any(
    action.get("resource_type") == "persona" and action.get("operation") == "blocked"
    for action in raw_prune.get("plan", {}).get("actions", [])
):
    raise AssertionError("raw HTTP prune did not retain blocked persona action in plan")
PY

"$BIN" --base-url "$BASE_URL" --output json runtime set-permission-mode default \
  >"$EVIDENCE/runtime-stack/runtime-drift-set.json"
set +e
"$BIN" --base-url "$BASE_URL" --output json stack verify \
  --file "$EVIDENCE/runtime-stack.yaml" \
  >"$EVIDENCE/runtime-stack/verify-after-runtime-drift.json" \
  2>"$EVIDENCE/runtime-stack/verify-after-runtime-drift.err"
VERIFY_AFTER_RUNTIME_DRIFT_STATUS=$?
set -e
if [[ "$VERIFY_AFTER_RUNTIME_DRIFT_STATUS" -eq 0 ]]; then
  echo "runtime stack verify unexpectedly succeeded after runtime drift" >&2
  exit 1
fi

"$BIN" --base-url "$BASE_URL" --output json secrets delete \
  mcp.linear.LINEAR_API_KEY \
  >"$EVIDENCE/linear-triage-daily/delete-secret.json"
set +e
"$BIN" --base-url "$BASE_URL" --output json stack verify \
  --file "$ROOT/examples/stacks/linear-triage-daily/Kheishfile.yaml" \
  >"$EVIDENCE/linear-triage-daily/verify-after-secret-delete.json" \
  2>"$EVIDENCE/linear-triage-daily/verify-after-secret-delete.err"
VERIFY_AFTER_SECRET_DELETE_STATUS=$?
set -e
if [[ "$VERIFY_AFTER_SECRET_DELETE_STATUS" -eq 0 ]]; then
  echo "linear-triage verify unexpectedly succeeded after required secret deletion" >&2
  exit 1
fi

"$BIN" --base-url "$BASE_URL" --output json connectors delete \
  http stack-review-webhook \
  >"$EVIDENCE/playbook-review/delete-connector.json"
set +e
"$BIN" --base-url "$BASE_URL" --output json stack verify \
  --file "$ROOT/examples/stacks/playbook-review/Kheishfile.yaml" \
  >"$EVIDENCE/playbook-review/verify-after-connector-delete.json" \
  2>"$EVIDENCE/playbook-review/verify-after-connector-delete.err"
VERIFY_AFTER_DELETE_STATUS=$?
set -e
if [[ "$VERIFY_AFTER_DELETE_STATUS" -eq 0 ]]; then
  echo "playbook-review verify unexpectedly succeeded after connector deletion" >&2
  exit 1
fi

python3 - "$EVIDENCE" <<'PY' | tee "$EVIDENCE/verdict.json"
import json
import pathlib
import sys

evidence = pathlib.Path(sys.argv[1])
scenarios = ["generic", "linear-triage-daily", "playbook-review"]
checks = []
for scenario in scenarios:
    verify = json.loads((evidence / scenario / "verify.json").read_text(encoding="utf-8"))
    verify_second = json.loads((evidence / scenario / "verify-second.json").read_text(encoding="utf-8"))
    apply_second = json.loads((evidence / scenario / "apply-second.json").read_text(encoding="utf-8"))
    diff = json.loads((evidence / scenario / "diff.json").read_text(encoding="utf-8"))
    checks.append({
        "scenario": scenario,
        "verified": bool(verify.get("valid")),
        "verified_second": bool(verify_second.get("valid")),
        "second_apply_actions": len(apply_second.get("applied", [])),
        "diff_actions": len(diff.get("actions", [])),
    })
api_plan = json.loads((evidence / "playbook-review" / "api-plan.json").read_text(encoding="utf-8"))
api_ledger = json.loads((evidence / "playbook-review" / "api-ledger.json").read_text(encoding="utf-8"))
raw_apply = json.loads((evidence / "raw-api-apply.json").read_text(encoding="utf-8"))
raw_verify = json.loads((evidence / "raw-api-verify.json").read_text(encoding="utf-8"))
runtime_apply = json.loads((evidence / "runtime-stack" / "apply.json").read_text(encoding="utf-8"))
runtime_verify = json.loads((evidence / "runtime-stack" / "verify.json").read_text(encoding="utf-8"))
runtime_apply_second = json.loads((evidence / "runtime-stack" / "apply-second.json").read_text(encoding="utf-8"))
runtime_diff = json.loads((evidence / "runtime-stack" / "diff.json").read_text(encoding="utf-8"))
runtime_state = json.loads((evidence / "runtime-stack" / "runtime.json").read_text(encoding="utf-8"))
checks.append({
    "scenario": "runtime-stack",
    "verified": runtime_state.get("permission_mode") == "plan" and bool(runtime_verify.get("valid")),
    "verified_second": True,
    "second_apply_actions": len(runtime_apply_second.get("applied", [])),
    "diff_actions": len(runtime_diff.get("actions", [])),
})
checks.append({
    "scenario": "daemon-stack-api",
    "verified": bool(api_plan.get("valid")),
    "verified_second": api_ledger.get("stack") is not None,
    "second_apply_actions": 0,
    "diff_actions": len(api_plan.get("actions", [])),
})
checks.append({
    "scenario": "raw-http-stack-api",
    "verified": bool(raw_verify.get("valid")),
    "verified_second": raw_apply.get("verification", {}).get("valid") is True,
    "second_apply_actions": 0,
    "diff_actions": 0,
})
failed = [
    check
    for check in checks
    if (
        not check["verified"]
        or not check["verified_second"]
        or check["second_apply_actions"] != 0
        or check["diff_actions"] != 0
    )
]
verdict = {
    "scenario": "stack_as_code_true_binary",
    "status": "failed" if failed else "passed",
    "checks": checks,
    "evidence_root": str(evidence),
}
print(json.dumps(verdict, indent=2))
sys.exit(1 if failed else 0)
PY

echo "evidence: $EVIDENCE"
