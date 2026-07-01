# Linear GitHub Feature Loop

This stack demonstrates scheduled Flow starts for a Linear-to-GitHub feature workflow.

Prerequisites:

- the daemon is already started with Linear and GitHub MCP servers available;
- `LINEAR_API_KEY` and `GITHUB_TOKEN` are present in the daemon environment when applying with `--allow-secret-env`;
- the actual MCP tool ids match the allow-list in `Kheishfile.yaml`; adjust them after checking `kheish-daemon runtime get`;
- the `openai` route is configured for `gpt-5.5`.

The stack stays generic: Linear and GitHub are data-plane MCP surfaces. The daemon only reconciles secrets, persona/session scopes, a published Playbook, and schedules that start Flows.
