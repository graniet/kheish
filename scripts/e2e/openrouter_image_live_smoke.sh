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

PLANNER_MODEL="${KHEISH_OPENROUTER_IMAGE_PLANNER_MODEL:-${KHEISH_OPENAI_MODEL:-gpt-5.4}}"
IMAGE_MODEL="${KHEISH_OPENROUTER_IMAGE_MODEL:-openai/gpt-5-image-mini}"
RUN_WAIT_SECONDS="${KHEISH_OPENROUTER_IMAGE_SMOKE_WAIT_SECONDS:-900}"

if [[ -z "${OPENAI_API_KEY:-}" && -n "${KHEISH_OPENAI_API_KEY:-}" ]]; then
  OPENAI_API_KEY="$KHEISH_OPENAI_API_KEY"
  export OPENAI_API_KEY
fi

if [[ -z "${OPENROUTER_API_KEY:-}" && -n "${KHEISH_OPENROUTER_API_KEY:-}" ]]; then
  OPENROUTER_API_KEY="$KHEISH_OPENROUTER_API_KEY"
  export OPENROUTER_API_KEY
fi

if [[ -z "${OPENAI_API_KEY:-}" || -z "${OPENROUTER_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY and OPENROUTER_API_KEY are required for this live smoke." >&2
  exit 2
fi

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPENROUTER_IMAGE_SMOKE_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPENROUTER_IMAGE_SMOKE_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/openrouter-image-live.XXXXXX")"
STATE="$TMP/state"
WORKSPACE="$TMP/workspace"
ROUTES="$TMP/routes.toml"
DAEMON_PID=""
BASE_URL=""

cleanup() {
  if [[ -n "${DAEMON_PID:-}" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    kill "$DAEMON_PID" 2>/dev/null || true
    wait "$DAEMON_PID" 2>/dev/null || true
  fi
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

kheish() {
  "$BIN" --base-url "$BASE_URL" --output json "$@"
}

submit_and_wait_with_approvals() {
  local session_id="$1"
  local prompt_file="$2"
  local allowed_tools="$3"
  local run_json
  run_json="$(kheish sessions input "$session_id" --provider openai --model "$PLANNER_MODEL" --content-file "$prompt_file")"
  local run_id
  run_id="$(jq -r '.run_id' <<<"$run_json")"
  if [[ -z "$run_id" || "$run_id" == "null" ]]; then
    echo "sessions input did not return a run_id" >&2
    exit 1
  fi

  local deadline=$((SECONDS + RUN_WAIT_SECONDS))
  while (( SECONDS < deadline )); do
    run_json="$(kheish runs get "$run_id")"
    local status
    status="$(jq -r '.status' <<<"$run_json")"
    case "$status" in
      completed|failed|cancelled|canceled|interrupted)
        printf '%s\n' "$run_json"
        return 0
        ;;
      waiting_for_approval)
        local approvals_json
        approvals_json="$(kheish approvals list --session-id "$session_id" 2>/dev/null || printf '[]')"
        if jq -e --arg allowed "$allowed_tools" '
          if type=="array" then .
          elif has("approvals") then .approvals
          else []
          end
          | length > 0
          and all(.[]; ((.request.tool_name // "") | test($allowed)))
        ' <<<"$approvals_json" >/dev/null; then
          kheish approvals allow-all \
            --session-id "$session_id" \
            --justification "approved for OpenRouter image live smoke" \
            >/dev/null || true
        elif jq -e 'if type=="array" then length > 0 elif has("approvals") then (.approvals | length) > 0 else false end' <<<"$approvals_json" >/dev/null; then
          echo "unexpected approval requested during OpenRouter image smoke" >&2
          jq '.' <<<"$approvals_json" >&2
          exit 1
        fi
        ;;
      *)
        local approvals_json
        approvals_json="$(kheish approvals list --session-id "$session_id" 2>/dev/null || printf '[]')"
        if jq -e 'if type=="array" then length > 0 elif has("approvals") then (.approvals | length) > 0 else false end' <<<"$approvals_json" >/dev/null; then
          if jq -e --arg allowed "$allowed_tools" '
            if type=="array" then .
            elif has("approvals") then .approvals
            else []
            end
            | all(.[]; ((.request.tool_name // "") | test($allowed)))
          ' <<<"$approvals_json" >/dev/null; then
            kheish approvals allow-all \
              --session-id "$session_id" \
              --justification "approved for OpenRouter image live smoke" \
              >/dev/null || true
          else
            echo "unexpected approval requested during OpenRouter image smoke" >&2
            jq '.' <<<"$approvals_json" >&2
            exit 1
          fi
        fi
        ;;
    esac
    sleep 1
  done

  echo "run $run_id did not reach a terminal status within ${RUN_WAIT_SECONDS}s" >&2
  kheish runs get "$run_id" >&2 || true
  exit 1
}

assert_image_asset() {
  local asset_id="$1"
  local output_path="$2"
  local asset_json
  asset_json="$(kheish assets get "$asset_id")"
  curl -fsS "$BASE_URL/v1/assets/$asset_id/raw" -o "$output_path"
  python3 - "$output_path" "$asset_json" <<'PY'
import hashlib
import json
import pathlib
import struct
import sys

path = pathlib.Path(sys.argv[1])
asset = json.loads(sys.argv[2])
data = path.read_bytes()
if len(data) != asset["byte_length"]:
    raise SystemExit(f"byte length mismatch: {len(data)} != {asset['byte_length']}")
sha = hashlib.sha256(data).hexdigest()
if sha != asset["sha256"]:
    raise SystemExit(f"sha mismatch: {sha} != {asset['sha256']}")

media_type = asset["media_type"]
width = height = None
if data.startswith(b"\x89PNG\r\n\x1a\n"):
    if media_type != "image/png":
        raise SystemExit(f"PNG bytes had media type {media_type}")
    width, height = struct.unpack(">II", data[16:24])
elif data.startswith(b"\xff\xd8"):
    if media_type != "image/jpeg":
        raise SystemExit(f"JPEG bytes had media type {media_type}")
    index = 2
    while index + 9 < len(data):
        if data[index] != 0xFF:
            index += 1
            continue
        marker = data[index + 1]
        index += 2
        if marker in {0xD8, 0xD9}:
            continue
        if index + 2 > len(data):
            break
        segment_length = int.from_bytes(data[index:index + 2], "big")
        if segment_length < 2 or index + segment_length > len(data):
            break
        if marker in {0xC0, 0xC1, 0xC2, 0xC3, 0xC5, 0xC6, 0xC7, 0xC9, 0xCA, 0xCB, 0xCD, 0xCE, 0xCF}:
            height = int.from_bytes(data[index + 3:index + 5], "big")
            width = int.from_bytes(data[index + 5:index + 7], "big")
            break
        index += segment_length
else:
    raise SystemExit(f"unsupported image bytes for {media_type}")

if not width or not height:
    raise SystemExit("could not read image dimensions")
if not (1 <= width <= 4096 and 1 <= height <= 4096):
    raise SystemExit(f"unexpected image dimensions {width}x{height}")
PY
}

assert_openrouter_image_debug_is_metadata_only() {
  local request_json="$1"
  local response_json="$2"
  local expected_model="$3"
  local expected_inputs="$4"
  jq -e --arg model "$expected_model" --argjson inputs "$expected_inputs" '
    .provider=="openrouter"
    and .method=="POST"
    and (.url | endswith("/chat/completions"))
    and (.headers.authorization=="<redacted>" or .headers.Authorization=="<redacted>")
    and .body.model==$model
    and .body.modalities==["image","text"]
    and .body.stream==false
    and .body.image_config==null
    and (.body.prompt_chars > 0)
    and .body.image_inputs==$inputs
  ' <<<"$request_json" >/dev/null
  jq -e --arg model "$expected_model" '
    .provider=="openrouter"
    and (.status >= 200 and .status < 300)
    and (.body.model==$model or (.body.model | type)=="string")
    and (.body.choice_count >= 1)
    and (.body.image_count >= 1)
    and (.body.text_chars >= 0)
  ' <<<"$response_json" >/dev/null
  if jq -e 'tostring | contains("data:image") or contains("b64_json") or contains("sk-")' <<<"$request_json" >/dev/null; then
    echo "OpenRouter image request debug artifact leaked image data or key-looking text" >&2
    exit 1
  fi
  if jq -e 'tostring | contains("data:image") or contains("b64_json") or contains("sk-")' <<<"$response_json" >/dev/null; then
    echo "OpenRouter image response debug artifact leaked image data or key-looking text" >&2
    exit 1
  fi
}

mkdir -p "$STATE" "$WORKSPACE"
cat >"$ROUTES" <<TOML
version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "$PLANNER_MODEL"
api_key_env = "OPENAI_API_KEY"
image_generation = false
image_edit = false
audio_generation = false
transcription = false

[routes.openrouter]
driver = "openrouter"
default_model = "$IMAGE_MODEL"
model_support = "any"
api_key_env = "OPENROUTER_API_KEY"
image_generation = true
image_edit = true
TOML

PORT="$(free_port)"
BASE_URL="http://127.0.0.1:$PORT"
"$BIN" serve \
  --bind "127.0.0.1:$PORT" \
  --state-root "$STATE" \
  --workspace-root "$WORKSPACE" \
  --routes-file "$ROUTES" \
  --default-route openai \
  --mcp-discovery disabled \
  >"$TMP/daemon.log" 2>&1 &
DAEMON_PID="$!"
wait_http_ok "$BASE_URL/readyz"

kheish status >/dev/null
kheish runtime set-debug-level full >/dev/null
kheish runtime get \
  | jq -e '
      any(.routes[]?; .route_id=="openai" and .provider=="openai" and .capabilities.image_generation==false and .capabilities.image_edit==false)
      and any(.routes[]?; .route_id=="openrouter" and .provider=="openrouter" and .capabilities.image_generation==true and .capabilities.image_edit==true)
    ' >/dev/null

SESSION="openrouter-image-live"
kheish sessions create "$SESSION" >/dev/null
cat >"$TMP/prompt-generate.txt" <<PROMPT
Call generate_image with provider "openrouter", model "$IMAGE_MODEL", prompt exactly "A small black square centered on a plain white background, minimal test image.", and count 1. Do not pass a size field. If a tool call is rejected because the arguments are malformed, correct the arguments and retry. After generate_image succeeds, call emit_output with content exactly OPENROUTER_IMAGE_GENERATE_LIVE_OK, artifact_ids containing the returned image asset id, and include_artifacts_inline=true. Do not finish until emit_output has succeeded.
PROMPT

RUN_JSON="$(submit_and_wait_with_approvals "$SESSION" "$TMP/prompt-generate.txt" '^(generate_image|emit_output)$')"
RUN_ID="$(jq -r '.run_id' <<<"$RUN_JSON")"
jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENROUTER_IMAGE_GENERATE_LIVE_OK")) and ((.outputs[-1].parts // []) | any(.type=="attachment" and (.attachment.media_type|startswith("image/")))) and ((.outputs[-1].artifacts // []) | length > 0)' <<<"$RUN_JSON" >/dev/null

EVENTS_JSON="$(kheish sessions events "$SESSION")"
jq -e '[.. | objects | select(.tool_name?=="generate_image" and .is_error?==false and .output.provider?=="openrouter")] | length == 1' <<<"$EVENTS_JSON" >/dev/null
jq -e '[.. | objects | select(.tool_name?=="emit_output" and .is_error?==false)] | length >= 1' <<<"$EVENTS_JSON" >/dev/null
GENERATED_ASSET_ID="$(jq -r '[.. | objects | select(.tool_name?=="generate_image" and .output.assets?[0].id?) | .output.assets[0].id] | last // empty' <<<"$EVENTS_JSON")"
if [[ -z "$GENERATED_ASSET_ID" ]]; then
  echo "generate_image did not return an asset id" >&2
  exit 1
fi

GEN_REQ_JSON="$(kheish runs debug-artifact "$RUN_ID" openrouter-image-provider-request)"
GEN_RESP_JSON="$(kheish runs debug-artifact "$RUN_ID" openrouter-image-provider-response)"
assert_openrouter_image_debug_is_metadata_only "$GEN_REQ_JSON" "$GEN_RESP_JSON" "$IMAGE_MODEL" 0
assert_image_asset "$GENERATED_ASSET_ID" "$TMP/generated-image"

EDIT_SESSION="openrouter-image-edit-live"
kheish sessions create "$EDIT_SESSION" >/dev/null
cat >"$TMP/prompt-edit.txt" <<PROMPT
Call edit_image with provider "openrouter", model "$IMAGE_MODEL", image_asset_ids containing exactly "$GENERATED_ASSET_ID", prompt exactly "Keep the black square and add a thin red border around it.", and count 1. Do not pass a size field. Do not call generate_image in this run. If a tool call is rejected because the arguments are malformed, correct the arguments and retry. After edit_image succeeds, call emit_output with content exactly OPENROUTER_IMAGE_EDIT_LIVE_OK, artifact_ids containing the returned image asset id, and include_artifacts_inline=true. Do not finish until emit_output has succeeded.
PROMPT

EDIT_RUN_JSON="$(submit_and_wait_with_approvals "$EDIT_SESSION" "$TMP/prompt-edit.txt" '^(edit_image|emit_output)$')"
EDIT_RUN_ID="$(jq -r '.run_id' <<<"$EDIT_RUN_JSON")"
jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENROUTER_IMAGE_EDIT_LIVE_OK")) and ((.outputs[-1].parts // []) | any(.type=="attachment" and (.attachment.media_type|startswith("image/")))) and ((.outputs[-1].artifacts // []) | length > 0)' <<<"$EDIT_RUN_JSON" >/dev/null

EDIT_EVENTS_JSON="$(kheish sessions events "$EDIT_SESSION")"
jq -e '[.. | objects | select(.tool_name?=="generate_image" and .is_error?==false)] | length == 0' <<<"$EDIT_EVENTS_JSON" >/dev/null
jq -e '[.. | objects | select(.tool_name?=="edit_image" and .is_error?==false and .output.provider?=="openrouter")] | length == 1' <<<"$EDIT_EVENTS_JSON" >/dev/null
EDITED_ASSET_ID="$(jq -r '[.. | objects | select(.tool_name?=="edit_image" and .output.assets?[0].id?) | .output.assets[0].id] | last // empty' <<<"$EDIT_EVENTS_JSON")"
if [[ -z "$EDITED_ASSET_ID" ]]; then
  echo "edit_image did not return an asset id" >&2
  exit 1
fi
if [[ "$EDITED_ASSET_ID" == "$GENERATED_ASSET_ID" ]]; then
  echo "edit_image reused the source asset id" >&2
  exit 1
fi

EDIT_REQ_JSON="$(kheish runs debug-artifact "$EDIT_RUN_ID" openrouter-image-provider-request)"
EDIT_RESP_JSON="$(kheish runs debug-artifact "$EDIT_RUN_ID" openrouter-image-provider-response)"
assert_openrouter_image_debug_is_metadata_only "$EDIT_REQ_JSON" "$EDIT_RESP_JSON" "$IMAGE_MODEL" 1
assert_image_asset "$EDITED_ASSET_ID" "$TMP/edited-image"

printf 'openrouter image live smoke passed: %s %s %s %s\n' "$TMP" "$RUN_ID" "$GENERATED_ASSET_ID" "$EDITED_ASSET_ID"
