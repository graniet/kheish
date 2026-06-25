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

TEXT_MODEL="${KHEISH_OPENAI_TRANSCRIPTION_SMOKE_MODEL:-gpt-5.4}"
STT_MODEL="${KHEISH_OPENAI_STT_MODEL:-gpt-4o-transcribe}"
TTS_MODEL="${KHEISH_OPENAI_TTS_MODEL:-gpt-4o-mini-tts}"
SPOKEN_TEXT="${KHEISH_OPENAI_TRANSCRIPTION_SMOKE_TEXT:-OpenAI transcription smoke ready.}"

if [[ -z "${OPENAI_API_KEY:-}" && -n "${KHEISH_OPENAI_API_KEY:-}" ]]; then
  OPENAI_API_KEY="$KHEISH_OPENAI_API_KEY"
  export OPENAI_API_KEY
fi

if [[ -z "${OPENAI_API_KEY:-}" ]]; then
  echo "OPENAI_API_KEY is required for this live smoke." >&2
  exit 2
fi

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPENAI_TRANSCRIPTION_SMOKE_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPENAI_TRANSCRIPTION_SMOKE_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/openai-transcription-live.XXXXXX")"
STATE="$TMP/state"
WORKSPACE="$TMP/workspace"
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

set_control_plane_curl_args() {
  local token="${KHEISH_DAEMON_TOKEN:-}"
  if [[ -z "$token" && -n "${KHEISH_DAEMON_TOKEN_FILE:-}" ]]; then
    token="$(tr -d '\r\n' <"$KHEISH_DAEMON_TOKEN_FILE")"
  fi
  if [[ -z "$token" && -n "${KHEISH_DAEMON_ADMIN_TOKEN:-}" ]]; then
    token="$KHEISH_DAEMON_ADMIN_TOKEN"
  fi
  if [[ -z "$token" && -n "${KHEISH_DAEMON_ADMIN_TOKEN_FILE:-}" ]]; then
    token="$(tr -d '\r\n' <"$KHEISH_DAEMON_ADMIN_TOKEN_FILE")"
  fi
  CURL_AUTH_ARGS=()
  if [[ -n "$token" ]]; then
    CURL_AUTH_ARGS=(-H "Authorization: Bearer $token")
  fi
}

kheish() {
  "$BIN" --base-url "$BASE_URL" --output json "$@"
}

mkdir -p "$STATE" "$WORKSPACE"

PORT="$(free_port)"
BASE_URL="http://127.0.0.1:$PORT"
"$BIN" serve \
  --bind "127.0.0.1:$PORT" \
  --state-root "$STATE" \
  --workspace-root "$WORKSPACE" \
  --provider openai \
  --model "$TEXT_MODEL" \
  --transcription-provider openai \
  --transcription-model "$STT_MODEL" \
  --mcp-discovery disabled \
  >"$TMP/daemon.log" 2>&1 &
DAEMON_PID="$!"
wait_http_ok "$BASE_URL/readyz"

kheish status >/dev/null
kheish runtime set-debug-level full >/dev/null
kheish runtime get \
  | jq -e --arg model "$TEXT_MODEL" '
      .provider=="openai"
      and .model==$model
      and any(.routes[]?; .provider=="openai" and .capabilities.audio_generation==true and .capabilities.transcription==true)
    ' >/dev/null

assert_audio_probe() {
  local path="$1"
  if ! command -v ffprobe >/dev/null 2>&1; then
    return 0
  fi
  local probe_json
  probe_json="$(ffprobe -v error -show_entries stream=codec_type,codec_name,sample_rate,channels -show_entries format=duration -of json "$path")"
  jq -e '
    ([.streams[]? | select(.codec_type=="audio")] | length) >= 1
    and ([.streams[]? | select(.codec_type=="audio")][0].sample_rate | tonumber) >= 8000
    and ([.streams[]? | select(.codec_type=="audio")][0].sample_rate | tonumber) <= 192000
    and ([.streams[]? | select(.codec_type=="audio")][0].channels | tonumber) >= 1
    and ([.streams[]? | select(.codec_type=="audio")][0].channels | tonumber) <= 8
    and (.format.duration | tonumber) > 0
    and (.format.duration | tonumber) <= 1800
  ' <<<"$probe_json" >/dev/null
}

AUDIO_SESSION="openai-audio-live"
kheish sessions create "$AUDIO_SESSION" >/dev/null
cat >"$TMP/prompt-audio.txt" <<PROMPT
You must call generate_audio exactly once with route.provider exactly openai, model "$TTS_MODEL", input exactly "$SPOKEN_TEXT", voice "alloy", and format "mp3". After generate_audio succeeds, call emit_output with content exactly OPENAI_AUDIO_LIVE_OK, artifact_ids containing the returned audio asset id, and include_artifacts_inline=true. Do not finish until emit_output has succeeded.
PROMPT

AUDIO_RUN_JSON="$(kheish sessions input "$AUDIO_SESSION" --provider openai --model "$TEXT_MODEL" --wait --poll-interval-ms 1000 --content-file "$TMP/prompt-audio.txt")"
AUDIO_RUN_ID="$(jq -r '.run_id' <<<"$AUDIO_RUN_JSON")"
jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENAI_AUDIO_LIVE_OK")) and ((.outputs[-1].parts // []) | any(.type=="attachment" and (.attachment.media_type|startswith("audio/")))) and ((.outputs[-1].artifacts // []) | length > 0)' <<<"$AUDIO_RUN_JSON" >/dev/null

AUDIO_EVENTS_JSON="$(kheish sessions events "$AUDIO_SESSION")"
jq -e '[.. | objects | select(.tool_name?=="generate_audio" and .is_error?==false and .output.provider?=="openai")] | length == 1' <<<"$AUDIO_EVENTS_JSON" >/dev/null
jq -e '[.. | objects | select(.tool_name?=="emit_output" and .is_error?==false)] | length >= 1' <<<"$AUDIO_EVENTS_JSON" >/dev/null
ASSET_ID="$(jq -r '[.. | objects | select(.tool_name?=="generate_audio" and .output.assets?[0].id?) | .output.assets[0].id] | last // empty' <<<"$AUDIO_EVENTS_JSON")"
if [[ -z "$ASSET_ID" ]]; then
  echo "generate_audio did not return an asset id" >&2
  exit 1
fi

AUDIO_REQ_JSON="$(kheish runs debug-artifact "$AUDIO_RUN_ID" openai-audio-speech-provider-request)"
AUDIO_RESP_JSON="$(kheish runs debug-artifact "$AUDIO_RUN_ID" openai-audio-speech-provider-response)"
ASSET_JSON="$(kheish assets get "$ASSET_ID")"
jq -e --arg model "$TTS_MODEL" '.provider=="openai" and .body.model==$model and .body.voice=="alloy" and .body.response_format=="mp3" and (.body.input_chars > 0) and (.body.input? == null)' <<<"$AUDIO_REQ_JSON" >/dev/null
jq -e --arg sha "$(jq -r '.body.sha256' <<<"$AUDIO_RESP_JSON")" --argjson len "$(jq -r '.body.byte_len' <<<"$AUDIO_RESP_JSON")" '.sha256==$sha and .byte_length==$len and .media_type=="audio/mpeg"' <<<"$ASSET_JSON" >/dev/null
if jq -e --arg text "$SPOKEN_TEXT" 'tostring | contains($text)' <<<"$AUDIO_REQ_JSON" >/dev/null; then
  echo "OpenAI TTS request debug artifact leaked raw input text" >&2
  exit 1
fi

SOURCE_AUDIO="$TMP/source.mp3"
set_control_plane_curl_args
curl -fsS "${CURL_AUTH_ARGS[@]}" "$BASE_URL/v1/assets/$ASSET_ID/raw" -o "$SOURCE_AUDIO"
assert_audio_probe "$SOURCE_AUDIO"

DERIVATION_JSON="$(kheish derivations create --profile canonical-text --asset-id "$ASSET_ID")"
DERIVATION_ID="$(jq -r '.derivation_id' <<<"$DERIVATION_JSON")"
TEXT_ASSET_ID="$(jq -r '.result_asset_id' <<<"$DERIVATION_JSON")"
jq -e --arg model "$STT_MODEL" '
  .status=="completed"
  and .cache_status=="miss"
  and .backend.kind=="transcription"
  and .backend.route_id=="openai"
  and .backend.provider=="openai"
  and .backend.model==$model
  and .backend.pipeline_version==1
  and .backend.stitching_strategy=="single_part"
  and .backend.part_count==1
' <<<"$DERIVATION_JSON" >/dev/null

TRANSCRIPT="$TMP/transcript.txt"
curl -fsS "${CURL_AUTH_ARGS[@]}" "$BASE_URL/v1/assets/$TEXT_ASSET_ID/raw" -o "$TRANSCRIPT"
python3 - "$TRANSCRIPT" <<'PY'
import pathlib
import sys

text = pathlib.Path(sys.argv[1]).read_text().lower()
required = ["transcription", "smoke", "ready"]
missing = [word for word in required if word not in text]
if missing:
    raise SystemExit(f"transcript missing {missing}: {text!r}")
PY

DUPLICATE_JSON="$(kheish derivations create --profile canonical-text --asset-id "$ASSET_ID")"
jq -e --arg id "$DERIVATION_ID" '.derivation_id==$id and .cache_status=="hit"' <<<"$DUPLICATE_JSON" >/dev/null

FORCED_JSON="$(kheish derivations create --profile canonical-text --asset-id "$ASSET_ID" --force-refresh)"
FORCED_ID="$(jq -r '.derivation_id' <<<"$FORCED_JSON")"
if [[ "$FORCED_ID" == "$DERIVATION_ID" ]]; then
  echo "force-refresh returned the original derivation id" >&2
  exit 1
fi
jq -e '
  .status=="completed"
  and .cache_status=="miss"
  and .backend.pipeline_version==1
  and .backend.stitching_strategy=="single_part"
  and .backend.part_count==1
' <<<"$FORCED_JSON" >/dev/null

SESSION="openai-transcription-live"
kheish sessions create "$SESSION" >/dev/null
RUN_JSON="$(kheish sessions input "$SESSION" \
  --provider openai \
  --model "$TEXT_MODEL" \
  --tool-choice none \
  --max-output-tokens 64 \
  --wait \
  --poll-interval-ms 1000 \
  --asset "$ASSET_ID" \
  "Read the text rendered from the attached audio asset. Do not inspect files. If the rendered attachment text mentions transcription smoke ready, reply exactly OPENAI_TRANSCRIPTION_LIVE_OK. Otherwise reply exactly OPENAI_TRANSCRIPTION_LIVE_MISSING.")"
jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENAI_TRANSCRIPTION_LIVE_OK"))' <<<"$RUN_JSON" >/dev/null

POST_FORCE_JSON="$(kheish derivations create --profile canonical-text --asset-id "$ASSET_ID")"
jq -e --arg id "$FORCED_ID" '.derivation_id==$id and .cache_status=="hit"' <<<"$POST_FORCE_JSON" >/dev/null

printf 'openai transcription live smoke passed: %s %s %s\n' "$TMP" "$ASSET_ID" "$FORCED_ID"
