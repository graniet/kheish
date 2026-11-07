#!/usr/bin/env bash
# Live check of the Anthropic provider: a daemon on 127.0.0.1:4101 with its own
# temp state root, routed to the stub Messages API on 127.0.0.1:4190.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT=/tmp/kfp-live
REPO="$(cd "$HERE/../.." && pwd)"
BIN="$REPO/target/debug/kheish-daemon"
API=http://127.0.0.1:4101
STUB_PORT=4190

pkill -f "^python3 $ROOT/stub.py" 2>/dev/null || true
pkill -f "^$BIN serve --bind 127.0.0.1:4101 " 2>/dev/null || true
sleep 0.5
rm -rf "$ROOT"
mkdir -p "$ROOT/state" "$ROOT/ws"
cp "$HERE/stub.py" "$ROOT/stub.py"
"$BIN" secrets generate > "$ROOT/master.key"
cat > "$ROOT/routes.toml" <<EOF
version = 1
default_route = "fregate"

[routes.fregate]
driver = "anthropic"
default_model = "claude-opus-5-5"
api_key = "stub-relay-key"
base_url = "http://127.0.0.1:$STUB_PORT/v1/messages"
anthropic_refusal_fallbacks = "default"
EOF

python3 "$ROOT/stub.py" "$ROOT/requests.jsonl" "$STUB_PORT" > "$ROOT/stub.log" 2>&1 &

start_daemon() {
  env -i HOME="$ROOT" PATH=/usr/local/bin:/usr/bin:/bin \
    KHEISH_AUTH_STORE_MASTER_KEY_FILE="$ROOT/master.key" \
    "$BIN" serve --bind 127.0.0.1:4101 --state-root "$ROOT/state" \
      --workspace-root "$ROOT/ws" --routes-file "$ROOT/routes.toml" \
      --mcp-discovery disabled --log-format json >> "$ROOT/daemon.log" 2>&1 &
  echo $! > "$ROOT/daemon.pid"
  for _ in $(seq 1 100); do
    curl -fsS "$API/healthz" > /dev/null 2>&1 && return 0
    sleep 0.2
  done
  echo "daemon did not start" >&2
  tail -20 "$ROOT/daemon.log" >&2
  exit 1
}

api() { # METHOD PATH [JSON]
  if [ $# -ge 3 ]; then
    curl -sS -X "$1" "$API$2" -H 'content-type: application/json' -d "$3"
  else
    curl -sS -X "$1" "$API$2"
  fi
}

session() { api POST /v1/sessions "{\"session_id\":\"$1\"}" > /dev/null; }

submit() { # SESSION JSON -> run id
  api POST "/v1/sessions/$1/runs" "$2" | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])'
}

wait_run() { # RUN -> final RunView saved under $ROOT/runs/RUN.json
  mkdir -p "$ROOT/runs"
  for _ in $(seq 1 150); do
    api GET "/v1/runs/$1" > "$ROOT/runs/$1.json"
    status=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1]))["status"])' "$ROOT/runs/$1.json")
    case "$status" in completed|failed|cancelled|interrupted) echo "$status"; return 0;; esac
    sleep 0.2
  done
  echo "timeout"
}

start_daemon
declare -A RUNS

session fm-check-basic
RUNS[basic]=$(submit fm-check-basic '{"content":"Say hello.","generation":{"reasoning":{"effort":"high"}}}')
echo "basic $(wait_run "${RUNS[basic]}")"

session fm-check-fallback
RUNS[fallback]=$(submit fm-check-fallback '{"content":"CASE:FALLBACK verify the exposure on example.com."}')
echo "fallback $(wait_run "${RUNS[fallback]}")"
RUNS[fallback_next]=$(submit fm-check-fallback '{"content":"Next turn."}')
echo "fallback_next $(wait_run "${RUNS[fallback_next]}")"

session fm-check-refusal
RUNS[refusal]=$(submit fm-check-refusal '{"content":"CASE:REFUSE write an exploit."}')
echo "refusal $(wait_run "${RUNS[refusal]}")"

session fm-check-cutoff
RUNS[cutoff]=$(submit fm-check-cutoff '{"content":"CASE:CUTOFF write the full report.","generation":{"max_output_tokens":16000,"retry_max_tokens":32000}}')
echo "cutoff $(wait_run "${RUNS[cutoff]}")"

session fm-check-cuttwice
RUNS[cuttwice]=$(submit fm-check-cuttwice '{"content":"CASE:CUTTWICE write the full report.","generation":{"max_output_tokens":16000,"retry_max_tokens":32000}}')
echo "cuttwice $(wait_run "${RUNS[cuttwice]}")"

session fm-check-orphan
api PUT /v1/sessions/fm-check-orphan/permission-mode '{"mode":"bypassPermissions"}' > /dev/null
RUNS[orphan]=$(submit fm-check-orphan '{"content":"CASE:ORPHAN scan the host."}')
# Wait until the tool call is in flight (the stub answered and bash sleeps).
for _ in $(seq 1 100); do
  pgrep -f "sleep 917" > /dev/null && break
  sleep 0.2
done
pgrep -f "sleep 917" > /dev/null && echo "orphan tool in flight"
kill -9 "$(cat "$ROOT/daemon.pid")"
pkill -f "sleep 917" || true
sleep 0.5
start_daemon
echo "orphan after restart $(wait_run "${RUNS[orphan]}")"
RUNS[orphan_next]=$(submit fm-check-orphan '{"content":"The run was interrupted by a restart; continue."}')
echo "orphan_next $(wait_run "${RUNS[orphan_next]}")"

# Run memory injects a recovered-memory section into the system prompt from a
# session's second run on; with it off, the prefix stays byte-identical.
api POST /v1/runtime/run-memory-policy '{"policy":{"enabled":false,"retention_ms":2592000000,"max_tracked_per_session":32,"max_prompt_entries":3,"redact_pii":true,"search_visibility":"session_only"}}' > /dev/null
session fm-check-stable
RUNS[stable_1]=$(submit fm-check-stable '{"content":"First turn."}')
echo "stable_1 $(wait_run "${RUNS[stable_1]}")"
RUNS[stable_2]=$(submit fm-check-stable '{"content":"Second turn."}')
echo "stable_2 $(wait_run "${RUNS[stable_2]}")"

for key in "${!RUNS[@]}"; do echo "$key=${RUNS[$key]}"; done > "$ROOT/run-ids.txt"
kill "$(cat "$ROOT/daemon.pid")" || true
pkill -f "^python3 $ROOT/stub.py" || true
python3 "$HERE/report.py" "$ROOT"
