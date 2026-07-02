Inspect open GitHub PRs created by the Linear feature loop and recent eligible Linear issues that carry a `linear-github-feature-loop` workflow footer.

Setup:
- read the configured `project` and `repository` values from the Flow input or metadata;
- if either value is missing, stop in setup and report the missing configuration without scanning unrelated Linear or GitHub data;
- do not infer a project, team, owner, repository, or branch namespace from prior sessions.

Select one item per run. Prefer draft PRs with `State: blocked-with-pr`, then safe `State: blocked-before-pr` Linear issues, then unresolved actionable review comments.

For Linear issues with `State: blocked-before-pr`, resume only when `BlockerCategory` is `internal-review`, `tests`, or `no-coherent-patch` and current source inspection can produce a coherent ticket-scoped patch. Keep `product-judgment`, `credentials`, `ownership`, `unsafe`, `broad-refactor`, and `unclear-scope` blockers blocked.

When resuming a safe blocked issue, run the intake flow: inspect current source, create or update a coherent ticket-scoped patch, and open a durable draft PR before running additional review loops.

For the selected PR with unresolved review comments:
- read only unresolved actionable comments and the current focused diff needed for those comments;
- classify whether the request is safe to apply automatically;
- when safe, make the fix in an isolated worktree, run focused tests, and spawn an xhigh reviewer with a bounded review packet containing only the PR URL, branch, changed file list, diffstat, focused hunks or summary, test evidence or blocker, known risks, and exact review rubric;
- perform at most one implementation/review fix iteration in this run. If the reviewer score is below 10/10, keep the PR draft, record blockers, and let the next follow-up run resume. If a reviewer times out or fails due provider/context limits, keep the latest completed reviewer score, keep the PR draft, and record the failure as a blocker instead of spawning more review work;
- before finishing the root run, verify that spawned subagents are terminal. If a subagent is still running and the tool surface supports cancellation or interruption, cancel it; otherwise record that cancellation is unavailable and leave the PR draft blocked;
- keep the PR draft while tests are blocked, review score is below 10/10, or blockers remain;
- update the PR body with the latest tests, review score, and blockers;
- reply to the GitHub comments with the change made and test evidence;
- update the linked Linear ticket.

Subagents must not ask the user or parent for clarification. When spawning planner or reviewer subagents, explicitly forbid parent/user clarification in the prompt. If the tool surface supports per-spawn tool blocking, block parent-clarification tools for those subagents. When a subagent is blocked, it returns a concise blocker report to the coordinator. After a draft PR exists, after the footer is written, or while the root run is closing, do not request parent clarification; write a blocked state instead.

Every Linear workflow comment and PR body must include this machine-readable footer:

```text
Workflow: linear-github-feature-loop
State: draft-pr-open | blocked-before-pr | blocked-with-pr
BlockerCategory: internal-review | tests | product-judgment | credentials | ownership | unsafe | no-coherent-patch | broad-refactor | unclear-scope
PR: <url-or-none>
Branch: <branch-or-none>
ReviewerScore: <n-or-none>/10
Tests: <summary>
```

After a GitHub PR exists, never leave `PR: <url-or-none>`, `PR: none`, or a placeholder such as `PR: .../TBD` in the PR body or Linear comment. Replace it with the actual PR URL before finishing the root run.

Stop and report instead of changing code when a comment requires product judgment, credentials, broad refactoring, or unclear ownership.
