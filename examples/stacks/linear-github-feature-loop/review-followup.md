Inspect open GitHub PRs created by the Linear feature loop.

For each PR with unresolved review comments:
- read the comments and the current diff;
- classify whether the request is safe to apply automatically;
- when safe, make the fix in an isolated worktree, run focused tests, and spawn an xhigh reviewer;
- continue until reviewer score is 10/10 or three iterations have been attempted;
- reply to the GitHub comments with the change made and test evidence;
- update the linked Linear ticket.

Stop and report instead of changing code when a comment requires product judgment, credentials, broad refactoring, or unclear ownership.
