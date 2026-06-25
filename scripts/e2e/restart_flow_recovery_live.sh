#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="${ROOT_DIR}/target/debug/kheish-daemon"
RUN_ID="restart-flow-$(date +%Y%m%d-%H%M%S)"
EVIDENCE_DIR="${EVIDENCE_DIR:-${ROOT_DIR}/tmp/evidence/${RUN_ID}}"
STATE_ROOT="${STATE_ROOT:-${EVIDENCE_DIR}/state}"
WORKSPACE_ROOT="${WORKSPACE_ROOT:-${EVIDENCE_DIR}/workspace}"
WORKER_SESSION="restart-flow-worker"
ROOT_SESSION="restart-flow-root"
FLOW_ID="restart-flow-live"
PLAYBOOK_ID="restart-recovery-live"
PLAYBOOK_VERSION="2026-04-23"
ROOT_PROVIDER="anthropic"
ROOT_MODEL="claude-opus-4-6"
CHILD_PROVIDER="openai"
CHILD_MODEL="gpt-5.4"
FINAL_MARKER="FLOW_FINAL_TICK"
FALSE_OK_MARKER="WORKER_RESTART_OK"
ARTIFACT_REL="reports/restart-flow-result.txt"
ARTIFACT_PATH="${WORKSPACE_ROOT}/${ARTIFACT_REL}"
DAEMON_PID=""
BASE_URL=""
TASK_ID=""
APPROVAL_ID=""

mkdir -p "${EVIDENCE_DIR}" "${STATE_ROOT}" "${WORKSPACE_ROOT}"

if [[ -f "${ROOT_DIR}/.env" ]]; then
  set -a
  # shellcheck disable=SC1091
  source "${ROOT_DIR}/.env"
  set +a
fi

if [[ -z "${ANTHROPIC_API_KEY:-}" ]]; then
  echo "ANTHROPIC_API_KEY is required for this live E2E." >&2
  exit 2
fi
if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is required for this live E2E." >&2
  exit 2
fi

pick_port() {
  python3 - <<'PY'
import socket
s = socket.socket()
s.bind(("127.0.0.1", 0))
print(s.getsockname()[1])
s.close()
PY
}

json_eval() {
  local file="$1"
  local expr="$2"
  python3 - "$file" "$expr" <<'PY'
import json, sys
with open(sys.argv[1]) as f:
    root = json.load(f)
print(eval(sys.argv[2], {"root": root}))
PY
}

kheish() {
  "${BIN}" --base-url "${BASE_URL}" --output json "$@"
}

kheish_with_timeout() {
  local timeout_seconds="$1"
  shift
  python3 - "${timeout_seconds}" "${BIN}" "${BASE_URL}" "$@" <<'PY'
import subprocess
import sys

timeout_seconds = float(sys.argv[1])
cmd = [sys.argv[2], "--base-url", sys.argv[3], "--output", "json", *sys.argv[4:]]
try:
    completed = subprocess.run(cmd, timeout=timeout_seconds)
except subprocess.TimeoutExpired:
    print(
        f"command timed out after {timeout_seconds:g}s: {' '.join(cmd)}",
        file=sys.stderr,
    )
    sys.exit(124)
sys.exit(completed.returncode)
PY
}

capture() {
  local name="$1"
  shift
  "$@" > "${EVIDENCE_DIR}/${name}.json"
}

stop_daemon() {
  if [[ -n "${DAEMON_PID}" ]] && kill -0 "${DAEMON_PID}" 2>/dev/null; then
    kill "${DAEMON_PID}" 2>/dev/null || true
    wait "${DAEMON_PID}" 2>/dev/null || true
  fi
  DAEMON_PID=""
}

start_daemon() {
  local port
  port="$(pick_port)"
  BASE_URL="http://127.0.0.1:${port}"
  "${BIN}" serve \
    --bind "127.0.0.1:${port}" \
    --state-root "${STATE_ROOT}" \
    --workspace-root "${WORKSPACE_ROOT}" \
    --provider "${ROOT_PROVIDER}" \
    --model "${ROOT_MODEL}" \
    > "${EVIDENCE_DIR}/daemon-${port}.log" 2>&1 &
  DAEMON_PID="$!"
  for _ in $(seq 1 120); do
    if kheish status > "${EVIDENCE_DIR}/status-latest.json" 2>/dev/null; then
      return 0
    fi
    sleep 0.5
  done
  echo "daemon did not become ready; see ${EVIDENCE_DIR}/daemon-${port}.log" >&2
  exit 1
}

wait_for_worker_approval_and_task() {
  local approvals_file="${EVIDENCE_DIR}/approvals-before-restart.json"
  local tasks_file="${EVIDENCE_DIR}/tasks-before-restart.json"
  local root_run_file="${EVIDENCE_DIR}/root-run-waiting-for-worker.json"
  for _ in $(seq 1 180); do
    if [[ -e "${ARTIFACT_PATH}" ]]; then
      cp "${ARTIFACT_PATH}" "${EVIDENCE_DIR}/artifact-before-approval.txt"
      echo "artifact exists before approval: ${ARTIFACT_PATH}" >&2
      exit 1
    fi
    kheish approvals list --session-id "${WORKER_SESSION}" > "${approvals_file}" 2>/dev/null || true
    kheish tasks list "${WORKER_SESSION}" > "${tasks_file}" 2>/dev/null || true
    local match
    if match="$(python3 - "${approvals_file}" "${tasks_file}" <<'PY'
import json, sys
try:
    approvals = json.load(open(sys.argv[1]))
    tasks = json.load(open(sys.argv[2]))
except Exception:
    sys.exit(1)
write_approvals = [
    approval for approval in approvals
    if approval.get("request", {}).get("tool_name") == "write_file"
]
shell_tasks = [
    task for task in tasks
    if task.get("metadata", {}).get("kind") == "background_shell"
    and task.get("status") in {"pending", "in_progress"}
]
if write_approvals and shell_tasks:
    print(write_approvals[0]["request"]["id"])
    print(shell_tasks[0]["id"])
    sys.exit(0)
sys.exit(1)
PY
    )"; then
      APPROVAL_ID="$(printf '%s\n' "${match}" | sed -n '1p')"
      TASK_ID="$(printf '%s\n' "${match}" | sed -n '2p')"
      return 0
    fi
    if [[ -n "${ROOT_RUN_ID:-}" ]] && kheish runs get "${ROOT_RUN_ID}" > "${root_run_file}" 2>/dev/null; then
      if python3 - "${root_run_file}" <<'PY'
import json, sys
run = json.load(open(sys.argv[1]))
status = run.get("status")
sys.exit(0 if status in {"completed", "failed", "cancelled", "interrupted"} else 1)
PY
      then
        echo "root run reached a terminal status before worker approval and task appeared" >&2
        exit 1
      fi
    fi
    sleep 2
  done
  echo "worker did not reach pending write_file approval with a live shell task" >&2
  exit 1
}

assert_approval_survived_restart() {
  python3 - "${EVIDENCE_DIR}/approvals-after-restart.json" "${APPROVAL_ID}" <<'PY'
import json, sys
approvals = json.load(open(sys.argv[1]))
approval_id = sys.argv[2]
ok = any(
    approval.get("request", {}).get("id") == approval_id
    and approval.get("request", {}).get("tool_name") == "write_file"
    for approval in approvals
)
if not ok:
    print(f"write_file approval {approval_id} did not survive restart", file=sys.stderr)
    sys.exit(1)
PY
}

wait_for_failed_recovered_task() {
  for _ in $(seq 1 60); do
    kheish tasks output "${WORKER_SESSION}" "${TASK_ID}" --full > "${EVIDENCE_DIR}/task-output-after-restart.json" 2>/dev/null || true
    if python3 - "${EVIDENCE_DIR}/task-output-after-restart.json" <<'PY'
import json, sys
try:
    view = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
task = view.get("task", {})
metadata = task.get("metadata", {})
sys.exit(0 if (
    task.get("status") == "failed"
    and metadata.get("terminal_reason") == "daemon_restarted"
    and metadata.get("recovered_on_boot") is True
) else 1)
PY
    then
      return 0
    fi
    sleep 1
  done
  echo "background task did not recover as failed after restart" >&2
  exit 1
}

wait_for_partial_output() {
  for _ in $(seq 1 90); do
    kheish tasks output "${WORKER_SESSION}" "${TASK_ID}" --full > "${EVIDENCE_DIR}/task-output-before-restart.json" 2>/dev/null || true
    if grep -q "FLOW_TICK_1" "${EVIDENCE_DIR}/task-output-before-restart.json"; then
      return 0
    fi
    sleep 1
  done
  echo "background task did not produce partial output before restart" >&2
  exit 1
}

wait_for_run_terminal() {
  local run_id="$1"
  local evidence_name="$2"
  local run_file="${EVIDENCE_DIR}/${evidence_name}.json"
  for _ in $(seq 1 180); do
    kheish runs get "${run_id}" > "${run_file}" 2>/dev/null || true
    if python3 - "${run_file}" <<'PY'
import json
import sys

try:
    run = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
sys.exit(
    0
    if run.get("status") in {"completed", "failed", "cancelled", "interrupted"}
    else 1
)
PY
    then
      return 0
    fi
    sleep 1
  done
  echo "run ${run_id} did not reach a terminal status" >&2
  exit 1
}

write_inputs() {
  cat > "${EVIDENCE_DIR}/playbook.json" <<JSON
{
  "playbook_id": "${PLAYBOOK_ID}",
  "version": "${PLAYBOOK_VERSION}",
  "title": "Restart recovery live flow",
  "objective": "Validate fail-closed restart recovery across root and worker agents.",
  "roles": [
    {"role_id": "coordinator", "purpose": "Spawn and supervise the worker."},
    {"role_id": "worker", "purpose": "Launch a long shell task and request a write approval."}
  ],
  "phases": [
    {
      "phase_id": "delegate",
      "objective": "Coordinator delegates to a real OpenAI worker.",
      "acceptance_criteria": ["A sidechain worker run is visible through agents and runs."]
    },
    {
      "phase_id": "restart",
      "objective": "Daemon restart settles active shell task fail-closed.",
      "acceptance_criteria": ["The shell task is failed with terminal_reason daemon_restarted."]
    }
  ],
  "acceptance_criteria": [
    "Pending approval survives restart.",
    "Partial shell output remains inspectable.",
    "No WORKER_RESTART_OK is accepted without completed task status and the final tick."
  ],
  "evidence_expectations": [
    "flows get",
    "tasks output --full",
    "runs debug-artifact provider-request",
    "verdict.json"
  ]
}
JSON

  cat > "${EVIDENCE_DIR}/flow-request.json" <<JSON
{
  "source_plugin": "daemon",
  "source_kind": "e2e",
  "actor_id": "operator",
  "provider": "${ROOT_PROVIDER}",
  "content": "You are the coordinator for a restart recovery E2E. Use spawn_agent exactly once. Spawn a worker in session '${WORKER_SESSION}' with provider '${CHILD_PROVIDER}', model '${CHILD_MODEL}', wait=true, run_in_background=false. Do not set the child mode field; permissions must remain default so write_file requires approval. The worker prompt: launch a daemon-managed background bash task with command 'for i in \$(seq 1 120); do echo FLOW_TICK_\$i; sleep 1; done; echo ${FINAL_MARKER}'; after launching it, run bash command 'mkdir -p reports', then request write_file for ${ARTIFACT_REL} with content PENDING_RESTART_REVIEW. There is a forbidden success token formed by concatenating 'WORKER', '_RESTART', and '_OK'. Do not print, write, quote, or claim that exact token unless task_output later proves task.status=completed, metadata.exit_code=0, and ${FINAL_MARKER} is present. If those conditions are not proven, say 'restart recovery not ready' instead. After spawning, wait for the worker and report only evidence-backed status.",
  "input_items": [],
  "attachments": [],
  "generation": null,
  "completion_requirements": null,
  "metadata": null,
  "binding_keys": [],
  "reply_targets": [],
  "reply_plugin": null,
  "reply_address": null
}
JSON

  cat > "${EVIDENCE_DIR}/hooks.json" <<'JSON'
{
  "hooks": {
    "permission_request": [
      {
        "name": "allow-bash-for-restart-e2e",
        "matcher": "bash",
        "executor": {
          "type": "command",
          "command": "printf '{\"permission\":\"allow\"}'",
          "shell": "/bin/bash",
          "timeout_ms": 1000
        }
      }
    ]
  }
}
JSON
}

write_verdict() {
  local verdict_path="${EVIDENCE_DIR}/verdict.json"
  python3 - "${EVIDENCE_DIR}" "${ARTIFACT_PATH}" "${TASK_ID}" "${APPROVAL_ID}" "${FINAL_MARKER}" "${FALSE_OK_MARKER}" <<'PY'
import json, pathlib, sys
evidence = pathlib.Path(sys.argv[1])
artifact = pathlib.Path(sys.argv[2])
task_id = sys.argv[3]
approval_id = sys.argv[4]
final_marker = sys.argv[5]
false_ok = sys.argv[6]

def load_json(name, default):
    try:
        return json.load(open(evidence / name))
    except Exception:
        return default

def collect_run_outputs(value):
    texts = []
    if isinstance(value, dict):
        outputs = value.get("outputs")
        if isinstance(outputs, list):
            for output in outputs:
                if isinstance(output, dict) and isinstance(output.get("content"), str):
                    texts.append(output["content"])
        for child in value.values():
            texts.extend(collect_run_outputs(child))
    elif isinstance(value, list):
        for item in value:
            texts.extend(collect_run_outputs(item))
    return texts

task_output = json.load(open(evidence / "task-output-after-restart.json"))
task = task_output["task"]
output = task_output.get("output_text") or task_output.get("output_excerpt") or ""
artifact_text = artifact.read_text() if artifact.exists() else ""
provider_root = (evidence / "root-provider-request.txt").read_text(errors="ignore") if (evidence / "root-provider-request.txt").exists() else ""
provider_child = (evidence / "child-provider-request.txt").read_text(errors="ignore") if (evidence / "child-provider-request.txt").exists() else ""
approvals_after_restart = load_json("approvals-after-restart.json", [])
approvals_after_approval = load_json("approvals-after-approval.json", [])
approval_allow = load_json("approval-allow.json", {})
model_output_text = "\n".join(
    collect_run_outputs(load_json("root-run-after-approval.json", {}))
    + collect_run_outputs(load_json("worker-runs-after-approval.json", []))
    + collect_run_outputs(load_json("flow-after-approval.json", {}))
)

checks = {
    "task_id": task_id,
    "task_failed_on_restart": task.get("status") == "failed",
    "terminal_reason_daemon_restarted": task.get("metadata", {}).get("terminal_reason") == "daemon_restarted",
    "recovered_on_boot": task.get("metadata", {}).get("recovered_on_boot") is True,
    "partial_output_present": "FLOW_TICK_1" in output,
    "final_marker_absent": final_marker not in output,
    "artifact_absent_before_approval": not (evidence / "artifact-before-approval.txt").exists(),
    "artifact_created_after_approval": artifact.exists()
    and artifact_text == "PENDING_RESTART_REVIEW",
    "root_provider_debug_anthropic": "claude-opus" in provider_root or "anthropic" in provider_root.lower(),
    "child_provider_debug_openai": "gpt-5.4" in provider_child or "openai" in provider_child.lower(),
    "approval_survived_restart": any(
        approval.get("request", {}).get("id") == approval_id
        and approval.get("request", {}).get("tool_name") == "write_file"
        for approval in approvals_after_restart
    ),
    "approval_allow_returned_run": approval_allow.get("run_id") is not None,
    "approval_absent_after_allow": not any(
        approval.get("request", {}).get("id") == approval_id
        for approval in approvals_after_approval
    ),
}
false_success_surface = artifact_text + "\n" + model_output_text
false_success = false_ok in false_success_surface and not (
    task.get("status") == "completed"
    and task.get("metadata", {}).get("exit_code") == 0
    and final_marker in output
)
checks["anti_false_success"] = not false_success
ok = all(checks.values())
verdict = {
    "ok": ok,
    "checks": checks,
    "artifact_exists_after_approval": artifact.exists(),
    "artifact_text": artifact_text,
    "model_output_text": model_output_text,
}
(evidence / "verdict.json").write_text(json.dumps(verdict, indent=2, sort_keys=True))
sys.exit(0 if ok else 1)
PY
  cat "${verdict_path}"
}

trap stop_daemon EXIT

cd "${ROOT_DIR}"
cargo build -p kheish-daemon
write_inputs
start_daemon

capture "status-initial" kheish status
capture "runtime-initial" kheish runtime get
kheish runtime set-debug-level full > "${EVIDENCE_DIR}/runtime-debug.json"
kheish runtime hooks set --file "${EVIDENCE_DIR}/hooks.json" > "${EVIDENCE_DIR}/runtime-hooks.json"
kheish sessions create "${ROOT_SESSION}" > "${EVIDENCE_DIR}/session-root.json"

kheish playbooks validate --manifest-file "${EVIDENCE_DIR}/playbook.json" > "${EVIDENCE_DIR}/playbook-validate.json"
kheish playbooks create --manifest-file "${EVIDENCE_DIR}/playbook.json" > "${EVIDENCE_DIR}/playbook-create.json"
DIGEST="$(json_eval "${EVIDENCE_DIR}/playbook-validate.json" "root['digest']")"
kheish playbooks publish "${PLAYBOOK_ID}" \
  --version "${PLAYBOOK_VERSION}" \
  --digest "${DIGEST}" \
  --status active \
  --evidence-refs-json '[{"kind":"operator_script","id":"scripts/e2e/restart_flow_recovery_live.sh","description":"live binary restart recovery harness"}]' \
  > "${EVIDENCE_DIR}/playbook-publish.json"

kheish flows start \
  --flow-id "${FLOW_ID}" \
  --idempotency-key "${FLOW_ID}" \
  --playbook-id "${PLAYBOOK_ID}" \
  --version "${PLAYBOOK_VERSION}" \
  --digest "${DIGEST}" \
  --session-id "${ROOT_SESSION}" \
  --request-file "${EVIDENCE_DIR}/flow-request.json" \
  > "${EVIDENCE_DIR}/flow-start.json"
ROOT_RUN_ID="$(json_eval "${EVIDENCE_DIR}/flow-start.json" "root['run_id']")"

wait_for_worker_approval_and_task
wait_for_partial_output

capture "flow-before-restart" kheish flows get "${FLOW_ID}"
capture "agents-before-restart" kheish agents list
capture "root-run-before-restart" kheish runs get "${ROOT_RUN_ID}"

stop_daemon
start_daemon

capture "status-after-restart" kheish status
capture "runtime-after-restart" kheish runtime get
capture "flow-after-restart" kheish flows get "${FLOW_ID}"
capture "agents-after-restart" kheish agents list
capture "approvals-after-restart" kheish approvals list --session-id "${WORKER_SESSION}"
capture "tasks-after-restart" kheish tasks list "${WORKER_SESSION}"
assert_approval_survived_restart
wait_for_failed_recovered_task
kheish sessions events "${ROOT_SESSION}" > "${EVIDENCE_DIR}/root-session-events.json"
kheish sessions events "${WORKER_SESSION}" > "${EVIDENCE_DIR}/worker-session-events.json"
kheish runs debug "${ROOT_RUN_ID}" > "${EVIDENCE_DIR}/root-run-debug.json" || true
kheish runs debug-artifact "${ROOT_RUN_ID}" turn-0001-attempt-0001-provider-request > "${EVIDENCE_DIR}/root-provider-request.txt" || true

CHILD_RUN_ID="$(kheish runs list --session-id "${WORKER_SESSION}" > "${EVIDENCE_DIR}/worker-runs-after-restart.json"; json_eval "${EVIDENCE_DIR}/worker-runs-after-restart.json" "root[0]['run_id']")"
kheish runs debug "${CHILD_RUN_ID}" > "${EVIDENCE_DIR}/child-run-debug.json" || true
kheish runs debug-artifact "${CHILD_RUN_ID}" turn-0001-attempt-0001-provider-request > "${EVIDENCE_DIR}/child-provider-request.txt" || true

kheish_with_timeout 60 approvals allow "${WORKER_SESSION}" "${APPROVAL_ID}" --justification "restart recovery live E2E" > "${EVIDENCE_DIR}/approval-allow.json"
wait_for_run_terminal "${CHILD_RUN_ID}" "worker-run-after-approval-terminal"
sleep 3
capture "flow-after-approval" kheish flows get "${FLOW_ID}"
capture "approvals-after-approval" kheish approvals list --session-id "${WORKER_SESSION}"
capture "tasks-after-approval" kheish tasks list "${WORKER_SESSION}"
capture "root-run-after-approval" kheish runs get "${ROOT_RUN_ID}"
kheish runs list --session-id "${WORKER_SESSION}" > "${EVIDENCE_DIR}/worker-runs-after-approval.json"
kheish sessions events "${ROOT_SESSION}" > "${EVIDENCE_DIR}/root-session-events-after-approval.json"
kheish sessions events "${WORKER_SESSION}" > "${EVIDENCE_DIR}/worker-session-events-after-approval.json"
kheish tasks output "${WORKER_SESSION}" "${TASK_ID}" --full > "${EVIDENCE_DIR}/task-output-after-approval.json" || true
if [[ -e "${ARTIFACT_PATH}" ]]; then
  cp "${ARTIFACT_PATH}" "${EVIDENCE_DIR}/artifact-after-approval.txt"
fi

write_verdict
