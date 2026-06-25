#!/usr/bin/env python3
"""Verify that an operator runbook still documents the commands smokes cover."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from dataclasses import dataclass
from pathlib import Path


DEFAULT_REQUIRED = [
    "doctor",
    "doctor --cors-origin",
    "doctor routes --check-auth",
    "doctor routes --check-references",
    "status",
    "runtime get",
    "sha256sum -c",
    "secrets set openai.prod",
    "secrets set sidecars.webhook.routes",
    "connectors put-external ops-webhook",
    "runtime auth subject connector:ops-webhook",
    "runtime auth lease <lease_id>",
    "runtime auth revoke-slot openai.prod",
    "runtime auth revoke-slot sidecars.webhook.routes",
    "runtime set-permission-mode dont-ask",
    "runtime set-debug-level off",
    "runtime auth revoke-subject",
    "runs list",
    "runs get <run_id>",
    "sessions events <session_id>",
    "tasks list <session_id>",
    "deliveries list --run-id <run_id>",
    "runs external-actions <run_id>",
    "sessions interrupt <session_id>",
    "runs cancel <run_id>",
    "tasks stop <session_id> <task_id>",
    "scripts/e2e/ops_backup_restore_smoke.sh",
    "scripts/e2e/ops_runbook_live_smoke.sh",
    "scripts/e2e/ops_tls_proxy_live_smoke.sh",
    "scripts/e2e/ops_slo_probe_smoke.sh",
    "scripts/e2e/ops_reverse_proxy_config_smoke.sh",
    "scripts/e2e/run_mcp_local_true_binary.sh",
]

MACOS_USER_HOME_PREFIX = "/" + "Users/"

DEFAULT_FORBIDDEN = [
    "runtime set-permission-mode default",
    MACOS_USER_HOME_PREFIX,
]


@dataclass(frozen=True)
class Fence:
    lang: str
    body: str
    start_line: int


def fenced_blocks(markdown: str) -> list[Fence]:
    fences: list[Fence] = []
    in_fence = False
    lang = ""
    body: list[str] = []
    start_line = 0
    for line_no, line in enumerate(markdown.splitlines(), start=1):
        stripped = line.strip()
        if not in_fence and stripped.startswith("```"):
            in_fence = True
            lang = stripped[3:].strip().split()[0] if stripped[3:].strip() else ""
            body = []
            start_line = line_no
            continue
        if in_fence and stripped == "```":
            fences.append(Fence(lang=lang, body="\n".join(body), start_line=start_line))
            in_fence = False
            continue
        if in_fence:
            body.append(line)
    return fences


def command_surface(markdown: str) -> str:
    fenced = [
        fence.body
        for fence in fenced_blocks(markdown)
        if fence.lang in {"bash", "sh", "shell"}
    ]
    inline_spans = re.findall(r"`([^`\n]+)`", markdown)
    raw = "\n".join(fenced + inline_spans)
    without_continuations = re.sub(r"\\\s*\n\s*", " ", raw)
    return re.sub(r"\s+", " ", without_continuations).strip()


def repo_root_for(path: Path) -> Path:
    for candidate in [path.parent, *path.parents]:
        if (candidate / "Cargo.toml").exists() and (candidate / "scripts").is_dir():
            return candidate
    return Path.cwd()


def check_referenced_scripts(surface: str, repo_root: Path) -> list[str]:
    errors: list[str] = []
    for script in sorted(set(re.findall(r"scripts/e2e/[A-Za-z0-9_.-]+\.sh", surface))):
        path = repo_root / script
        if not path.exists():
            errors.append(f"referenced script does not exist: {script}")
            continue
        result = subprocess.run(
            ["bash", "-n", str(path)],
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            errors.append(f"referenced script failed bash -n: {script}: {detail}")
    return errors


def check_bash_fence_syntax(markdown: str) -> tuple[list[str], list[str]]:
    errors: list[str] = []
    skipped: list[str] = []
    for fence in fenced_blocks(markdown):
        if fence.lang not in {"bash", "sh", "shell"}:
            continue
        if re.search(r"<[^>\n]+>", fence.body):
            skipped.append(f"line {fence.start_line}: contains placeholder")
            continue
        result = subprocess.run(
            ["bash", "-n"],
            input=fence.body,
            check=False,
            capture_output=True,
            text=True,
        )
        if result.returncode != 0:
            detail = (result.stderr or result.stdout).strip()
            errors.append(f"line {fence.start_line}: {detail}")
    return errors, skipped


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("path", type=Path)
    parser.add_argument(
        "--require",
        action="append",
        default=[],
        help="Additional command fragment that must appear in the command surface.",
    )
    parser.add_argument(
        "--forbid",
        action="append",
        default=[],
        help="Additional stale command fragment that must not appear in the command surface.",
    )
    parser.add_argument(
        "--no-defaults",
        action="store_true",
        help="Use only explicitly supplied --require/--forbid fragments.",
    )
    parser.add_argument(
        "--check-bash-syntax",
        action="store_true",
        help="Run bash -n on bash/sh/shell fences that do not contain placeholders.",
    )
    parser.add_argument(
        "--skip-script-check",
        action="store_true",
        help="Do not check that referenced scripts exist and pass bash -n.",
    )
    args = parser.parse_args()

    markdown = args.path.read_text(encoding="utf-8")
    surface = command_surface(markdown)
    if not surface:
        print(f"{args.path}: no bash blocks or inline command spans found", file=sys.stderr)
        return 1

    required = list(args.require)
    forbidden = list(args.forbid)
    if not args.no_defaults:
        required = DEFAULT_REQUIRED + required
        forbidden = DEFAULT_FORBIDDEN + forbidden

    missing = [fragment for fragment in required if fragment not in surface]
    stale = [fragment for fragment in forbidden if fragment in surface]
    script_errors: list[str] = []
    syntax_errors: list[str] = []
    skipped_fences: list[str] = []
    if not args.skip_script_check:
        script_errors = check_referenced_scripts(surface, repo_root_for(args.path.resolve()))
    if args.check_bash_syntax:
        syntax_errors, skipped_fences = check_bash_fence_syntax(markdown)

    if missing or stale or script_errors or syntax_errors:
        if missing:
            print("missing documented command fragments:", file=sys.stderr)
            for fragment in missing:
                print(f"  - {fragment}", file=sys.stderr)
        if stale:
            print("stale documented command fragments:", file=sys.stderr)
            for fragment in stale:
                print(f"  - {fragment}", file=sys.stderr)
        if script_errors:
            print("script reference errors:", file=sys.stderr)
            for error in script_errors:
                print(f"  - {error}", file=sys.stderr)
        if syntax_errors:
            print("bash fence syntax errors:", file=sys.stderr)
            for error in syntax_errors:
                print(f"  - {error}", file=sys.stderr)
        return 1

    if skipped_fences:
        print(f"skipped bash syntax for {len(skipped_fences)} placeholder block(s)")
    print(f"runbook command coverage ok: {args.path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
