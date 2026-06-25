#!/usr/bin/env bash
set -euo pipefail

healthcheck_url="${KHEISH_HEALTHCHECK_URL:-}"
if [[ -z "$healthcheck_url" ]]; then
  bind="${KHEISH_BIND:-0.0.0.0:4000}"
  port="${bind##*:}"
  healthcheck_url="http://127.0.0.1:${port}/readyz"
fi

exec curl --fail --silent "$healthcheck_url" > /dev/null
