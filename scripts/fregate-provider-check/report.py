"""Checks the live run's captured requests and run views; prints a markdown report."""
import json
import os
import sys

ROOT = sys.argv[1]
requests = [json.loads(line) for line in open(os.path.join(ROOT, "requests.jsonl"))]
runs = dict(line.strip().split("=", 1) for line in open(os.path.join(ROOT, "run-ids.txt")) if "=" in line)
views = {name: json.load(open(os.path.join(ROOT, "runs", f"{run_id}.json"))) for name, run_id in runs.items()}
results = []


def check(name, ok, evidence):
    results.append((name, bool(ok), evidence))


def by_session(session):
    return [r for r in requests if r["headers"].get("x-kheish-session-id") == session]


def compact(value, limit=400):
    text = json.dumps(value, separators=(",", ":"))
    return text if len(text) <= limit else text[:limit] + "..."


# 1. Headers on every request.
pairs = sorted({(r["headers"].get("x-kheish-session-id"), r["headers"].get("x-kheish-run-id")) for r in requests})
known_runs = set(runs.values())
check("every request carries x-kheish-session-id and x-kheish-run-id of its run",
      all(s and rid in known_runs for s, rid in pairs) and len(requests) > 0,
      f"{len(requests)} requests; (session, run) pairs: {pairs}")
basic = by_session("fm-check-basic")[0]
check("the basic request names its own session and run",
      basic["headers"]["x-kheish-session-id"] == "fm-check-basic"
      and basic["headers"]["x-kheish-run-id"] == runs["basic"],
      compact({k: basic["headers"][k] for k in ("x-kheish-session-id", "x-kheish-run-id")}))

# 2. Adaptive thinking on claude-opus-5-5.
b = basic["body"]
check("claude-opus-5-5 gets adaptive thinking with effort, never budget_tokens or temperature",
      b["model"] == "claude-opus-5-5" and b["thinking"]["type"] == "adaptive"
      and b.get("output_config") == {"effort": "high"} and "budget_tokens" not in json.dumps(b)
      and "temperature" not in b,
      compact({"model": b["model"], "thinking": b["thinking"], "output_config": b.get("output_config")}))

# 3. Cache breakpoints.
tools = b.get("tools", [])
check("cache breakpoints on the system prompt, the last tool, and the conversation tail",
      b["system"][0]["cache_control"] == {"type": "ephemeral"}
      and tools and tools[-1].get("cache_control") == {"type": "ephemeral"}
      and all("cache_control" not in t for t in tools[:-1])
      and b.get("cache_control") == {"type": "ephemeral"}
      and [t["name"] for t in tools] == sorted(t["name"] for t in tools),
      f"system[0].cache_control={compact(b['system'][0]['cache_control'])}; {len(tools)} tools sorted by name, "
      f"last={tools[-1]['name'] if tools else None} with cache_control; top-level cache_control={compact(b.get('cache_control'))}")
fb_requests = by_session("fm-check-fallback")
cut_pair = by_session("fm-check-cutoff")
check("within a run, the cached prefix (system, tools) is byte-identical from request to request",
      len(cut_pair) == 2 and cut_pair[0]["body"]["system"] == cut_pair[1]["body"]["system"]
      and cut_pair[0]["body"]["tools"] == cut_pair[1]["body"]["tools"],
      f"fm-check-cutoff: {len(cut_pair)} requests of run {runs['cutoff']}, system and tools equal")
stable = by_session("fm-check-stable")
s0, s1 = stable[0]["body"], stable[-1]["body"]
check("across runs with run memory off, system and tools are byte-identical and the history only grows",
      len(stable) == 2 and s0["system"] == s1["system"] and s0["tools"] == s1["tools"]
      and s1["messages"][:len(s0["messages"])] == s0["messages"],
      f"fm-check-stable: runs {runs['stable_1']} and {runs['stable_2']}; second request starts with the first's "
      f"{len(s0['messages'])} message(s), then {len(s1['messages']) - len(s0['messages'])} appended")
a_sys = fb_requests[0]["body"]["system"][0]["text"]
b_sys = fb_requests[1]["body"]["system"][0]["text"]
added = b_sys[len(a_sys):] if b_sys.startswith(a_sys) else None
check("finding (run memory on, the default): a session's second run gains a Recovered Memory section in the system prompt",
      fb_requests[0]["body"]["tools"] == fb_requests[1]["body"]["tools"] and added is not None
      and "# Recovered Memory" in added,
      f"fm-check-fallback run 2 system prompt = run 1 system prompt + {len(added or '')} chars starting "
      f"{(added or '').strip()[:40]!r}")

# 4. Betas and fallbacks.
betas = basic["headers"].get("anthropic-beta", "").split(",")
check("betas: thinking-binding-controls-2026-08-01 and server-side-fallback-2026-07-01; fallbacks default; drop_block",
      "thinking-binding-controls-2026-08-01" in betas and "server-side-fallback-2026-07-01" in betas
      and b.get("fallbacks") == "default"
      and b["thinking"].get("block_binding") == {"prefix_mismatch_behavior": "drop_block"},
      f"anthropic-beta: {basic['headers'].get('anthropic-beta')}; fallbacks={compact(b.get('fallbacks'))}")

# 5. Fallback parsed and replayed.
replay = fb_requests[1]["body"]["messages"][1] if len(fb_requests) == 2 else {}
expected = [
    {"type": "text", "text": "Looking at the service. "},
    {"type": "fallback", "from": {"model": "claude-opus-5-5"}, "to": {"model": "claude-opus-5"},
     "trigger": {"type": "refusal", "category": "cyber"}},
    {"type": "text", "text": "The port is closed."},
]
check("a fallback answer completes the run and replays text and marker, without the declined thinking",
      views["fallback"]["status"] == "completed" and replay.get("role") == "assistant"
      and replay.get("content") == expected,
      f"run {runs['fallback']} {views['fallback']['status']}; next request messages[1]={compact(replay)}")

# 6. Refusal ends the run.
rv = views["refusal"]
check("a final refusal ends the run failed with error_code model_refusal and its category",
      rv["status"] == "failed" and rv.get("error_code") == "model_refusal"
      and (rv.get("refusal") or {}).get("category") == "cyber",
      compact({k: rv.get(k) for k in ("status", "error", "error_code", "refusal")}, 600))

# 7. Output-limit retry.
cut = by_session("fm-check-cutoff")
check("a max_tokens cut-off is discarded and asked again once with retry_max_tokens",
      views["cutoff"]["status"] == "completed" and [r["body"]["max_tokens"] for r in cut] == [16000, 32000]
      and cut[0]["body"]["messages"] == cut[1]["body"]["messages"],
      f"max_tokens per request: {[r['body']['max_tokens'] for r in cut]}; same messages: "
      f"{len(cut) == 2 and cut[0]['body']['messages'] == cut[1]['body']['messages']}; run {views['cutoff']['status']}")
twice = by_session("fm-check-cuttwice")
tv = views["cuttwice"]
check("a second cut-off ends the run failed with error_code output_limit_exceeded",
      tv["status"] == "failed" and tv.get("error_code") == "output_limit_exceeded" and len(twice) == 2,
      compact({"requests": [r["body"]["max_tokens"] for r in twice],
               **{k: tv.get(k) for k in ("status", "error", "error_code")}}, 600))

# 8. Orphaned tool call recovery.
orphan = by_session("fm-check-orphan")
first, last = orphan[0]["body"], orphan[-1]["body"]
messages = last["messages"]
assistant = messages[1] if len(messages) > 1 else {}
repair = messages[2]["content"][0] if len(messages) > 2 else {}
check("after a kill -9 mid tool call, the next run replays the call verbatim, appends the error result, sends drop_block",
      views["orphan"]["status"] == "interrupted" and views["orphan_next"]["status"] == "completed"
      and first["messages"][0] == messages[0]
      and assistant.get("content", [{}])[0] == {"type": "thinking", "thinking": "", "signature": "sig-orphan-call"}
      and assistant["content"][1]["type"] == "tool_use" and assistant["content"][1]["id"] == "toolu_orphan"
      and repair.get("type") == "tool_result" and repair.get("tool_use_id") == "toolu_orphan" and repair.get("is_error") is True
      and last["thinking"].get("block_binding") == {"prefix_mismatch_behavior": "drop_block"},
      f"run {runs['orphan']} {views['orphan']['status']}, next run {views['orphan_next']['status']}; "
      f"messages[1]={compact(assistant)}; messages[2].content[0]={compact(repair)}; "
      f"thinking={compact(last['thinking'])}")

print("| Check | Result | Evidence |")
print("|---|---|---|")
for name, ok, evidence in results:
    print(f"| {name} | {'PASS' if ok else 'FAIL'} | `{evidence.replace('|', '/')}` |")
print()
print(f"{sum(ok for _, ok, _ in results)}/{len(results)} checks passed")
