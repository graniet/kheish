<p align="center">
  <img src="docs/logo.png" alt="Kheish" width="180">
</p>

<h1 align="center">Kheish</h1>

<p align="center">
  <em>Persistent agent runtime and control plane for long-lived, tool-using AI workflows.</em>
</p>

<p align="center">
  <a href="https://kheish.ai"><img src="https://img.shields.io/badge/Website-kheish.ai-ED1C2E?style=for-the-badge&logo=firefoxbrowser&logoColor=white" alt="Website"></a>
  &nbsp;
  <a href="https://docs.kheish.ai"><img src="https://img.shields.io/badge/Docs-docs.kheish.ai-0B0C0E?style=for-the-badge&logo=readthedocs&logoColor=white" alt="Documentation"></a>
</p>

---

## The invariant: agents outlive callers

Kheish enforces **one rule**:

> An agent's execution is durable, scoped, and accountable — independently of any caller, process, or human reachability.

Everything else in this README is a consequence of that single invariant. If a feature does not protect this rule, it does not belong in the daemon.

The rule has three properties, and they are not separable:

- **Caller / execution separation.** The thing that *asks* for an agent run is not the thing that *runs* it. A CLI, a Slack message, a webhook, your code — all of them submit; the daemon executes. The caller can crash, disconnect, log out, get rate-limited, or never come back. The run keeps going.
- **Survives interruptions.** Process restart, machine reboot, network blip, missing human, expired credential, paused approval, deferred question — none of these are exceptions. They are normal points in the run's lifecycle, journaled, replay-safe, and resumed by the daemon when the world is ready again.
- **Governed.** Every action an agent takes happens under a named authority (a session, a sidechain, a brokered lease) with a stated scope and an append-only audit trail. Approvals, leases, scope narrowing, and signed external-action records are not optional features — they are how the rule is upheld.

That is the product. The daemon, the connectors, the capture pipeline, the multi-provider routing, the skills, the schedules — they exist to keep that one rule true under real-world conditions.

## What this addresses

LangChain, AutoGen, CrewAI and vanilla SDK loops are excellent at composing agents *inside an application*. Kheish addresses a different failure mode: **what happens when the application, the human, or the credential boundary disappears while the run is still alive.**

In the in-process model, the agent and the caller share a fate:

- the agent lives inside the caller — caller dies, run dies
- state is in process memory — reboot is data loss
- the agent runs with the caller's credentials — every script holds long-lived secrets
- "human in the loop" is a blocked thread waiting on stdin
- audit is whatever you remembered to log

Kheish decouples them. The agent is a daemon-managed resource, like a database row or a workflow execution. Callers — a CLI, Slack, a webhook, your code — submit and observe; they do not host the run. Composition stays in your application; execution moves into the daemon.

## What Kheish is not

- Not a **coding agent** like OpenHands / Aider / Cursor. You can run one on top of Kheish; coding is one workload among many.
- Not an **LLM router** like LiteLLM. Routing is included, but Kheish is the layer above.
- Not a **chatbot framework**. Slack and Telegram are *one* ingress among many.
- Not the right tool if you only need a single LLM call. Use the SDK directly.

The thing to read this against: **Kubernetes is a control plane for containers. Temporal is a control plane for workflows. Kheish is a control plane for AI agents.** Same shape, same reason it exists — to make the running thing outlive the caller, and to make the operator the source of truth instead of the script.

## Quickstart

Build the daemon binary; it acts as the server *and* the CLI client.

```bash
cargo build -p kheish-daemon
```

Start the daemon. By default it binds `127.0.0.1:4000` and journals state under `./.kheish-daemon`. To actually run an agent you need a provider key — set at least one of `ANTHROPIC_API_KEY`, `OPENAI_API_KEY`, `GOOGLE_API_KEY`, or `XAI_API_KEY` in the daemon's environment before `serve`:

```bash
export ANTHROPIC_API_KEY=sk-ant-...   # any one provider is enough
./target/debug/kheish-daemon serve
```

In another terminal, talk to it via the same binary as a client:

```bash
# 1. Confirm the daemon is alive (works without any provider key)
./target/debug/kheish-daemon status

# 2. Open a session
./target/debug/kheish-daemon sessions create demo

# 3. Submit input — returns a run id immediately; the run continues in the daemon
./target/debug/kheish-daemon sessions input demo "Inspect this repo and summarize it."

# 4. Observe runs attached to the session
./target/debug/kheish-daemon runs list --session-id demo

# 5. Wait until a run reaches a terminal state, or stream live events
./target/debug/kheish-daemon runs wait <run-id>
./target/debug/kheish-daemon runs stream <run-id>
```

Stop the daemon, restart `serve`, and the same `runs list` reflects journaled state — including any approval or user-question gates that paused mid-run. That round-trip is the invariant in action.

> If you only want to confirm the control plane works without spending tokens, the no-provider path is `status` → `capabilities` → `sessions create demo` → `sessions get demo`. Submitting input requires a configured provider.

## Stacks: the deployment is a file

Creating sessions by hand is fine for exploring. For anything you intend to keep running, Kheish has a declarative layer: a **Kheishfile** describes the personas, sessions, scopes, secrets, MCP requirements, playbooks, and schedules that make up one agent deployment, and the daemon **reconciles** live state against it — the same plan / apply / verify contract you know from Terraform, pointed at agents instead of infrastructure.

```bash
# Write a starter Kheishfile.yaml in the current directory
./target/debug/kheish-daemon stack init

# See what would change, change it, then prove the live daemon matches
./target/debug/kheish-daemon stack plan
./target/debug/kheish-daemon stack apply
./target/debug/kheish-daemon stack verify
```

A few properties worth knowing before you rely on it:

- **Fail-closed on capability drift.** If the stack declares an MCP server or tool that the live daemon does not actually expose, `plan`, `apply`, and `verify` refuse rather than deploy an agent that would silently miss a tool.
- **Secrets are referenced, never embedded.** A Kheishfile names the secret slots it needs; values arrive through the daemon's auth store or an explicitly allowed environment import. Managed secrets carry a salted fingerprint, so a value rotated behind the daemon's back shows up as drift at `plan`/`verify` time without the value ever being stored.
- **Ownership is tracked.** `apply` records what the stack created in a local ledger; `import` adopts pre-existing resources; `down` tears down only what the ledger owns.
- **Schedules make it autonomous.** A stack can attach cron schedules that start playbook-driven Flows — that is how a Kheishfile becomes a standing loop instead of a one-shot setup.

The complete worked example is [`examples/stacks/linear-github-feature-loop`](examples/stacks/linear-github-feature-loop): a stack that scans Linear for eligible tickets on a morning schedule, implements one ticket at a time in an isolated git worktree, opens a draft GitHub PR, and follows up on review feedback every half hour — with an internal 10/10 review gate before a PR leaves draft. It doubles as the reference for what production-shaped stacks look like (scoped sessions, versioned playbooks, operator contact, machine-readable state footers).

Reference documentation lives in [`docs/operators/kheish-stack.mdx`](docs/operators/kheish-stack.mdx) and the feature-loop guide in [`docs/operators/linear-github-feature-loop.mdx`](docs/operators/linear-github-feature-loop.mdx).

## Core Capabilities

- Persistent sessions with journaled state, checkpoints, and restart recovery
- Declarative stacks (Kheishfile) with plan / apply / verify reconciliation, drift detection, and an ownership ledger
- Detached runs with approvals and structured user-question flows
- Model-initiated operator contact: agents can notify a human or suspend on a question, through session-scoped, default-off policy
- Multi-agent orchestration with sidechains, tasks, schedules, and mailboxes; implementation subagents work in daemon-owned isolated git worktrees
- Brokered runtime auth with per-session and per-agent subjects, revocable short-lived leases, and signed external-action audit
- Daemon-owned observation sources, captured records, and materialization flows for screenshots, webcam snapshots, microphone segments, and other external capture producers
- Extensible runtime with built-in tools, hooks, skills, MCP, daemon-managed ingress connectors, and generic output plugins
- Operator control plane over HTTP/SSE with runtime reconfiguration and debug capture

## Architecture

Kheish is organized as a Rust workspace centered around a daemon control plane, a provider-agnostic runtime, an orchestration layer, and a shared session and journal model. The daemon is the operational entry point and the source of truth for sessions, runs, tasks, approvals, runtime configuration, and external integrations.

The runtime supports daemon-managed route inventories across Anthropic, OpenAI, Google, and xAI drivers, plus built-in coding and web tools, reusable skills, lifecycle hooks, MCP-backed tools, daemon-managed ingress connectors, and generic routed output delivery. Long-lived credentials stay in daemon-managed auth slots, while runs and child sidecars resolve brokered short-lived leases instead of receiving root secrets directly. Long-lived agent state is persisted through append-only session records and restored on restart.

## Capture and Observations

Kheish models capture as **durable observations**, not provider-specific media input.

A screen snapshot, webcam frame, microphone segment, browser event, or external connector payload is ingested into daemon-owned observation sources, retained under policy, and later materialized into a normal session run. Sources carry retention, sensitivity, and materialization policy; ingest is gated by per-source upload tokens with leases, expiration, and one-command revocation.

This means agents can reason over external context without depending on where it came from — a host-local capture process, a browser extension, a webhook, a connector, or another system. From the agent's point of view, it is just an observation.

See [`docs/capture.md`](docs/capture.md) for supported source kinds and media types, capture-agent provisioning, capture-group correlation, transcription-derived canonical text, and the `kheish-capture` host-local reference runtime.

## Security

Before exposing the daemon beyond localhost, enable control-plane authentication and review the active permission, hook, connector, and output-routing configuration for your deployment. Kheish includes built-in bearer auth for the control plane, brokered runtime auth for credential-backed execution, and signed append-only audit records for external actions.

Sessions and sidechains can further narrow auth-backed resources through `CredentialScope`, so delegated work can keep route access without inheriting connector or MCP credentials by default. Autonomous runs carry a bounded turn ceiling by default; removing it (`KHEISH_AGENT_MAX_TURNS=0`) is an explicit operator decision, and the daemon says so loudly at startup.

Two honest caveats worth reading before production use: the external-action audit chain is only tamper-evident if its signing key lives off-box (see the integrity caveat in [`docs/operators/security-and-auth.mdx`](docs/operators/security-and-auth.mdx)), and mid-run durability currently checkpoints at run boundaries and gates rather than after every tool call — the remediation plan in [`REMEDIATION.md`](REMEDIATION.md) tracks both.

## Contributing

See [AGENTS.md](AGENTS.md) for the operator guide used when working in this repository. CI runs rustfmt, a full workspace build, and the per-crate test matrix under `-D warnings`; a weekly live end-to-end workflow exercises the real Linear/GitHub feature loop when credentials are configured.

## License

Apache-2.0 — see [LICENSE](LICENSE).
