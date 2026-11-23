#!/usr/bin/env python3
"""Check recorded S03 draft heads against the same sandbox PR and CI identities."""
import importlib.util
import json
import os
from pathlib import Path

path = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("sandbox_driver", path / "sandbox.py")
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

REPO = "schlange-sandbox/queue-lab"
ROOT = f"/repos/{REPO}"


def inspect():
    if os.environ.get("GITHUB_ACTIONS") != "true":
        return {"complete": False, "reason": "actions_required"}
    expected = json.loads((path / "s03_head_crosscheck.json").read_text())
    report = {"complete": False, "repository": REPO,
              "archived_workflow_run": expected["archived_workflow_run"],
              "archived_workflow_commit": expected["archived_workflow_commit"],
              "inspection_workflow_run": os.environ.get("GITHUB_RUN_ID"),
              "inspection_workflow_commit": os.environ.get("GITHUB_SHA"), "drafts": []}
    try:
        client = driver.human_client()
        for draft in expected["drafts"]:
            number = draft["number"]
            data = client.read_json(f"{ROOT}/pulls/{number}")
            head = data.get("head") or {}
            base = data.get("base") or {}
            identity = (data.get("number") == number and data.get("draft") is True
                        and (data.get("user") or {}).get("login") == "mergify[bot]"
                        and (head.get("ref") or "").startswith("mergify/merge-queue/")
                        and base.get("ref") == "main"
                        and (base.get("repo") or {}).get("full_name") == REPO)
            row = {"number": number, "draft_identity_equal": identity,
                   "head_equal": identity and head.get("sha") == draft["head"], "checks": []}
            for check in draft["checks"]:
                observed = client.read_json(f"{ROOT}/check-runs/{check['id']}")
                row["checks"].append({
                    "identity_equal": observed.get("id") == check["id"]
                    and observed.get("name") == "ci-ok"
                    and (observed.get("app") or {}).get("slug") == "github-actions",
                    "head_equal": observed.get("head_sha") == draft["head"],
                    "conclusion_equal": observed.get("status") == "completed"
                    and observed.get("conclusion") == check["conclusion"],
                })
            report["drafts"].append(row)
        report["complete"] = len(report["drafts"]) == 5
        report["all_equal"] = report["complete"] and all(
            row["draft_identity_equal"] and row["head_equal"] and row["checks"]
            and all(all(item.values()) for item in row["checks"]) for row in report["drafts"]
        )
    except (AttributeError, KeyError, OSError, RuntimeError, TypeError, ValueError):
        report["reason"] = "read_failed"
    return report


if __name__ == "__main__":
    print(json.dumps(inspect(), indent=2))
