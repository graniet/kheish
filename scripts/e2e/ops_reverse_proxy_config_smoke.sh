#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
CONFIG="${KHEISH_OPS_NGINX_CONFIG:-"$ROOT/deploy/reverse-proxy/nginx-kheish.conf"}"
WORK_BASE="${KHEISH_OPS_PROXY_CONFIG_ROOT:-"$ROOT/.tmp"}"
TMP=""

cleanup() {
  if [[ -n "${TMP:-}" && -d "$TMP" ]]; then
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

if [[ ! -f "$CONFIG" ]]; then
  echo "missing Nginx reverse-proxy fixture: $CONFIG" >&2
  exit 1
fi

python3 "$ROOT/scripts/e2e/verify_runbook_commands.py" \
  "$ROOT/docs/operators/production-runbooks.mdx" >/dev/null

python3 - "$CONFIG" <<'PY'
import json
import re
import sys
from pathlib import Path

config_path = Path(sys.argv[1])
raw = config_path.read_text(encoding="utf-8")
surface = re.sub(r"#.*", "", raw)
surface = re.sub(r"\s+", " ", surface).strip()

required_fragments = {
    "TLS certificate": "ssl_certificate ",
    "TLS private key": "ssl_certificate_key ",
    "TLS protocol floor": "ssl_protocols TLSv1.2 TLSv1.3;",
    "daemon upstream": "server 127.0.0.1:4000;",
    "TLS listener": "listen 8443 ssl http2;",
    "server name": "server_name kheish.example.com;",
    "proxy pass": "proxy_pass http://kheish_daemon;",
    "HTTP/1.1 upstream": "proxy_http_version 1.1;",
    "Authorization forwarding": "proxy_set_header Authorization $http_authorization;",
    "Host forwarding": "proxy_set_header Host $host;",
    "forwarded proto": "proxy_set_header X-Forwarded-Proto $scheme;",
    "forwarded for": "proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;",
    "SSE response buffering disabled": "proxy_buffering off;",
    "request buffering disabled": "proxy_request_buffering off;",
    "request headers preserved": "proxy_pass_request_headers on;",
    "cache disabled": "proxy_cache off;",
    "SSE buffering header": "add_header X-Accel-Buffering no always;",
    "read timeout": "proxy_read_timeout 1h;",
    "send timeout": "proxy_send_timeout 1h;",
    "body size limit": "client_max_body_size 32m;",
}
missing = [
    label
    for label, fragment in required_fragments.items()
    if fragment not in surface
]
forbidden_patterns = {
    "wildcard CORS": r"Access-Control-Allow-Origin\s+\*",
    "disabled TLS verification": r"proxy_ssl_verify\s+off\s*;",
    "inline bearer token": r"Bearer\s+[A-Za-z0-9._~+/=-]{12,}",
}
for label, pattern in forbidden_patterns.items():
    if re.search(pattern, surface, flags=re.IGNORECASE):
        missing.append(f"forbidden {label}")
if missing:
    raise SystemExit(
        "reverse proxy fixture failed checks: " + ", ".join(sorted(missing))
    )
print(
    json.dumps(
        {
            "scenario": "ops_reverse_proxy_config_smoke",
            "status": "static_passed",
            "config": str(config_path),
        },
        sort_keys=True,
    )
)
PY

NGINX_T_STATUS="skipped"
if command -v nginx >/dev/null 2>&1 && command -v openssl >/dev/null 2>&1; then
  mkdir -p "$WORK_BASE"
  TMP="$(mktemp -d "$WORK_BASE/kheish-ops-nginx.XXXXXX")"
  TLS_CERT="$TMP/fullchain.pem"
  TLS_KEY="$TMP/privkey.pem"
  NGINX_CONFIG="$TMP/nginx.conf"
  NGINX_PREFIX="$TMP/nginx-prefix"
  mkdir -p "$NGINX_PREFIX/logs"
  openssl req \
    -x509 \
    -newkey rsa:2048 \
    -nodes \
    -keyout "$TLS_KEY" \
    -out "$TLS_CERT" \
    -days 1 \
    -subj "/CN=kheish.example.com" >/dev/null 2>&1
  sed \
    -e "s#/etc/kheish/tls/fullchain.pem#$TLS_CERT#g" \
    -e "s#/etc/kheish/tls/privkey.pem#$TLS_KEY#g" \
    "$CONFIG" >"$NGINX_CONFIG"
  nginx -t -p "$NGINX_PREFIX" -c "$NGINX_CONFIG" >/dev/null
  NGINX_T_STATUS="passed"
fi

printf '{"scenario":"ops_reverse_proxy_config_smoke","status":"passed","nginx_t":"%s","config":"%s"}\n' \
  "$NGINX_T_STATUS" "$CONFIG"
