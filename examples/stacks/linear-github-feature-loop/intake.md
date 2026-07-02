Read eligible Linear issues for the Linear project/team and GitHub repository configured on this Flow. Recover previously blocked Kheish workflow issues that have no GitHub PR yet only when `BlockerCategory` is `internal-review`, `tests`, or `no-coherent-patch` and current source inspection can produce a coherent ticket-scoped patch. Keep `product-judgment`, `credentials`, `ownership`, `unsafe`, `broad-refactor`, and `unclear-scope` blockers blocked.

Setup:
- read the configured `project` and `repository` values from the Flow input or metadata;
- if either value is missing, stop in setup and report the missing configuration without scanning unrelated Linear or GitHub data;
- do not infer a project, team, owner, repository, or branch namespace from prior sessions.

Process one feature ticket per run:
- first check for existing open pull requests or workflow branches for the same Linear issue and continue that artifact instead of creating a duplicate;
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
- perform at most one implementation/review fix iteration in this run. If the reviewer score is below 10/10 after that iteration, keep the PR draft, record blockers, and let the follow-up schedule resume later;
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
```

After a GitHub PR exists, never leave `PR: <url-or-none>`, `PR: none`, or a placeholder such as `PR: .../TBD` in the PR body or Linear comment. Replace it with the actual PR URL before finishing the root run.

Do not open a PR when the ticket is unsafe to implement, requires product judgment, lacks enough source context for a coherent patch, or would require broad unrelated refactoring. In those cases, comment on Linear with the blocker and leave the issue for a future run.
