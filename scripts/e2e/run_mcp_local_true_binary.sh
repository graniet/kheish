#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
RUN_ID="mcp-local-true-binary-$(date +%Y%m%d-%H%M%S)-$$"
EVIDENCE="$ROOT/tmp/e2e/$RUN_ID"
mkdir -p "$EVIDENCE"

cd "$ROOT"

if [[ "${KHEISH_E2E_SKIP_BUILD:-0}" != "1" ]]; then
  cargo build -p kheish-daemon 2>&1 | tee "$EVIDENCE/build.log"
fi

SCENARIOS=(
  "mcp_catalog_true_binary.sh"
  "mcp_secret_store_true_binary.sh"
  "mcp_oauth_protocol_true_binary.sh"
)

RESULTS_JSONL="$EVIDENCE/results.jsonl"
: > "$RESULTS_JSONL"

for scenario in "${SCENARIOS[@]}"; do
  log="$EVIDENCE/${scenario%.sh}.log"
  set +e
  "scripts/e2e/$scenario" 2>&1 | tee "$log"
  status=${PIPESTATUS[0]}
  set -e
  python3 - "$scenario" "$status" "$log" >> "$RESULTS_JSONL" <<'PY'
import json
import sys

scenario, status, log = sys.argv[1], int(sys.argv[2]), sys.argv[3]
print(json.dumps({
    "scenario": scenario,
    "status": "passed" if status == 0 else "failed",
    "exit_code": status,
    "log": log,
}))
PY
  if [[ "$status" -ne 0 ]]; then
    break
  fi
done

python3 - "$EVIDENCE" "$RESULTS_JSONL" <<'PY' | tee "$EVIDENCE/verdict.json"
import json
import pathlib
import sys

evidence = pathlib.Path(sys.argv[1])
results_path = pathlib.Path(sys.argv[2])
results = [
    json.loads(line)
    for line in results_path.read_text(encoding="utf-8").splitlines()
    if line.strip()
]
failed = [result for result in results if result["status"] != "passed"]
verdict = {
    "scenario": "mcp_local_true_binary_suite",
    "status": "failed" if failed else "passed",
    "results": results,
    "evidence_root": str(evidence),
}
print(json.dumps(verdict, indent=2))
sys.exit(1 if failed else 0)
PY

echo "evidence: $EVIDENCE"
