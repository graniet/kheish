#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${ROOT}/target/debug/kheish-daemon"
if [[ -n "${KHEISH_E2E_PORT:-}" ]]; then
  PORT="${KHEISH_E2E_PORT}"
else
  PORT="$(python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
)"
fi
BASE_URL="http://127.0.0.1:${PORT}"
RUN_ID="mcp-catalog-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE_ROOT="${ROOT}/tmp/e2e/${RUN_ID}"
STATE_ROOT="${EVIDENCE_ROOT}/state"
WORKSPACE_ROOT="${EVIDENCE_ROOT}/workspace"
DAEMON_LOG="${EVIDENCE_ROOT}/daemon.log"
PID=""

mkdir -p "${EVIDENCE_ROOT}" "${STATE_ROOT}" "${WORKSPACE_ROOT}"

cleanup() {
  if [[ -n "${PID}" ]] && kill -0 "${PID}" >/dev/null 2>&1; then
    kill "${PID}" >/dev/null 2>&1 || true
    wait "${PID}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ ! -x "${BIN}" ]]; then
  echo "missing daemon binary: ${BIN}" >&2
  echo "run: cargo build -p kheish-daemon" >&2
  exit 2
fi

if "${BIN}" --base-url "${BASE_URL}" status --output json >/dev/null 2>&1; then
  echo "refusing to run: ${BASE_URL} already answers as a daemon" >&2
  exit 2
fi

"${BIN}" mcp catalog list --profile docs --supported-only --output json \
  >"${EVIDENCE_ROOT}/catalog-docs.json"
"${BIN}" mcp profiles get docs --output json \
  >"${EVIDENCE_ROOT}/profile-docs.json"

"${BIN}" serve \
  --bind "127.0.0.1:${PORT}" \
  --state-root "${STATE_ROOT}" \
  --workspace-root "${WORKSPACE_ROOT}" \
  --mcp-discovery disabled \
  --mcp-profile docs \
  --provider openai \
  --model gpt-5.4 \
  --api-key "${OPENAI_API_KEY:-test-openai-key}" \
  >"${DAEMON_LOG}" 2>&1 &
PID="$!"

READY=0
for _ in {1..120}; do
  if ! kill -0 "${PID}" >/dev/null 2>&1; then
    echo "daemon exited during startup" >&2
    cat "${DAEMON_LOG}" >&2
    exit 1
  fi
  if "${BIN}" --base-url "${BASE_URL}" status --output json >"${EVIDENCE_ROOT}/status.json" 2>"${EVIDENCE_ROOT}/status.err"; then
    READY=1
    break
  fi
  sleep 0.5
done
if [[ "${READY}" != "1" ]]; then
  echo "daemon did not become ready at ${BASE_URL}" >&2
  cat "${DAEMON_LOG}" >&2
  exit 1
fi

"${BIN}" --base-url "${BASE_URL}" runtime get --output json \
  >"${EVIDENCE_ROOT}/runtime.json"

python3 - "${EVIDENCE_ROOT}" <<'PY'
import json
import pathlib
import sys

root = pathlib.Path(sys.argv[1])
runtime = json.loads((root / "runtime.json").read_text())
catalog = json.loads((root / "catalog-docs.json").read_text())
mcp = runtime.get("mcp") or {}
servers = mcp.get("servers") or []

errors = []
expected_tools = {
    "mcp__openaiDeveloperDocs__search_openai_docs",
    "mcp__openaiDeveloperDocs__fetch_openai_doc",
}
openai_docs = next(
    (server for server in servers if server.get("server") == "openaiDeveloperDocs"),
    None,
)
if "docs" not in mcp.get("selected_profiles", []):
    errors.append("runtime.mcp.selected_profiles does not contain docs")
if not any(server.get("source") == "built_in_catalog" for server in servers):
    errors.append("runtime.mcp.servers has no built_in_catalog server")
if openai_docs is None:
    errors.append("openaiDeveloperDocs server is missing from runtime snapshot")
else:
    if openai_docs.get("source") != "built_in_catalog":
        errors.append("openaiDeveloperDocs source is not built_in_catalog")
    if "docs" not in openai_docs.get("profiles", []):
        errors.append("openaiDeveloperDocs profiles does not include docs")
    if openai_docs.get("catalog_entry_id") != "openai-docs":
        errors.append("openaiDeveloperDocs catalog_entry_id is not openai-docs")
    if openai_docs.get("connected") is not True:
        errors.append("openaiDeveloperDocs is not connected")
    server_tools = set(openai_docs.get("tools") or [])
    missing_server_tools = sorted(expected_tools - server_tools)
    if missing_server_tools:
        errors.append(f"openaiDeveloperDocs missing tools: {missing_server_tools}")
runtime_tools = set(mcp.get("tool_names") or [])
missing_runtime_tools = sorted(expected_tools - runtime_tools)
if missing_runtime_tools:
    errors.append(f"runtime.mcp.tool_names missing OpenAI docs tools: {missing_runtime_tools}")
if not any(entry.get("id") == "openai-docs" for entry in catalog):
    errors.append("openai-docs is missing from docs profile catalog output")

verdict = {
    "scenario": "mcp_catalog_true_binary",
    "status": "failed" if errors else "passed",
    "errors": errors,
    "server_count": len(servers),
    "connected_servers": [
        server.get("server") for server in servers if server.get("connected")
    ],
    "evidence_root": str(root),
}
(root / "verdict.json").write_text(json.dumps(verdict, indent=2) + "\n")
if errors:
    for error in errors:
        print(f"FAIL: {error}", file=sys.stderr)
    sys.exit(1)
print(json.dumps(verdict, indent=2))
PY

echo "evidence: ${EVIDENCE_ROOT}"
