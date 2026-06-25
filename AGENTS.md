# Kheish Operator Guide

This file is for future agent instances working inside `kheish/`.

Read this first when you need to change code, run the daemon, validate behavior on real providers, or debug a broken session.

This is an operational guide, not a full architecture spec.

## Working Rules

- Treat the daemon as the source of truth.
- Prefer the real CLI and HTTP API over direct inspection of JSON state files.
- Use fresh `--state-root` values for real validation.
- Use fresh sessions for E2E tests.
- Validate on the real daemon, not only with unit tests.
- Keep examples and paths portable. This repository may be cloned anywhere.
- When behavior differs across providers, keep the abstraction provider-neutral unless the provider wire format truly requires divergence.
- Do not infer product behavior from stale sessions created by an older daemon build.

## Repository Basics

Repository root:
- the directory that contains this `AGENTS.md`

Important files:
- `.env`
- `Cargo.toml`
- `README.md`
- `AGENTS.md`

Most active crates:
- `crates/kheish-daemon`: daemon HTTP API, CLI integration, run/session/task control plane
- `crates/kheish-runtime`: provider drivers, prompt building, permissions, runtime assembly
- `crates/kheish-core`: engine loop, compaction, hook integration
- `crates/kheish-coding-tools`: built-in tools such as `read_file`, `write_file`, `bash`, `web_fetch`, `web_search`
- `crates/kheish-mcp`: MCP loading, config import, tool/resource adaptation
- `crates/kheish-agent`: agent orchestration, snapshots, sidechains, mailbox
- `crates/kheish-types`: shared data contracts

Important old state roots that may still exist in this repo:
- `.kheish-daemon`
- `.kheish-daemon2`
- `.kheish-daemon-fix`
- `.kheish-daemon-fix2`

Do not assume the daemon you are talking to uses the state root you expect. Always be explicit.

## Environment

Load provider keys before live testing:

```bash
cd "$(git rev-parse --show-toplevel)"
set -a
source .env
set +a
```

Supported providers in normal operation:
- `anthropic`
- `google`
- `openai`
- `openrouter`
- `xai`

If you are testing OpenAI live, prefer an explicit model such as `gpt-5.4`. Some local defaults may reject options like `temperature`.

For route details, see `docs/runtime/providers-and-routing.mdx`.

## Build

Normal build:

```bash
cd "$(git rev-parse --show-toplevel)"
cargo build -p kheish-daemon
```

Fast sanity build:

```bash
cargo b -p kheish-daemon
```

Daemon binary:
- `target/debug/kheish-daemon`

## Start the Daemon

Canonical isolated start:

```bash
cd "$(git rev-parse --show-toplevel)"
set -a
source .env
set +a

mkdir -p ./.kheish-workspace-test

./target/debug/kheish-daemon serve \
  --bind 127.0.0.1:4000 \
  --state-root .kheish-daemon-test \
  --workspace-root ./.kheish-workspace-test
```

Serve options you will use often:
- `--provider anthropic|openai`
- `--model ...`
- `--api-key ...`
- `--workspace-root ...`
- `--state-root ...`
- `--http-cors-allow-origin http://localhost:5173` to narrow browser CORS to exact local origins

Health checks:

```bash
./target/debug/kheish-daemon status
./target/debug/kheish-daemon runtime get
```

Against another daemon URL:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4011 status
```

If `status` or `runtime get` fails with a decode error such as a missing `hooks` field, the CLI and daemon are on different schema versions. Rebuild and restart the daemon, or talk to the matching binary.

## Default Operating Workflow

### 1. Create a session

```bash
./target/debug/kheish-daemon sessions create demo
```

### 2. Submit one input

Detached by default. Save the returned `run_id`.

```bash
./target/debug/kheish-daemon sessions input demo \
  "Analyze the machine, inspect the relevant files, and summarize what you found."
```

If you want provider/model overrides on one run:

```bash
./target/debug/kheish-daemon sessions input demo \
  --provider openai \
  --model gpt-5.4 \
  "Analyze the machine and write a concise report."
```

or:

```bash
./target/debug/kheish-daemon sessions input demo \
  --provider anthropic \
  --model claude-opus-4-6 \
  "Analyze the machine and write a concise report."
```

These overrides are run-scoped. They do not mutate the daemon globally.

### 3. Observe the run

```bash
./target/debug/kheish-daemon runs list --session-id demo
./target/debug/kheish-daemon runs get <run_id>
./target/debug/kheish-daemon runs wait <run_id>
./target/debug/kheish-daemon runs stream <run_id>
```

### 4. Inspect the session

```bash
./target/debug/kheish-daemon sessions get demo
./target/debug/kheish-daemon sessions events demo
./target/debug/kheish-daemon sessions stream demo
```

## Completion Semantics

Do not rely on implicit completion contracts inferred from the prompt. That behavior was removed.

Current rule:
- only explicit completion requirements are enforced
- otherwise verification is driven by prompt discipline, tools, tasks, and explicit reporting

Use explicit requirements only when you truly need a deterministic artifact contract:

```bash
./target/debug/kheish-daemon sessions input demo \
  --require-workspace-file-path reports/demo.txt \
  "Write the report into reports/demo.txt and then confirm the path."
```

Do not use this for every file-producing task by reflex. Prefer normal agent verification unless the test specifically needs a hard artifact guarantee.

## Approvals

Approvals are normal for shell-heavy or edit-heavy runs.

List approvals:

```bash
./target/debug/kheish-daemon approvals list
./target/debug/kheish-daemon approvals list --session-id demo
```

Approve one:

```bash
./target/debug/kheish-daemon approvals allow demo approval-bash-1 --justification "approved"
```

Approve the current wave:

```bash
./target/debug/kheish-daemon approvals allow-all \
  --session-id demo \
  --justification "approved for this run" \
  --wait
```

Deny:

```bash
./target/debug/kheish-daemon approvals deny-all \
  --session-id demo \
  --reason "unsafe"
```

Rules:
- one `allow-all` is not always enough
- shell-heavy runs may produce multiple approval waves
- if `runs wait` appears stuck, check approvals before assuming the run is hung
- `runs get` is the authority for run status

## Provider and Model Routing

The daemon supports mixed-provider operation on the same process.

You can run:
- one session on `anthropic`
- another on `openai`
- or two runs in the same session with different explicit provider/model overrides

The resolved route is pinned per run. If you later change the daemon default model/provider, queued or active runs do not drift.

Sidechains also support explicit provider/model overrides:

```bash
./target/debug/kheish-daemon agents spawn-sidechain ... \
  --provider openai \
  --model gpt-5.4
```

## Shell Tasks

`bash` can run in foreground or background. Shell tasks are first-class daemon tasks.

Important commands:

```bash
./target/debug/kheish-daemon tasks list demo
./target/debug/kheish-daemon tasks get demo <task_id>
./target/debug/kheish-daemon tasks output demo <task_id>
./target/debug/kheish-daemon tasks output demo <task_id> --full
./target/debug/kheish-daemon tasks stop demo <task_id>
```

Current behavior:
- background shell tasks persist output to disk
- foreground shell tasks are also represented as daemon tasks
- interrupted runs can leave a shell task alive and inspectable
- `task_output` and `task_stop` are exposed to the default, coordinator, and verification agent profiles

Use this when validating long-running shell behavior:
- launch a long command
- inspect `tasks list`
- inspect `tasks output`
- stop the task and verify the terminal status

For the API and CLI shape, see `docs/reference/tasks-schedules-agents-api.mdx`.

## Web Research

Current web research pattern is provider-neutral:
- `web_search` to discover sources
- `web_fetch` only when page-level details are needed

The system prompt now explicitly asks for:
- a `Sources:` section
- markdown links in the form `- [Title](URL)`

This works on both Anthropic and OpenAI. Use real provider tests when touching:
- `web_search`
- prompt source guidance
- citations behavior
- web result parsing

## MCP

`kheish` now has a real MCP path through `kheish-mcp`.

Operational notes:
- daemon startup can load MCP from Codex-compatible config and credentials files
- MCP tools are surfaced into runtime and prompt context
- runtime state includes MCP snapshot information

After startup, inspect runtime:

```bash
./target/debug/kheish-daemon runtime get
```

Look for:
- active MCP tools
- MCP server instructions

When debugging MCP, validate on both providers. Good real scenarios:
- OpenAI docs MCP search + fetch
- Linear MCP list/query flow

## Hooks

`kheish` now has real daemon-configurable hooks. This is no longer just test-only `ToolHook` plumbing.

Inspect hooks:

```bash
./target/debug/kheish-daemon runtime hooks get
```

Replace hooks from a file:

```bash
./target/debug/kheish-daemon runtime hooks set --file hooks.json
```

Or from stdin:

```bash
cat hooks.json | ./target/debug/kheish-daemon runtime hooks set --stdin
```

Use hooks when validating:
- permission gating
- session start / end behavior
- worktree events
- task lifecycle interactions

Do not assume hook behavior from unit tests alone. Validate on a real daemon.

## Debug Mode

Enable debug capture before the run you want to inspect:

```bash
./target/debug/kheish-daemon runtime set-debug-level full
```

Levels:
- `off`
- `on`
- `redacted`
- `full`

Important:
- `full` is daemon-global, not run-scoped
- use it only on isolated daemon instances
- lower it again when done

Inspect a run bundle:

```bash
./target/debug/kheish-daemon runs debug <run_id>
```

Inspect one artifact:

```bash
./target/debug/kheish-daemon runs debug-artifact <run_id> turn-0001-attempt-0001-model-request
./target/debug/kheish-daemon runs debug-artifact <run_id> turn-0001-attempt-0001-provider-request
./target/debug/kheish-daemon runs debug-artifact <run_id> turn-0001-attempt-0001-provider-events
./target/debug/kheish-daemon runs debug-artifact <run_id> turn-0001-attempt-0001-model-response
```

Use these first:
- system prompt actually sent
- tool surface actually sent
- provider request body
- provider event stream
- normalized final response

## Multi-Agent Surface

Current control-plane commands:

```bash
./target/debug/kheish-daemon agents list
./target/debug/kheish-daemon agents get agent-1
./target/debug/kheish-daemon agents spawn-sidechain ...
./target/debug/kheish-daemon mailboxes post ...
```

Agent-side tools now include:
- `spawn_agent`
- agent messaging/wait tools
- task tools
- plan/todo tools
- shell task follow-up tools
- web tools

Do not assume a specific profile has a tool. Check the actual surface or tests in `crates/kheish-daemon/src/control_tools.rs`.

## Runtime Reconfiguration

You can change runtime settings without restarting the daemon:

```bash
./target/debug/kheish-daemon runtime set-model claude-opus-4-6
./target/debug/kheish-daemon runtime set-permission-mode accept-edits
./target/debug/kheish-daemon runtime set-debug-level redacted
./target/debug/kheish-daemon runtime set-system-prompt --mode append \
  "When a tool can perform the next step directly, use it instead of narrating intent."
```

Always verify afterwards:

```bash
./target/debug/kheish-daemon runtime get
```

## Recommended Test Matrix

Minimum local validation before claiming a change is safe:

```bash
cargo test -p kheish-types -p kheish-core -p kheish-runtime --lib
cargo test -p kheish-coding-tools --lib
cargo test -p kheish-daemon --lib
```

For live provider validation:

```bash
set -a
source .env
set +a

cargo test -p kheish-daemon --test anthropic_hooks_live -- --nocapture
cargo test -p kheish-daemon --test openai_live -- --nocapture
```

When a change touches the daemon behavior, also validate with the real binary:

1. start an isolated daemon
2. hit it through the CLI
3. use a fresh session
4. verify the final artifact or final answer directly

Do this especially for:
- approvals
- shell tasks
- provider routing
- hooks
- MCP
- web research
- multi-agent behavior

## Suggested Real E2E Scenarios

### Report scenario

```bash
./target/debug/kheish-daemon sessions create report-demo
./target/debug/kheish-daemon sessions input report-demo \
  --provider anthropic \
  --model claude-opus-4-6 \
  "Analyze the machine, inspect relevant system details, and write a report in reports/report-demo.txt"
```

### Non-file verification scenario

```bash
./target/debug/kheish-daemon sessions create inspect-demo
./target/debug/kheish-daemon sessions input inspect-demo \
  --provider openai \
  --model gpt-5.4 \
  "Inspect the workspace root, summarize the current directory layout, and explicitly say what you verified."
```

### Web-sourced post scenario

```bash
./target/debug/kheish-daemon runtime set-permission-mode bypass-permissions
./target/debug/kheish-daemon sessions create web-post-demo
./target/debug/kheish-daemon sessions input web-post-demo \
  --provider openai \
  --model gpt-5.4 \
  "Use web_search and web_fetch to write a sourced markdown post about stale-while-revalidate into posts/swr.md. Include a Sources: section with markdown links."
```

### Background shell scenario

Ask the agent to launch a long `bash` task in background, then inspect it with `tasks list` and `tasks output`.

## Known Operational Pitfalls

1. State-root mixups are common.
   Many apparent bugs are actually “wrong daemon / wrong state root”.

2. CLI/daemon schema drift is real.
   If decode errors mention missing fields, rebuild and restart the daemon or use a matching CLI.

3. Fresh sessions are better than resurrected sessions for product validation.
   Old interrupted sessions are useful for debugging, not for clean validation.

4. Killed daemons can leave awkward recovery state around approvals or foreground shell tasks.
   Inspect `runs get`, `approvals list`, `sessions get`, and `tasks list` together before resuming.

5. Shell-heavy runs often need more than one approval wave.

6. `web_search` quality depends on the actual HTML search response shape.
   If it suddenly returns empty results, inspect the provider output and the search HTML before blaming the prompt.

7. OpenAI live scenarios are more reliable with an explicit `--model gpt-5.4`.

## Evidence-First Debugging Order

When a run looks wrong, prefer this order:

1. `runs get <run_id>`
2. `approvals list --session-id <session_id>`
3. `tasks list <session_id>`
4. `tasks output <session_id> <task_id>`
5. `sessions get <session_id>`
6. `sessions events <session_id>`
7. `runs debug <run_id>`
8. state files only if the daemon is unavailable

## Quick Reference

Daemon:

```bash
./target/debug/kheish-daemon serve --bind 127.0.0.1:4000 --state-root .kheish-daemon --workspace-root ./.kheish-workspace
./target/debug/kheish-daemon status
./target/debug/kheish-daemon doctor
```

Sessions:

```bash
./target/debug/kheish-daemon sessions create demo
./target/debug/kheish-daemon sessions get demo
./target/debug/kheish-daemon sessions input demo "..."
./target/debug/kheish-daemon sessions events demo
./target/debug/kheish-daemon sessions stream demo
./target/debug/kheish-daemon sessions interrupt demo
./target/debug/kheish-daemon sessions end demo --reason "done"
```

Runs:

```bash
./target/debug/kheish-daemon runs list --session-id demo
./target/debug/kheish-daemon runs get <run_id>
./target/debug/kheish-daemon runs wait <run_id>
./target/debug/kheish-daemon runs stream <run_id>
./target/debug/kheish-daemon runs cancel <run_id>
./target/debug/kheish-daemon runs debug <run_id>
./target/debug/kheish-daemon runs debug-artifact <run_id> <artifact-id>
```

Approvals:

```bash
./target/debug/kheish-daemon approvals list --session-id demo
./target/debug/kheish-daemon approvals allow-all --session-id demo --justification "approved"
./target/debug/kheish-daemon approvals deny-all --session-id demo --reason "unsafe"
```

Tasks:

```bash
./target/debug/kheish-daemon tasks list demo
./target/debug/kheish-daemon tasks get demo <task_id>
./target/debug/kheish-daemon tasks output demo <task_id> --full
./target/debug/kheish-daemon tasks stop demo <task_id>
```

Runtime:

```bash
./target/debug/kheish-daemon runtime get
./target/debug/kheish-daemon runtime hooks get
./target/debug/kheish-daemon runtime set-model claude-opus-4-6
./target/debug/kheish-daemon runtime set-permission-mode default
./target/debug/kheish-daemon runtime set-debug-level full
./target/debug/kheish-daemon runtime set-system-prompt --mode append "..."
```

If you need architecture detail, start with:
- `README.md`

If you need ground truth about behavior, trust:
- the daemon CLI
- real provider runs
- debug artifacts
- daemon tests

Use raw state files only as a fallback.
