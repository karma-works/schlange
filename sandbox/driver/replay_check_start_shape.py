#!/usr/bin/env python3
"""Read only the archived S03 Activity Log interval. Emit field shapes, never raw rows."""
import importlib.util
import json
import re
from pathlib import Path

spec = importlib.util.spec_from_file_location("sandbox_driver", Path(__file__).with_name("sandbox.py"))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

state = {
    "org": "schlange-sandbox", "repo": "queue-lab",
    "run_id": "s03-queue-lab-20261123-100112", "timed_out": False,
    "started": "2026-11-23T10:01:12Z", "finished": "2026-11-23T10:22:44Z",
    "prs": {key: {"number": number} for key, number in zip("abcd", (322, 323, 324, 325))},
}
selected = {row["number"] for row in state["prs"].values()}

def ident(value):
    if value is None:
        return "missing"
    if isinstance(value, str) and re.fullmatch(r"[0-9a-f]{8}(?:-[0-9a-f]{4}){3}-[0-9a-f]{12}", value):
        return "uuid"
    return type(value).__name__


class Probe:
    def __init__(self):
        self.client = driver.mergify_client()
        self.rows = []
        self.runs = {}

    def activity_page(self, url):
        page, headers = self.client.activity_page(url)
        for row in page.get("events", [])[:100]:
            if not isinstance(row, dict):
                self.rows.append({"row_type": type(row).__name__})
                continue
            meta = row.get("metadata") or {}
            members = meta.get("pull_request_numbers")
            check = meta.get("speculative_check_pull_request") or {}
            pr = row.get("pull_request")
            number = check.get("number")
            sha = check.get("head_sha")
            parents = meta.get("parent_batch_ids")
            data = {
                "known_repository": row.get("repository") in ("queue-lab", "schlange-sandbox/queue-lab"),
                "known_type": row.get("type") == "action.queue.checks_start",
                "pr": pr if type(pr) is int and pr in selected else None,
                "pr_outside": pr is not None and (type(pr) is not int or pr not in selected),
                "member_list": isinstance(members, list),
                "field_locations": {
                    location: {
                        field: type(fields[field]).__name__
                        for field in ("pull_request_numbers", "pull_requests", "in_place", "kind", "type",
                                      "checks", "checks_state", "batch", "queue_branch_name")
                        if field in fields
                    }
                    for location, fields in (("event", row), ("metadata", meta), ("check", check),
                                             ("check_state", meta.get("checks_state")))
                    if isinstance(fields, dict)
                },
                "nested_selected_members": [
                    n for n in check.get("pull_request_numbers", [])
                    if type(n) is int and n in selected
                ] if isinstance(check.get("pull_request_numbers"), list) else [],
                "nested_in_place": check.get("in_place") if type(check.get("in_place")) is bool else None,
                "selected_members": [n for n in members if type(n) is int and n in selected] if isinstance(members, list) else [],
                "member_count": len(members) if isinstance(members, list) else 0,
                "in_place": meta.get("in_place") if type(meta.get("in_place")) is bool else None,
                "draft_number_valid": type(number) is int and number > 0,
                "draft_number_selected": type(number) is int and number in selected,
                "head_sha_valid": isinstance(sha, str) and bool(re.fullmatch(r"[0-9a-f]{40}", sha)),
                "run_id_type": ident(meta.get("run_id")),
                "batch_id_type": ident(meta.get("batch_id")),
                "session_id_type": ident(meta.get("session_id")),
                "parents_type": type(parents).__name__,
                "parent_id_types": [ident(value) for value in parents[:128]] if isinstance(parents, list) else [],
                "parents_unique": len({value for value in parents if isinstance(value, str)}) == len(parents)
                                  if isinstance(parents, list) else None,
                "parent_contains_batch": meta.get("batch_id") in parents if isinstance(parents, list) else None,
            }
            try:
                item, _ = driver._public_activity_event(
                    row, selected, driver._activity_time(state["started"]),
                    driver._activity_time(state["finished"]), driver.CHECK_START_TYPES,
                )
            except driver._ReferenceActivityError as error:
                data["validation"] = error.args[0]
            else:
                data["validation"] = "accepted"
                identity = (item["batch_id"], item["pull_request_numbers"], item["draft_pull_request"],
                            item["head_sha"], item["in_place"])
                previous = self.runs.setdefault(item["run_id"], identity)
                data["same_run_field_changes"] = [
                    key for key, a, b in zip(("batch", "members", "draft", "head", "in_place"), previous, identity)
                    if a != b
                ]
            self.rows.append(data)
        return page, headers


if __name__ == "__main__":
    probe = Probe()
    result = driver.capture_reference_check_starts(state, probe, driver.now_iso())
    print(json.dumps({"archived_run": 45150473573, "validation": result["incomplete_reason"],
                      "rows": probe.rows[:150]}, indent=2))
