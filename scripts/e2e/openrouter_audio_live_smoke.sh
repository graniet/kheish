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

PLANNER_MODEL="${KHEISH_OPENROUTER_AUDIO_PLANNER_MODEL:-gpt-5.4}"
TTS_MODEL="${KHEISH_OPENROUTER_TTS_MODEL:-openai/gpt-4o-mini-tts-2025-12-15}"
STT_MODEL="${KHEISH_OPENROUTER_STT_MODEL:-openai/gpt-4o-mini-transcribe}"

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

if [[ -z "${KHEISH_BIN:-}" && "${KHEISH_OPENROUTER_AUDIO_SMOKE_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon
elif [[ ! -x "$BIN" ]]; then
  cargo build -p kheish-daemon
fi

WORK_BASE="${KHEISH_OPENROUTER_AUDIO_SMOKE_ROOT:-"$ROOT/.tmp"}"
mkdir -p "$WORK_BASE"
TMP="$(mktemp -d "$WORK_BASE/openrouter-audio-live.XXXXXX")"
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

mkdir -p "$STATE" "$WORKSPACE"
cat >"$ROUTES" <<TOML
version = 1
default_route = "openai"

[routes.openai]
driver = "openai"
default_model = "$PLANNER_MODEL"
api_key_env = "OPENAI_API_KEY"
transcription = false

[routes.openrouter]
driver = "openrouter"
default_model = "openai/gpt-5.4-mini"
model_support = "any"
api_key_env = "OPENROUTER_API_KEY"
audio_generation = true
transcription = true
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
      any(.routes[]?; .route_id=="openai" and .provider=="openai" and .capabilities.transcription==false)
      and any(.routes[]?; .route_id=="openrouter" and .provider=="openrouter" and .capabilities.audio_generation==true and .capabilities.transcription==true)
    ' >/dev/null

SESSION="openrouter-audio-live"
kheish sessions create "$SESSION" >/dev/null
cat >"$TMP/prompt-ok.txt" <<PROMPT
You must call generate_audio exactly once with route.provider exactly openrouter, model "$TTS_MODEL", input exactly "Kheish OpenRouter audio OK.", voice "alloy", and format "mp3". After generate_audio succeeds, call emit_output with content exactly OPENROUTER_AUDIO_LIVE_OK, artifact_ids containing the returned audio asset id, and include_artifacts_inline=true. Do not finish until emit_output has succeeded.
PROMPT

RUN_JSON="$(kheish sessions input "$SESSION" --provider openai --model "$PLANNER_MODEL" --wait --poll-interval-ms 1000 --content-file "$TMP/prompt-ok.txt")"
RUN_ID="$(jq -r '.run_id' <<<"$RUN_JSON")"
jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENROUTER_AUDIO_LIVE_OK")) and ((.outputs[-1].parts // []) | any(.type=="attachment" and (.attachment.media_type|startswith("audio/")))) and ((.outputs[-1].artifacts // []) | length > 0)' <<<"$RUN_JSON" >/dev/null

EVENTS_JSON="$(kheish sessions events "$SESSION")"
jq -e '[.. | objects | select(.tool_name?=="generate_audio" and .is_error?==false and .output.provider?=="openrouter")] | length == 1' <<<"$EVENTS_JSON" >/dev/null
jq -e '[.. | objects | select(.tool_name?=="emit_output" and .is_error?==false)] | length >= 1' <<<"$EVENTS_JSON" >/dev/null
ASSET_ID="$(jq -r '[.. | objects | select(.tool_name?=="generate_audio" and .output.assets?[0].id?) | .output.assets[0].id] | last // empty' <<<"$EVENTS_JSON")"
if [[ -z "$ASSET_ID" ]]; then
  echo "generate_audio did not return an asset id" >&2
  exit 1
fi

REQ_JSON="$(kheish runs debug-artifact "$RUN_ID" openrouter-audio-speech-provider-request)"
RESP_JSON="$(kheish runs debug-artifact "$RUN_ID" openrouter-audio-speech-provider-response)"
ASSET_JSON="$(kheish assets get "$ASSET_ID")"

jq -e --arg model "$TTS_MODEL" '.provider=="openrouter" and .body.model==$model and .body.voice=="alloy" and .body.response_format=="mp3" and (.body.input_chars > 0) and (.body.input? == null)' <<<"$REQ_JSON" >/dev/null
jq -e --arg sha "$(jq -r '.body.sha256' <<<"$RESP_JSON")" --argjson len "$(jq -r '.body.byte_len' <<<"$RESP_JSON")" '.sha256==$sha and .byte_length==$len and (.media_type|startswith("audio/"))' <<<"$ASSET_JSON" >/dev/null

RAW_AUDIO="$TMP/$ASSET_ID.mp3"
curl -fsS "$BASE_URL/v1/assets/$ASSET_ID/raw" -o "$RAW_AUDIO"
RAW_SHA="$(python3 - "$RAW_AUDIO" <<'PY'
import hashlib
import sys

with open(sys.argv[1], "rb") as handle:
    print(hashlib.sha256(handle.read()).hexdigest())
PY
)"
RAW_BYTES="$(python3 - "$RAW_AUDIO" <<'PY'
import os
import sys

print(os.path.getsize(sys.argv[1]))
PY
)"
jq -e --arg sha "$RAW_SHA" --argjson len "$RAW_BYTES" '.sha256==$sha and .byte_length==$len' <<<"$ASSET_JSON" >/dev/null

if command -v ffprobe >/dev/null 2>&1; then
  FFPROBE_JSON="$(ffprobe -v error -show_entries stream=codec_type,codec_name,sample_rate,channels -show_entries format=duration -of json "$RAW_AUDIO")"
  jq -e '
    ([.streams[]? | select(.codec_type=="audio")] | length) >= 1
    and ([.streams[]? | select(.codec_type=="audio")][0].sample_rate | tonumber) >= 8000
    and ([.streams[]? | select(.codec_type=="audio")][0].sample_rate | tonumber) <= 192000
    and ([.streams[]? | select(.codec_type=="audio")][0].channels | tonumber) >= 1
    and ([.streams[]? | select(.codec_type=="audio")][0].channels | tonumber) <= 8
    and (.format.duration | tonumber) > 0
    and (.format.duration | tonumber) <= 1800
  ' <<<"$FFPROBE_JSON" >/dev/null
fi

assert_transcript_contains_openrouter_audio() {
  local transcript_path="$1"
  python3 - "$transcript_path" <<'PY'
import pathlib
import re
import sys

text = pathlib.Path(sys.argv[1]).read_text().lower()
normalized = re.sub(r"[^a-z0-9]+", "", text)
if "openrouteraudio" not in normalized:
    raise SystemExit(f"transcript did not contain OpenRouter audio marker: {text!r}")
PY
}

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

assert_openrouter_derivation() {
  local derivation_json="$1"
  local asset_id="$2"
  jq -e --arg asset "$asset_id" --arg model "$STT_MODEL" '
    .status=="completed"
    and .subject.type=="asset"
    and .subject.asset_id==$asset
    and .backend.kind=="transcription"
    and .backend.route_id=="openrouter"
    and .backend.provider=="openrouter"
    and .backend.model==$model
    and .backend.pipeline_version==1
    and .backend.stitching_strategy=="single_part"
    and .backend.part_count==1
  ' <<<"$derivation_json" >/dev/null
}

TRANS_SESSION="openrouter-transcription-live"
kheish sessions create "$TRANS_SESSION" >/dev/null
cat >"$TMP/prompt-transcribe.txt" <<PROMPT
Read the text rendered from the attached audio asset. Do not inspect files. If the rendered attachment text mentions OpenRouter audio, reply exactly OPENROUTER_TRANSCRIPTION_LIVE_OK. Otherwise reply exactly OPENROUTER_TRANSCRIPTION_LIVE_MISSING.
PROMPT

TRANS_RUN_JSON="$(kheish sessions input "$TRANS_SESSION" --provider openai --model "$PLANNER_MODEL" --tool-choice none --max-output-tokens 64 --wait --poll-interval-ms 1000 --asset "$ASSET_ID" --content-file "$TMP/prompt-transcribe.txt")"
TRANS_RUN_ID="$(jq -r '.run_id' <<<"$TRANS_RUN_JSON")"
jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENROUTER_TRANSCRIPTION_LIVE_OK"))' <<<"$TRANS_RUN_JSON" >/dev/null

TRANS_REQ_JSON="$(kheish runs debug-artifact "$TRANS_RUN_ID" openrouter-audio-transcription-provider-request)"
TRANS_RESP_JSON="$(kheish runs debug-artifact "$TRANS_RUN_ID" openrouter-audio-transcription-provider-response)"
jq -e --arg model "$STT_MODEL" '
  .provider=="openrouter"
  and .body.model==$model
  and .body.media_type=="audio/mpeg"
  and .body.format=="mp3"
  and (.body.byte_len > 0)
  and (.body.sha256 | length == 64)
  and (.body.input_audio? == null)
  and (.body.prompt? == null)
' <<<"$TRANS_REQ_JSON" >/dev/null
jq -e '.provider=="openrouter" and (.body.text_chars > 0)' <<<"$TRANS_RESP_JSON" >/dev/null
if jq -e 'tostring | contains("Kheish OpenRouter audio OK")' <<<"$TRANS_REQ_JSON" >/dev/null; then
  echo "OpenRouter STT request debug artifact leaked transcript text" >&2
  exit 1
fi
if jq -e 'tostring | contains("Kheish OpenRouter audio OK")' <<<"$TRANS_RESP_JSON" >/dev/null; then
  echo "OpenRouter STT response debug artifact leaked transcript text" >&2
  exit 1
fi

DERIVATIONS_JSON="$(kheish derivations list --query "$ASSET_ID")"
DERIVATION_JSON="$(jq -c --arg asset "$ASSET_ID" '[.[] | select(.subject.type=="asset" and .subject.asset_id==$asset and .backend.provider=="openrouter")] | first // empty' <<<"$DERIVATIONS_JSON")"
if [[ -z "$DERIVATION_JSON" ]]; then
  echo "OpenRouter STT session did not create an asset derivation" >&2
  exit 1
fi
DERIVATION_ID="$(jq -r '.derivation_id' <<<"$DERIVATION_JSON")"
TEXT_ASSET_ID="$(jq -r '.result_asset_id' <<<"$DERIVATION_JSON")"
assert_openrouter_derivation "$DERIVATION_JSON" "$ASSET_ID"
TRANSCRIPT="$TMP/openrouter-transcript.txt"
curl -fsS "$BASE_URL/v1/assets/$TEXT_ASSET_ID/raw" -o "$TRANSCRIPT"
assert_transcript_contains_openrouter_audio "$TRANSCRIPT"

DUPLICATE_DERIVATION_JSON="$(kheish derivations create --profile canonical-text --asset-id "$ASSET_ID")"
jq -e --arg id "$DERIVATION_ID" '.derivation_id==$id and .cache_status=="hit"' <<<"$DUPLICATE_DERIVATION_JSON" >/dev/null

set +e
BAD_TIMESTAMP_OUTPUT="$(kheish derivations create --profile canonical-text --asset-id "$ASSET_ID" --transcription-timestamp-granularity word 2>&1)"
BAD_TIMESTAMP_STATUS=$?
set -e
if [[ "$BAD_TIMESTAMP_STATUS" -eq 0 ]]; then
  echo "OpenRouter timestamp granularity request unexpectedly succeeded" >&2
  exit 1
fi
if [[ "$BAD_TIMESTAMP_OUTPUT" != *"whisper-1"* && "$BAD_TIMESTAMP_OUTPUT" != *"timestamp"* ]]; then
  echo "OpenRouter timestamp granularity rejection was not explicit: $BAD_TIMESTAMP_OUTPUT" >&2
  exit 1
fi

check_transcoded_variant() {
  local label="$1"
  local path="$2"
  local media_type="$3"
  assert_audio_probe "$path"
  local imported_json
  imported_json="$(kheish assets import "$path" --media-type "$media_type")"
  local variant_asset_id
  variant_asset_id="$(jq -r '.asset_id' <<<"$imported_json")"
  local derivation_json
  derivation_json="$(kheish derivations create --profile canonical-text --asset-id "$variant_asset_id")"
  assert_openrouter_derivation "$derivation_json" "$variant_asset_id"
  local text_asset_id
  text_asset_id="$(jq -r '.result_asset_id' <<<"$derivation_json")"
  local transcript="$TMP/${label}.transcript.txt"
  curl -fsS "$BASE_URL/v1/assets/$text_asset_id/raw" -o "$transcript"
  assert_transcript_contains_openrouter_audio "$transcript"
}

if command -v ffmpeg >/dev/null 2>&1 && command -v ffprobe >/dev/null 2>&1; then
  WAV_AUDIO="$TMP/openrouter-transcoded.wav"
  M4A_AUDIO="$TMP/openrouter-transcoded.m4a"
  WEBM_AUDIO="$TMP/openrouter-transcoded.webm"
  ffmpeg -y -v error -i "$RAW_AUDIO" -ar 16000 -ac 1 "$WAV_AUDIO"
  ffmpeg -y -v error -i "$RAW_AUDIO" -vn -c:a aac -b:a 64k "$M4A_AUDIO"
  ffmpeg -y -v error -i "$RAW_AUDIO" -vn -c:a libopus -b:a 32k "$WEBM_AUDIO"
  check_transcoded_variant "wav" "$WAV_AUDIO" "audio/wav"
  check_transcoded_variant "m4a" "$M4A_AUDIO" "audio/m4a"
  check_transcoded_variant "webm" "$WEBM_AUDIO" "audio/webm;codecs=opus"
fi

BAD_SESSION="openrouter-audio-invalid-voice"
kheish sessions create "$BAD_SESSION" >/dev/null
cat >"$TMP/prompt-bad.txt" <<PROMPT
You must call generate_audio exactly once with route.provider exactly openrouter, model "$TTS_MODEL", input exactly "This invalid voice must fail locally.", voice definitely_not_a_voice, and format mp3. The tool is expected to fail. Then reply exactly OPENROUTER_INVALID_VOICE_REJECTED. Do not call emit_output.
PROMPT

BAD_RUN_JSON="$(kheish sessions input "$BAD_SESSION" --provider openai --model "$PLANNER_MODEL" --wait --poll-interval-ms 1000 --content-file "$TMP/prompt-bad.txt")"
BAD_RUN_ID="$(jq -r '.run_id' <<<"$BAD_RUN_JSON")"
BAD_EVENTS_JSON="$(kheish sessions events "$BAD_SESSION")"
BAD_DEBUG_JSON="$(kheish runs debug "$BAD_RUN_ID")"

jq -e '.status=="completed" and ((.outputs[-1].content // "") | contains("OPENROUTER_INVALID_VOICE_REJECTED"))' <<<"$BAD_RUN_JSON" >/dev/null
jq -e '[.. | objects | select(.tool_name?=="generate_audio" and .is_error==true and ((.output.error? // "") | contains("unsupported OpenRouter speech voice")))] | length == 1' <<<"$BAD_EVENTS_JSON" >/dev/null
jq -e '([.artifacts[]?.artifact_id] | index("openrouter-audio-speech-provider-request") | not) and ([.artifacts[]?.artifact_id] | index("openrouter-audio-speech-provider-response") | not)' <<<"$BAD_DEBUG_JSON" >/dev/null

printf 'openrouter audio live smoke passed: %s %s %s %s\n' "$TMP" "$RUN_ID" "$ASSET_ID" "$DERIVATION_ID"
