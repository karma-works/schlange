#!/usr/bin/env python3
"""Read the existing sandbox queue app's webhook configuration without exposing its URL."""
import importlib.util
import json
import os
from pathlib import Path
import urllib.error
import urllib.parse
import urllib.request

spec = importlib.util.spec_from_file_location("sandbox_driver", Path(__file__).with_name("sandbox.py"))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

READ_PATHS = {
    "/app", "/app/installations?per_page=2", "/app/hook/config",
    "/app/hook/deliveries?per_page=20",
}
LIMIT = 200_000


def read_json(path, token):
    if path not in READ_PATHS:
        raise ValueError("scope_invalid")
    request = urllib.request.Request(driver.API + path, method="GET", headers={
        "Accept": "application/vnd.github+json",
        "Authorization": f"Bearer {token}",
        "User-Agent": "schlange-sandbox-driver",
        "X-GitHub-Api-Version": "2022-11-28",
    })
    with driver.NO_REDIRECT_OPENER.open(request, timeout=30) as response:
        raw = response.read(LIMIT + 1)
        if len(raw) > LIMIT:
            raise ValueError("oversize")
        return json.loads(raw)


def route_kind(value):
    if value is None or value == "":
        return "empty"
    if not isinstance(value, str):
        return "unsupported"
    parsed = urllib.parse.urlsplit(value)
    if parsed.scheme != "https" or not parsed.hostname or parsed.username or parsed.password:
        return "unsupported"
    if parsed.hostname == "smee.io":
        return "smee_relay"
    return "https_other"


def inspect():
    report = {
        "schema": 1, "organization": "schlange-sandbox", "complete": False,
        "workflow_run_id": os.environ.get("GITHUB_RUN_ID"),
        "workflow_commit": os.environ.get("GITHUB_SHA"),
        "workflow_run_attempt": os.environ.get("GITHUB_RUN_ATTEMPT"),
    }
    try:
        token = driver.app_jwt("queue")
        expected = driver.app_meta("queue")["id"]
        app = read_json("/app", token)
        if type(expected) is not int or app.get("id") != expected:
            report["reason"] = "app_identity_invalid"
            return report
        installations = read_json("/app/installations?per_page=2", token)
        if (not isinstance(installations, list) or len(installations) != 1
                or installations[0].get("app_id") != expected
                or (installations[0].get("account") or {}).get("login") != "schlange-sandbox"
                or type(installations[0].get("id")) is not int
                or installations[0]["id"] <= 0):
            report["reason"] = "installation_scope_unverified"
            return report
        installation_id = installations[0]["id"]
        config = read_json("/app/hook/config", token)
        deliveries = read_json("/app/hook/deliveries?per_page=20", token)
        events = app.get("events")
        if not isinstance(deliveries, list) or len(deliveries) > 20 or not isinstance(events, list):
            report["reason"] = "response_invalid"
            return report
        selected = [row for row in deliveries if isinstance(row, dict)
                    and row.get("installation_id") == installation_id
                    and row.get("event") in ("issue_comment", "pull_request")]
        statuses = [row.get("status_code") for row in selected
                    if type(row.get("status_code")) is int]
        report.update({
            "complete": True, "route_kind": route_kind(config.get("url")),
            "configured_json_payload": config.get("content_type") == "json",
            "verifies_tls": config.get("insecure_ssl") in ("0", 0),
            "subscribes_issue_comment": "issue_comment" in events,
            "subscribes_pull_request": "pull_request" in events,
            "recent_sandbox_delivery_count": len(selected),
            "recent_sandbox_delivery_successes": sum(200 <= code < 300 for code in statuses),
        })
    except (AttributeError, KeyError, OSError, TypeError, ValueError, urllib.error.URLError):
        report["reason"] = "readonly_request_failed"
    return report


if __name__ == "__main__":
    if os.environ.get("GITHUB_ACTIONS") != "true":
        raise SystemExit(64)
    print(json.dumps(inspect(), indent=2))
