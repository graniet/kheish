#!/usr/bin/env bash
# Real-binary session storage load test: 50 concurrent clients drive 200
# create→complete task runs plus one pre-bloated ~32 MiB session against a
# real `kheish-daemon serve`, with status and task-list readers hammering the
# API throughout. Asserts zero errors, exact post-burst state, and generous
# latency bounds. The heavy lifting lives in the ignored cargo test so the
# scenario stays a single source of truth.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "${ROOT}"

exec cargo test -p kheish-daemon --test routes_file_cli_e2e --locked -- \
  --ignored fifty_concurrent_clients_hammer_tasks_and_runs_on_a_real_daemon "$@"
