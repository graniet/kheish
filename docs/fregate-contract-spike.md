# Fregate <-> Kheish daemon contract spike

Working note, 2026-11-06. Branch `feat/fregate-runtime` at `d3ae78f`, `cargo build -p kheish-daemon` (debug).
Everything below was run against a live daemon on `127.0.0.1:4100` with local stubs, unless marked
"code only". No code was changed.

## Test rig

- Daemon: `/tmp/kheish-spike-stub/start-daemon.sh` (clean `env -i`, state `/tmp/kheish-spike-state`,
  workspace `/tmp/kheish-spike-ws`).
- Stubs: `/tmp/kheish-spike-stub/stub.py` (one process):
  - `:4101` Anthropic Messages SSE stub. It logs every request. `mode.json` can force an HTTP status.
    The scripted replies are: `CALL_TOOL` returns a `tool_use` for the first `mcp__fregate__*` tool,
    `CALL_BASH` returns a `tool_use` for `bash`, and a `tool_result` in the last user turn returns final text.
  - `:4102` MCP streamable HTTP stub (`initialize` sets `Mcp-Session-Id`, one tool `scan_target`,
    optional delay).
  - `:4103` hook endpoint (allow / deny / ask / 500 from `mode.json`).
  - `:4104` CONNECT catcher (used as `HTTPS_PROXY` to prove where traffic would go without leaving the box).
- Helpers: `k.sh METHOD PATH [JSON]` (adds the admin bearer), `mk_session.sh`, `run.sh`.

---

## 1. Auth, start, and turning the coding tools off

### Start command (verified)

```bash
KHEISH_AUTH_STORE_MASTER_KEY_FILE=/tmp/kheish-spike-stub/master.key \
kheish-daemon serve \
  --bind 127.0.0.1:4100 \
  --state-root /tmp/kheish-spike-state \
  --workspace-root /tmp/kheish-spike-ws \
  --routes-file /tmp/kheish-spike-stub/routes.toml \
  --http-auth-mode bearer \                       # env KHEISH_HTTP_AUTH_MODE
  --http-admin-token-file .../admin.token \       # env KHEISH_DAEMON_ADMIN_TOKEN_FILE
  --mcp-discovery disabled \                      # env KHEISH_MCP_DISCOVERY; default `auto` reads ~/.codex/config.toml
  --log-format json
```

- Master key: `kheish-daemon secrets generate > master.key`. Use `KHEISH_AUTH_STORE_MASTER_KEY_FILE`
  or `KHEISH_AUTH_STORE_MASTER_KEY`. Without it, secret writes do not persist.
- Run the daemon with a clean environment. `ANTHROPIC_BASE_URL` from the environment is applied to any
  anthropic route that has no `base_url` (`cli/serve/support.rs:1455-1461`). `HTTPS_PROXY` is honoured by reqwest.
- Auth results:
  - `GET /v1/runtime` with no token returns `401 {"code":"missing_token","domain":"auth"}`.
  - A wrong token returns `401 {"code":"invalid_token"}`.
  - `/healthz` and `/readyz` are unauthenticated (`200 ok`).
- A second `serve` on the same state root exits immediately. See section 9.

### Coding tools: what exists

| Mechanism | Hides built-ins? | Evidence |
|---|---|---|
| Daemon-wide flag or env | **none exists** | `builders.rs:687-695` calls `register_default_coding_tools` and `register_daemon_control_tools` unconditionally |
| Persona `capability_scope` | no. It only covers `skill_*`, `mcp_server_*` and `mcp_tool_*` | `kheish-types/src/capabilities.rs:17-36` |
| Session `capability_scope` | no (same type) | same |
| Permission rules | no API to add deny rules. The rules are hard-coded in `builders.rs:86-154`; hooks can only push `updated_permissions` | code |
| **Session tool overrides** | **yes**, but only as an exact-name deny list | `PUT /v1/sessions/{id}/tool-overrides`, applied in `kheish-runtime/src/runtime.rs:2404-2431` |

The baseline (no overrides) shows that the model receives **57 tools**: `apply_patch`, `bash`, `read_file`,
`write_file`, `edit_file`, `list_files`, `glob_search`, `grep_search`, `web_search`, `web_fetch`, `spawn_agent`,
`schedule_*`, `task_*`, `project_*`, `todo_write`, `use_skill`, `list_mcp_resources`, ... and `mcp__fregate__scan_target`.

The call that works today:

```http
PUT /v1/sessions/mission-m1/tool-overrides
{"disable":["apply_patch","ask_user_question","bash","board_draw","board_view","create_channel_stimulus",
 "create_goal","edit_file","emit_output","enter_plan_mode","exit_plan_mode","get_agent","get_goal",
 "glob_search","grep_search","list_agent_summaries","list_agents","list_files","list_mcp_resource_templates",
 "list_mcp_resources","list_skills","message_agent","project_claim_task","project_create_task",
 "project_list_tasks","project_update_task","read_channel_thread","read_file","read_mcp_resource",
 "request_parent_clarification","schedule_cancel","schedule_create","schedule_get","schedule_list",
 "schedule_pause","schedule_resume","schedule_trigger_now","set_channel_reaction","spawn_agent","store_asset",
 "task_create","task_delete","task_get","task_list","task_output","task_stop","task_update","todo_write",
 "update_goal","use_skill","wait_agent","wake_after","wake_at","web_fetch","web_search","write_file",
 "generate_image","edit_image","generate_audio","remember_about","notify_operator","ask_operator"]}
-> 200 SessionView
```

Proof, captured at the model stub:
- After the overrides, `tools` in the Anthropic request is exactly `["mcp__fregate__scan_target"]`.
- The model calls a hidden tool (`CALL_BASH`):
  - In `bypassPermissions` mode, the tool result is `{"error":"tool bash is not available for this agent"}`,
    the run completes and nothing executes. The workspace is still empty.
  - In `default` mode, the permission check runs **before** the visibility check. The run goes to
    `waiting_for_approval` with `pending_approvals[0].tool_name = "bash"`. If that approval is **allowed**,
    execution is still refused ("tool bash is not available for this agent"). So the hidden tool is safe,
    but Fregate sees a spurious approval.
- The system prompt still contains `Primary working directory: /tmp/kheish-spike-ws` and `Shell: /bin/bash`.

Gaps:
- The deny list is not fail-closed: a new built-in in a future build appears automatically.
  - Minimal fix: add `allow: Vec<String>` to `SessionToolOverrides` (`kheish-types/src/session.rs:411`) and
    apply it as `filter.allowlist` in `effective_tool_surface` (`runtime.rs:2377-2431`).
  - Alternative: a `--builtin-tools none` serve flag that skips `builders.rs:687-695`.
- `CreateSessionRequest` cannot carry `tool_overrides`, so it is two calls. `/v1/stacks/apply` can do it in one document.
- Visibility should be checked before permission. Fix: in `kheish-core/src/engine.rs:1111`
  (`evaluate_tool_permissions`), deny calls whose name is not in the advertised tool set.

## 2. Routes and the anthropic driver

### Runtime route API: unusable for Fregate

```http
POST /v1/runtime/routes {"route_id":"fregate-rt","provider":"anthropic","model":"claude-sonnet-4-5",
                         "api_key_secret_ref":"fregate.relay","base_url":"http://127.0.0.1:4101/v1/messages"}
-> 400 "auth slot `fregate.relay` is not a generic secret"
```

- With a generic slot or an inline `api_key`, the route add returns `200`. Every run on that route then fails:
  `400 route_not_ready: route ... expects provider anthropic but auth_ref ... is generic`.
- `base_url` is **silently ignored**: `AddModelRouteRequest` (`api/types.rs:4308`) has no such field and no
  `deny_unknown_fields`. `build_inline_api_key_route` (`model_routing.rs:1227`) uses
  `https://api.anthropic.com/v1/messages`.
- The key value is copied into the route at add time (`state/runtime.rs:373-426`), so rotating it later has no effect.
- Minimal fix:
  - Accept `base_url` and `auth_ref` (a typed provider slot) in `AddModelRouteRequest`.
  - Build the route through the same brokered path as the routes file (`resolve_configured_anthropic_route`, `support.rs:~1400-1471`).
  - Persist both in `RouteOverlayEntry`.

### Routes file: works

```toml
version = 1
default_route = "fregate"
[routes.fregate]
driver = "anthropic"
default_model = "claude-sonnet-4-5"
auth_ref = "fregate.relay"
base_url = "http://127.0.0.1:4101/v1/messages"   # full endpoint URL, posted to as-is
```

- The slot must exist **before boot** ("route references missing auth_ref" aborts `serve`). Seed it with
  `kheish-daemon secrets set fregate.relay --offline --state-root S --provider anthropic --from-env RELAY_KEY`.
- Rotating the key via the API takes effect on the next request, with no restart:

```http
POST /v1/runtime/secrets {"slot_id":"fregate.relay","provider":"anthropic","mode":"api_key",
  "state":{"kind":"api_key","api_key":"relay-token-mission-B"},"updated_at_ms":1794004700000}
-> 200 {"slot_id":"fregate.relay","provider":"anthropic","mode":"api_key","summary":"api_key",...}
```

- The next model request carries `x-api-key: relay-token-mission-B`.

### What the driver sends (captured)

```
POST /v1/messages
x-api-key: <slot value>          anthropic-version: 2023-06-01
content-type: application/json   accept: */*      (no Authorization, no anthropic-beta, no user-agent)
{"model":"claude-sonnet-4-5","max_tokens":8000,"stream":true,
 "tool_choice":{"type":"auto","disable_parallel_tool_use":false},
 "system":"<string, ~5 KB>","messages":[{"role":"user","content":[{"type":"text","text":"Say hello"}]}],
 "tools":[{name, description, input_schema} ...]}
```

- There is no `thinking` field unless the run asks for it:
  - `generation.reasoning.effort=medium` on `claude-sonnet-4-5` sends `"thinking":{"type":"enabled","budget_tokens":4096}`.
  - `effort=high` on `claude-opus-4-7` sends `"thinking":{"type":"adaptive"},"output_config":{"effort":"high"}`.
- Run `metadata` is **not** forwarded to the provider.
- The model can be overridden per run (`generation.model`), and the route accepted `claude-opus-4-7`.

### Error behaviour (stub returns an Anthropic-style `{"type":"error","error":{"type":..,"message":..}}`)

Daemon retry policy (`builders.rs:61`): `max_attempts=2`, `base_backoff_ms=1000`, `Retry-After` honoured with **no cap**.

| Stub status | Attempts (time offset) | Final run | `RunView.error` |
|---|---|---|---|
| 402 billing_error | 1 | `failed` | `Anthropic request failed with status 402: type=billing_error` |
| 403 permission_error | 1 | `failed` | `... status 403: type=permission_error` |
| 400 | 1 | `failed` | `... status 400: type=invalid_request_error` |
| 401 | 2 (immediate forced auth refresh) | `failed` | `... status 401: type=authentication_error` |
| 429 | 2 (+0s, +1.0s) | `failed` | `... status 429: type=rate_limit_error` |
| 429 + `retry-after: 4` | 2 (+0s, +4.0s) | `failed` | same |
| 529 | 2 (+0s, +1.0s) | `failed` | `... status 529: type=overloaded_error` |
| 500 | 2 (+0s, +1.0s) | `failed` | `... status 500: type=api_error` |

- The upstream `error.message` is dropped (sanitised). Only the status and `error.type` survive.
- The session stays usable after a failure.
- For a terminal relay answer with no retry, use **402 `billing_error`** (or 403). `generation.fallback_model`
  only switches models on 429/5xx.

## 3. MCP credentials: per server only (gap)

```http
POST /v1/runtime/secrets {"slot_id":"mcp.fregate.token","provider":"generic","mode":"opaque_secret",
                          "state":{"value":"mcp-token-AAA"},"updated_at_ms":1794004389042}   -> 200
POST /v1/runtime/mcp/servers {"name":"fregate","url":"http://127.0.0.1:4102/mcp",
  "bearer_token_secret_ref":"mcp.fregate.token","http_header_secret_refs":{"X-Fregate-Tenant":"mcp.fregate.token"},
  "headers":{"X-Static":"s1"},"tool_timeout_sec":120}
-> 200 RuntimeSettingsView; mcp.servers[0] = {"server":"fregate","source":"runtime_api","connected":true,
   "credential_secret_refs":["mcp.fregate.token"],"tools":["mcp__fregate__scan_target"]}
```

The stub received the following `tools/call` from two different sessions (`mission-m1` and `mission-m2`):

```
authorization: Bearer mcp-token-AAA   x-fregate-tenant: mcp-token-AAA   x-static: s1
mcp-session-id: fregate-mcp-fef78aa5  (identical for both sessions)   mcp-protocol-version: 2025-06-18
body: {"method":"tools/call","params":{"name":"scan_target","arguments":{"target":"example.com"},"_meta":{"progressToken":1}}}
```

- The credential is the same for every session, and so is the MCP session.
- No session id, run id, tool_call_id or run metadata is sent. The Fregate MCP server **cannot tell which mission is calling**.
- `CredentialScope.mcp_server_allow/deny` only gates whether the server resolves for a session. It cannot select a slot.

Code:
- `McpClient` holds one `service: Mutex<Option<ServiceHandle>>` per server (`kheish-mcp/src/client.rs:829-837`).
- The static bearer and headers are resolved once at connect (`client.rs:735-740`, `936-985`).
- `call_tool` sends only the name and arguments (`client.rs:811-827`, `manager.rs:819-863`).
- The OAuth path re-resolves per execution scope, but it reconnects the *shared* service whenever the
  fingerprint changes (`client.rs:839-870`). Concurrent sessions with different tokens would race.
  This is not a per-session mechanism.

Minimal change, two options:
- **A (S, recommended first)**: in `McpManager::call_tool` (`manager.rs:819`), read
  `kheish_runtime::current_execution_scope()` (it already carries session_id, run_id, agent_id and
  tool_call_id, see `execution.rs:22`). Pass `_meta {"kheish/session_id", "kheish/run_id", "kheish/tool_call_id", ...}`
  through `client.call_tool` (rmcp `CallToolRequestParams::with_meta`). Fregate then authorises per call against
  the session ids it created. About 50-80 lines plus tests.
- **B (M, 2-4 days)**: true per-session credentials.
  - Add `mcp_credential_overrides: {server: slot}` to `CredentialScope` (types, normalisation, `restrict_with`, validation).
  - Turn `McpClient`'s single service into a pool keyed by resolved-credential fingerprint, so each session gets
    its own rmcp service and `Mcp-Session-Id`.
  - Resolve headers from the scope's slot in `resolve_http_headers`, and track `ensure_static_secret_refs_current` per slot.

Bug found:
- After rotating a static MCP slot, the daemon logs "updated MCP servers after auth secret change" and marks the server disconnected.
- The tool stays advertised to the model, and calls return "MCP tool ... is not available in this session".
- `DELETE /v1/runtime/mcp/servers/fregate` then returns 200, but re-adding returns
  `502 "MCP tool name mcp__fregate__scan_target collides with an existing tool"`. Only a daemon restart recovers.
- Cause: `shutdown_servers_referencing_secret_ref` clears `server.tools` (`manager.rs:564-567`), so
  `remove_server` (`manager.rs:689-701`) retires nothing from `self.tools` or the `ToolRuntime`.
- Fix: retire by `tool.server_name == name` from `self.tools`.

## 4. Projects, sessions, scopes, metadata

```http
POST /v1/personas {"persona_id":"kheish-fregate","display_name":"Kheish (Fregate)",
  "soul":"You are Kheish, the exposure analyst inside Fregate. Use only the Fregate tools.",
  "capability_scope":{"mcp_server_allow":["fregate"],"skill_deny":["*"]},"metadata":{"fregate_workspace_id":"ws-1"}}
-> 201 {..., "version":1, "capability_scope":{"skill_deny":["*"],"mcp_server_allow":["fregate"]}}

POST /v1/projects {"project_id":"eng-ws1-e42","display_name":"Engagement 42",
  "metadata":{"fregate_workspace_id":"ws-1","fregate_engagement_id":"e42"}}
-> 200 (not 201) {"project_id":"eng-ws1-e42","status":"active","member_count":0,...,"metadata":{...}}

POST /v1/sessions {"session_id":"mission-m1","persona_id":"kheish-fregate",
  "capability_scope":{"mcp_server_allow":["fregate"],"mcp_tool_allow":["mcp__fregate__scan_target"]},
  "credential_scope":{"route_allow":["fregate"],"mcp_server_allow":["fregate"],"connector_deny":["*"]}}
-> 201 SessionView; effective_capability_scope = {"skill_deny":["*"],"mcp_server_allow":["fregate"],
   "mcp_tool_allow":["mcp__fregate__scan_target"]}

PUT /v1/sessions/mission-m1/tool-overrides {...section 1...}                                -> 200
PUT /v1/sessions/mission-m1/permission-mode {"mode":"default"}   (or bypassPermissions)     -> 200
PUT /v1/sessions/mission-m1/route-policy {"route_policy":{"provider":"fregate","generation":{"model":"claude-sonnet-4-5"}}} -> 200
POST /v1/projects/eng-ws1-e42/members {"member_id":"mission-m1","member_kind":"session","display_name":"Mission m1",
  "session_id":"mission-m1","metadata":{"fregate_mission_id":"m1"}}                         -> 200
```

- **Session binding to a project** means project membership (`member_kind:"session"`). Its only runtime effect
  is adding `project:<id>` to the session's learning scopes.
- **`route_allow` enforcement**: a run with `"provider":"fregate-rt"` on that session returns
  `400 route_blocked_by_credential_scope`.
- **Wildcards**: `mcp_tool_allow:["mcp__fregate__*"]` exposes **zero** tools. Only an exact name or `*` matches
  (`capabilities.rs:269`). Use `mcp_server_allow:["fregate"]`.
- **Idempotency**:
  - `POST /v1/sessions` has no `idempotency_key`. Unknown fields (`idempotency_key`, `metadata`, `project_id`)
    are **silently ignored** (201).
  - Re-posting the same `session_id` with the same body returns 201 (reuse). With a different persona it returns
    `409 "already bound to a different persona"`. The caller-chosen `session_id` is the idempotency handle.
  - `POST /v1/sessions/{id}/runs` supports `idempotency_key`, durable across restarts:
    - The same key and body return `202` with the original run (`run-4`, already `completed`).
    - The same key with a different body returns `409 idempotency_conflict`.
    - Keys are scoped per session.
- **Metadata**:
  - Sessions have no metadata field. Encode the mission in `session_id` (for example `fregate-ws1-m42`).
  - Run `metadata` is stored as `RunView.input_metadata`. It comes back in every `run_updated` SSE event and in
    `GET /v1/runs*`.
  - It is **absent** from `trace` events, hook payloads, MCP calls and provider requests.
  - Project, persona and project-member metadata are stored and returned on their own resources.

## 5. Runs and events

`POST /v1/sessions/mission-m1/runs {"idempotency_key":"m1-r-tool","content":"CALL_TOOL on example.com","metadata":{"fregate_mission_id":"m1","correlation_id":"corr-tool-1"}}`
returns `202 {"run_id":"run-4","status":"running","input_metadata":{...},...}`.

Sequence on `GET /v1/events/stream?session_id=mission-m1` (the run stream `?cursor=0` gives the same minus the session frames):

```
run_updated            {"type":"run_updated","run":{"run_id":"run-4","status":"running",...,"input_metadata":{"correlation_id":"corr-tool-1","fregate_mission_id":"m1"}}}
session_state_changed  {"type":"session_state_changed","session_id":"mission-m1","agent_id":"agent-2","status":"running","pending_approvals":0,"pending_questions":0}
trace model_attempt_started {"turn":1,"attempt":1}
trace external_action  request/response kind=model_provider target="anthropic:http://127.0.0.1:4101" outcome "200"
trace model_attempt_finished
trace permission_evaluated {"tool_name":"mcp__fregate__scan_target","decision":"allow"}
trace tool_started     {"type":"trace","trace":{"timestamp_ms":1794004516675,"session_id":"mission-m1","agent_id":"agent-2","run_id":"run-4","principal_id":"agent:agent-2","tool_call_id":"toolu_ca1c624cd5","kind":{"type":"tool_started","tool_name":"mcp__fregate__scan_target","call_id":"toolu_ca1c624cd5"}}}
trace external_action  kind=tool / kind=mcp target="mcp:fregate/scan_target" (request digest, response digest, outcome "ok")
trace tool_finished    {"type":"trace","trace":{...,"tool_call_id":"toolu_ca1c624cd5","kind":{"type":"tool_finished","tool_name":"mcp__fregate__scan_target","call_id":"toolu_ca1c624cd5","is_error":false}}}
trace model_attempt_started/finished (turn 2), session_persisted, external_action output_dispatch target="daemon:address_sha256:..."
run_updated (running)
output                 {"type":"output","output":{"session_id":"mission-m1","run_id":"run-4","content":"FINAL after tool: ...","parts":[{"type":"text",...}],"source_kind":"assistant_text","plugin":"daemon","address":"mission-m1"}}
trace output_dispatched {"plugin":"daemon"}
run_updated            {"run":{"run_id":"run-4","status":"completed","finished_at_ms":1794004516735,...}}
session_state_changed  {"status":"idle",...}
session_snapshot       {...}
```

- Trace frames carry **no** tool input or output, only names, ids, digests and `is_error`.
  Persisted lifecycle: `GET /v1/runs/{id}/events` returns `accepted, started, [waiting_for_approval, approval_resolved, started], output, completed`.
- SSE ids are decimal strings seeded from a per-boot epoch (for example `7705190248354938992`).
- **Reconnect**:
  - `GET /v1/runs/run-36/stream` with `Last-Event-ID: <6th id>` replayed 19 frames, all with id greater than the cursor.
  - Without a cursor the stream is live-only (a finished run gives nothing). Use `?cursor=0` to replay.
  - A cursor from before a restart returns
    `stream_gap {"skipped":...,"resume_after_id":"...","reason":"restart_epoch","scope":"session","skipped_is_estimate":true}`.
- **Cancel**: with the tool sleeping 15 s, `POST /v1/runs/run-37/cancel` returns `200` with `status:"cancelled"` in 0.03 s.
  - Trace `tool_finished is_error:true`, then `run_updated cancelled`. The session goes back to idle. A second cancel returns 200, unchanged.
  - The MCP server received **no** `notifications/cancelled`. The HTTP request is just dropped.
- **SIGKILL mid tool call** (tool sleeping 6 s, `kill -9`, restart):
  - The run becomes `interrupted` with `error:"daemon restarted while the run was active"`.
    Persisted events: `accepted, started, interrupted`.
  - It is **not resumed**, and the tool is **not re-executed** (MCP stub saw 1 call, which completed on its side).
  - The session is `idle`. The next run's prompt contains the orphaned `tool_use` with a synthesised
    `tool_result` `{"error":"tool result missing during provider prompt normalization"}`.
  - The runtime-added MCP server reconnected at boot from `mcp-overlay.json`.
  - A caller sees: the SSE connection drops, then `stream_gap reason=restart_epoch` on reconnect, then `GET /v1/runs/{id}` returns `interrupted`.
  - Fregate must reconcile side effects itself, and today it cannot correlate them because no tool_call_id reaches MCP (section 3).

## 6. Approvals via hooks

- **Where hooks are configured**: daemon-wide only, through `POST /v1/runtime/hooks` (persisted in the state root,
  revisioned). There is no per-session or per-project hook map, so per-session policy lives in the hook endpoint,
  keyed by `session_id`. `matcher` supports `*` globs.
- **HTTP hooks cannot reach Fregate's internal endpoint**:

```http
POST /v1/runtime/hooks {"settings":{"hooks":{"permission_request":[{"name":"fregate-approval",
  "failure_policy":{"mode":"closed"},"executor":{"type":"http","url":"<U>","timeout_ms":5000}}]}}}
U=http://127.0.0.1:4103/hook  -> 400 "HTTP hook URL must not target private or local addresses"
U=http://localhost:4103/hook  -> 400 "... must not target localhost"
U=http://10.0.0.5/hook, http://172.20.0.3:8080/hook -> 400 (private)
U=http://fregate-api.internal/hook -> 200 at config time; at run time DNS is resolved and checked, and resolution failed:
  the tool was denied (fail-closed) and a dead-letter was written.
```

- HTTP hooks also cannot send an auth header (only `Idempotency-Key`, `hooks.rs:2208-2214`).
- Gap, minimal fix:
  - Add an operator allow-list (for example `--hook-http-allow-cidr` / `KHEISH_HOOK_HTTP_ALLOWED_HOSTS`) checked in
    `validate_hook_http_target_static` (`hooks.rs:1724-1751`) and in the resolve-time `is_blocked_hook_ip` check (`hooks.rs:~2326`).
  - Add `bearer_token_secret_ref` / `header_secret_refs` to `HookExecutorConfig::Http`
    (`kheish-types/src/hooks.rs:140`), resolved from the auth store in `execute_http_hook_request`.
- **Measured with a `command` executor instead** (same JSON on stdin):
  `{"type":"command","command":"curl -sS --fail-with-body --max-time 5 -H Content-Type:application/json --data-binary @- http://127.0.0.1:4103/hook","timeout_ms":8000}`.

Exact body received for `permission_request` (it only fires when the base decision is `ask`; `mcp__*` is `ask` in
`default` mode and `allow` in `bypassPermissions`):

```json
{"contract_version":1,"event":"permission_request","invocation_key":"hook-invocation-ed2b1f377c7e89f0d688cb4b5ab1b30c",
 "session_id":"mission-m3","subject":"mcp__fregate__scan_target",
 "payload":{"approval_request":{"id":"approval-mcp__fregate__scan_target-2-1c5bc7ef","input":{"target":"example.com"},
   "reason":"MCP tool calls can affect external systems and require approval","scope":"session",
   "tool_call_id":"toolu_e1f7cf7a73","tool_name":"mcp__fregate__scan_target"},
  "tool_call":{"id":"toolu_e1f7cf7a73","name":"mcp__fregate__scan_target","input":{"target":"example.com"},
   "assistant_message_id":"msg_314b95d7f33e","assistant_provider_response_id":"msg_314b95d7f33e"}}}
```

- The body has the full arguments and the session id. It has **no `run_id`, no `agent_id`, and no run metadata**.
- `pre_tool_use` does carry `run_id` and `agent_id`, plus the tool descriptor.
- Minor gap: add run_id to the `PermissionRequest` dispatch (`kheish-core/src/engine.rs:1128`).

Responses (stdout of the command, or the HTTP body):

| Hook returns | Effect (verified) |
|---|---|
| `{"contract_version":1,"permission":"allow"}` | tool runs, run completes |
| `{"contract_version":1,"permission":"deny","stop_reason":"denied by fregate policy"}` | tool is not called. The model gets `{"error":"denied by fregate policy","permission_denied":true,...}` and the run **completes** (not failed) |
| `{"contract_version":1,"permission":"ask"}` (or empty `{}`) | run goes to `waiting_for_approval` |
| also accepted | `{"decision":"approve"|"block"}`, `{"continue_execution":false}`, `updated_input`, `additional_contexts` |

"ask" surfaces as follows:
- `run_updated` with `status:"waiting_for_approval"`, `pending_approval_ids` and `pending_approvals[]` (full input).
- `session_state_changed` with `status:"waiting_for_approval","pending_approvals":1`.
- `GET /v1/runs/{id}`, `GET /v1/runs?session_id=..&priority_active=true` and `GET /v1/sessions/{id}`
  (`snapshot.pending_approvals`).
- The approval id is the same one the hook saw, so Fregate can key on it.

Resolve:

```http
POST /v1/runs/run-33/approvals {"idempotency_key":"fregate-appr-777","resolutions":[{"request_id":
  "approval-mcp__fregate__scan_target-4-f1168ed9","behavior":"allow","justification":"approved by analyst alice in Fregate"}]}
-> 202 {"run_id":"run-33","kind":"approval_resume","status":"running",...}
```

- The run completes, the tool is called once, and the persisted event is
  `approval_resolved {"resolutions":[{"request_id":...,"behavior":"allow","justification":...}]}`.
- Re-using the key with a different body returns `409 idempotency_conflict`.
- `behavior:"deny"` with `reason` is also supported.

Fail-closed (verified, `failure_policy.mode:"closed"`):

| Case | Result |
|---|---|
| hook endpoint returns 500 | tool denied: `"hook fregate-approval failed after 1 attempt(s): ... returned error: 500"` |
| hook endpoint down (port closed, `max_retries:1`) | tool denied after 2 attempts |
| HTTP hook DNS failure | tool denied |

- In every case the MCP stub saw 0 calls and the run completed with the denial as a tool error.
- Dead letters go to `GET /v1/runtime/hooks/dead-letter`, with counters under `GET /v1/status` → `hooks`.
- Note: the hook error text reaches the model.

## 7. Limits

| Limit | Exists? | Scope | Where |
|---|---|---|---|
| `KHEISH_AGENT_MAX_TURNS` | yes (default 500, `0` = unbounded) | **daemon-wide**, per run (turn counter starts at 1 each run) | `builders.rs:513-540`, `engine.rs:701-707,1108` |
| `KHEISH_MODEL_BUDGET_MAX_TOTAL_OUTPUT_TOKENS` (1,000,000) / `_COST_USD` (500) | yes | **per route, cumulative for the process lifetime** (resets on restart). When exceeded, every run on that route fails | `model.rs:530-540`, `builders.rs:79-84,833` |
| Session goal `token_budget` | yes | goal-continuation loop only (`BudgetLimited`), not a hard stop | `state/goal_workflow.rs`, `kheish-types/session.rs:224` |
| Per-run wall clock | **no** | only the model attempt `stream_timeout_ms=240000`, `inactivity_timeout_ms=90000`, and tool timeouts (`tool_timeout_sec`, `/v1/runtime/tool-limits max_timeout_ms`) | `builders.rs:61-68` |

- Verified: restarting with `KHEISH_AGENT_MAX_TURNS=1` and running `CALL_TOOL` executes the tool, then the run
  ends `failed` with `"agent loop exceeded max_turns=1"`.
- Minimal change for caller-set limits on `POST /v1/sessions/{id}/runs` (`SubmitRunRequest`, `api/types.rs:2291`):
  - `max_turns: Option<usize>`:
    - Carry it into the input envelope metadata and then into `RunMetaSnapshot` (`build_run_meta`, `engine.rs:502`).
    - In `drive_loop` (`engine.rs:701-707`) use `min(policy.max_turns, run_meta.max_turns)`.
    - The existing bail at `engine.rs:1108` produces a clean `failed`.
  - `max_duration_ms: Option<u64>`:
    - Persist it on the run record.
    - In `execute_run` (`state/run_workflow.rs:2060`) spawn a timer that calls the existing
      `cancel_run_with_error(run_id, Some("run exceeded max_duration_ms"))` (`run_workflow.rs:3048`) if the run is
      still non-terminal. Decide whether `waiting_for_approval` time counts.
  - Both are about S.

## 8. Learning plane

- What gets written automatically:
  - Default policy is `mode:"shadow"`, `capture.run_summary_candidates:true`.
  - Every run writes one `run_summary` learning candidate, scoped `session:<id>`, `sensitivity:"sensitive"`,
    `state:"pending"`, `manual_review`. Nothing is published automatically. `GET /v1/learnings` is `[]`.
  - Run memory (`recovered_memory`) is recorded per session and injected only into the same session's prompt
    (`search_visibility:"session_only"`).
- Disabling capture works:

```http
POST /v1/runtime/learning-policy {"policy":{"mode":"manual_only","capture":{"run_summary_candidates":false,
  "semantic_candidates":{"enabled":false}},"publication":{"default_action":"manual_review",
  "allow_api_origin_active_publication":false},"judge":{"enabled":false}}}   -> 200
```

- After that, a run added **0** candidates (37 before, 37 after). `POST /v1/runtime/run-memory-policy {"policy":{"enabled":false,...}}` also works.
- **Workspace scope cannot be disabled**:
  - `learning_scopes_for_session` (`state/learning_workflow.rs:41-70`) always appends `workspace:default`.
  - It also appends `persona:<persona_id>` and every `project:<id>` the session is a member of.
  - Observed: `["session:mission-m1","persona:kheish-fregate","project:eng-ws1-e42","workspace:default"]`.
  - Any learning published to workspace scope, or to a shared persona, is read by every Fregate tenant.
    Promoted procedural skills are workspace-only (`procedural_skills.rs:964`). Our persona's `skill_deny:["*"]` hides them.
- Gap, minimal fix: a daemon flag (for example `--learning-workspace-scope off`) checked at `learning_workflow.rs:65-68`,
  plus rejecting workspace-scope publication when it is off.
- Interim:
  - Set `manual_only` with capture off, and never publish.
  - Use one persona per Fregate workspace (`kheish-fregate-<ws>`) so persona scope does not cross tenants.

## 9. State lock and standby

- `cli/state_lock.rs:31` uses `flock(fd, LOCK_EX | LOCK_NB)` on `<state_root>/daemon.lock`, which contains `pid=..\nmechanism=flock`.
- The lock is taken **before** binding the port (`serve.rs:127`).
- A second `serve` on the same state root (port 4109) exits with code 1 after 0.02 s:
  `Error: state root /tmp/kheish-spike-state is already locked by another daemon: Resource temporarily unavailable (os error 11)`.
  Nothing listens on 4109.
- There is **no wait-for-lock option** (no flag or env; non-Unix acquisition is a no-op).
- Minimal change: a `--state-lock-wait <ms|forever>` option that polls `LOCK_NB` with backoff, or runs a blocking
  `flock` in `spawn_blocking`, before the `bail!`. A warm standby on the same host or shared filesystem then takes
  over on the primary's death. flock over NFS is not reliable; use a local or block volume.

## 10. Output fan-out (REMEDIATION P1.2)

- No connectors are configured: `GET /v1/runtime/connectors` returns `[]`.
- Runs with no `reply_targets`, and with an explicit `"reply_targets":[]`, both produced:
  - outputs `[("daemon","mission-m2")]`
  - trace `output_dispatched {"plugin":"daemon"}` with target `daemon:address_sha256:...`
  - `GET /v1/deliveries` returns `[]`, `RunView.deliveries` is null, and `status.delivery` is all zeros.
- Nothing left the process.
- Why: `envelope_reply_targets` (`state/session_ingress.rs:1289-1305`) always appends `daemon:<session_id>`, so
  API-submitted runs never reach the empty-target broadcast in `kheish-runtime/src/runtime.rs:2742` /
  `kheish-output/src/lib.rs:86-98`.
- If that path were reached:
  - The queued `http/external/slack/telegram` plugins (always registered, `connectors/output/mod.rs:61-66`)
    reject an envelope without a reply (`delivery.rs:1416-1419`), so nothing is delivered externally.
  - The resulting error would fail the output dispatch.
- Fregate is safe as long as it never sets session `reply-targets`.

---

## After the changes

Branch `feat/fregate-runtime`, commits K1 to K10 (`b172ab0` .. `bdf91a0`), `cargo build -p kheish-daemon` (debug).
Live check run on 2026-11-07 against a daemon on `127.0.0.1:4100` with its own temp state root
(`/tmp/kheish-live-check/state`), the spike stubs (model `:4101`, MCP `:4102`, hook `:4103`) and a
standby on `127.0.0.1:4105`. The driver is a Python script that records every observation in
`/tmp/kheish-live-check/results.json`.

### Rig

- Started with `env -i`, a **private `HOME`**, and a routes file holding one stub route (`boot`). The daemon
  options: `--learning-read-scopes session,project`, `--model-budget-max-total-output-tokens off`,
  `--model-budget-max-total-cost-usd off`, `--mcp-discovery disabled`, bearer auth, and
  `KHEISH_AUTH_STORE_REQUIRE_ENCRYPTED=true`.
- Slots: `fregate.relay` (anthropic, `api_key`) for the relay route, and `mcp.fregate.callback` (generic) for the
  MCP bearer and the hook header.
- Finding: a first attempt with `--allow-empty-routes`, no routes file, and the real `HOME` resolved
  `anthropic` and `openai` routes from the account credentials under `~/.claude` and `~/.codex`. Its first runs went
  to the real Anthropic API (4 short test prompts, all answered `429`) instead of the stub. A Fregate deployment
  must give the daemon a routes file (or `--allow-empty-routes`) **and** a HOME without provider credentials.
- Finding: MCP `bearer_token_secret_ref` and `http_header_secret_refs` only resolve slots named `mcp.*`; a
  `fregate.callback` slot is rejected (`500` on add). Combined with K2 needing a provider-typed slot, Fregate keeps the
  callback secret in two slots with the same value: an anthropic slot for the relay route and an `mcp.*` generic slot
  for MCP and the hook.

### Results

| Change | What was exercised | Observed |
|---|---|---|
| K1 | `PUT /v1/sessions/mission-m1/tool-overrides {"allow":["mcp__fregate__scan_target"]}`, then `CALL_BASH` in `default` mode | `200`, view `tool_overrides = {"allow":[…]}`. Every model request advertised exactly `["mcp__fregate__scan_target"]`. The run **completed** with no `waiting_for_approval` event and no `bash` permission audit; the tool result was `tool bash is not available for this agent`. A misspelled field (`{"alow":…}`) returns `422`. |
| K2 | `POST /v1/runtime/routes {"route_id":"fregate","provider":"anthropic","model":"claude-sonnet-4-5","auth_ref":"fregate.relay","base_url":"http://127.0.0.1:4101/v1/messages"}` | `200`, route listed with `auth_ref: fregate.relay`. The same body with `base_uri` returns `422`. `routes-overlay.json` holds `auth_ref` and `base_url`, no credential. Runs on the route reached `/v1/messages` on the stub with `x-api-key: relay-token-A` (no `route_not_ready`). |
| K3 | `CALL_TOOL` on `mcp__fregate__scan_target` | `tools/call` params: `"_meta": {"kheish/session_id":"mission-m1","kheish/run_id":"run-2","kheish/tool_call_id":"toolu_41b23f2af7","progressToken":1}`, `arguments: {"target":"example.com"}`, `authorization: Bearer callback-secret-1`. |
| K3 rotation | Rotate `mcp.fregate.callback`, then `DELETE` and re-add the server | After the rotation the server shows `connected: false` with ``credential secret `mcp.fregate.callback` changed or was revoked``. `DELETE` returns `200` and the re-add `200` (it was `502 tool name collides`). The next call sent `Bearer callback-secret-2`. |
| K4 | `permission_request` HTTP hook on `http://127.0.0.1:4103/hook`, fail-closed, `header_secret_refs {"x-fregate-callback":"mcp.fregate.callback"}` | Without `allow_private`: `400 invalid_hook_settings`. With it: `200`. `GET /v1/runtime/hooks` shows the slot name, never the value. The hook received `x-fregate-callback: callback-secret-1` (then `-2` after rotation), `event: permission_request`, `run_id: run-2`, and `run_metadata: {"mission_id":"m1","correlation_id":"corr-1","workspace_id":"ws-1"}`. |
| K5 max_turns | `CALL_TOOL` with `"max_turns": 1` | `failed`, `error_code: max_turns_exceeded`, `error: agent loop exceeded max_turns=1`, `limits: {"max_turns":1}`. |
| K5 max_duration_ms | MCP tool sleeping 6 s, run with `"max_duration_ms": 1500` | `cancelled` after 1.54 s, `error_code: deadline_exceeded`, `error: run exceeded max_duration_ms=1500`, `limits: {"max_duration_ms":1500,"deadline_at_ms":…}`. |
| K6 | `GET /v1/sessions/mission-m1/memory-context` | `learning_scopes: ["session:mission-m1"]`: no `workspace:default` (the session has no persona and no project). |
| K7 | Second daemon on `:4105`, same state root, `--state-lock-wait forever`; then `kill -9` the primary | The standby was not ready while the primary lived and logged `state root is locked by another daemon; waiting as a standby`. It was ready **1.81 s** after the kill and listed `mission-m1`. |
| K8 | `auth/global-slots.json` | `{"version":2,"label":"kheish.auth-store",…}`; no secret in clear. The daemon booted with `KHEISH_AUTH_STORE_REQUIRE_ENCRYPTED=true`. |
| K9 | On the new active daemon: `DELETE /v1/sessions/mission-m1` before and after `POST …/end` | Before: `409 session_not_ended`. After: `200` with `run_ids: run-1 … run-5` and `learning_candidate_ids: learning-candidate-1 … 5` (the run-summary candidates). Then `GET /v1/sessions/mission-m1` and `GET /v1/runs/run-1 … run-5` return `404`. No file is left under `sessions/`, `runs/`, `run-events/`, `run-memories/` or `outputs/`, and no purge plan remains. `GET /v1/agents/audit` shows `session_purged` (reason `retention`), then `removed`. The 6 signed external-action ledger files are kept. |
| K10 | Both budgets `off` | Accepted at boot; no budget error over the whole check. |

### Tests and checks

- `cargo fmt --all -- --check` is clean. `cargo build --workspace --locked` passes with no warning.
- Lib tests: kheish-types 55, kheish-core 66, kheish-runtime 312, kheish-coding-tools 26 and kheish-auth 44 all pass.
  kheish-daemon passes 1432 of 1434, and its binary tests pass 241 of 241. The two daemon failures are pre-existing:
  - `daemon_stop_task_kills_orphaned_pipe_holder_and_is_session_scoped` also fails on `844f080` in this environment.
  - `daemon_http_runs_reject_non_image_edit_assets` is a load flake already listed in `REMEDIATION.md`; it passes alone.
- kheish-mcp passes 59 of 60. `mcp_tool_schema_import_preserves_closed_nested_shapes` fails identically on `844f080`.
- Clippy (stable, the 7 touched crates, `--lib --bins --tests`) reports no warning on any line these commits changed.

### After merging `feat/fregate-runtime-provider`

Rerun on 2026-11-07 against the merged build (`cargo build --workspace --locked`), same rig. The driver itself also
ran under `env -i` with a private `HOME`. Every K1 to K9 observation matches the tables above except the two K5 rows:
run limits now use the same `RunErrorCode` as the model conditions, so `error` starts with the code and the
persisted terminal run event carries it.

| Change | Observed |
|---|---|
| K5 max_turns | `failed`, `error_code: max_turns_exceeded`, `error: max_turns_exceeded: agent loop exceeded max_turns=1`, `limits: {"max_turns":1}`. Last run event: `{"type":"failed","error":"max_turns_exceeded: …","code":"max_turns_exceeded"}`. |
| K5 max_duration_ms | `cancelled` after 1.53 s, `error_code: deadline_exceeded`, `error: deadline_exceeded: run exceeded max_duration_ms=1500`. Last run event: `{"type":"cancelled","code":"deadline_exceeded"}`. |

The provider check (`scripts/fregate-provider-check/run.sh`, daemon on `:4101`) passed 13 of 13 on the same build.

Tests on the merged tree: kheish-types 63, kheish-core 70, kheish-runtime 340, kheish-coding-tools 26 and
kheish-auth 44 pass; kheish-daemon passes 1435 of 1436 and its binary tests 242 of 242; kheish-mcp passes 59 of 60.
The two failures are the pre-existing ones above, with the same assertion as on `844f080`.
