#!/usr/bin/env bash
#
# Build the kheish-air web console and copy its production bundle into the
# daemon crate so `include_dir!` embeds it into the `kheish-daemon` binary.
#
# The committed `console-dist/` only holds a placeholder `index.html`; this
# script overwrites the directory contents with the real build output. Nothing
# it writes (besides the tracked placeholder) is committed — see the
# `console-dist/.gitignore`.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
dest="${repo_root}/crates/kheish-daemon/console-dist"

# Locate the console source tree. Prefer a sibling checkout, then an explicit
# KHEISH_CONSOLE_SRC override for out-of-tree layouts.
if [ -d "${repo_root}/../kheish-air" ]; then
  console_src="$(cd "${repo_root}/../kheish-air" && pwd)"
elif [ -n "${KHEISH_CONSOLE_SRC:-}" ] && [ -d "${KHEISH_CONSOLE_SRC}" ]; then
  console_src="$(cd "${KHEISH_CONSOLE_SRC}" && pwd)"
else
  echo "error: could not find the kheish-air console source tree" >&2
  echo "       set KHEISH_CONSOLE_SRC or place the checkout at ../kheish-air" >&2
  exit 1
fi

echo "building console from ${console_src}"
(cd "${console_src}" && npm run build)

build_out="${console_src}/dist"
if [ ! -d "${build_out}" ]; then
  echo "error: console build did not produce ${build_out}" >&2
  exit 1
fi

echo "syncing ${build_out} -> ${dest}"
mkdir -p "${dest}"
# Wipe everything except the tracked ignore file, then copy the fresh bundle.
find "${dest}" -mindepth 1 -not -name '.gitignore' -delete
if command -v rsync >/dev/null 2>&1; then
  rsync -a --exclude '.gitignore' "${build_out}/" "${dest}/"
else
  cp -R "${build_out}/." "${dest}/"
fi

echo "console bundle staged in ${dest}"
echo "rebuild the daemon (cargo build -p kheish-daemon) to embed it"
