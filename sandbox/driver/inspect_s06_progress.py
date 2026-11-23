#!/usr/bin/env python3
"""Read structural progress for one known in-flight queue-lab S06 fixture PR."""
from collections import Counter
import importlib.util
import json
import os
from pathlib import Path
import re

spec = importlib.util.spec_from_file_location("sandbox_driver", Path(__file__).with_name("sandbox.py"))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

REPOSITORY = "schlange-sandbox/queue-lab"
PULL = 336
CHECKS = ("ci-ok", "Risk class and data protection", "KI-Review")


def inspect():
    if os.environ.get("GITHUB_ACTIONS") != "true":
        return {"complete": False, "reason": "actions_required"}
    report = {"complete": False, "repository": REPOSITORY, "pull_request": PULL}
    try:
        client = driver.human_client()
        root = f"/repos/{REPOSITORY}"
        pull = client.read_json(f"{root}/pulls/{PULL}")
        head = pull.get("head") or {}
        ref = head.get("ref")
        sha = head.get("sha")
        if (pull.get("number") != PULL or not re.fullmatch(r"sbx/s06-queue-lab-[0-9-]+/a", ref or "")
                or (head.get("repo") or {}).get("full_name") != REPOSITORY
                or (pull.get("base") or {}).get("ref") != "main"
                or not re.fullmatch(r"[0-9a-f]{40}", sha or "")):
            return {**report, "reason": "selected_pull_unconfirmed"}
        page = client.read_json(f"{root}/commits/{sha}/check-runs?filter=all&per_page=100")
        runs = page.get("check_runs")
        if not isinstance(runs, list) or page.get("total_count") != len(runs) or len(runs) > 100:
            return {**report, "reason": "check_history_incomplete"}
        rows = {}
        for name in CHECKS:
            candidates = [run for run in runs if isinstance(run, dict) and run.get("name") == name
                          and run.get("head_sha") == sha and type(run.get("id")) is int
                          and (run.get("app") or {}).get("slug") == "github-actions"]
            latest = max(candidates, key=lambda run: run["id"], default={})
            status = latest.get("status")
            conclusion = latest.get("conclusion")
            rows[name] = {"present": bool(latest),
                          "status": status if status in ("queued", "in_progress", "completed", "pending") else None,
                          "conclusion": conclusion if conclusion in driver.CHECK_CONCLUSIONS else None}
        counts = Counter()
        for run in runs:
            if isinstance(run, dict) and run.get("head_sha") == sha:
                slug = (run.get("app") or {}).get("slug")
                app = slug if slug in ("github-actions", "mergify") else "other"
                conclusion = run.get("conclusion")
                if conclusion not in driver.CHECK_CONCLUSIONS:
                    conclusion = "pending_or_unknown"
                counts[f"{app}/{conclusion}"] += 1
        cancelled_reference_roles = []
        for item in runs:
            if (isinstance(item, dict) and item.get("head_sha") == sha
                    and item.get("conclusion") == "cancelled"
                    and (item.get("app") or {}).get("slug") == "mergify"):
                name = item.get("name")
                value = "queue_check" if name == "Mergify Merge Queue" else (
                    "rule_check" if isinstance(name, str) and name.startswith("Rule: ") else "other")
                cancelled_reference_roles.append(value)
        status_page = client.read_json(f"{root}/commits/{sha}/status?per_page=100")
        statuses = status_page.get("statuses")
        status_complete = isinstance(statuses, list) and status_page.get("total_count") == len(statuses)
        status_counts = Counter()
        if status_complete:
            for status in statuses:
                mode = status.get("state") if isinstance(status, dict) else None
                status_counts[mode if mode in ("pending", "success", "failure", "error") else "other"] += 1
        stamped = driver.now_iso()
        state = {"org": "schlange-sandbox", "repo": "queue-lab", "run_id": ref.split("/")[1],
                 "started": "2026-11-23T10:24:09Z", "finished": stamped,
                 "timed_out": False, "prs": {"a": {"number": PULL}}}
        reference = driver.mergify_client()
        activity = driver.capture_reference_activity(state, reference, stamped)
        unqueued_state = "unavailable"
        try:
            unqueued, headers = reference.activity_page(
                f"{driver.MERGIFY_API}/repos/{REPOSITORY}/merge-queue/unqueued-pulls?branch=main&per_page=100")
            items = unqueued.get("pull_requests")
            target_states = [item.get("state") for item in items if isinstance(item, dict)
                             and item.get("number") == PULL] if isinstance(items, list) else []
            if target_states in (["waiting_for_conditions"], ["ready"]):
                unqueued_state = target_states[0]
            elif isinstance(items, list) and not target_states and not driver._activity_link_header(headers):
                unqueued_state = "absent"
        except (AttributeError, RuntimeError, TypeError, ValueError):
            pass
        return {**report, "complete": activity["complete"], "observed_at": stamped,
                "reference_incomplete_reason": activity["incomplete_reason"],
                "open": pull.get("state") == "open", "merged": pull.get("merged") is True,
                "mergeable": pull.get("mergeable") if type(pull.get("mergeable")) is bool else None,
                "checks_on_current_head": rows,
                "check_conclusion_counts": counts,
                "cancelled_reference_check_roles": cancelled_reference_roles,
                "reference_unqueued_state": unqueued_state,
                "commit_statuses_complete": status_complete,
                "commit_status_counts": status_counts,
                "lifecycle": [{key: event[key] for key in ("kind", "received_at", "reason_code") if key in event}
                              for event in activity["events"]]}
    except (AttributeError, KeyError, OSError, RuntimeError, TypeError, ValueError):
        return {**report, "reason": "read_failed"}


if __name__ == "__main__":
    print(json.dumps(inspect(), indent=2))
