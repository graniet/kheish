#!/usr/bin/env bash
set -euo pipefail
umask 077

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
BIN="$ROOT/target/debug/kheish-daemon"
STACK_FILE="$ROOT/examples/stacks/linear-github-feature-loop/Kheishfile.yaml"
STACK_DIR="$(cd "$(dirname "$STACK_FILE")" && pwd)"
RUN_ID="linear-github-feature-loop-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE="$ROOT/tmp/e2e/$RUN_ID"
STATE_ROOT="$EVIDENCE/state"
WORKSPACE_ROOT="$EVIDENCE/workspace"
STACK_EVIDENCE_DIR="$EVIDENCE/stack"
STACK_PLAN_FILE="$STACK_EVIDENCE_DIR/Kheishfile.no-secret-env.yaml"
LIVE_STACK_FILE="$STACK_PLAN_FILE"
PROVENANCE_FILE="$EVIDENCE/provenance.json"
MCP_CONFIG="$EVIDENCE/codex-mcp.toml"
ADMIN_TOKEN_FILE="$EVIDENCE/admin.token"
AUTH_MASTER_KEY_FILE="$EVIDENCE/auth-store-master.key"
MANAGED_SECRET_STACK_DIR="$EVIDENCE/managed-secret-canary"
MANAGED_SECRET_STACK_FILE="$MANAGED_SECRET_STACK_DIR/Kheishfile.yaml"
MANAGED_SECRET_SLOT="stack.e2e.MANAGED_SECRET"
MANAGED_SECRET_ENV_NAME="KHEISH_E2E_MANAGED_SECRET_VALUE"
MANAGED_SECRET_VALUE=""
LOG="$EVIDENCE/daemon.log"
PID=""
GITHUB_MCP_IMAGE_REF="${GITHUB_MCP_IMAGE:-}"
GITHUB_MCP_RUN_IMAGE=""
GITHUB_MCP_CONTAINER_NAME="kheish-github-mcp-$RUN_ID"
RUN_STACK_FLOW="${KHEISH_E2E_RUN_STACK_FLOW:-0}"
OPENAI_FLOW_AUTH_SOURCE="${KHEISH_E2E_OPENAI_AUTH_SOURCE:-codex}"
STACK_FLOW_ID="stack-direct-flow-smoke-$RUN_ID"

if [[ "$RUN_STACK_FLOW" != "0" && "$RUN_STACK_FLOW" != "1" ]]; then
  echo "KHEISH_E2E_RUN_STACK_FLOW must be 0 or 1" >&2
  exit 2
fi
export KHEISH_E2E_RUN_STACK_FLOW="$RUN_STACK_FLOW"

mkdir -p "$STATE_ROOT" "$WORKSPACE_ROOT" "$STACK_EVIDENCE_DIR" "$MANAGED_SECRET_STACK_DIR"
if git -C "$ROOT" check-ignore -q "$EVIDENCE"; then
  printf 'true\n' >"$EVIDENCE/evidence-gitignored.txt"
else
  printf 'false\n' >"$EVIDENCE/evidence-gitignored.txt"
fi
cp -R "$STACK_DIR"/. "$STACK_EVIDENCE_DIR"/
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

python3 - "$PROVENANCE_FILE" "$BIN" "$STACK_FILE" "$GITHUB_MCP_IMAGE_REF" <<'PY'
import hashlib
import json
import os
import pathlib
import subprocess
import sys

output = pathlib.Path(sys.argv[1])
binary = pathlib.Path(sys.argv[2])
stack_file = pathlib.Path(sys.argv[3])
image_ref = sys.argv[4]


def run_git(args):
    try:
        return subprocess.check_output(
            ["git", *args],
            cwd=stack_file.parents[3],
            text=True,
            stderr=subprocess.DEVNULL,
        ).strip()
    except Exception:
        return ""


binary_bytes = binary.read_bytes()
payload = {
    "build_skipped": os.environ.get("KHEISH_E2E_SKIP_BUILD") == "1",
    "binary": {
        "path": str(binary),
        "sha256": hashlib.sha256(binary_bytes).hexdigest(),
        "size_bytes": len(binary_bytes),
        "mtime_ns": binary.stat().st_mtime_ns,
    },
    "git": {
        "head": run_git(["rev-parse", "HEAD"]),
        "status_porcelain": run_git(["status", "--short"]),
    },
    "stack_file": str(stack_file),
    "github_mcp_image": image_ref,
    "run_stack_flow": os.environ.get("KHEISH_E2E_RUN_STACK_FLOW") == "1",
}
output.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")
PY

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
    scope = metadata_value(stack_text, "repository")
if not scope:
    manifest = first_manifest_file(stack_text)
    if manifest:
        playbook_file = (stack_file.parent / manifest).resolve()
        if playbook_file.is_file():
            scope = metadata_value(
                playbook_file.read_text(encoding="utf-8"),
                "repository",
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
            "state": "all",
            "perPage": 1,
        },
        separators=(",", ":"),
    )
    + "\n",
    encoding="utf-8",
)
PY

if [[ -n "${KHEISH_AUTH_STORE_MASTER_KEY:-}" && -n "${KHEISH_AUTH_STORE_MASTER_KEY_FILE:-}" ]]; then
  echo "KHEISH_AUTH_STORE_MASTER_KEY and KHEISH_AUTH_STORE_MASTER_KEY_FILE are mutually exclusive" >&2
  exit 2
fi
if [[ -n "${KHEISH_AUTH_STORE_MASTER_KEY:-}" ]]; then
  printf '%s\n' "$KHEISH_AUTH_STORE_MASTER_KEY" >"$AUTH_MASTER_KEY_FILE"
  chmod 600 "$AUTH_MASTER_KEY_FILE"
  unset KHEISH_AUTH_STORE_MASTER_KEY
  export KHEISH_AUTH_STORE_MASTER_KEY_FILE="$AUTH_MASTER_KEY_FILE"
elif [[ -z "${KHEISH_AUTH_STORE_MASTER_KEY_FILE:-}" ]]; then
  "$BIN" secrets generate >"$AUTH_MASTER_KEY_FILE"
  chmod 600 "$AUTH_MASTER_KEY_FILE"
  export KHEISH_AUTH_STORE_MASTER_KEY_FILE="$AUTH_MASTER_KEY_FILE"
fi

"$BIN" secrets generate >"$ADMIN_TOKEN_FILE"
chmod 600 "$ADMIN_TOKEN_FILE"
MANAGED_SECRET_VALUE="$("$BIN" secrets generate)"

cat >"$MANAGED_SECRET_STACK_FILE" <<YAML
apiVersion: kheish.ai/v1alpha1
kind: KheishStack
metadata:
  name: managed-secret-canary
spec:
  requires:
    secrets:
      - ref: $MANAGED_SECRET_SLOT
        provider: generic
        value_env: $MANAGED_SECRET_ENV_NAME
YAML

docker pull "$GITHUB_MCP_IMAGE_REF" >"$EVIDENCE/github-mcp-docker-pull.log"
GITHUB_MCP_RUN_IMAGE="$(docker image inspect \
  --format '{{.Id}}' \
  "$GITHUB_MCP_IMAGE_REF")"
printf '%s\n' "$GITHUB_MCP_RUN_IMAGE" >"$EVIDENCE/github-mcp-image-id.txt"

python3 - "$STACK_FILE" "$STACK_PLAN_FILE" <<'PY'
import pathlib
import sys

source = pathlib.Path(sys.argv[1])
plan_target = pathlib.Path(sys.argv[2])
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
plan_target.write_text("\n".join(plan_lines) + "\n", encoding="utf-8")
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

env "$MANAGED_SECRET_ENV_NAME=$MANAGED_SECRET_VALUE" \
  "$BIN" --output json secrets set "$MANAGED_SECRET_SLOT" \
  --provider generic \
  --from-env "$MANAGED_SECRET_ENV_NAME" \
  --offline \
  --state-root "$STATE_ROOT" \
  >"$EVIDENCE/managed-secret-preseed.json"

python3 - "$MCP_CONFIG" "$GITHUB_MCP_RUN_IMAGE" "$GITHUB_MCP_CONTAINER_NAME" <<'PY'
import json
import os
import pathlib
import re
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
DAEMON_ENV+=("$MANAGED_SECRET_ENV_NAME=$MANAGED_SECRET_VALUE")

OPENAI_SERVE_ARGS=(
  --provider openai
  --model gpt-5.5
)
if [[ "$RUN_STACK_FLOW" == "1" ]]; then
  OPENAI_SERVE_ARGS+=(--openai-auth-source "$OPENAI_FLOW_AUTH_SOURCE")
  if [[ -n "${KHEISH_E2E_OPENAI_AUTH_FILE:-}" ]]; then
    OPENAI_SERVE_ARGS+=(--openai-auth-file "$KHEISH_E2E_OPENAI_AUTH_FILE")
  elif [[ "$OPENAI_FLOW_AUTH_SOURCE" == "codex" && -f "${HOME:-$ROOT}/.codex/auth.json" ]]; then
    OPENAI_SERVE_ARGS+=(--openai-auth-file "${HOME:-$ROOT}/.codex/auth.json")
  fi
else
  OPENAI_SERVE_ARGS+=(--api-key "sk-e2e-no-network")
fi

env -i "${DAEMON_ENV[@]}" "$BIN" serve \
  --bind "127.0.0.1:$PORT" \
  --state-root "$STATE_ROOT" \
  --workspace-root "$WORKSPACE_ROOT" \
  --mcp-config "$MCP_CONFIG" \
  --mcp-discovery disabled \
  --mcp-profile planning \
  "${OPENAI_SERVE_ARGS[@]}" \
  --http-auth-mode bearer \
  --http-admin-token-file "$ADMIN_TOKEN_FILE" \
  --disable-scheduler \
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

python3 - \
  "$GITHUB_REPOSITORY_FULL_NAME" \
  "$EVIDENCE/mcp-github-list-pull-requests.json" \
  "$EVIDENCE/mcp-github-pull-request-read-input.json" <<'PY'
import json
import pathlib
import sys

owner, repo = sys.argv[1].split("/", 1)
list_result = json.loads(pathlib.Path(sys.argv[2]).read_text(encoding="utf-8"))
content = list_result.get("output", {}).get("output", {}).get("content", [])
pulls = []
if content:
    pulls = json.loads(content[0].get("text", "[]"))
if not pulls:
    raise SystemExit("GitHub list_pull_requests returned no PRs; pull_request_read cannot be exercised")
pathlib.Path(sys.argv[3]).write_text(
    json.dumps(
        {
            "owner": owner,
            "repo": repo,
            "pullNumber": pulls[0]["number"],
            "method": "get",
        },
        separators=(",", ":"),
    )
    + "\n",
    encoding="utf-8",
)
PY
cli mcp tools call mcp__github__pull_request_read \
  --input-file "$EVIDENCE/mcp-github-pull-request-read-input.json" \
  >"$EVIDENCE/mcp-github-pull-request-read.json"

cli mcp tools call mcp__linear__list_issues \
  --input-json '{"limit":1}' \
  >"$EVIDENCE/mcp-linear-list-issues.json"

python3 - "$EVIDENCE/mcp-linear-list-issues.json" "$EVIDENCE/mcp-linear-get-issue-input.json" <<'PY'
import json
import pathlib
import sys

list_result = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
content = list_result.get("output", {}).get("output", {}).get("content", [])
if not content:
    raise SystemExit("Linear list_issues returned no content")
payload = json.loads(content[0].get("text", "{}"))
issues = payload.get("issues") or []
if not issues:
    raise SystemExit("Linear list_issues returned no issues")
issue_id = issues[0].get("id")
if not issue_id:
    raise SystemExit("Linear first issue has no id")
pathlib.Path(sys.argv[2]).write_text(
    json.dumps({"id": issue_id}, separators=(",", ":")) + "\n",
    encoding="utf-8",
)
PY

cli mcp tools call mcp__linear__get_issue \
  --input-file "$EVIDENCE/mcp-linear-get-issue-input.json" \
  >"$EVIDENCE/mcp-linear-get-issue.json"

cli stack validate \
  --file "$STACK_FILE" \
  >"$EVIDENCE/validate.json"

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

cli runs list --session-id feature-pr-loop-v012 \
  >"$EVIDENCE/runs-after-apply.json"

cli tasks list feature-pr-loop-v012 \
  >"$EVIDENCE/tasks-after-apply.json"

if [[ "$RUN_STACK_FLOW" == "1" ]]; then
  if [[ "${KHEISH_E2E_FLOW_DEBUG_FULL:-0}" == "1" ]]; then
    cli runtime set-debug-level full \
      >"$EVIDENCE/stack-flow-debug-level.json"
  fi

  cli playbooks get linear-github-feature-pr-loop \
    >"$EVIDENCE/playbook-after-apply.json"

  python3 - \
    "$EVIDENCE/playbook-after-apply.json" \
    "$EVIDENCE/playbook-digest.txt" \
    "$EVIDENCE/stack-flow-request.json" \
    "$GITHUB_REPOSITORY_FULL_NAME" <<'PY'
import json
import pathlib
import sys

playbook = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
digest_file = pathlib.Path(sys.argv[2])
request_file = pathlib.Path(sys.argv[3])
repo = sys.argv[4]

selected = playbook.get("selected_version") or {}
digest = selected.get("digest")
if not digest:
    raise SystemExit("playbook detail has no selected_version.digest")
digest_file.write_text(digest + "\n", encoding="utf-8")

request = {
    "provider": "openai",
    "content": "\n".join(
        [
            "E2E smoke for the Kheishfile-installed Linear/GitHub feature loop.",
            "Use the persona, session capability scope, declared MCP surface, and playbook installed by the Kheishfile.",
            f"Repository scope: {repo}. Linear scope: Evapayrent.",
            "This smoke validates that the Kheishfile-installed playbook and session can call one read-only MCP tool through the Flow API.",
            "Call exactly one tool: mcp__github__get_me with empty input. Do not call bash or any other local tool.",
            "Do not create, update, close, comment on, branch, push, or delete anything in GitHub or Linear during this smoke run.",
            "Do not spawn subagents for this smoke because it is not an implementation task.",
            "Finish with a concise report that includes these exact strings: linear-github-feature-pr-loop, 0.1.5, feature-pr-loop-v012, openai, gpt-5.5, mcp__github__get_me.",
        ]
    ),
    "generation": {
        "model": "gpt-5.5",
        "allow_parallel_tool_calls": False,
        "reasoning": {"effort": "medium"},
    },
    "metadata": {
        "workflow": "stack-direct-flow-smoke",
        "source": "kheishfile-e2e",
        "repository": repo,
    },
}
request_file.write_text(
    json.dumps(request, indent=2, separators=(",", ": ")) + "\n",
    encoding="utf-8",
)
PY

  PLAYBOOK_DIGEST="$(cat "$EVIDENCE/playbook-digest.txt")"
  cli flows start \
    --flow-id "$STACK_FLOW_ID" \
    --idempotency-key "$STACK_FLOW_ID" \
    --playbook-id linear-github-feature-pr-loop \
    --version "0.1.5" \
    --digest "$PLAYBOOK_DIGEST" \
    --session-id feature-pr-loop-v012 \
    --request-file "$EVIDENCE/stack-flow-request.json" \
    --metadata-json '{"workflow":"stack-direct-flow-smoke","source":"kheishfile-e2e"}' \
    >"$EVIDENCE/stack-flow-start.json"

  python3 - "$EVIDENCE/stack-flow-start.json" "$EVIDENCE/stack-flow-run-id.txt" <<'PY'
import json
import pathlib
import sys

flow = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8"))
run_id = flow.get("run_id")
if not run_id:
    raise SystemExit("flow start returned no run_id")
pathlib.Path(sys.argv[2]).write_text(run_id + "\n", encoding="utf-8")
PY

  STACK_FLOW_RUN_ID="$(cat "$EVIDENCE/stack-flow-run-id.txt")"
  approve_stack_flow_smoke() {
    local decisions_file="$EVIDENCE/stack-flow-approval-decisions.jsonl"
    local poll_file="$EVIDENCE/stack-flow-approvals-poll.json"
    local run_file="$EVIDENCE/stack-flow-run-poll.json"
    local max_polls="${KHEISH_E2E_APPROVAL_POLLS:-450}"
    local poll_index
    : >"$decisions_file"
    for ((poll_index = 1; poll_index <= max_polls; poll_index++)); do
      "$BIN" --base-url "$BASE_URL" --token-file "$ADMIN_TOKEN_FILE" --output json \
        approvals list --session-id feature-pr-loop-v012 \
        >"$poll_file" 2>/dev/null || true

      python3 - "$poll_file" "$decisions_file" <<'PY' |
import json
import pathlib
import sys

poll_file = pathlib.Path(sys.argv[1])
decisions_file = pathlib.Path(sys.argv[2])
allowed = {
    "mcp__github__get_me",
    "mcp__github__list_pull_requests",
    "mcp__github__pull_request_read",
    "mcp__linear__list_issues",
    "mcp__linear__get_issue",
    "mcp__linear__list_comments",
    "mcp__linear__list_issue_statuses",
    "mcp__linear__list_projects",
}
try:
    approvals = json.loads(poll_file.read_text(encoding="utf-8"))
except Exception:
    approvals = []
if not isinstance(approvals, list):
    approvals = []
seen = set()
if decisions_file.exists():
    for line in decisions_file.read_text(encoding="utf-8").splitlines():
        try:
            seen.add(json.loads(line).get("request_id"))
        except Exception:
            pass
for approval in approvals:
    request = approval.get("request", {})
    request_id = request.get("id")
    tool = request.get("tool_name")
    if not request_id or request_id in seen:
        continue
    action = "allow" if tool in allowed else "deny"
    print(json.dumps({"action": action, "request_id": request_id, "tool": tool}))
PY
      while IFS= read -r decision; do
        [[ -n "$decision" ]] || continue
        action="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["action"])' "$decision")"
        request_id="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["request_id"])' "$decision")"
        tool_name="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1]).get("tool") or "")' "$decision")"
        output_file="$EVIDENCE/stack-flow-approval-$request_id.json"
        error_file="$EVIDENCE/stack-flow-approval-$request_id.err"
        if [[ "$action" == "allow" ]]; then
          if "$BIN" --base-url "$BASE_URL" --token-file "$ADMIN_TOKEN_FILE" --output json \
            approvals allow feature-pr-loop-v012 "$request_id" \
            --justification "approved read-only Kheishfile E2E smoke tool: $tool_name" \
            >"$output_file" 2>"$error_file"; then
            printf '%s\n' "$decision" >>"$decisions_file"
          fi
        else
          if "$BIN" --base-url "$BASE_URL" --token-file "$ADMIN_TOKEN_FILE" --output json \
            approvals deny feature-pr-loop-v012 "$request_id" \
            --reason "not part of read-only Kheishfile E2E smoke: $tool_name" \
            --justification "Kheishfile E2E smoke only permits read-only GitHub/Linear MCP probes" \
            >"$output_file" 2>"$error_file"; then
            printf '%s\n' "$decision" >>"$decisions_file"
          fi
        fi
      done

      "$BIN" --base-url "$BASE_URL" --token-file "$ADMIN_TOKEN_FILE" --output json \
        runs get "$STACK_FLOW_RUN_ID" >"$run_file" 2>/dev/null || true
      if python3 - "$run_file" <<'PY'
import json
import pathlib
import sys

try:
    status = json.loads(pathlib.Path(sys.argv[1]).read_text(encoding="utf-8")).get("status")
except Exception:
    status = None
raise SystemExit(0 if status in {"completed", "failed", "cancelled", "interrupted"} else 1)
PY
      then
        return 0
      fi
      sleep 2
    done
  }
  approve_stack_flow_smoke &
  STACK_FLOW_APPROVAL_PID="$!"
  set +e
  timeout "${KHEISH_E2E_FLOW_TIMEOUT_SEC:-900}" \
    "$BIN" --base-url "$BASE_URL" --token-file "$ADMIN_TOKEN_FILE" --output json \
      runs wait "$STACK_FLOW_RUN_ID" \
      >"$EVIDENCE/stack-flow-run-wait.json" \
      2>"$EVIDENCE/stack-flow-run-wait.err"
  STACK_FLOW_WAIT_EXIT="$?"
  set -e
  wait "$STACK_FLOW_APPROVAL_PID" >/dev/null 2>&1 || true
  printf '%s\n' "$STACK_FLOW_WAIT_EXIT" >"$EVIDENCE/stack-flow-run-wait.exit"

  cli flows get "$STACK_FLOW_ID" \
    >"$EVIDENCE/stack-flow-final.json" || true
  cli runs get "$STACK_FLOW_RUN_ID" \
    >"$EVIDENCE/stack-flow-run.json" || true
  cli runs events "$STACK_FLOW_RUN_ID" \
    >"$EVIDENCE/stack-flow-run-events.json" || true
  cli runs external-actions "$STACK_FLOW_RUN_ID" \
    >"$EVIDENCE/stack-flow-external-actions.json" || true
  cli approvals list --session-id feature-pr-loop-v012 \
    >"$EVIDENCE/stack-flow-approvals.json" || true
fi

cli stack validate \
  --file "$MANAGED_SECRET_STACK_FILE" \
  >"$EVIDENCE/managed-secret-validate.json"

cli stack import \
  --file "$MANAGED_SECRET_STACK_FILE" \
  --resource "secret/$MANAGED_SECRET_SLOT" \
  --allow-secret-env \
  >"$EVIDENCE/managed-secret-import.json"

cli stack plan \
  --file "$MANAGED_SECRET_STACK_FILE" \
  --allow-secret-env \
  >"$EVIDENCE/managed-secret-plan.json"

cli stack apply \
  --file "$MANAGED_SECRET_STACK_FILE" \
  --allow-secret-env \
  >"$EVIDENCE/managed-secret-apply.json"

cli stack verify \
  --file "$MANAGED_SECRET_STACK_FILE" \
  >"$EVIDENCE/managed-secret-verify.json"

cli stack diff \
  --file "$MANAGED_SECRET_STACK_FILE" \
  --allow-secret-env \
  >"$EVIDENCE/managed-secret-diff.json"

python3 - "$EVIDENCE" <<'PY' | tee "$EVIDENCE/verdict.json"
import json
import os
import pathlib
import re
import stat
import sys

evidence = pathlib.Path(sys.argv[1])
stack_dir = evidence / "stack"
original_text = (stack_dir / "Kheishfile.yaml").read_text(encoding="utf-8")
playbook_text = (stack_dir / "playbook.yaml").read_text(encoding="utf-8")
plan_text = (stack_dir / "Kheishfile.no-secret-env.yaml").read_text(encoding="utf-8")
live_text = plan_text
mcp_config_text = (evidence / "codex-mcp.toml").read_text(encoding="utf-8")
daemon_log_text = (evidence / "daemon.log").read_text(encoding="utf-8", errors="replace")
evidence_gitignored = (
    (evidence / "evidence-gitignored.txt").read_text(encoding="utf-8").strip()
    == "true"
)
provenance = json.loads((evidence / "provenance.json").read_text(encoding="utf-8"))
status = json.loads((evidence / "status.json").read_text(encoding="utf-8"))
runtime = json.loads((evidence / "runtime.json").read_text(encoding="utf-8"))
daemon_env_keys = set(
    (evidence / "daemon-env-keys.txt").read_text(encoding="utf-8").splitlines()
)
validate = json.loads((evidence / "validate.json").read_text(encoding="utf-8"))
validate_live = json.loads((evidence / "validate-live.json").read_text(encoding="utf-8"))
github_get_me = json.loads((evidence / "mcp-github-get-me.json").read_text(encoding="utf-8"))
github_list_pull_requests = json.loads((evidence / "mcp-github-list-pull-requests.json").read_text(encoding="utf-8"))
github_pull_request_read = json.loads((evidence / "mcp-github-pull-request-read.json").read_text(encoding="utf-8"))
linear_list_issues = json.loads((evidence / "mcp-linear-list-issues.json").read_text(encoding="utf-8"))
linear_get_issue = json.loads((evidence / "mcp-linear-get-issue.json").read_text(encoding="utf-8"))
plan_original = json.loads((evidence / "plan-original.json").read_text(encoding="utf-8"))
apply = json.loads((evidence / "apply.json").read_text(encoding="utf-8"))
verify = json.loads((evidence / "verify.json").read_text(encoding="utf-8"))
apply_second = json.loads((evidence / "apply-second.json").read_text(encoding="utf-8"))
diff = json.loads((evidence / "diff.json").read_text(encoding="utf-8"))
schedules = json.loads((evidence / "schedules-after-apply.json").read_text(encoding="utf-8"))
runs_after_apply = json.loads((evidence / "runs-after-apply.json").read_text(encoding="utf-8"))
tasks_after_apply = json.loads((evidence / "tasks-after-apply.json").read_text(encoding="utf-8"))
managed_validate = json.loads((evidence / "managed-secret-validate.json").read_text(encoding="utf-8"))
managed_import = json.loads((evidence / "managed-secret-import.json").read_text(encoding="utf-8"))
managed_plan = json.loads((evidence / "managed-secret-plan.json").read_text(encoding="utf-8"))
managed_apply = json.loads((evidence / "managed-secret-apply.json").read_text(encoding="utf-8"))
managed_verify = json.loads((evidence / "managed-secret-verify.json").read_text(encoding="utf-8"))
managed_diff = json.loads((evidence / "managed-secret-diff.json").read_text(encoding="utf-8"))
ledger = json.loads((evidence / "state/kheish-apply/ledger.json").read_text(encoding="utf-8"))


def mode_is_private(path):
    return stat.S_IMODE(path.stat().st_mode) & 0o077 == 0


def load_json_optional(name):
    path = evidence / name
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text(encoding="utf-8"))
    except Exception as error:
        return {"__json_error": str(error)}


def read_text_optional(name):
    path = evidence / name
    if not path.exists():
        return None
    return path.read_text(encoding="utf-8", errors="replace")


run_stack_flow = os.environ.get("KHEISH_E2E_RUN_STACK_FLOW") == "1"
playbook_after_apply = load_json_optional("playbook-after-apply.json")
stack_flow_request = load_json_optional("stack-flow-request.json")
stack_flow_start = load_json_optional("stack-flow-start.json")
stack_flow_final = load_json_optional("stack-flow-final.json")
stack_flow_run_wait = load_json_optional("stack-flow-run-wait.json")
stack_flow_run = load_json_optional("stack-flow-run.json")
stack_flow_events = load_json_optional("stack-flow-run-events.json")
stack_flow_external_actions = load_json_optional("stack-flow-external-actions.json")
stack_flow_approvals = load_json_optional("stack-flow-approvals.json")
stack_flow_approval_decisions_text = read_text_optional("stack-flow-approval-decisions.jsonl")
stack_flow_wait_exit_text = read_text_optional("stack-flow-run-wait.exit")
stack_flow_wait_exit = (
    int(stack_flow_wait_exit_text.strip())
    if stack_flow_wait_exit_text and stack_flow_wait_exit_text.strip().isdigit()
    else None
)




def action_key(action):
    return (
        action.get("phase"),
        action.get("resource_type"),
        action.get("resource_id"),
        action.get("operation"),
    )


def list_item_block(text, marker):
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if line.strip() != marker:
            continue
        indent = len(line) - len(line.lstrip(" "))
        end = len(lines)
        for cursor in range(index + 1, len(lines)):
            candidate = lines[cursor]
            stripped = candidate.strip()
            candidate_indent = len(candidate) - len(candidate.lstrip(" "))
            if stripped and candidate_indent <= indent:
                end = cursor
                break
        return "\n".join(lines[index:end])
    return ""


def has_all(text, snippets):
    return all(snippet in text for snippet in snippets)


linear_secret_block = list_item_block(original_text, "- ref: mcp.linear.LINEAR_API_KEY")
github_secret_block = list_item_block(original_text, "- ref: mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN")
intake_schedule_block = list_item_block(original_text, "- name: linear-intake-0800-v015")
followup_schedule_block = list_item_block(original_text, "- name: github-review-followup-hourly-v015")
original_manifest_expected = {
    "linear_secret_source": has_all(
        linear_secret_block,
        ["provider: generic", "value_env: LINEAR_API_KEY"],
    ),
    "github_secret_source": has_all(
        github_secret_block,
        ["provider: generic", "value_env: GITHUB_PERSONAL_ACCESS_TOKEN"],
    ),
    "intake_schedule_definition": has_all(
        intake_schedule_block,
        [
            "target_session_id: feature-pr-loop-v012",
            'expression: "0 0 8 * * *"',
            "timezone: Europe/Paris",
            "overlap_policy: skip",
            "type: coalesce_once",
            "playbook_id: linear-github-feature-pr-loop",
            'version: "0.1.5"',
            "provider: openai",
            "content_file: intake.md",
            "model: gpt-5.5",
            "max_output_tokens: 6000",
            "effort: medium",
            "workflow: linear-intake",
            "project: Evapayrent",
            "repository: graniet/evapayrent",
            "max_tickets: 1",
        ],
    ),
    "followup_schedule_definition": has_all(
        followup_schedule_block,
        [
            "target_session_id: feature-pr-loop-v012",
            'expression: "0 0 * * * *"',
            "timezone: Europe/Paris",
            "overlap_policy: skip",
            "type: coalesce_once",
            "playbook_id: linear-github-feature-pr-loop",
            'version: "0.1.5"',
            "provider: openai",
            "content_file: review-followup.md",
            "model: gpt-5.5",
            "max_output_tokens: 6000",
            "effort: medium",
            "workflow: github-review-followup",
            "project: Evapayrent",
            "repository: graniet/evapayrent",
        ],
    ),
    "playbook_publish_active": bool(
        re.search(
            r"(?ms)manifest_file:\s*playbook\.yaml.*?publish:.*?status:\s*active",
            original_text,
        )
    ),
    "playbook_runtime_defaults": has_all(
        playbook_text,
        [
            "playbook_id: linear-github-feature-pr-loop",
            'version: "0.1.5"',
            "model: gpt-5.5",
            "workflow: linear-github-feature-loop",
        ],
    ),
}


expected_apply = {
    ("personas", "persona", "feature-pr-operator-v012", "create"),
    ("sessions", "session", "feature-pr-loop-v012", "create"),
    ("playbooks", "playbook", "linear-github-feature-pr-loop/0.1.5", "apply"),
    ("schedules", "schedule", "linear-intake-0800-v015", "create"),
    ("schedules", "schedule", "github-review-followup-hourly-v015", "create"),
}
expected_managed_imports = {
    ("import", "secret", "stack.e2e.MANAGED_SECRET", "adopt"),
}
expected_managed_plan_actions = {
    ("secrets", "secret", "stack.e2e.MANAGED_SECRET", "noop"),
}
expected_requirement_actions = {
    ("requirements", "mcp_server", "github", "noop"),
    ("requirements", "mcp_server", "linear", "noop"),
    ("requirements", "mcp_tool", "mcp__github__get_me", "noop"),
    ("requirements", "mcp_tool", "mcp__github__search_code", "noop"),
    ("requirements", "mcp_tool", "mcp__github__search_pull_requests", "noop"),
    ("requirements", "mcp_tool", "mcp__github__get_file_contents", "noop"),
    ("requirements", "mcp_tool", "mcp__github__list_branches", "noop"),
    ("requirements", "mcp_tool", "mcp__github__create_branch", "noop"),
    ("requirements", "mcp_tool", "mcp__github__create_or_update_file", "noop"),
    ("requirements", "mcp_tool", "mcp__github__push_files", "noop"),
    ("requirements", "mcp_tool", "mcp__github__create_pull_request", "noop"),
    ("requirements", "mcp_tool", "mcp__github__update_pull_request", "noop"),
    ("requirements", "mcp_tool", "mcp__github__list_pull_requests", "noop"),
    ("requirements", "mcp_tool", "mcp__github__pull_request_read", "noop"),
    ("requirements", "mcp_tool", "mcp__github__add_reply_to_pull_request_comment", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__get_issue", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__list_comments", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__list_issues", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__list_issue_statuses", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__list_projects", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__save_comment", "noop"),
    ("requirements", "mcp_tool", "mcp__linear__save_issue", "noop"),
}
expected_schedule_actions = {
    ("schedules", "schedule", "linear-intake-0800-v015", "create"),
    ("schedules", "schedule", "github-review-followup-hourly-v015", "create"),
}
expected_secret_actions = {
    ("secrets", "secret", "mcp.linear.LINEAR_API_KEY", "verify"),
    ("secrets", "secret", "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN", "verify"),
}
expected_playbook_actions = {
    ("playbooks", "playbook", "linear-github-feature-pr-loop/0.1.5", "create"),
    ("playbooks", "playbook_release", "linear-github-feature-pr-loop/0.1.5", "update"),
}
expected_runtime_mcp_servers = {
    ("github", "codex_config", True, ("mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN",)),
    ("linear", "built_in_catalog", True, ("mcp.linear.LINEAR_API_KEY",)),
}
expected_verify_checks = {
    ("requirement", "mcp_server/github"),
    ("requirement", "mcp_server/linear"),
    ("requirement", "mcp_tool/mcp__github__get_me"),
    ("requirement", "mcp_tool/mcp__github__search_code"),
    ("requirement", "mcp_tool/mcp__github__search_pull_requests"),
    ("requirement", "mcp_tool/mcp__github__get_file_contents"),
    ("requirement", "mcp_tool/mcp__github__list_branches"),
    ("requirement", "mcp_tool/mcp__github__create_branch"),
    ("requirement", "mcp_tool/mcp__github__create_or_update_file"),
    ("requirement", "mcp_tool/mcp__github__push_files"),
    ("requirement", "mcp_tool/mcp__github__create_pull_request"),
    ("requirement", "mcp_tool/mcp__github__update_pull_request"),
    ("requirement", "mcp_tool/mcp__github__list_pull_requests"),
    ("requirement", "mcp_tool/mcp__github__pull_request_read"),
    ("requirement", "mcp_tool/mcp__github__add_reply_to_pull_request_comment"),
    ("requirement", "mcp_tool/mcp__linear__get_issue"),
    ("requirement", "mcp_tool/mcp__linear__list_comments"),
    ("requirement", "mcp_tool/mcp__linear__list_issues"),
    ("requirement", "mcp_tool/mcp__linear__list_issue_statuses"),
    ("requirement", "mcp_tool/mcp__linear__list_projects"),
    ("requirement", "mcp_tool/mcp__linear__save_comment"),
    ("requirement", "mcp_tool/mcp__linear__save_issue"),
    ("secret", "mcp.linear.LINEAR_API_KEY"),
    ("secret", "mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN"),
    ("persona", "feature-pr-operator-v012"),
    ("session", "feature-pr-loop-v012"),
    ("playbook", "linear-github-feature-pr-loop/0.1.5"),
    ("schedule", "linear-intake-0800-v015"),
    ("schedule", "github-review-followup-hourly-v015"),
    ("probe", "persona"),
    ("probe", "session"),
    ("probe", "intake-schedule"),
    ("probe", "followup-schedule"),
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
secret_actions = [
    action for action in plan_actions
    if action.get("phase") == "secrets"
    and action.get("resource_type") == "secret"
]
playbook_actions = [
    action for action in plan_actions
    if action.get("phase") == "playbooks"
    and action.get("resource_type") in {"playbook", "playbook_release"}
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
runtime_mcp_server_keys = {
    (
        server.get("server"),
        server.get("source"),
        bool(server.get("connected")),
        tuple(server.get("credential_secret_refs", [])),
    )
    for server in runtime.get("mcp", {}).get("servers", [])
}
mcp_config_raw_secret_leaks = [
    name
    for name in ["LINEAR_API_KEY", "GITHUB_PERSONAL_ACCESS_TOKEN"]
    if (value := os.environ.get(name, "")) and value in mcp_config_text
]
managed_stack = ledger.get("stacks", {}).get("managed-secret-canary", {})
managed_resources = managed_stack.get("resources", {})
managed_secrets = managed_stack.get("secrets", {})
managed_apply_checks = managed_apply.get("verification", {}).get("checks", [])
managed_failed_apply_checks = [
    check for check in managed_apply_checks if not check.get("ok")
]
managed_verify_checks = managed_verify.get("checks", [])
managed_failed_verify_checks = [
    check for check in managed_verify_checks if not check.get("ok")
]
managed_plan_secret_actions = [
    action for action in managed_plan.get("actions", [])
    if action.get("phase") == "secrets"
    and action.get("resource_type") == "secret"
]
schedules_by_name = {schedule.get("name"): schedule for schedule in schedules}
expected_schedule_names = {"linear-intake-0800-v015", "github-review-followup-hourly-v015"}
required_schedule_quiescence_fields = {
    "status",
    "execution_count",
    "next_fire_at_ms",
    "scheduler_retry_attempt",
    "consecutive_failures",
}
schedule_quiescence_fields_present = all(
    name in schedules_by_name
    and required_schedule_quiescence_fields.issubset(schedules_by_name[name])
    for name in expected_schedule_names
)
schedule_dispatch_quiescent = (
    schedule_quiescence_fields_present
    and all(
        schedules_by_name[name]["status"] == "active"
        and schedules_by_name[name]["execution_count"] == 0
        and schedules_by_name[name]["next_fire_at_ms"] is not None
        and schedules_by_name[name]["scheduler_retry_attempt"] == 0
        and schedules_by_name[name]["consecutive_failures"] == 0
        for name in expected_schedule_names
    )
)
sensitive_evidence_guarded_locally = (
    evidence_gitignored
    and mode_is_private(evidence / "admin.token")
    and mode_is_private(evidence / "auth-store-master.key")
    and mode_is_private(evidence / "state/auth/global-slots.json")
)
stack_flow_selected_playbook = (
    (playbook_after_apply or {}).get("selected_version")
    if isinstance(playbook_after_apply, dict)
    else None
) or {}
stack_flow_start_ref = (
    (stack_flow_start or {}).get("playbook_ref")
    if isinstance(stack_flow_start, dict)
    else None
) or {}
stack_flow_final_ref = (
    (stack_flow_final or {}).get("playbook_ref")
    if isinstance(stack_flow_final, dict)
    else None
) or {}
stack_flow_run_id = (
    (stack_flow_start or {}).get("run_id")
    if isinstance(stack_flow_start, dict)
    else None
)
stack_flow_run_wait_status = (
    (stack_flow_run_wait or {}).get("status")
    if isinstance(stack_flow_run_wait, dict)
    else None
)
stack_flow_run_status = (
    (stack_flow_run or {}).get("status")
    if isinstance(stack_flow_run, dict)
    else None
)
stack_flow_final_status = (
    (stack_flow_final or {}).get("status")
    if isinstance(stack_flow_final, dict)
    else None
)
stack_flow_request_content = (
    (stack_flow_request or {}).get("content", "")
    if isinstance(stack_flow_request, dict)
    else ""
)
stack_flow_request_generation = (
    (stack_flow_request or {}).get("generation")
    if isinstance(stack_flow_request, dict)
    else None
) or {}
stack_flow_request_reasoning = stack_flow_request_generation.get("reasoning") or {}
stack_flow_approval_count = (
    len(stack_flow_approvals)
    if isinstance(stack_flow_approvals, list)
    else None
)
stack_flow_event_tools = []
if isinstance(stack_flow_events, list):
    for entry in stack_flow_events:
        event = entry.get("event", {}) if isinstance(entry, dict) else {}
        if event.get("type") != "waiting_for_approval":
            continue
        for request in event.get("requests", []):
            if isinstance(request, dict) and request.get("tool_name"):
                stack_flow_event_tools.append(request["tool_name"])
stack_flow_approval_decisions = []
if stack_flow_approval_decisions_text:
    for line in stack_flow_approval_decisions_text.splitlines():
        try:
            decision = json.loads(line)
        except Exception:
            continue
        if isinstance(decision, dict):
            stack_flow_approval_decisions.append(decision)
stack_flow_decision_tools = [
    decision.get("tool")
    for decision in stack_flow_approval_decisions
    if decision.get("tool")
]
stack_flow_denied_tools = [
    decision.get("tool")
    for decision in stack_flow_approval_decisions
    if decision.get("action") == "deny"
]
stack_flow_output_text = "\n".join(
    output.get("content", "")
    for output in (stack_flow_run or {}).get("outputs", [])
    if isinstance(output, dict)
)
stack_flow_external_action_records = (
    stack_flow_external_actions
    if isinstance(stack_flow_external_actions, list)
    else []
)
stack_flow_external_action_has_tool_request = any(
    record.get("run_id") == stack_flow_run_id
    and record.get("phase") == "request"
    and record.get("kind") == "tool"
    and record.get("target") == "mcp_tool:mcp__github__get_me"
    for record in stack_flow_external_action_records
    if isinstance(record, dict)
)
stack_flow_external_action_has_tool_ok = any(
    record.get("run_id") == stack_flow_run_id
    and record.get("phase") == "response"
    and record.get("kind") == "tool"
    and record.get("target") == "mcp_tool:mcp__github__get_me"
    and record.get("outcome") == "ok"
    for record in stack_flow_external_action_records
    if isinstance(record, dict)
)
stack_flow_external_action_has_mcp_request = any(
    record.get("run_id") == stack_flow_run_id
    and record.get("phase") == "request"
    and record.get("kind") == "mcp"
    and record.get("target") == "mcp:github/get_me"
    for record in stack_flow_external_action_records
    if isinstance(record, dict)
)
stack_flow_external_action_has_mcp_ok = any(
    record.get("run_id") == stack_flow_run_id
    and record.get("phase") == "response"
    and record.get("kind") == "mcp"
    and record.get("target") == "mcp:github/get_me"
    and record.get("outcome") == "ok"
    for record in stack_flow_external_action_records
    if isinstance(record, dict)
)
stack_flow_exercised_tools = sorted(
    set(stack_flow_event_tools).union(stack_flow_decision_tools)
)
mcp_tool_call_exercised = all(
    [
        github_get_me.get("tool_name") == "mcp__github__get_me",
        github_list_pull_requests.get("tool_name") == "mcp__github__list_pull_requests",
        github_pull_request_read.get("tool_name") == "mcp__github__pull_request_read",
        linear_list_issues.get("tool_name") == "mcp__linear__list_issues",
        linear_get_issue.get("tool_name") == "mcp__linear__get_issue",
    ]
)
stack_flow_checks = {
    "enabled": run_stack_flow,
    "playbook_detail_loaded": isinstance(playbook_after_apply, dict)
    and "__json_error" not in playbook_after_apply,
    "flow_start_loaded": isinstance(stack_flow_start, dict)
    and "__json_error" not in stack_flow_start,
    "flow_final_loaded": isinstance(stack_flow_final, dict)
    and "__json_error" not in stack_flow_final,
    "run_wait_loaded": isinstance(stack_flow_run_wait, dict)
    and "__json_error" not in stack_flow_run_wait,
    "run_loaded": isinstance(stack_flow_run, dict)
    and "__json_error" not in stack_flow_run,
    "run_id_recorded": bool(stack_flow_run_id),
    "flow_start_uses_stack_playbook": (
        stack_flow_start_ref.get("playbook_id") == "linear-github-feature-pr-loop"
        and stack_flow_start_ref.get("version") == "0.1.5"
    ),
    "flow_start_uses_stack_session": (
        isinstance(stack_flow_start, dict)
        and stack_flow_start.get("session_id") == "feature-pr-loop-v012"
    ),
    "flow_digest_matches_published_playbook": (
        bool(stack_flow_selected_playbook.get("digest"))
        and stack_flow_start_ref.get("digest") == stack_flow_selected_playbook.get("digest")
        and stack_flow_final_ref.get("digest") == stack_flow_selected_playbook.get("digest")
    ),
    "request_is_bounded_smoke": all(
        snippet in stack_flow_request_content
        for snippet in [
            "Kheishfile-installed",
            "Call exactly one tool: mcp__github__get_me",
            "Do not create, update, close, comment on, branch, push, or delete anything",
        ]
    ),
    "request_uses_openai_gpt55_medium_single_tool": (
        (stack_flow_request or {}).get("provider") == "openai"
        and stack_flow_request_generation.get("model") == "gpt-5.5"
        and stack_flow_request_generation.get("allow_parallel_tool_calls") is False
        and stack_flow_request_reasoning.get("effort") == "medium"
    ),
    "flow_requested_github_get_me": "mcp__github__get_me" in stack_flow_exercised_tools,
    "flow_requested_no_denied_tools": not stack_flow_denied_tools,
    "flow_approval_decisions_recorded": (
        not run_stack_flow
        or bool(stack_flow_approval_decisions)
    ),
    "flow_output_mentions_expected_contract": all(
        snippet in stack_flow_output_text
        for snippet in [
            "linear-github-feature-pr-loop",
            "0.1.5",
            "feature-pr-loop-v012",
            "openai",
            "gpt-5.5",
            "mcp__github__get_me",
        ]
    ),
    "flow_external_actions_loaded": (
        not run_stack_flow
        or isinstance(stack_flow_external_actions, list)
    ),
    "flow_external_tool_request_recorded": stack_flow_external_action_has_tool_request,
    "flow_external_tool_response_ok": stack_flow_external_action_has_tool_ok,
    "flow_external_mcp_request_recorded": stack_flow_external_action_has_mcp_request,
    "flow_external_mcp_response_ok": stack_flow_external_action_has_mcp_ok,
    "flow_exercised_tools": stack_flow_exercised_tools,
    "flow_denied_tools": stack_flow_denied_tools,
    "wait_exit_zero": stack_flow_wait_exit == 0,
    "run_completed": stack_flow_run_wait_status == "completed"
    and stack_flow_run_status == "completed",
    "flow_succeeded": stack_flow_final_status == "succeeded",
    "no_pending_approvals": stack_flow_approval_count == 0,
}
stack_flow_required = [
    "playbook_detail_loaded",
    "flow_start_loaded",
    "flow_final_loaded",
    "run_wait_loaded",
    "run_loaded",
    "run_id_recorded",
    "flow_start_uses_stack_playbook",
    "flow_start_uses_stack_session",
    "flow_digest_matches_published_playbook",
    "request_is_bounded_smoke",
    "request_uses_openai_gpt55_medium_single_tool",
    "flow_requested_github_get_me",
    "flow_requested_no_denied_tools",
    "flow_approval_decisions_recorded",
    "flow_output_mentions_expected_contract",
    "flow_external_actions_loaded",
    "flow_external_tool_request_recorded",
    "flow_external_tool_response_ok",
    "flow_external_mcp_request_recorded",
    "flow_external_mcp_response_ok",
    "wait_exit_zero",
    "run_completed",
    "flow_succeeded",
    "no_pending_approvals",
]
stack_flow_checks["passed"] = (
    not run_stack_flow
    or all(stack_flow_checks.get(name) is True for name in stack_flow_required)
)

check = {
    "provenance_recorded": bool(
        provenance.get("binary", {}).get("sha256")
        and provenance.get("binary", {}).get("mtime_ns")
        and provenance.get("git", {}).get("head")
    ),
    "original_manifest_expected": original_manifest_expected,
    "plan_copy_has_no_value_env": "value_env:" not in plan_text,
    "plan_copy_keeps_schedule_drift": all(
        name in plan_text
        for name in ["linear-intake-0800-v015", "github-review-followup-hourly-v015"]
    ),
    "live_copy_has_no_value_env": "value_env:" not in live_text,
    "live_copy_keeps_schedules": (
        "linear-intake-0800-v015" in live_text
        and "github-review-followup-hourly-v015" in live_text
        and "intake-schedule" in live_text
        and "followup-schedule" in live_text
    ),
    "scheduler_disabled_warning_logged": (
        "background schedule dispatch worker disabled by configuration" in daemon_log_text
    ),
    "status_reports_scheduler_dispatch_worker_disabled": (
        status.get("schedules", {}).get("dispatch_worker_enabled") is False
    ),
    "sensitive_evidence_guarded_locally": sensitive_evidence_guarded_locally,
    "evidence_gitignored": evidence_gitignored,
    "mcp_config_raw_secret_leaks": mcp_config_raw_secret_leaks,
    "runtime_mcp_servers_expected": runtime_mcp_server_keys == expected_runtime_mcp_servers,
    "daemon_env_checked": "__unavailable__" not in daemon_env_keys,
    "daemon_master_key_env_absent": "KHEISH_AUTH_STORE_MASTER_KEY" not in daemon_env_keys,
    "daemon_master_key_file_env_present": "KHEISH_AUTH_STORE_MASTER_KEY_FILE" in daemon_env_keys,
    "daemon_managed_canary_env_present": "KHEISH_E2E_MANAGED_SECRET_VALUE" in daemon_env_keys,
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
    "github_pull_request_read_called": github_pull_request_read.get("tool_name") == "mcp__github__pull_request_read",
    "github_pull_request_read_is_error": github_pull_request_read.get("output", {}).get("output", {}).get("is_error"),
    "linear_list_issues_called": linear_list_issues.get("tool_name") == "mcp__linear__list_issues",
    "linear_list_issues_is_error": linear_list_issues.get("output", {}).get("output", {}).get("is_error"),
    "linear_get_issue_called": linear_get_issue.get("tool_name") == "mcp__linear__get_issue",
    "linear_get_issue_is_error": linear_get_issue.get("output", {}).get("output", {}).get("is_error"),
    "original_validated": bool(validate.get("valid")),
    "live_validated": bool(validate_live.get("valid")),
    "original_plan_valid": bool(plan_original.get("valid")),
    "original_plan_errors": plan_original.get("errors", []),
    "original_requirement_actions": sorted(action_key(action) for action in requirement_actions),
    "original_schedule_actions": sorted(action_key(action) for action in schedule_actions),
    "original_secret_actions": sorted(action_key(action) for action in secret_actions),
    "original_playbook_actions": sorted(action_key(action) for action in playbook_actions),
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
    "schedule_names_after_apply": sorted(schedules_by_name),
    "schedule_quiescence_fields_present": schedule_quiescence_fields_present,
    "schedule_dispatch_quiescent": schedule_dispatch_quiescent,
    "runs_after_apply": len(runs_after_apply),
    "tasks_after_apply": len(tasks_after_apply),
    "stack_direct_flow": stack_flow_checks,
    "mcp_tool_call_exercised": mcp_tool_call_exercised,
    "managed_secret_validated": bool(managed_validate.get("valid")),
    "managed_secret_import_actions": sorted(action_key(action) for action in managed_import.get("adopted", [])),
    "managed_secret_plan_valid": bool(managed_plan.get("valid")),
    "managed_secret_plan_errors": managed_plan.get("errors", []),
    "managed_secret_plan_actions": sorted(action_key(action) for action in managed_plan_secret_actions),
    "managed_secret_apply_verification": bool(managed_apply.get("verification", {}).get("valid")),
    "managed_secret_failed_apply_checks": managed_failed_apply_checks,
    "managed_secret_verified": bool(managed_verify.get("valid")),
    "managed_secret_failed_verify_checks": managed_failed_verify_checks,
    "managed_secret_diff_valid": bool(managed_diff.get("valid")),
    "managed_secret_diff_errors": managed_diff.get("errors", []),
    "managed_secret_diff_actions": len(managed_diff.get("actions", [])),
    "managed_secret_ledger_resource_owned": "secret/stack.e2e.MANAGED_SECRET" in managed_resources,
    "managed_secret_ledger_fingerprint_recorded": "stack.e2e.MANAGED_SECRET" in managed_secrets,
}
failed = (
    not check["provenance_recorded"]
    or not all(check["original_manifest_expected"].values())
    or not check["plan_copy_has_no_value_env"]
    or not check["plan_copy_keeps_schedule_drift"]
    or not check["live_copy_has_no_value_env"]
    or not check["live_copy_keeps_schedules"]
    or not check["scheduler_disabled_warning_logged"]
    or not check["status_reports_scheduler_dispatch_worker_disabled"]
    or not check["sensitive_evidence_guarded_locally"]
    or bool(check["mcp_config_raw_secret_leaks"])
    or not check["runtime_mcp_servers_expected"]
    or not check["daemon_env_checked"]
    or not check["daemon_master_key_env_absent"]
    or not check["daemon_master_key_file_env_present"]
    or not check["daemon_managed_canary_env_present"]
    or not check["daemon_secret_env_scrubbed"]
    or not check["runtime_has_planning_profile"]
    or not check["github_get_me_called"]
    or check["github_get_me_is_error"] is not False
    or not check["github_list_pull_requests_called"]
    or check["github_list_pull_requests_is_error"] is not False
    or not check["github_pull_request_read_called"]
    or check["github_pull_request_read_is_error"] is not False
    or not check["linear_list_issues_called"]
    or check["linear_list_issues_is_error"] is not False
    or not check["linear_get_issue_called"]
    or check["linear_get_issue_is_error"] is not False
    or not check["original_validated"]
    or not check["live_validated"]
    or not check["original_plan_valid"]
    or bool(check["original_plan_errors"])
    or set(check["original_requirement_actions"]) != expected_requirement_actions
    or set(check["original_schedule_actions"]) != expected_schedule_actions
    or set(check["original_secret_actions"]) != expected_secret_actions
    or set(check["original_playbook_actions"]) != expected_playbook_actions
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
    or check["schedules_after_apply"] != 2
    or set(check["schedule_names_after_apply"]) != expected_schedule_names
    or not check["schedule_quiescence_fields_present"]
    or not check["schedule_dispatch_quiescent"]
    or check["runs_after_apply"] != 0
    or check["tasks_after_apply"] != 0
    or not check["stack_direct_flow"]["passed"]
    or not check["mcp_tool_call_exercised"]
    or not check["managed_secret_validated"]
    or set(check["managed_secret_import_actions"]) != expected_managed_imports
    or not check["managed_secret_plan_valid"]
    or bool(check["managed_secret_plan_errors"])
    or set(check["managed_secret_plan_actions"]) != expected_managed_plan_actions
    or not check["managed_secret_apply_verification"]
    or bool(check["managed_secret_failed_apply_checks"])
    or not check["managed_secret_verified"]
    or bool(check["managed_secret_failed_verify_checks"])
    or not check["managed_secret_diff_valid"]
    or bool(check["managed_secret_diff_errors"])
    or check["managed_secret_diff_actions"] != 0
    or not check["managed_secret_ledger_resource_owned"]
    or not check["managed_secret_ledger_fingerprint_recorded"]
)
verdict = {
    "scenario": "linear_github_feature_loop_live",
    "status": "failed" if failed else "passed",
    "mode": (
        "stack_direct_flow_live"
        if run_stack_flow
        else "non_destructive_scheduler_disabled_full_apply"
    ),
    "limits": [
        "The original Kheishfile is validated. Planning uses an evidence copy with value_env sources removed after offline secret pre-seeding, including schedule drift.",
        "The live apply uses an evidence copy with value_env sources removed and schedules retained; the daemon scheduler worker is disabled so schedules are reconciled but not dispatched.",
        "When KHEISH_E2E_RUN_STACK_FLOW=1, the harness starts the playbook/session installed by the Kheishfile through the Flow API with a bounded read-only MCP smoke request.",
        "The live provider probe calls non-destructive GitHub and Linear MCP read tools before stack reconciliation.",
        "Managed value_env secret import/apply/diff is covered by a synthetic canary stack, not by the provider tokens.",
        "The evidence directory contains encrypted auth-store state and local admin credentials; keep it local and do not publish it as a CI artifact.",
    ],
    "check": check,
    "evidence_root": str(evidence),
}
print(json.dumps(verdict, indent=2))
sys.exit(1 if failed else 0)
PY

echo "evidence: $EVIDENCE"
