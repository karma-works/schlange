#!/usr/bin/env python3
"""Fail on missing workers, unhealthy ingress, halted trains or invalid configuration."""
import argparse
import json
from pathlib import Path
import sys
from urllib.request import Request, build_opener, ProxyHandler, HTTPRedirectHandler


class NoRedirect(HTTPRedirectHandler):
    def redirect_request(self, *args, **kwargs):
        return None


def problems(status, repos):
    failures = []
    if not isinstance(status, dict):
        return ["invalid /status response"]
    for repo in repos:
        worker = status.get(repo)
        if not isinstance(worker, dict):
            failures.append(f"{repo}: missing worker")
            continue
        if worker.get("halted"):
            failures.append(f"{repo}: halted train")
        if worker.get("config_error") or worker.get("config_warnings"):
            failures.append(f"{repo}: rejected or unsupported configuration")
        if worker.get("state", {}).get("check_identity_unavailable"):
            failures.append(f"{repo}: check identity unavailable")
        if worker.get("role") not in ("leader", "shadow"):
            failures.append(f"{repo}: not the webhook leader")
    return failures


def check(url, token, repos):
    # This is a local host monitor; never forward its bearer token via environment proxies
    # or to a redirect destination. No response bodies (PR metadata) enter system logs.
    opener = build_opener(ProxyHandler({}), NoRedirect())
    for path in ("/healthz", "/readyz"):
        with opener.open(url + path, timeout=10) as response:
            response.read(16384)
    request = Request(url + "/status", headers={"Authorization": f"Bearer {token}"})
    with opener.open(request, timeout=10) as response:
        status = json.load(response)
    failures = problems(status, repos)
    if failures:
        raise RuntimeError("; ".join(failures))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--url", default="http://127.0.0.1:18080")
    parser.add_argument("--token-file", type=Path, required=True)
    parser.add_argument("--repos", required=True, help="comma-separated owner/name list")
    args = parser.parse_args()
    try:
        token = args.token_file.read_text().strip()
        if not token or any(character.isspace() for character in token):
            raise ValueError("admin token must be nonempty and contain no whitespace")
        check(args.url.rstrip("/"), token, args.repos.split(","))
    except Exception as error:
        print(f"Schlange monitor failed: {error}", file=sys.stderr)
        sys.exit(1)
