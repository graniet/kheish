# Fregate provider check: the Anthropic driver against a stub Messages API

Working note, 2026-11-07. Branch `feat/fregate-runtime-provider`, `cargo build -p kheish-daemon`
(debug). It proves K2 (request headers), K11 (the Anthropic provider on the current API) and K12
(the output-limit retry) from ADR 0023 on a live daemon. No real API key was used: the stub answers
with the shapes the Claude API documentation gives, and the proof against the real API happens later
through Fregate's relay with a workspace's own key.

## Rig

- **Daemon:** `kheish-daemon serve --bind 127.0.0.1:4101 --state-root /tmp/kfp-live/state
  --workspace-root /tmp/kfp-live/ws --routes-file /tmp/kfp-live/routes.toml --mcp-discovery disabled`,
  started with a clean environment (`env -i`, only `HOME`, `PATH` and
  `KHEISH_AUTH_STORE_MASTER_KEY_FILE`). The routes file:

  ```toml
  version = 1
  default_route = "fregate"

  [routes.fregate]
  driver = "anthropic"
  default_model = "claude-opus-5-5"
  api_key = "stub-relay-key"
  base_url = "http://127.0.0.1:4190/v1/messages"
  anthropic_refusal_fallbacks = "default"
  ```

- **Stub Messages API** on `127.0.0.1:4190` (`scripts/fregate-provider-check/stub.py`, Python standard library). It
  appends every request, headers and body, to `requests.jsonl` and answers server-sent events built
  from the documented shapes (`message_start`, `content_block_*`, `message_delta`, `message_stop`;
  thinking blocks with empty text and a `signature_delta`, as under the default `display: "omitted"`;
  the `fallback` block and `usage.iterations` of the refusals and fallback page; `stop_reason:
  "refusal"` with `stop_details`). A marker in the last user text picks the case: `CASE:FALLBACK` (a
  mid-stream fallback from `claude-opus-5-5` to `claude-opus-5`), `CASE:REFUSE` (the whole chain
  declines, category `cyber`), `CASE:CUTOFF` (`max_tokens` below 32,000 is cut off, 32,000 answers),
  `CASE:CUTTWICE` (always cut off), `CASE:ORPHAN` (a thinking block and a `bash` call that sleeps).
- **Driver:** `scripts/fregate-provider-check/run.sh` creates one session per case through the HTTP API, submits the runs, kills
  the daemon with `kill -9` while the orphan case's tool call is in flight, restarts it on the same
  state root, and `report.py` checks the captured requests and the run views. To reproduce:
  `cargo build -p kheish-daemon && scripts/fregate-provider-check/run.sh` (it needs `python3` and
  `curl`, and ports 4101 and 4190 free).

## Results

13 of 13 checks passed (`report.py`, run on the final build of this branch).

| Check | Result | Evidence |
|---|---|---|
| Every request carries `x-kheish-session-id` and `x-kheish-run-id` of its run | PASS | 12 requests; pairs `(fm-check-basic, run-1)`, `(fm-check-fallback, run-2)`, `(fm-check-fallback, run-3)`, `(fm-check-refusal, run-4)`, `(fm-check-cutoff, run-5)`, `(fm-check-cuttwice, run-6)`, `(fm-check-orphan, run-7)`, `(fm-check-orphan, run-8)`, `(fm-check-stable, run-9)`, `(fm-check-stable, run-10)` |
| The basic request names its own session and run | PASS | `x-kheish-session-id: fm-check-basic`, `x-kheish-run-id: run-1` |
| `claude-opus-5-5` gets adaptive thinking with effort, never `budget_tokens` or `temperature` | PASS | `"thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}}`, `"output_config": {"effort": "high"}` |
| Cache breakpoints on the system prompt, the last tool, and the conversation tail | PASS | `system[0].cache_control`, 56 tools sorted by name with `cache_control` on the last (`write_file`) only, top-level `cache_control` |
| Within a run, the cached prefix is byte-identical from request to request | PASS | `fm-check-cutoff`: the two requests of `run-5` have equal `system` and `tools` |
| Across runs with run memory off, `system` and `tools` are byte-identical and the history only grows | PASS | `fm-check-stable`: `run-10`'s request starts with `run-9`'s message, then appends two |
| Finding, run memory on (the default): a session's second run gains a Recovered Memory section in the system prompt | PASS (observed) | `fm-check-fallback` run 2 system prompt = run 1 system prompt + 463 characters starting `# Recovered Memory` (see Findings) |
| Betas and fallbacks | PASS | `anthropic-beta: thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01`, `"fallbacks": "default"` |
| A fallback answer completes the run and replays text and marker, without the declined thinking | PASS | `run-2` completed; `run-3`'s request replays `[text "Looking at the service. ", fallback {from claude-opus-5-5, to claude-opus-5, trigger cyber}, text "The port is closed."]` |
| A final refusal ends the run failed with `error_code: model_refusal` and its category | PASS | `run-4`: `"status": "failed"`, `"error_code": "model_refusal"`, `"refusal": {"category": "cyber", "explanation": "...", "model": "claude-opus-5"}` |
| A `max_tokens` cut-off is discarded and asked again once with `retry_max_tokens` | PASS | `run-5`: `max_tokens` 16000 then 32000, identical `messages`, run completed |
| A second cut-off ends the run failed with `error_code: output_limit_exceeded` | PASS | `run-6`: 16000 then 32000, `"error_code": "output_limit_exceeded"` |
| After `kill -9` mid tool call, the next run replays the call verbatim, appends the error result, sends `drop_block` | PASS | `run-7` interrupted, `run-8` completed (request below) |

### The basic request (`run-1`)

```text
POST /v1/messages
x-api-key: stub-relay-key
anthropic-version: 2023-06-01
anthropic-beta: thinking-binding-controls-2026-08-01,server-side-fallback-2026-07-01
x-kheish-session-id: fm-check-basic
x-kheish-run-id: run-1
```

```json
{
  "model": "claude-opus-5-5",
  "max_tokens": 8000,
  "stream": true,
  "thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
  "output_config": {"effort": "high"},
  "fallbacks": "default",
  "cache_control": {"type": "ephemeral"},
  "tool_choice": {"type": "auto", "disable_parallel_tool_use": false},
  "system": [{"type": "text", "text": "You are Kheish, a daemon-first autonomou...", "cache_control": {"type": "ephemeral"}}],
  "tools": "[56 tools sorted by name; the last, write_file, with cache_control]",
  "messages": [{"role": "user", "content": [{"type": "text", "text": "Say hello."}]}]
}
```

The run was submitted with `generation.reasoning.effort = "high"`; before this branch the same run
was sent `"thinking": {"type": "enabled", "budget_tokens": 8192}`, which the model rejects with a 400.

### The fallback turn (`run-2`) as stored

The session journal keeps the turn's usage per attempt, each priced at its own model:

```json
{"input_tokens": 1200, "output_tokens": 30, "cost_usd": 0.01191, "iterations": [
  {"type": "message", "model": "claude-opus-5-5", "input_tokens": 1200, "output_tokens": 18, "cost_usd": 0.00516},
  {"type": "fallback_message", "model": "claude-opus-5", "input_tokens": 1200, "output_tokens": 30, "cost_usd": 0.00675}]}
```

and its provider context, the blocks the next request replays (the declined attempt's thinking
block `sig-declined-attempt` is withdrawn, its text and the marker stay):

```json
{"anthropic": {"content_blocks": [
  {"type": "text", "text": "Looking at the service. "},
  {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-5"}, "trigger": {"type": "refusal", "category": "cyber"}},
  {"type": "text", "text": "The port is closed."}]}}
```

### The refusal (`run-4`)

`GET /v1/runs/run-4/events` after a restart:

```json
{"type": "accepted"}
{"type": "started"}
{"type": "failed", "error": "model_refusal: the model declined the request by claude-opus-5 (refusal category: cyber)",
 "code": "model_refusal",
 "refusal": {"category": "cyber", "explanation": "This request was declined because it could enable cyber harm.", "model": "claude-opus-5"}}
```

### The orphaned tool call (`run-7`, then `run-8`)

The stub answered `run-7` with a thinking block and a `bash` call (`sleep 917`); the daemon was
killed with `kill -9` while the call slept and restarted on the same state root. `run-7` came back
`interrupted`. The request of the next run, `run-8`:

```json
"thinking": {"type": "adaptive", "block_binding": {"prefix_mismatch_behavior": "drop_block"}},
"messages": [
  {"role": "user", "content": [{"type": "text", "text": "CASE:ORPHAN scan the host."}]},
  {"role": "assistant", "content": [
    {"type": "thinking", "thinking": "", "signature": "sig-orphan-call"},
    {"type": "tool_use", "id": "toolu_orphan", "name": "bash", "input": {"command": "sleep 917"}}]},
  {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_orphan", "is_error": true,
    "content": [{"type": "text", "text": "{\"error\":\"tool result missing during provider prompt normalization\",\"tool\":\"bash\"}"}]}]},
  {"role": "user", "content": [{"type": "text", "text": "The run was interrupted by a restart; continue."}]}]
```

The first message is byte-identical to `run-7`'s, the thinking block keeps its empty text and its
signature, and the synthetic error result is appended after the call. With run memory on, `run-8`'s
system prompt also gained a Recovered Memory section, a prefix edit before that thinking block: on
the real API `drop_block` makes it drop the block instead of failing the run with a 400.

## Findings

1. **Run memory re-renders the system prompt between runs.** With the default run-memory policy,
   a session's second run carries a `# Recovered Memory` section appended to the top-level system
   prompt (`fm-check-fallback`, `fm-check-orphan`). That changes the cached prefix (a full cache
   miss on the first turn of every run) and, on models with preserved thinking, invalidates every
   thinking block of the earlier runs; `drop_block` keeps the requests valid, at the cost of that
   reasoning. Within a run the prefix is stable, and with run memory off it is stable across runs
   (`fm-check-stable`). For Fregate: disable run memory when provisioning
   (`POST /v1/runtime/run-memory-policy` with `"enabled": false`), as the spike (section 8) found
   possible; in Kheish, the durable fix is to deliver recovered memory as an appended message rather
   than in `system`.
2. **Compaction re-renders its restoration at the tail.** Read in the code, not exercised by this
   check: `build_prompt_workspace` (`crates/kheish-core/src/engine.rs`) appends the checkpoint's
   post-compaction restoration after the newest turn of every request, so after a compaction the
   restoration moves on each turn and every turn's thinking is minted under a prefix the next request
   changes. With `drop_block` the requests stay valid but lose that reasoning every turn after a
   compaction. The append-only form pins the restoration where it was first sent (after the turns the
   compaction kept). Fregate's M1 missions (12 turns, 1M-token window) do not reach compaction.
3. **Fregate's legacy adapter drops the `fallback` marker on replay**
   (`apps/server/internal/adapter/llm/anthropic/anthropic.go`, `response`). The refusals and fallback
   page says to keep it exactly where it appeared, because the API validates the thinking blocks
   around it by its position; Kheish keeps it.

## Not covered here

- The real Messages API. The stub answers the documented shapes; acceptance by the API (the beta
  values, `block_binding`, `fallbacks`, the replayed `fallback` block) is proven later through
  Fregate's relay with a workspace's own key.
- Claude Sonnet 5.5, Claude Haiku 5.5 and the Fable models: their request shapes are covered by unit
  tests per model id (`crates/kheish-runtime/src/providers/anthropic.rs`, `anthropic_models.rs`).
