#!/usr/bin/env python3
"""PostToolUse hook: remind the agent to run the codebase-safety skill.

Fires after Write/Edit/MultiEdit (and Bash commands that write) on a GitHub Actions
workflow, an agent instruction or config file, or a shell script. Reminds once per file
and session. Never blocks; any error exits 0 silently.
"""

from __future__ import annotations

import hashlib
import json
import os
import re
import sys
import tempfile
from pathlib import Path

INSTRUCTION_NAMES = {"agents.md", "claude.md", ".cursorrules", "skill.md", ".mcp.json"}
SHELL_SHEBANG = re.compile(rb"^#!.*\b(ba|z|k|da)?sh\b")
# Bash commands that may write files: redirection, tee, sed -i, mv/cp/install, git checkout/apply.
BASH_WRITE = re.compile(r"(>|\btee\b|\bsed\s+-i|\bperl\s+-pi|\bmv\b|\bcp\b|\binstall\b|\bgit\s+(apply|checkout|restore|am)\b|\bchmod\s+\+x)")
BASH_PATH = re.compile(r"[\w./~-]*(?:\.github/workflows/[\w./-]+|action\.ya?ml|AGENTS\.md|CLAUDE\.md|\.cursorrules|SKILL\.md|settings(?:\.local)?\.json|\.mcp\.json|[\w./-]+\.(?:sh|bash|zsh)|justfile|Makefile)\b", re.IGNORECASE)


def kind_of(path: str) -> str | None:
    p = path.replace("\\", "/")
    name = p.rsplit("/", 1)[-1].lower()
    if "/.github/workflows/" in "/" + p or name in {"action.yml", "action.yaml"}:
        return "GitHub Actions workflow"
    if name in INSTRUCTION_NAMES or ("/.claude/" in "/" + p and name.endswith(".json")) or "/hooks/" in p:
        return "agent instructions/config"
    if name.endswith((".sh", ".bash", ".zsh")) or name in {"justfile", "makefile"}:
        return "shell script"
    try:
        with open(path, "rb") as fh:
            if SHELL_SHEBANG.match(fh.readline(200)):
                return "shell script"
    except OSError:
        pass
    return None


def touched_paths(data: dict) -> list[str]:
    tool = data.get("tool_name", "")
    inp = data.get("tool_input") or {}
    if tool in {"Write", "Edit", "MultiEdit", "NotebookEdit"}:
        path = inp.get("file_path") or inp.get("notebook_path")
        return [path] if path else []
    if tool == "Bash":
        cmd = inp.get("command", "")
        if BASH_WRITE.search(cmd):
            return BASH_PATH.findall(cmd)
    return []


def main() -> None:
    data = json.load(sys.stdin)
    hits = [(p, k) for p in touched_paths(data) if (k := kind_of(p))]
    if not hits:
        return
    marker_dir = Path(tempfile.gettempdir()) / "codebase-safety-hook"
    marker_dir.mkdir(exist_ok=True)
    fresh = []
    for path, kind in hits:
        key = hashlib.sha256(f"{data.get('session_id', '')}:{os.path.abspath(path)}".encode()).hexdigest()[:24]
        marker = marker_dir / key
        if not marker.exists():
            marker.touch()
            fresh.append(f"{path} ({kind})")
    if not fresh:
        return
    msg = (
        "Safety-relevant file changed: " + ", ".join(fresh) + ". "
        "Invoke the `codebase-safety` skill now and run its change review on these files "
        "before committing (deletes, credentials, CI permissions/untrusted input, prose-only rules, "
        "bypass flags, unbounded sends)."
    )
    print(json.dumps({"hookSpecificOutput": {"hookEventName": "PostToolUse", "additionalContext": msg}}))


if __name__ == "__main__":
    try:
        main()
    except Exception:  # never break the session over a reminder
        pass
