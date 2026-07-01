# Linear GitHub Feature Loop

This stack demonstrates scheduled Flow starts for a Linear-to-GitHub feature workflow.

Prerequisites:

- the daemon is already started with Linear and GitHub MCP servers available; this example expects Linear from the built-in `planning` profile/catalog entry and GitHub from explicit Codex-compatible MCP config;
- the Stack declares the required server and tool names under `spec.requires.mcp`, so `plan`, `apply`, and `verify` fail closed if the active daemon does not expose them;
- `mcp.linear.LINEAR_API_KEY` and `mcp.github.GITHUB_PERSONAL_ACCESS_TOKEN` were written to the daemon secret store before startup, and `LINEAR_API_KEY` plus `GITHUB_PERSONAL_ACCESS_TOKEN` are present in the daemon environment when using `--allow-secret-env`;
- the actual MCP tool ids match `spec.requires.mcp.tools` and the persona/session allow-lists in `Kheishfile.yaml`; adjust them after checking `kheish-daemon runtime get`;
- the `openai` route is configured for `gpt-5.5`.

Operational guardrails:

- in the default permission mode, scheduled runs can pause on approval when they reach GitHub or Linear write tools; configure an explicit approval workflow, permission mode, or hook policy before treating the stack as unattended;
- do not run this with broad GitHub or Linear credentials. Use tokens scoped to the intended repository/project/team and validate the first run on a non-production or read-only target before allowing PR/comment/status mutations.

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
