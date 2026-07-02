#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/debug/kheish-daemon"
STACK_FILE="$ROOT/examples/stacks/linear-github-feature-loop/Kheishfile.yaml"
RUN_ID="linear-github-feature-loop-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE="$ROOT/tmp/e2e/$RUN_ID"
STATE_ROOT="$EVIDENCE/state"
WORKSPACE_ROOT="$EVIDENCE/workspace"
STACK_PLAN_FILE="$EVIDENCE/Kheishfile.no-secret-env.yaml"
LIVE_STACK_FILE="$EVIDENCE/Kheishfile.no-secret-env.no-schedules.yaml"
MCP_CONFIG="$EVIDENCE/codex-mcp.toml"
ADMIN_TOKEN_FILE="$EVIDENCE/admin.token"
LOG="$EVIDENCE/daemon.log"
PID=""
GITHUB_MCP_IMAGE_REF="${GITHUB_MCP_IMAGE:-}"
GITHUB_MCP_RUN_IMAGE=""
GITHUB_MCP_CONTAINER_NAME="kheish-github-mcp-$RUN_ID"

mkdir -p "$STATE_ROOT" "$WORKSPACE_ROOT"
cd "$ROOT"

cleanup() {
  if [[ -n "$GITHUB_MCP_CONTAINER_NAME" ]] && command -v docker >/dev/null 2>&1; then
    docker rm -f "$GITHUB_MCP_CONTAINER_NAME" >/dev/null 2>&1 || true
  fi
  if [[ -n "$PID" ]] && kill -0 "$PID" >/dev/null 2>&1; then
    local children
    children="$(pgrep -P "$PID" 2>/dev/null || true)"
    if [[ -n "$children" ]]; then
      kill $children >/dev/null 2>&1 || true
    fi
    kill "$PID" >/dev/null 2>&1 || true
    for _ in {1..40}; do
      if ! kill -0 "$PID" >/dev/null 2>&1; then
        wait "$PID" >/dev/null 2>&1 || true
        return
      fi
      sleep 0.1
    done
    children="$(pgrep -P "$PID" 2>/dev/null || true)"
    if [[ -n "$children" ]]; then
      kill -KILL $children >/dev/null 2>&1 || true
    fi
    kill -KILL "$PID" >/dev/null 2>&1 || true
    wait "$PID" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

if [[ "${KHEISH_E2E_SKIP_BUILD:-0}" != "1" ]]; then
  env -i \
    "PATH=${PATH:-/usr/bin:/bin}" \
    "HOME=${HOME:-$ROOT}" \
    "USER=${USER:-kheish}" \
    "LOGNAME=${LOGNAME:-${USER:-kheish}}" \
    "SHELL=${SHELL:-/bin/sh}" \
    "CARGO_HOME=${HOME:-$ROOT}/.cargo" \
    "RUSTUP_HOME=${HOME:-$ROOT}/.rustup" \
    cargo build -p kheish-daemon 2>&1 | tee "$EVIDENCE/build.log"
fi

if [[ ! -x "$BIN" ]]; then
  echo "missing daemon binary: $BIN" >&2
  exit 1
fi

missing_env=()
for name in LINEAR_API_KEY GITHUB_PERSONAL_ACCESS_TOKEN; do
  if [[ -z "${!name:-}" ]]; then
    missing_env+=("$name")
  fi
done
if (( ${#missing_env[@]} > 0 )); then
  printf 'missing required env vars: %s\n' "${missing_env[*]}" >&2
  exit 2
fi

if ! command -v docker >/dev/null 2>&1; then
  echo "missing docker; GitHub MCP live E2E uses ghcr.io/github/github-mcp-server" >&2
  exit 2
fi
if [[ -z "$GITHUB_MCP_IMAGE_REF" ]]; then
  echo "missing GITHUB_MCP_IMAGE; set it to a pinned digest such as ghcr.io/github/github-mcp-server@sha256:..." >&2
  exit 2
fi
if [[ "$GITHUB_MCP_IMAGE_REF" != *@sha256:* ]]; then
  echo "GITHUB_MCP_IMAGE must be pinned by digest, got: $GITHUB_MCP_IMAGE_REF" >&2
  exit 2
fi

python3 - "$STACK_FILE" "$EVIDENCE/github-repository-full-name.txt" <<'PY'
import json
import os
import pathlib
import re
import sys
import urllib.error
import urllib.request

stack_file = pathlib.Path(sys.argv[1])
output_file = pathlib.Path(sys.argv[2])


def metadata_value(text, key):
    match = re.search(
        rf"(?m)^\s*{re.escape(key)}:\s*[\"']?([^\"'\n#]+)",
        text,
    )
    if match:
        return match.group(1).strip()
    return ""


def first_manifest_file(text):
    match = re.search(
        r"(?m)^\s*(?:-\s*)?manifest_file:\s*[\"']?([^\"'\n#]+)",
        text,
    )
    if match:
        return match.group(1).strip()
    return ""


explicit = (
    os.environ.get("GITHUB_REPOSITORY_FULL_NAME")
    or os.environ.get("GITHUB_REPOSITORY")
    or ""
).strip()
if explicit:
    if "/" not in explicit:
        print(
            "GITHUB_REPOSITORY_FULL_NAME/GITHUB_REPOSITORY must be owner/repo",
            file=sys.stderr,
        )
        sys.exit(2)
    output_file.write_text(explicit + "\n", encoding="utf-8")
    sys.exit(0)

scope = os.environ.get("GITHUB_REPOSITORY_SCOPE", "").strip()
stack_text = stack_file.read_text(encoding="utf-8")
if not scope:
    scope = metadata_value(stack_text, "repository_scope")
if not scope:
    manifest = first_manifest_file(stack_text)
    if manifest:
        playbook_file = (stack_file.parent / manifest).resolve()
        if playbook_file.is_file():
            scope = metadata_value(
                playbook_file.read_text(encoding="utf-8"),
                "repository_scope",
            )
if not scope:
    print(
        "could not resolve repository scope; set GITHUB_REPOSITORY_FULL_NAME=owner/repo",
        file=sys.stderr,
    )
    sys.exit(2)
if "/" in scope:
    output_file.write_text(scope + "\n", encoding="utf-8")
    sys.exit(0)

token = os.environ["GITHUB_PERSONAL_ACCESS_TOKEN"]
matches = []
url = (
    "https://api.github.com/user/repos"
    "?per_page=100&affiliation=owner,collaborator,organization_member"
)
for _ in range(10):
    request = urllib.request.Request(
        url,
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "kheish-live-e2e",
        },
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            repos = json.loads(response.read().decode("utf-8"))
            link = response.headers.get("Link", "")
    except urllib.error.HTTPError as error:
        print(
            f"failed to list GitHub repositories for scope {scope!r}: HTTP {error.code}; "
            "set GITHUB_REPOSITORY_FULL_NAME=owner/repo",
            file=sys.stderr,
        )
        sys.exit(2)
    matches.extend(
        repo["full_name"]
        for repo in repos
        if repo.get("name", "").lower() == scope.lower()
    )
    next_url = None
    for part in link.split(","):
        if 'rel="next"' in part:
            next_url = part.split(";", 1)[0].strip()[1:-1]
            break
    if not next_url:
        break
    url = next_url

if len(matches) == 1:
    output_file.write_text(matches[0] + "\n", encoding="utf-8")
    sys.exit(0)
if len(matches) > 1:
    print(
        f"repository scope {scope!r} matched multiple repositories; "
        "set GITHUB_REPOSITORY_FULL_NAME=owner/repo",
        file=sys.stderr,
    )
else:
    print(
        f"repository scope {scope!r} was not visible to the GitHub token; "
        "set GITHUB_REPOSITORY_FULL_NAME=owner/repo",
        file=sys.stderr,
    )
sys.exit(2)
PY
GITHUB_REPOSITORY_FULL_NAME="$(cat "$EVIDENCE/github-repository-full-name.txt")"
python3 - "$GITHUB_REPOSITORY_FULL_NAME" "$EVIDENCE/mcp-github-list-pull-requests-input.json" <<'PY'
import json
import pathlib
import sys

owner, repo = sys.argv[1].split("/", 1)
pathlib.Path(sys.argv[2]).write_text(
    json.dumps(
        {
            "owner": owner,
            "repo": repo,
            "state": "open",
            "perPage": 1,
        },
        separators=(",", ":"),
    )
    + "\n",
    encoding="utf-8",
)
PY

if [[ -z "${KHEISH_AUTH_STORE_MASTER_KEY:-}" && -z "${KHEISH_AUTH_STORE_MASTER_KEY_FILE:-}" ]]; then
  export KHEISH_AUTH_STORE_MASTER_KEY="$("$BIN" secrets generate)"
fi

"$BIN" secrets generate >"$ADMIN_TOKEN_FILE"
chmod 600 "$ADMIN_TOKEN_FILE"

docker pull "$GITHUB_MCP_IMAGE_REF" >"$EVIDENCE/github-mcp-docker-pull.log"
GITHUB_MCP_RUN_IMAGE="$(docker image inspect \
  --format '{{.Id}}' \
  "$GITHUB_MCP_IMAGE_REF")"
printf '%s\n' "$GITHUB_MCP_RUN_IMAGE" >"$EVIDENCE/github-mcp-image-id.txt"

python3 - "$STACK_FILE" "$STACK_PLAN_FILE" "$LIVE_STACK_FILE" <<'PY'
import pathlib
import sys

source = pathlib.Path(sys.argv[1])
plan_target = pathlib.Path(sys.argv[2])
live_target = pathlib.Path(sys.argv[3])
lines = source.read_text(encoding="utf-8").splitlines()


def without_secret_env(source_lines):
    return [
        line
        for line in source_lines
        if not (
            line.strip().startswith("value_env:")
            and len(line) - len(line.lstrip(" ")) >= 8
        )
    ]


plan_lines = without_secret_env(lines)
live_lines = []
i = 0
while i < len(plan_lines):
    line = plan_lines[i]
    stripped = line.strip()
    indent = len(line) - len(line.lstrip(" "))
    if indent == 2 and stripped == "schedules:":
        live_lines.append("  schedules: []")
        i += 1
        while i < len(plan_lines):
            next_line = plan_lines[i]
            next_stripped = next_line.strip()
            next_indent = len(next_line) - len(next_line.lstrip(" "))
            if next_stripped and next_indent <= 2:
                break
            i += 1
        continue
    if indent == 4 and stripped.startswith("- name: ") and stripped.split(":", 1)[1].strip() in {
        "intake-schedule",
        "followup-schedule",
    }:
        i += 1
        while i < len(plan_lines):
            next_line = plan_lines[i]
            next_stripped = next_line.strip()
            next_indent = len(next_line) - len(next_line.lstrip(" "))
            if next_stripped and next_indent <= 4:
                break
            i += 1
        continue
    live_lines.append(line)
    i += 1
plan_target.write_text("\n".join(plan_lines) + "\n", encoding="utf-8")
live_target.write_text("\n".join(live_lines) + "\n", encoding="utf-8")
PY

pick_port() {
  python3 - <<'PY'
import socket
with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
    sock.bind(("127.0.0.1", 0))
    print(sock.getsockname()[1])
PY
}

PORT="${KHEISH_E2E_PORT:-$(pick_port)}"
BASE_URL="http://127.0.0.1:$PORT"

if "$BIN" --base-url "$BASE_URL" status --output json >/dev/null 2>&1; then
  echo "refusing to reuse live daemon at $BASE_URL" >&2
  exit 1
fi

cli() {
  "$BIN" --base-url "$BASE_URL" --token-file "$ADMIN_TOKEN_FILE" --output json "$@"
}

"$BIN" --output json mcp auth set linear \
  --from-env LINEAR_API_KEY \
  --offline \
  --state-root "$STATE_ROOT" \
  >"$EVIDENCE/linear-auth.json"

"$BIN" --output json secrets set mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN \
  --provider generic \
  --from-env GITHUB_PERSONAL_ACCESS_TOKEN \
  --offline \
  --state-root "$STATE_ROOT" \
  >"$EVIDENCE/github-secret.json"

python3 - "$MCP_CONFIG" "$GITHUB_MCP_RUN_IMAGE" "$GITHUB_MCP_CONTAINER_NAME" <<'PY'
import json
import os
import pathlib
import sys

path = pathlib.Path(sys.argv[1])
image = sys.argv[2]
container = sys.argv[3]
env = {"GITHUB_TOOLSETS": "context,repos,issues,pull_requests,users"}
for key in [
    "DOCKER_HOST",
    "DOCKER_CONTEXT",
    "DOCKER_CONFIG",
    "DOCKER_TLS_VERIFY",
    "DOCKER_CERT_PATH",
    "XDG_RUNTIME_DIR",
    "HOME",
]:
    value = os.environ.get(key)
    if value:
        env[key] = value
env_inline = ", ".join(
    f"{key} = {json.dumps(value)}" for key, value in sorted(env.items())
)
args = [
    "run",
    "-i",
    "--rm",
    "--name",
    container,
    "-e",
    "GITHUB_PERSONAL_ACCESS_TOKEN",
    "-e",
    "GITHUB_TOOLSETS",
    image,
]
args_block = "\n".join(f"  {json.dumps(arg)}," for arg in args)
content = f"""[mcp_servers.github]
command = "docker"
args = [
{args_block}
]
env = {{ {env_inline} }}
env_secret_refs = {{ GITHUB_PERSONAL_ACCESS_TOKEN = "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN" }}
inherit_env = false
required = true
startup_timeout_sec = 90
tool_timeout_sec = 120
"""
path.write_text(content, encoding="utf-8")
PY
python3 - "$MCP_CONFIG" <<'PY'
import os
import pathlib
import sys

content = pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")
for name in ["LINEAR_API_KEY", "GITHUB_PERSONAL_ACCESS_TOKEN"]:
    value = os.environ.get(name, "")
    if value and value in content:
        print(f"{name} value leaked into generated MCP config", file=sys.stderr)
        sys.exit(1)
PY

DAEMON_ENV=(
  "PATH=${PATH:-/usr/bin:/bin}"
  "HOME=${HOME:-$ROOT}"
  "USER=${USER:-kheish}"
  "LOGNAME=${LOGNAME:-${USER:-kheish}}"
  "SHELL=${SHELL:-/bin/sh}"
)
for name in \
  KHEISH_AUTH_STORE_MASTER_KEY \
  KHEISH_AUTH_STORE_MASTER_KEY_FILE \
  DOCKER_HOST \
  DOCKER_CONTEXT \
  DOCKER_CONFIG \
  DOCKER_TLS_VERIFY \
  DOCKER_CERT_PATH \
  XDG_RUNTIME_DIR; do
  if [[ -n "${!name:-}" ]]; then
    DAEMON_ENV+=("$name=${!name}")
  fi
done

env -i "${DAEMON_ENV[@]}" "$BIN" serve \
  --bind "127.0.0.1:$PORT" \
  --state-root "$STATE_ROOT" \
  --workspace-root "$WORKSPACE_ROOT" \
  --mcp-config "$MCP_CONFIG" \
  --mcp-discovery disabled \
  --mcp-profile planning \
  --provider openai \
  --model gpt-5.5 \
  --api-key "sk-e2e-no-network" \
  --http-auth-mode bearer \
  --http-admin-token-file "$ADMIN_TOKEN_FILE" \
  >"$LOG" 2>&1 &
PID="$!"

ready=0
for _ in {1..120}; do
  if cli status >"$EVIDENCE/status.json" 2>"$EVIDENCE/status.err"; then
    ready=1
    break
  fi
  if ! kill -0 "$PID" >/dev/null 2>&1; then
    break
  fi
  sleep 0.25
done

if [[ "$ready" != "1" ]]; then
  echo "daemon did not become ready; tailing daemon log" >&2
  tail -80 "$LOG" >&2 || true
  exit 1
fi

if [[ -r "/proc/$PID/environ" ]]; then
  tr '\0' '\n' <"/proc/$PID/environ" \
    | sed 's/=.*//' \
    | sort \
    >"$EVIDENCE/daemon-env-keys.txt"
else
  printf '__unavailable__\n' >"$EVIDENCE/daemon-env-keys.txt"
fi

cli runtime get \
  >"$EVIDENCE/runtime.json"

if ! cli mcp tools call mcp__github__get_me \
  >"$EVIDENCE/mcp-github-get-me.json" \
  2>"$EVIDENCE/mcp-github-get-me-empty.err"; then
  cli mcp tools call mcp__github__get_me \
    --input-json '{"dummy":true}' \
    >"$EVIDENCE/mcp-github-get-me.json"
fi

cli mcp tools call mcp__github__list_pull_requests \
  --input-file "$EVIDENCE/mcp-github-list-pull-requests-input.json" \
  >"$EVIDENCE/mcp-github-list-pull-requests.json"

cli mcp tools call mcp__linear__list_issues \
  --input-json '{"limit":1}' \
  >"$EVIDENCE/mcp-linear-list-issues.json"

cli stack validate \
  --file "$STACK_FILE" \
  >"$EVIDENCE/validate.json"

cli stack import \
  --file "$STACK_PLAN_FILE" \
  --resource secret/mcp.linear.LINEAR_API_KEY \
  --resource secret/mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN \
  >"$EVIDENCE/import-secrets.json"

cli stack plan \
  --file "$STACK_PLAN_FILE" \
  >"$EVIDENCE/plan-original.json"

cli stack validate \
  --file "$LIVE_STACK_FILE" \
  >"$EVIDENCE/validate-live.json"

cli stack apply \
  --file "$LIVE_STACK_FILE" \
  >"$EVIDENCE/apply.json"

cli stack verify \
  --file "$LIVE_STACK_FILE" \
  >"$EVIDENCE/verify.json"

cli stack apply \
  --file "$LIVE_STACK_FILE" \
  >"$EVIDENCE/apply-second.json"

cli stack diff \
  --file "$LIVE_STACK_FILE" \
  >"$EVIDENCE/diff.json"

cli schedules list \
  >"$EVIDENCE/schedules-after-apply.json"

python3 - "$EVIDENCE" <<'PY' | tee "$EVIDENCE/verdict.json"
import json
import pathlib
import sys

evidence = pathlib.Path(sys.argv[1])
runtime = json.loads((evidence / "runtime.json").read_text(encoding="utf-8"))
daemon_env_keys = set(
    (evidence / "daemon-env-keys.txt").read_text(encoding="utf-8").splitlines()
)
validate = json.loads((evidence / "validate.json").read_text(encoding="utf-8"))
validate_live = json.loads((evidence / "validate-live.json").read_text(encoding="utf-8"))
github_get_me = json.loads((evidence / "mcp-github-get-me.json").read_text(encoding="utf-8"))
github_list_pull_requests = json.loads((evidence / "mcp-github-list-pull-requests.json").read_text(encoding="utf-8"))
linear_list_issues = json.loads((evidence / "mcp-linear-list-issues.json").read_text(encoding="utf-8"))
plan_original = json.loads((evidence / "plan-original.json").read_text(encoding="utf-8"))
import_secrets = json.loads((evidence / "import-secrets.json").read_text(encoding="utf-8"))
apply = json.loads((evidence / "apply.json").read_text(encoding="utf-8"))
verify = json.loads((evidence / "verify.json").read_text(encoding="utf-8"))
apply_second = json.loads((evidence / "apply-second.json").read_text(encoding="utf-8"))
diff = json.loads((evidence / "diff.json").read_text(encoding="utf-8"))
schedules = json.loads((evidence / "schedules-after-apply.json").read_text(encoding="utf-8"))


def action_key(action):
    return (
        action.get("phase"),
        action.get("resource_type"),
        action.get("resource_id"),
        action.get("operation"),
    )


expected_imports = {
    ("import", "secret", "mcp.linear.LINEAR_API_KEY", "adopt"),
    ("import", "secret", "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN", "adopt"),
}
expected_apply = {
    ("personas", "persona", "feature-pr-operator", "create"),
    ("sessions", "session", "feature-pr-loop", "create"),
    ("playbooks", "playbook", "linear-github-feature-pr-loop/0.1.0", "apply"),
}
expected_requirement_actions = {
    ("requirements", "mcp_server", "github", "noop"),
    ("requirements", "mcp_server", "linear", "noop"),
    ("requirements", "mcp_tool", "mcp__github__create_pull_request", "noop"),
    ("requirements", "mcp_tool", "mcp__github__list_pull_requests", "noop"),
    ("requirements", "mcp_tool", "mcp__github__pull_request_read", "noop"),
    ("requirements", "mcp_tool", "mcp__github__add_reply_to_pull_request_comment", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__get_issue", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__list_issues", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__save_comment", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__save_issue", "noop"),
}
expected_schedule_actions = {
    ("schedules", "schedule", "linear-intake-0800", "create"),
    ("schedules", "schedule", "github-review-followup-hourly", "create"),
}
expected_verify_checks = {
    ("requirement", "mcp_server/github"),
    ("requirement", "mcp_server/linear"),
    ("requirement", "mcp_tool/mcp__github__create_pull_request"),
    ("requirement", "mcp_tool/mcp__github__list_pull_requests"),
    ("requirement", "mcp_tool/mcp__github__pull_request_read"),
    ("requirement", "mcp_tool/mcp__github__add_reply_to_pull_request_comment"),
    ("requirement", "mcp_tool/mcp__linear__get_issue"),
    ("requirement", "mcp_tool/mcp__linear__list_issues"),
    ("requirement", "mcp_tool/mcp__linear__save_comment"),
    ("requirement", "mcp_tool/mcp__linear__save_issue"),
    ("secret", "mcp.linear.LINEAR_API_KEY"),
    ("secret", "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN"),
    ("persona", "feature-pr-operator"),
    ("session", "feature-pr-loop"),
    ("playbook", "linear-github-feature-pr-loop/0.1.0"),
    ("probe", "persona"),
    ("probe", "session"),
    ("probe", "playbook"),
}
plan_actions = plan_original.get("actions", [])
requirement_actions = [
    action for action in plan_actions
    if action.get("phase") == "requirements"
    and action.get("resource_type") in {"mcp_server", "mcp_tool"}
]
schedule_actions = [
    action for action in plan_actions
    if action.get("phase") == "schedules"
    and action.get("resource_type") == "schedule"
]
verify_checks = verify.get("checks", [])
verify_check_keys = {
    (check.get("kind"), check.get("target")) for check in verify_checks
}
failed_verify_checks = [
    check for check in verify_checks if not check.get("ok")
]
apply_verify_checks = apply.get("verification", {}).get("checks", [])
apply_verify_check_keys = {
    (check.get("kind"), check.get("target")) for check in apply_verify_checks
}
failed_apply_verify_checks = [
    check for check in apply_verify_checks if not check.get("ok")
]
second_apply_verify_checks = apply_second.get("verification", {}).get("checks", [])
second_apply_verify_check_keys = {
    (check.get("kind"), check.get("target")) for check in second_apply_verify_checks
}
failed_second_apply_verify_checks = [
    check for check in second_apply_verify_checks if not check.get("ok")
]

check = {
    "daemon_env_checked": "__unavailable__" not in daemon_env_keys,
    "daemon_secret_env_scrubbed": not {
        "LINEAR_API_KEY",
        "GITHUB_PERSONAL_ACCESS_TOKEN",
    }.intersection(daemon_env_keys),
    "runtime_mcp_servers": [
        {
            "server": server.get("server"),
            "source": server.get("source"),
            "connected": server.get("connected"),
            "uses_credentials": server.get("uses_credentials"),
            "credential_secret_refs": server.get("credential_secret_refs", []),
        }
        for server in runtime.get("mcp", {}).get("servers", [])
    ],
    "runtime_has_planning_profile": "planning" in runtime.get("mcp", {}).get("selected_profiles", []),
    "github_get_me_called": github_get_me.get("tool_name") == "mcp__github__get_me",
    "github_get_me_is_error": github_get_me.get("output", {}).get("output", {}).get("is_error"),
    "github_list_pull_requests_called": github_list_pull_requests.get("tool_name") == "mcp__github__list_pull_requests",
    "github_list_pull_requests_is_error": github_list_pull_requests.get("output", {}).get("output", {}).get("is_error"),
    "linear_list_issues_called": linear_list_issues.get("tool_name") == "mcp__linear__list_issues",
    "linear_list_issues_is_error": linear_list_issues.get("output", {}).get("output", {}).get("is_error"),
    "original_validated": bool(validate.get("valid")),
    "live_validated": bool(validate_live.get("valid")),
    "original_plan_valid": bool(plan_original.get("valid")),
    "original_plan_errors": plan_original.get("errors", []),
    "original_requirement_actions": sorted(action_key(action) for action in requirement_actions),
    "original_schedule_actions": sorted(action_key(action) for action in schedule_actions),
    "import_actions": sorted(action_key(action) for action in import_secrets.get("adopted", [])),
    "apply_actions": sorted(action_key(action) for action in apply.get("applied", [])),
    "apply_verification": bool(apply.get("verification", {}).get("valid")),
    "apply_verification_checks": sorted(apply_verify_check_keys),
    "failed_apply_verification_checks": failed_apply_verify_checks,
    "verified": bool(verify.get("valid")),
    "verify_checks": sorted(verify_check_keys),
    "failed_verify_checks": failed_verify_checks,
    "second_apply_verification": bool(apply_second.get("verification", {}).get("valid")),
    "second_apply_verification_checks": sorted(second_apply_verify_check_keys),
    "failed_second_apply_verification_checks": failed_second_apply_verify_checks,
    "diff_valid": bool(diff.get("valid")),
    "diff_errors": diff.get("errors", []),
    "second_apply_actions": len(apply_second.get("applied", [])),
    "diff_actions": len(diff.get("actions", [])),
    "schedules_after_apply": len(schedules),
    "mcp_tool_call_exercised": True,
}
failed = (
    not check["daemon_env_checked"]
    or not check["daemon_secret_env_scrubbed"]
    or not check["runtime_has_planning_profile"]
    or not check["github_get_me_called"]
    or check["github_get_me_is_error"] is not False
    or not check["github_list_pull_requests_called"]
    or check["github_list_pull_requests_is_error"] is not False
    or not check["linear_list_issues_called"]
    or check["linear_list_issues_is_error"] is not False
    or not check["original_validated"]
    or not check["live_validated"]
    or not check["original_plan_valid"]
    or bool(check["original_plan_errors"])
    or set(check["original_requirement_actions"]) != expected_requirement_actions
    or set(check["original_schedule_actions"]) != expected_schedule_actions
    or set(check["import_actions"]) != expected_imports
    or set(check["apply_actions"]) != expected_apply
    or not check["apply_verification"]
    or set(check["apply_verification_checks"]) != expected_verify_checks
    or bool(check["failed_apply_verification_checks"])
    or not check["verified"]
    or set(check["verify_checks"]) != expected_verify_checks
    or bool(check["failed_verify_checks"])
    or not check["second_apply_verification"]
    or set(check["second_apply_verification_checks"]) != expected_verify_checks
    or bool(check["failed_second_apply_verification_checks"])
    or not check["diff_valid"]
    or bool(check["diff_errors"])
    or check["second_apply_actions"] != 0
    or check["diff_actions"] != 0
    or check["schedules_after_apply"] != 0
)
verdict = {
    "scenario": "linear_github_feature_loop_live",
    "status": "failed" if failed else "passed",
    "mode": "non_destructive_no_schedules_apply",
    "limits": [
        "The original Kheishfile is validated. Planning uses an evidence copy with value_env sources removed after offline secret import, including schedule drift.",
        "The live apply uses an evidence copy with value_env sources and schedules removed to avoid daemon env secret exposure and accidental provider mutations.",
        "The live provider probe calls non-destructive GitHub and Linear MCP read tools before stack reconciliation.",
    ],
    "check": check,
    "evidence_root": str(evidence),
}
print(json.dumps(verdict, indent=2))
sys.exit(1 if failed else 0)
PY

echo "evidence: $EVIDENCE"
