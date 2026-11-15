#!/usr/bin/env python3
"""Persist daily shadow reports privately; fail when any comparison disagrees."""
from datetime import datetime, timezone
import os
from pathlib import Path
import re
import subprocess
import sys


def main():
    if os.environ.get("SCHLANGE_SHADOW", "true").lower() != "true":
        return 0
    os.umask(0o077)
    directory = Path("/var/lib/schlange/reports")
    directory.mkdir(mode=0o700, parents=True, exist_ok=True)
    stamp = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%S.%fZ")
    failed = False
    for repo in os.environ["SCHLANGE_REPOS"].split(","):
        if not re.fullmatch(r"[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+", repo):
            raise ValueError("invalid SCHLANGE_REPOS")
        path = directory / f"{stamp}-{repo.replace('/', '--')}.json"
        with path.open("xb") as stream:
            result = subprocess.run(
                ["/usr/bin/docker", "compose", "--env-file", "/etc/schlange/stack.env",
                 "-f", "/etc/schlange/compose.yaml", "exec", "-T", "schlange",
                 "shadow", "report", "--repo", repo, "--data-dir", "/data", "--json"],
                stdout=stream, stderr=subprocess.PIPE, timeout=120, check=False)
            stream.flush()
            os.fsync(stream.fileno())
        if result.returncode:
            # Do not copy PR metadata or arbitrary subprocess output to journal logs.
            print(f"Shadow report needs attention: {path} (exit {result.returncode})",
                  file=sys.stderr)
            failed = True
    return int(failed)


if __name__ == "__main__":
    sys.exit(main())
