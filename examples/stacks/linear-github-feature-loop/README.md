# Linear GitHub Feature Loop

This stack demonstrates scheduled Flow starts for a Linear-to-GitHub feature workflow.

For a real Linear project or a different GitHub repository, do not apply this example file directly. Copy the stack, replace the project/repository metadata, and follow the operator guide in `docs/operators/linear-github-feature-loop.mdx`.

Prerequisites:

- the daemon is already started with Linear and GitHub MCP servers available; this example expects Linear from the built-in `planning` profile/catalog entry and GitHub from explicit Codex-compatible MCP config;
- the Stack declares the required server and tool names under `spec.requires.mcp`, so `plan`, `apply`, and `verify` fail closed if the active daemon does not expose them;
- `mcp.linear.LINEAR_API_KEY` and `mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN` were written to the daemon secret store before startup, and `LINEAR_API_KEY` plus `GITHUB_PERSONAL_ACCESS_TOKEN` are present in the daemon environment when using `--allow-secret-env`;
- the Telegram output connector named `feature-loop-operator-telegram` exists before `stack apply`, and `reply_targets[0].chat_id` has been replaced with the operator chat id for your deployment;
- the actual MCP tool ids match `spec.requires.mcp.tools` and the persona/session allow-lists in `Kheishfile.yaml`; adjust them after checking `kheish-daemon runtime get`;
- the `openai` route is configured for `gpt-5.5`.

Operational guardrails:

- in the default permission mode, scheduled runs can pause on approval when they reach GitHub or Linear write tools; configure an explicit approval workflow, permission mode, or hook policy before treating the stack as unattended;
- do not run this with broad GitHub or Linear credentials. Use tokens scoped to the intended repository/project/team and validate the first run on a non-production or read-only target before allowing PR/comment/status mutations.
- the workflow creates a draft PR once it has a coherent ticket-scoped patch. The 10/10 internal review gate controls whether the PR can leave draft status; it does not block creation of the durable PR artifact used for recovery and follow-up.
- fresh Linear intake scans open issues in the configured project or team; set `linear_team_key` when the Linear scope is a team rather than a Linear Project. The `Workflow: linear-github-feature-loop` footer is for recovery and deduplication, not a required marker for new work.
- the workflow intentionally processes one ticket or PR per root run. A session goal drives autonomous continuation while safe ticket-scoped work remains; the follow-up schedule is discovery and recovery, not the primary progress loop.
- missing host runtimes such as `php` or `composer` are recorded as a `tests` blocker only after repository-provided Docker Compose, Makefile, package script, CI, or devcontainer test paths have been tried or shown unavailable or unsafe. They do not prevent draft PR creation when the patch is coherent and the blocker does not invalidate it.
- follow-up runs recheck stale `Tests:` blockers before carrying them forward.
- goals stay active while the next action is safe and local to the selected ticket or PR. They are completed only after current tests, a 10/10 internal review, updated GitHub/Linear footers, and terminal subagents; they are paused for durable human or external blockers.
- the model may contact the operator with `notify_operator` or `ask_operator`; Telegram is just the configured reply target, not workflow-specific daemon logic.
- the GitHub follow-up schedule runs at `:15` and `:45` so it does not collide with the daily Linear intake at `08:00` Europe/Paris.
- schedule names include the playbook policy version because the current daemon API treats schedule definitions as create-only. Publish a new schedule name when changing scheduled prompt or playbook semantics.

For credentialed MCP servers, import the preloaded secrets into the stack ledger before the first apply:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4000 stack import \
  --file examples/stacks/linear-github-feature-loop/Kheishfile.yaml \
  --resource secret/mcp.linear.LINEAR_API_KEY \
  --resource secret/mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN \
  --allow-secret-env
```

Then run:

```bash
./target/debug/kheish-daemon --base-url http://127.0.0.1:4000 stack plan \
  --file examples/stacks/linear-github-feature-loop/Kheishfile.yaml \
  --allow-secret-env

./target/debug/kheish-daemon --base-url http://127.0.0.1:4000 stack apply \
  --file examples/stacks/linear-github-feature-loop/Kheishfile.yaml \
  --allow-secret-env

./target/debug/kheish-daemon --base-url http://127.0.0.1:4000 stack verify \
  --file examples/stacks/linear-github-feature-loop/Kheishfile.yaml
```

The stack stays generic: Linear and GitHub are data-plane MCP surfaces. The daemon only reconciles secrets, persona/session scopes, a published Playbook, and schedules that start Flows.
