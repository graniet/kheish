---
description: Create and manage webhook subscriptions for event-driven agent activation.
  Use when the user wants external services to trigger agent runs automatically.
version: 1.0.0
metadata:
  catalog:
    tags:
    - webhook
    - events
    - automation
    - integrations
---

## Kheish Compatibility

This skill is repo-local and stays inactive until explicitly activated.

When the original instructions refer to legacy tool names, use these Kheish mappings:

- `terminal` => `bash`
- `web_extract` => `web_fetch`, plus `web_search` when discovery is needed
- `search_files` => `grep_search` and `glob_search`
- `browser_*` tools require a browser-capable surfaced tool or MCP; if none is available, use the closest available surface and say so explicitly

When the instructions mention local helper files, resolve them from `${KHEISH_SKILL_DIR}`.

Kheish does not currently ship a dedicated webhook subscription CLI. In the examples below,
`webhook-manager` and `gateway-manager` are placeholders for your deployment tooling around
daemon connector config, ingress routing, and lifecycle management.

# Webhook Subscriptions

Create dynamic webhook subscriptions so external services (GitHub, GitLab, Stripe, CI/CD, IoT sensors, monitoring tools) can trigger Kheish agent runs by POSTing events to a URL.

## Setup (Required First)

The webhook platform must be enabled before subscriptions can be created. Check with:
```bash
webhook-manager list
```

If it says "Webhook platform is not enabled", set it up:

### Option 1: Setup wizard
```bash
gateway-manager setup
```
Follow the prompts to enable webhooks, set the port, and set a global HMAC secret.

### Option 2: Manual config
Add to `~/.kheish/config.yaml`:
```yaml
platforms:
  webhook:
    enabled: true
    extra:
      host: "0.0.0.0"
      port: 8644
      secret: "generate-a-strong-secret-here"
```

### Option 3: Environment variables
Add to `~/.kheish/.env`:
```bash
WEBHOOK_ENABLED=true
WEBHOOK_PORT=8644
WEBHOOK_SECRET=generate-a-strong-secret-here
```

After configuration, start (or restart) the gateway:
```bash
gateway-manager run
# Or if using systemd:
systemctl --user restart kheish-daemon
```

Verify it's running:
```bash
curl http://localhost:8644/health
```

## Commands

All management is via the `webhook-manager` CLI command:

### Create a subscription
```bash
webhook-manager subscribe <name> \
  --prompt "Prompt template with {payload.fields}" \
  --events "event1,event2" \
  --description "What this does" \
  --skills "skill1,skill2" \
  --deliver telegram \
  --deliver-chat-id "12345" \
  --secret "optional-custom-secret"
```

Returns the webhook URL and HMAC secret. The user configures their service to POST to that URL.

### List subscriptions
```bash
webhook-manager list
```

### Remove a subscription
```bash
webhook-manager remove <name>
```

### Test a subscription
```bash
webhook-manager test <name>
webhook-manager test <name> --payload '{"key": "value"}'
```

## Prompt Templates

Prompts support `{dot.notation}` for accessing nested payload fields:

- `{issue.title}` — GitHub issue title
- `{pull_request.user.login}` — PR author
- `{data.object.amount}` — Stripe payment amount
- `{sensor.temperature}` — IoT sensor reading

If no prompt is specified, the full JSON payload is dumped into the agent prompt.

## Common Patterns

### GitHub: new issues
```bash
webhook-manager subscribe github-issues \
  --events "issues" \
  --prompt "New GitHub issue #{issue.number}: {issue.title}\n\nAction: {action}\nAuthor: {issue.user.login}\nBody:\n{issue.body}\n\nPlease triage this issue." \
  --deliver telegram \
  --deliver-chat-id "-100123456789"
```

Then in GitHub repo Settings → Webhooks → Add webhook:
- Payload URL: the returned webhook_url
- Content type: application/json
- Secret: the returned secret
- Events: "Issues"

### GitHub: PR reviews
```bash
webhook-manager subscribe github-prs \
  --events "pull_request" \
  --prompt "PR #{pull_request.number} {action}: {pull_request.title}\nBy: {pull_request.user.login}\nBranch: {pull_request.head.ref}\n\n{pull_request.body}" \
  --skills "github-code-review" \
  --deliver github_comment
```

### Stripe: payment events
```bash
webhook-manager subscribe stripe-payments \
  --events "payment_intent.succeeded,payment_intent.payment_failed" \
  --prompt "Payment {data.object.status}: {data.object.amount} cents from {data.object.receipt_email}" \
  --deliver telegram \
  --deliver-chat-id "-100123456789"
```

### CI/CD: build notifications
```bash
webhook-manager subscribe ci-builds \
  --events "pipeline" \
  --prompt "Build {object_attributes.status} on {project.name} branch {object_attributes.ref}\nCommit: {commit.message}" \
  --deliver discord \
  --deliver-chat-id "1234567890"
```

### Generic monitoring alert
```bash
webhook-manager subscribe alerts \
  --prompt "Alert: {alert.name}\nSeverity: {alert.severity}\nMessage: {alert.message}\n\nPlease investigate and suggest remediation." \
  --deliver origin
```

## Security

- Each subscription gets an auto-generated HMAC-SHA256 secret (or provide your own with `--secret`)
- The webhook adapter validates signatures on every incoming POST
- Static routes from config.yaml cannot be overwritten by dynamic subscriptions
- Subscriptions persist to `~/.kheish/webhook_subscriptions.json`

## How It Works

1. `webhook-manager subscribe` writes to `~/.kheish/webhook_subscriptions.json`
2. The webhook adapter hot-reloads this file on each incoming request (mtime-gated, negligible overhead)
3. When a POST arrives matching a route, the adapter formats the prompt and triggers an agent run
4. The agent's response is delivered to the configured target (Telegram, Discord, GitHub comment, etc.)

## Troubleshooting

If webhooks aren't working:

1. **Is the gateway running?** Check with `systemctl --user status kheish-daemon` or `ps aux | grep gateway`
2. **Is the webhook server listening?** `curl http://localhost:8644/health` should return `{"status": "ok"}`
3. **Check gateway logs:** `grep webhook ~/.kheish/logs/gateway.log | tail -20`
4. **Signature mismatch?** Verify the secret in your service matches the one from `webhook-manager list`. GitHub sends `X-Hub-Signature-256`, GitLab sends `X-Gitlab-Token`.
5. **Firewall/NAT?** The webhook URL must be reachable from the service. For local development, use a tunnel (ngrok, cloudflared).
6. **Wrong event type?** Check `--events` filter matches what the service sends. Use `webhook-manager test <name>` to verify the route works.
