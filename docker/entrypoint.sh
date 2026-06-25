#!/usr/bin/env bash
set -euo pipefail

umask 077

die() {
  echo "kheish-entrypoint: $*" >&2
  exit 64
}

canonical_dir() {
  local path="$1"
  mkdir -p "$path"
  (
    cd "$path"
    pwd -P
  )
}

is_loopback_bind() {
  case "$1" in
    127.*:*|localhost:*|\[::1\]:*|::1:*)
      return 0
      ;;
    *)
      return 1
      ;;
  esac
}

ensure_readable_file() {
  local label="$1"
  local path="$2"
  [[ -f "$path" ]] || die "$label must point to a regular file: $path"
  [[ -r "$path" ]] || die "$label is not readable: $path"
}

if (($# == 0)); then
  set -- serve
elif [[ "${1}" == -* ]]; then
  set -- serve "$@"
fi

if [[ "$1" != "serve" ]]; then
  admin_token_inline="${KHEISH_DAEMON_ADMIN_TOKEN:-}"
  admin_token_file="${KHEISH_DAEMON_ADMIN_TOKEN_FILE:-}"
  readonly_token_inline="${KHEISH_DAEMON_READONLY_TOKEN:-}"
  readonly_token_file="${KHEISH_DAEMON_READONLY_TOKEN_FILE:-}"
  auth_store_master_key_inline="${KHEISH_AUTH_STORE_MASTER_KEY:-}"
  auth_store_master_key_file="${KHEISH_AUTH_STORE_MASTER_KEY_FILE:-}"

  if [[ -n "$admin_token_inline" && -n "$admin_token_file" ]]; then
    die "use either an inline admin token or KHEISH_DAEMON_ADMIN_TOKEN_FILE, not both"
  fi
  if [[ -n "$readonly_token_inline" && -n "$readonly_token_file" ]]; then
    die "use either an inline read-only token or KHEISH_DAEMON_READONLY_TOKEN_FILE, not both"
  fi
  if [[ -n "$auth_store_master_key_inline" && -n "$auth_store_master_key_file" ]]; then
    die "use either KHEISH_AUTH_STORE_MASTER_KEY or KHEISH_AUTH_STORE_MASTER_KEY_FILE, not both"
  fi
  if [[ -n "$admin_token_file" ]]; then
    ensure_readable_file "KHEISH_DAEMON_ADMIN_TOKEN_FILE" "$admin_token_file"
  fi
  if [[ -n "$readonly_token_file" ]]; then
    ensure_readable_file "KHEISH_DAEMON_READONLY_TOKEN_FILE" "$readonly_token_file"
  fi
  if [[ -n "$auth_store_master_key_file" ]]; then
    ensure_readable_file "KHEISH_AUTH_STORE_MASTER_KEY_FILE" "$auth_store_master_key_file"
  fi
  exec /usr/local/bin/kheish-daemon "$@"
fi

bind="${KHEISH_BIND:-0.0.0.0:4000}"
state_root="${KHEISH_STATE_ROOT:-/var/lib/kheish/state}"
workspace_root="${KHEISH_WORKSPACE_ROOT:-/workspace}"
routes_file="${KHEISH_ROUTES_FILE:-}"
http_auth_mode="${KHEISH_HTTP_AUTH_MODE:-bearer}"
admin_token_inline="${KHEISH_DAEMON_ADMIN_TOKEN:-}"
admin_token_file="${KHEISH_DAEMON_ADMIN_TOKEN_FILE:-}"
readonly_token_inline="${KHEISH_DAEMON_READONLY_TOKEN:-}"
readonly_token_file="${KHEISH_DAEMON_READONLY_TOKEN_FILE:-}"
auth_store_master_key_inline="${KHEISH_AUTH_STORE_MASTER_KEY:-}"
auth_store_master_key_file="${KHEISH_AUTH_STORE_MASTER_KEY_FILE:-}"

serve_args=("${@:2}")
index=0
while ((index < ${#serve_args[@]})); do
  arg="${serve_args[index]}"
  case "$arg" in
    --bind)
      ((index + 1 < ${#serve_args[@]})) || die "--bind requires a value"
      bind="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --bind=*)
      bind="${arg#*=}"
      ((index += 1))
      ;;
    --state-root)
      ((index + 1 < ${#serve_args[@]})) || die "--state-root requires a value"
      state_root="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --state-root=*)
      state_root="${arg#*=}"
      ((index += 1))
      ;;
    --workspace-root)
      ((index + 1 < ${#serve_args[@]})) || die "--workspace-root requires a value"
      workspace_root="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --workspace-root=*)
      workspace_root="${arg#*=}"
      ((index += 1))
      ;;
    --routes-file)
      ((index + 1 < ${#serve_args[@]})) || die "--routes-file requires a value"
      routes_file="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --routes-file=*)
      routes_file="${arg#*=}"
      ((index += 1))
      ;;
    --http-auth-mode)
      ((index + 1 < ${#serve_args[@]})) || die "--http-auth-mode requires a value"
      http_auth_mode="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --http-auth-mode=*)
      http_auth_mode="${arg#*=}"
      ((index += 1))
      ;;
    --http-admin-token)
      ((index + 1 < ${#serve_args[@]})) || die "--http-admin-token requires a value"
      admin_token_inline="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --http-admin-token=*)
      admin_token_inline="${arg#*=}"
      ((index += 1))
      ;;
    --http-admin-token-file)
      ((index + 1 < ${#serve_args[@]})) || die "--http-admin-token-file requires a value"
      admin_token_file="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --http-admin-token-file=*)
      admin_token_file="${arg#*=}"
      ((index += 1))
      ;;
    --http-readonly-token)
      ((index + 1 < ${#serve_args[@]})) || die "--http-readonly-token requires a value"
      readonly_token_inline="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --http-readonly-token=*)
      readonly_token_inline="${arg#*=}"
      ((index += 1))
      ;;
    --http-readonly-token-file)
      ((index + 1 < ${#serve_args[@]})) || die "--http-readonly-token-file requires a value"
      readonly_token_file="${serve_args[index + 1]}"
      ((index += 2))
      ;;
    --http-readonly-token-file=*)
      readonly_token_file="${arg#*=}"
      ((index += 1))
      ;;
    *)
      ((index += 1))
      ;;
  esac
done

[[ "$state_root" = /* ]] || die "--state-root must be an absolute path inside the container"
[[ "$workspace_root" = /* ]] || die "--workspace-root must be an absolute path inside the container"

if [[ -n "$routes_file" ]]; then
  [[ "$routes_file" = /* ]] || die "--routes-file must be an absolute path inside the container"
  ensure_readable_file "--routes-file" "$routes_file"
fi

if [[ -n "$admin_token_inline" && -n "$admin_token_file" ]]; then
  die "use either an inline admin token or KHEISH_DAEMON_ADMIN_TOKEN_FILE, not both"
fi

if [[ -n "$readonly_token_inline" && -n "$readonly_token_file" ]]; then
  die "use either an inline read-only token or KHEISH_DAEMON_READONLY_TOKEN_FILE, not both"
fi

if [[ -n "$auth_store_master_key_inline" && -n "$auth_store_master_key_file" ]]; then
  die "use either KHEISH_AUTH_STORE_MASTER_KEY or KHEISH_AUTH_STORE_MASTER_KEY_FILE, not both"
fi

if [[ -n "$admin_token_file" ]]; then
  ensure_readable_file "KHEISH_DAEMON_ADMIN_TOKEN_FILE" "$admin_token_file"
fi

if [[ -n "$readonly_token_file" ]]; then
  ensure_readable_file "KHEISH_DAEMON_READONLY_TOKEN_FILE" "$readonly_token_file"
fi

if [[ -n "$auth_store_master_key_file" ]]; then
  ensure_readable_file "KHEISH_AUTH_STORE_MASTER_KEY_FILE" "$auth_store_master_key_file"
fi

state_root_real="$(canonical_dir "$state_root")"
workspace_root_real="$(canonical_dir "$workspace_root")"

[[ "$state_root_real" != "$workspace_root_real" ]] || die "--state-root and --workspace-root must not resolve to the same directory"

if ! is_loopback_bind "$bind"; then
  if [[ "$http_auth_mode" != "bearer" && "${KHEISH_ALLOW_INSECURE_NON_LOOPBACK:-false}" != "true" ]]; then
    die "non-loopback binds require bearer control-plane auth; set KHEISH_ALLOW_INSECURE_NON_LOOPBACK=true only for explicit local testing"
  fi
  if [[ "$http_auth_mode" == "bearer" && -z "$admin_token_inline" && -z "$admin_token_file" ]]; then
    die "non-loopback bearer auth requires an admin token via KHEISH_DAEMON_ADMIN_TOKEN or KHEISH_DAEMON_ADMIN_TOKEN_FILE"
  fi
fi

exec /usr/local/bin/kheish-daemon "$@"
