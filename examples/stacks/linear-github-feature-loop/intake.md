Read eligible Linear issues for the Linear project/team and GitHub repository configured on this Flow. Recover previously blocked Kheish workflow issues that have no GitHub PR yet only when `BlockerCategory` is `internal-review`, `tests`, or `no-coherent-patch` and current source inspection can produce a coherent ticket-scoped patch. Keep `product-judgment`, `credentials`, `ownership`, `unsafe`, `broad-refactor`, and `unclear-scope` blockers blocked.

Setup:
- read the configured `project` and `repository` values from the Flow input or metadata exposed in the run context;
- read optional `linear_team_key` from Flow input or metadata. When present, treat that team key as the authoritative Linear issue scope, while `project` remains a human-readable deployment label;
- if either value is missing, stop in setup and report the missing configuration without scanning unrelated Linear or GitHub data;
- do not infer a project, team, owner, repository, or branch namespace from prior sessions.
- do not read daemon state files, session logs, run logs, `../state`, `.kheish-*`, or prior workflow outputs to discover configuration. If the run context does not expose required metadata, stop and report the missing field instead of using shell to inspect daemon state.

Fresh Linear intake:
- this is the Linear intake schedule, not the GitHub PR maintenance schedule. Existing workflow PRs for other issues must not block fresh Linear selection when there is no active goal;
- when the existing session goal is complete or otherwise inactive, do not continue that completed goal's issue or PR during intake. Select from fresh Linear candidates instead;
- before selecting a Linear issue, use only Linear tools plus `get_goal`. Do not run shell commands, inspect local files, scan the workspace, scan daemon state, or inspect GitHub PRs before a fresh candidate is selected;
- for new work, scan the configured Linear project or team for open issues directly. Do not require an existing `Workflow: linear-github-feature-loop` footer, label, comment, branch, or GitHub PR marker to select a fresh ticket;
- use the workflow footer only for recovery and deduplication: finding already-touched issues, blocked workflow issues, existing PR links, handled feedback watermarks, and no-duplicate checks;
- resolve the configured project/team with Linear project/team tools, then list candidate issues in that scope. If `linear_team_key` is set, resolve that team key first. If no Linear Project matches `project`, check Linear teams by key and case-insensitive name before concluding there is no work;
- list issues by the resolved Linear team key when the scope is a team. Prefer issues in non-terminal statuses such as backlog, todo, triage, ready, or in-progress, and skip terminal or clearly non-actionable statuses such as done, canceled, duplicate, archived, or explicitly blocked;
- for each fresh candidate, inspect the issue title, description, labels, status, and comments before selecting it. Skip tickets that require product judgment, credentials, ownership decisions, unsafe changes, broad unrelated refactors, or unclear scope;
- before starting implementation on a fresh issue, check GitHub for an existing workflow branch or PR for that selected Linear issue id and continue it instead of creating a duplicate. Do not inspect or maintain unrelated workflow PRs during intake; leave those to the GitHub follow-up schedule;
- if no fresh issue is selected, report the exact Linear project/team resolved, the issue filters or search terms used, the number of issues inspected, and the reason each plausible candidate was skipped. Do not summarize that as "no workflow issues found" unless you also scanned unmarked project or team issues.

Autonomy and goal handling:
- call `get_goal` before scanning Linear or GitHub work items;
- if the session already has an `Active` goal, do not select a new ticket. Continue only the issue, PR, or branch named in that goal. If this run is only a scheduled discovery run and cannot safely bind to the active goal, report that the active goal exists and finish; the daemon goal continuation will resume it;
- if the existing goal is `Paused`, `BudgetLimited`, or `Complete`, treat it as inactive for selection. When a new ticket or PR is selected, call `create_goal` with `replace_if_inactive: true`, an objective that names the issue id, repository, branch or PR URL when known, and the token budget from Flow metadata `goal_token_budget` when present;
- leave the goal `Active` whenever the selected item still has a safe, ticket-scoped next action. This is what lets the daemon continue autonomously after this run settles;
- call `update_goal` with `status: "complete"` only after tests are current, the internal reviewer score is 10/10, GitHub and Linear footers are updated, and every spawned subagent is terminal;
- call `update_goal` with `status: "paused"` only for durable human or external blockers: product judgment, credentials, ownership, unsafe work, broad refactor, unclear scope, repeated no-progress according to `max_no_progress`, or repeated provider/reviewer timeouts with no new evidence.

Process one feature ticket per run:
- after selecting a Linear issue, check for existing open pull requests or workflow branches for that same issue and continue that artifact instead of creating a duplicate;
- inspect the relevant source code before proposing work;
- spawn one planning subagent with reasoning effort xhigh, using a compact prompt that names only the Linear issue, relevant source files, known constraints, and the concrete decision needed;
- spawn implementation work in an isolated worktree;
- before declaring tests blocked because host runtimes or package managers are missing, inspect the repository for project-native dev/test entrypoints such as Docker Compose files, Makefile targets, justfile targets, package scripts, CI workflow commands, or devcontainer config;
- local Docker and Docker Compose are allowed when the repository provides them. Prefer the smallest project-native command that installs dependencies or runs the focused tests, tear down long-running services after use, and record the exact command and result;
- host missing tools such as `php`, `composer`, `node`, or language-specific package managers are not by themselves a test blocker until project-native containerized or scripted test paths have been tried or shown unavailable or unsafe;
- run focused tests through the project-native path when available;
- as soon as there is a coherent, ticket-scoped patch with passing focused tests, or a documented environment or pre-existing test blocker that does not invalidate the patch, push it to a workflow branch and open a GitHub draft PR against the configured repository, even when the internal reviewer score is below 10/10;
- make the draft PR body explicit about status, Linear issue, files changed, tests run or blocked, review score, known blockers, and the actual PR URL once GitHub returns it;
- spawn one reviewer subagent with reasoning effort xhigh and require a 10/10 score before treating the PR as ready for human review or merge. Give the reviewer a bounded review packet with the PR URL, branch, changed file list, diffstat, focused hunks or summary, test evidence or blocker, known risks, and exact review rubric. Do not pass the full session transcript, complete PR body, full logs, or large raw diffs;
- perform at most one implementation/review fix iteration in this run. If the reviewer score is below 10/10 after that iteration and the findings are actionable within the ticket scope, keep the PR draft, record blockers and next action, and leave the goal active so the daemon goal continuation resumes it;
- if a reviewer times out or fails due provider/context limits, keep the latest completed reviewer score and finalize the PR as blocked-with-pr instead of spawning more review work;
- before finishing the root run, verify that spawned subagents are terminal. If a subagent is still running and the tool surface supports cancellation or interruption, cancel it; otherwise record that cancellation is unavailable and leave the PR draft blocked;
- update the GitHub PR body when status, tests, review score, or blockers change;
- comment on the Linear ticket with the PR URL, tests run, review score, and any blocker.

Subagents must not ask the user or parent for clarification. When spawning planner or reviewer subagents, explicitly forbid parent/user clarification in the prompt. If the tool surface supports per-spawn tool blocking, block parent-clarification tools for those subagents. When a subagent is blocked, it returns a concise blocker report to the coordinator. The coordinator decides whether to proceed, leave the PR draft, or stop. After a draft PR exists, after the footer is written, or while the root run is closing, do not request parent clarification; write a blocked state instead.

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

Do not open a PR when the ticket is unsafe to implement, requires product judgment, lacks enough source context for a coherent patch, or would require broad unrelated refactoring. In those cases, comment on Linear with the blocker and leave the issue for a future run.
