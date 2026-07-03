Inspect open GitHub PRs created by the Linear feature loop and recent eligible Linear issues that carry a `linear-github-feature-loop` workflow footer.

Setup:
- read the configured `project` and `repository` values from the Flow input or metadata;
- if either value is missing, stop in setup and report the missing configuration without scanning unrelated Linear or GitHub data;
- do not infer a project, team, owner, repository, or branch namespace from prior sessions.

Autonomy and goal handling:
- call `get_goal` before scanning Linear or GitHub work items;
- if the session already has an `Active` goal, do not select a different PR or issue. Continue only the issue, PR, or branch named in that goal. If this run is only a scheduled discovery run and cannot safely bind to the active goal, report that the active goal exists and finish; the daemon goal continuation will resume it;
- if the existing goal is `Paused`, `BudgetLimited`, or `Complete`, treat it as inactive for selection. When a draft PR or resumable blocked issue is selected, call `create_goal` with `replace_if_inactive: true`, an objective that names the issue id, repository, branch or PR URL, and the token budget from Flow metadata `goal_token_budget` when present;
- leave the goal `Active` whenever the selected item still has a safe, ticket-scoped next action. This is what lets the daemon continue autonomously after this run settles;
- call `update_goal` with `status: "complete"` only after tests are current, the internal reviewer score is 10/10, GitHub and Linear footers are updated, and every spawned subagent is terminal;
- call `update_goal` with `status: "paused"` only for durable human or external blockers: product judgment, credentials, ownership, unsafe work, broad refactor, unclear scope, repeated no-progress according to `max_no_progress`, or repeated provider/reviewer timeouts with no new evidence.

Select one item per run. Prefer draft PRs with `State: blocked-with-pr`, then safe `State: blocked-before-pr` Linear issues, then PRs with new actionable human GitHub feedback.

Before selecting or mutating a PR, build a compact GitHub feedback inventory for each matching workflow PR that is not already terminal:
- use `pull_request_read` with the PR detail, review-summary, review-thread, and top-level-comment methods (`get`, `get_reviews`, `get_review_comments`, and `get_comments`) to inspect the PR summary, review summaries, unresolved review threads with replies, and top-level PR comments. If any required feedback method is unavailable or errors, record the tool failure and do not mutate the PR. Follow pagination or cursor fields when present; if pagination cannot be completed safely in this run, record the truncation before deciding;
- include only human-authored feedback newer than the latest handled feedback watermark, plus any unresolved threads regardless of age;
- record each item as `{source, id_or_url, author, created_at, updated_at, body_preview_or_hash, actionability, handled_or_skipped_reason}`;
- preserve each feedback body as a whole item. Never reduce a multi-line comment to only its last line;
- treat a GitHub review as one envelope: review summary/body when available, inline review comments, and replies. Do not act on one inline fragment until sibling comments and the parent review metadata have been inspected;
- if review metadata shows a human review exists but the summary/body is not available through the tool output, do not assume the inline comment is the full request. Continue only when the inline comment is self-contained; otherwise record a `product-judgment` or `unclear-scope` blocker with the missing feedback context;
- use the inventory, not raw chronological order alone, to select the single highest-priority item for this run.

For Linear issues with `State: blocked-before-pr`, resume only when `BlockerCategory` is `internal-review`, `tests`, or `no-coherent-patch` and current source inspection can produce a coherent ticket-scoped patch. Keep `product-judgment`, `credentials`, `ownership`, `unsafe`, `broad-refactor`, and `unclear-scope` blockers blocked.

When resuming a safe blocked issue, run the intake flow: inspect current source, create or update a coherent ticket-scoped patch, and open a durable draft PR before running additional review loops.

For a selected draft PR with `State: blocked-with-pr`, inspect the machine-readable footer before deciding that no work is needed. If `Tests:` records blocked or unavailable test evidence, rerun the focused tests first even when there are no unresolved GitHub review comments. Do not carry forward stale test blockers without rechecking the current repository and local project-native test entrypoints.

When retesting a blocked draft PR:
- read the PR branch files needed to discover test commands and the smallest focused test path;
- before declaring tests blocked because host runtimes or package managers are missing, inspect the repository for project-native dev/test entrypoints such as Docker Compose files, Makefile targets, justfile targets, package scripts, CI workflow commands, or devcontainer config;
- local Docker and Docker Compose are allowed when the repository provides them. Prefer the smallest project-native command that installs dependencies or runs the focused tests, tear down long-running services after use, and record the exact command and result;
- if focused tests pass, update the PR and Linear footer with passing evidence and keep the PR draft only for remaining blockers such as internal review below 10/10;
- if focused tests fail and the failure is ticket-scoped and safe to fix, perform at most one implementation/test fix iteration in this run before review;
- if tests remain blocked, replace the stale blocker with the current blocker and exact commands attempted.

For the selected PR with actionable GitHub feedback:
- read only the selected feedback envelope and the current focused diff needed for that feedback;
- classify whether the request is safe to apply automatically;
- when safe, make the fix in an isolated worktree;
- before declaring tests blocked because host runtimes or package managers are missing, inspect the repository for project-native dev/test entrypoints such as Docker Compose files, Makefile targets, justfile targets, package scripts, CI workflow commands, or devcontainer config;
- local Docker and Docker Compose are allowed when the repository provides them. Prefer the smallest project-native command that installs dependencies or runs the focused tests, tear down long-running services after use, and record the exact command and result;
- host missing tools such as `php`, `composer`, `node`, or language-specific package managers are not by themselves a test blocker until project-native containerized or scripted test paths have been tried or shown unavailable or unsafe;
- run focused tests through the project-native path when available, then spawn an xhigh reviewer with a bounded review packet containing only the PR URL, branch, changed file list, diffstat, focused hunks or summary, test evidence or blocker, known risks, and exact review rubric;
- perform at most one implementation/review fix iteration in this run. If the reviewer score is below 10/10 and the findings are actionable within the ticket scope, keep the PR draft, record blockers and next action, and leave the goal active so the daemon goal continuation resumes it. If a reviewer times out or fails due provider/context limits, keep the latest completed reviewer score, keep the PR draft, and record the failure as a blocker instead of spawning more review work;
- before finishing the root run, verify that spawned subagents are terminal. If a subagent is still running and the tool surface supports cancellation or interruption, cancel it; otherwise record that cancellation is unavailable and leave the PR draft blocked;
- keep the PR draft while tests are blocked, review score is below 10/10, or blockers remain;
- update the PR body with the latest tests, review score, blockers, `LastFeedbackSeenAt`, and `HandledFeedbackIDs` after replying to or intentionally skipping a feedback item;
- reply to the relevant GitHub feedback with the change made and test evidence. Use a threaded review-comment reply for inline threads and a top-level PR comment for top-level PR feedback;
- update the linked Linear ticket with the same footer watermarks so later runs do not re-handle the same feedback.

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
GoalId: <goal-id-or-none>
AutonomyAttempt: <integer>
LastProgress: <summary>
NextAction: <summary-or-none>
NoProgressCount: <integer>
LastFeedbackSeenAt: <iso8601-or-none>
HandledFeedbackIDs: <comma-separated-ids-or-none>
```

After a GitHub PR exists, never leave `PR: <url-or-none>`, `PR: none`, or a placeholder such as `PR: .../TBD` in the PR body or Linear comment. Replace it with the actual PR URL before finishing the root run.

Stop and report instead of changing code when a comment requires product judgment, credentials, broad refactoring, or unclear ownership.
