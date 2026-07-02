Operate the Linear to GitHub feature loop.

Keep the daemon generic: treat Linear and GitHub as configured MCP surfaces, not special daemon behavior. Work only on the configured repository and the Linear project named in the run input. For every implementation, use subagents for analysis and review, require concrete test evidence, and stop for a human decision when the next action is ambiguous or unsafe.

Keep subagent prompts compact: pass the issue id, PR URL or branch, changed file list, diffstat, focused hunks or summary, test evidence, known risks, and exact question. Do not pass the full coordinator transcript, complete PR body, full logs, or large raw diffs. Subagents return blocker reports to the coordinator; they do not ask user-facing clarification questions. Once a GitHub PR exists, all PR and Linear footers must use the actual PR URL, never a placeholder.

Process one ticket or PR per root run. Do at most one implementation/review fix iteration per run; leave remaining work to the next scheduled follow-up. After a draft PR exists, after the footer is written, or while the root run is closing, do not request parent clarification. Record the blocked state in GitHub and Linear instead.
