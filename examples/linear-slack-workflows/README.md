# Linear and Slack Workflows with Kheish

This guide shows how to run three daily engineering workflows on Kheish:

- daily Linear ticket triage
- a Slack-to-Linear intake concierge
- backlog grooming before planning

The recommended setup is intentionally conservative:

- use Kheish schedules for recurring Linear work
- use the built-in Linear MCP catalog entry through `--mcp-profile planning`
- use the native Slack connector for Slack ingress and output
- use approvals for Linear writes unless you have a very narrow write policy

Do not base this workflow on Slack MCP. In this repository, Slack MCP is a
catalog-only entry, while the Slack connector is the supported ingress/output
surface. Do not assume Linear has a native daemon trigger either; use a Kheish
schedule, or feed Linear events through an external webhook/poller that calls a
Kheish HTTP or external connector.

## Prerequisites

- A Linear personal API key.
- At least one model provider key for Kheish, such as `OPENAI_API_KEY` or
  `ANTHROPIC_API_KEY`.
- Optional: a Slack app with Events API callbacks, a bot token, and a signing
  secret if you want the Slack-to-Linear workflow.
- Optional: an external Linear webhook bridge or poller if you want near
  real-time Linear issue intake.

All commands below assume you are at the repository root.

```bash
cargo build --locked -p kheish-daemon
```

Use a dedicated state root and workspace root while setting this up:

```bash
export KHEISH_STATE_ROOT=.kheish-daemon-linear-slack
export KHEISH_WORKSPACE_ROOT=.kheish-workspace-linear-slack
mkdir -p "$KHEISH_WORKSPACE_ROOT"
```

## 1. Store the Linear MCP secret

Linear is a supported built-in MCP entry under the `planning` profile. Store the
Linear token in the daemon secret store before starting the daemon.

```bash
export KHEISH_AUTH_STORE_MASTER_KEY="$(./target/debug/kheish-daemon secrets generate)"
export LINEAR_API_KEY="lin_api_..."

./target/debug/kheish-daemon mcp auth slots linear
./target/debug/kheish-daemon mcp auth set linear \
  --from-env LINEAR_API_KEY \
  --offline \
  --state-root "$KHEISH_STATE_ROOT"
```

Keep the same `KHEISH_AUTH_STORE_MASTER_KEY` when you start the daemon. If you
change it later, the stored Linear secret cannot be decrypted.

## 2. Start the daemon with Linear MCP

Load your model provider key first. For example:

```bash
export OPENAI_API_KEY="sk-..."
```

Then start Kheish:

```bash
./target/debug/kheish-daemon serve \
  --bind 127.0.0.1:4021 \
  --state-root "$KHEISH_STATE_ROOT" \
  --workspace-root "$KHEISH_WORKSPACE_ROOT" \
  --mcp-discovery disabled \
  --mcp-profile planning
```

For any non-loopback or production deployment, add bearer auth and keep the
tokens in files:

```bash
./target/debug/kheish-daemon serve \
  --bind 127.0.0.1:4021 \
  --state-root "$KHEISH_STATE_ROOT" \
  --workspace-root "$KHEISH_WORKSPACE_ROOT" \
  --mcp-discovery disabled \
  --mcp-profile planning \
  --http-auth-mode bearer \
  --http-admin-token-file /run/secrets/kheish-admin-token
```

When bearer auth is enabled, use `--token-file /run/secrets/kheish-admin-token`
on CLI calls or export `KHEISH_DAEMON_TOKEN` in the client shell.

In another terminal, verify the daemon and MCP state:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 status
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 runtime get
```

Client commands do not need `KHEISH_AUTH_STORE_MASTER_KEY`. The daemon process
needs that key only when it starts or restarts and has to decrypt stored secrets.

In `runtime get`, the Linear MCP server should be connected and should use
credentials from the daemon secret store. If Linear reports `Auth required`,
check that:

- `mcp.linear.LINEAR_API_KEY` was written to the same `--state-root`
- the daemon was started with the same `KHEISH_AUTH_STORE_MASTER_KEY`
- the token is valid in Linear

## 3. Keep writes approval-gated first

Kheish defaults to asking for approval on MCP tools. Keep that behavior while
you prove the workflow:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  runtime set-permission-mode default
```

In `default` mode, Linear MCP calls can pause for approval, including reads.
That is intentionally conservative for the first rollout. After you confirm the
exact Linear tool names exposed by your daemon, you can add a narrower policy
for safe read-only Linear calls and keep mutations approval-gated.

## 4. Workflow A: daily Linear ticket triage

Create a durable session for triage:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  sessions create linear-triage \
  --capability-scope-json '{"mcp_server_allow":["linear"]}' \
  --credential-scope-json '{"mcp_server_allow":["linear"]}'
```

Create the triage prompt:

```bash
mkdir -p .kheish-prompts
cat > .kheish-prompts/linear-triage.md <<'EOF'
Use Linear through the available MCP tools.

Triage new and recently updated issues for the engineering team.

Do:
- if the target team is ambiguous, ask a structured question before scanning
- for the first rollout, inspect at most 20 active issues; report when more
  issues exist instead of paginating through the whole workspace
- group issues by team, impact, urgency, and likely owner
- detect duplicates and link related issues when the Linear tool surface supports it
- flag incomplete issues and ask structured questions instead of guessing
- propose priority, labels, owner, and next state changes as a diff first
- only mutate Linear after approval or an explicitly allowed write policy

Return:
- a sorted triage queue
- issue ids that need clarification
- proposed Linear changes
- changes applied after approval
- risks or blockers that need human attention
EOF
```

Schedule it for workdays at 09:00. Adjust the timezone and cron expression for
your team.

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  schedules create linear-triage-daily linear-triage \
  --content-file .kheish-prompts/linear-triage.md \
  --cron "0 0 9 * * 1-5" \
  --timezone Europe/Paris \
  --overlap-policy skip \
  --misfire-policy coalesce-once
```

Trigger it once manually:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 schedules list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  schedules trigger-now <schedule_id>
```

Observe the run:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  runs list --session-id linear-triage
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  approvals list --session-id linear-triage
```

Approve only the specific Linear request you want. Do not use `allow-all` until
you have inspected the whole pending approval wave.

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  approvals list --session-id linear-triage
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  approvals show <request_id> --session-id linear-triage
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  approvals allow linear-triage <request_id> --justification "approved triage update"
```

## 5. Optional real-time Linear intake

Use this only if you have a Linear webhook bridge or poller. Kheish does not
provide a native Linear webhook trigger in this repository.

The better pattern is:

1. Linear webhook or poller receives an issue event.
2. Your bridge converts the event into the Kheish HTTP connector payload.
3. The HTTP connector creates or reuses a durable Kheish session.
4. The agent reads and updates Linear through MCP with approvals.

Create an HTTP connector for normalized Linear events:

```bash
export LINEAR_TO_KHEISH_TOKEN="replace-with-random-token"

./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  secrets set connectors.http.linear-events.bearer_token \
  --provider generic \
  --from-env LINEAR_TO_KHEISH_TOKEN

cat > .kheish-prompts/linear-http-connector.json <<'JSON'
{
  "actor_id": "linear-webhook",
  "bearer_token": {
    "secret_ref": "connectors.http.linear-events.bearer_token"
  },
  "require_idempotency_key": true,
  "ingress_events_per_second": 5,
  "session_policy": {
    "create_if_missing": true,
    "capability_scope": {
      "mcp_server_allow": ["linear"]
    },
    "credential_scope": {
      "mcp_server_allow": ["linear"]
    }
  }
}
JSON

./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  connectors put-http linear-events \
  --file .kheish-prompts/linear-http-connector.json
```

Your bridge should call Kheish with a normalized payload like this:

```bash
curl -sS -X POST http://127.0.0.1:4021/v1/connectors/http/linear-events \
  -H "Authorization: Bearer $LINEAR_TO_KHEISH_TOKEN" \
  -H "Content-Type: application/json" \
  -d '{
    "binding_keys": ["linear:issue:ENG-123"],
    "content": "Linear issue ENG-123 changed. Inspect it, triage it, and propose a diff before any mutation.",
    "metadata": {
      "linear_issue_id": "ENG-123"
    },
    "idempotency_key": "linear-issue-ENG-123-updated-1700000000"
  }'
```

For production, prefer HMAC signing on the HTTP connector or an external
connector sidecar with its own platform verification. Keep the bridge small:
translate events and let Kheish own the session, run, approvals, and audit trail.

## 6. Workflow B: Slack-to-Linear concierge

Use the Slack connector, not Slack MCP. Slack MCP is catalog-only in the current
Kheish catalog; the Slack connector is the supported ingress/output path.

Set the Slack secrets in your current shell, store them as generic daemon
secrets, then point the connector at those secret refs:

```bash
export SLACK_BOT_TOKEN="xoxb-..."
export SLACK_SIGNING_SECRET="..."

./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  secrets set connectors.slack.linear-concierge.bot_token \
  --provider generic \
  --from-env SLACK_BOT_TOKEN

./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  secrets set connectors.slack.linear-concierge.signing_secret \
  --provider generic \
  --from-env SLACK_SIGNING_SECRET
```

Create a runtime-managed Slack connector:

```bash
cat > .kheish-prompts/slack-linear-concierge-persona.md <<'EOF'
# Slack Linear Concierge

You turn Slack support, bug, and feature-request messages into clean Linear
work. Clarify missing context in the Slack thread, search Linear for duplicates,
propose a ticket create/update diff, and ask for approval before mutating
Linear unless a narrow write policy explicitly allows it. Reply in Slack with
the Linear link, status, and remaining questions. If the requester goes silent,
schedule a follow-up on the same session/thread.
EOF

./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  personas import .kheish-prompts/slack-linear-concierge-persona.md \
  --persona-id slack-linear-concierge

cat > .kheish-prompts/slack-linear-connector.json <<'JSON'
{
  "bot_token": {
    "secret_ref": "connectors.slack.linear-concierge.bot_token"
  },
  "signing_secret": {
    "secret_ref": "connectors.slack.linear-concierge.signing_secret"
  },
  "include_self_output": true,
  "ingress_events_per_second": 5,
  "session_policy": {
    "create_if_missing": true,
    "persona_id": "slack-linear-concierge",
    "capability_scope": {
      "mcp_server_allow": ["linear"]
    },
    "credential_scope": {
      "mcp_server_allow": ["linear"]
    }
  }
}
JSON

./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  connectors put-slack linear-concierge \
  --file .kheish-prompts/slack-linear-connector.json
```

Configure your Slack app Events API request URL to point at:

```text
https://<your-public-kheish-host>/v1/connectors/slack/linear-concierge
```

For local testing, put a tunnel or reverse proxy in front of the daemon. For
production, do not expose the full unauthenticated `/v1` control plane to the
internet. Bind the daemon to loopback or a private network, enable bearer auth
for the control plane, and publish only the connector route through a proxy that
preserves Slack signature headers. Slack signing-secret verification must remain
enabled.

The imported persona above gives connector-created sessions their durable
behavior. Its core policy is:

```text
When a Slack message asks for a bug, feature request, or support follow-up:

1. Clarify missing reproduction steps, expected behavior, impact, and owner in
   the Slack thread.
2. Search Linear for similar issues through MCP.
3. If an issue exists, propose an update.
4. If no issue exists, propose a new Linear issue with title, description,
   labels, priority, and team.
5. Ask for approval before creating or mutating Linear unless a narrow write
   policy explicitly allows it.
6. Reply in Slack with the Linear link, status, and remaining questions.
7. If the requester does not answer, schedule a follow-up on the same session.
```

Verify after a Slack event:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 connectors list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 runs list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 deliveries list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 approvals list
```

## 7. Workflow C: backlog grooming before planning

Create a dedicated session:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  sessions create linear-backlog-grooming \
  --capability-scope-json '{"mcp_server_allow":["linear"]}' \
  --credential-scope-json '{"mcp_server_allow":["linear"]}'
```

Create the grooming prompt:

```bash
cat > .kheish-prompts/linear-backlog-grooming.md <<'EOF'
Use Linear through MCP.

Prepare the engineering planning meeting for the target team.

Do:
- if the target team or planning window is ambiguous, ask a structured question
  before scanning
- inspect current and next cycle/backlog issues for the next planning window
- for the first rollout, inspect at most 25 issues per state or cycle; report
  when more issues exist instead of paginating through the whole workspace
- mark each issue as ready, not ready, blocked, duplicate, or needs split
- summarize blockers and missing decisions
- propose splits for large issues
- identify likely duplicates or related issues
- create a diff of Linear changes before any mutation
- request approval before grouped Linear updates
- if the backlog is large, propose a sidechain split by epic or domain before
  delegating review work
- for verification sidechains, request `agent_type: "verification"` and
  `generation: {"reasoning":{"effort":"xhigh"}}` when the active provider
  supports xhigh reasoning

Return:
- planning digest
- issues ready for planning
- issues needing clarification
- proposed Linear diff
- approved/applied changes
- scope risks
EOF
```

Schedule the run before planning:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  schedules create linear-backlog-weekly linear-backlog-grooming \
  --content-file .kheish-prompts/linear-backlog-grooming.md \
  --cron "0 0 16 * * 4" \
  --timezone Europe/Paris \
  --overlap-policy skip \
  --misfire-policy coalesce-once
```

Trigger and inspect it:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 schedules list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  schedules trigger-now <schedule_id>
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  runs list --session-id linear-backlog-grooming
```

## 8. Operational checks

Before relying on the setup, run these checks:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 doctor
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 runtime get
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 connectors list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 schedules list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 approvals list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 deliveries list
```

For a read-only rollout test, also inspect the daemon event log and confirm no
Linear mutation tool was requested:

```bash
mkdir -p .kheish-evidence
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  sessions events linear-triage \
  --output json > .kheish-evidence/linear-triage-events.json
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 \
  sessions events linear-backlog-grooming \
  --output json > .kheish-evidence/linear-backlog-events.json

rg 'mcp__linear__(save_|create_|delete_|prepare_attachment_upload|create_attachment|create_attachment_from_upload)' \
  .kheish-evidence
```

The final `rg` command should return no matches during a read-only validation.

If a run appears stuck, inspect in this order:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 runs list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 approvals list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 questions list
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 tasks list <session_id>
./target/debug/kheish-daemon --base-url http://127.0.0.1:4021 deliveries dead-letter
```

## Why this is the better setup

- Linear MCP is built into the supported `planning` profile, so use it instead
  of hand-rolled Linear GraphQL calls for the agent-facing workflow.
- Slack is supported as a connector, while Slack MCP is catalog-only, so use the
  Slack connector for message ingress and replies.
- Daily triage and backlog grooming are naturally scheduled work, so use Kheish
  schedules instead of inventing an external cron plus an in-process agent loop.
- Real-time Linear intake is optional because this repository does not provide a
  native Linear trigger; use a small external bridge only when the latency is
  worth the extra operational surface.
- Keep MCP calls approval-gated first. A narrow allow policy can come later
  after the exact Linear tool names and team workflow are proven on a real
  daemon.
