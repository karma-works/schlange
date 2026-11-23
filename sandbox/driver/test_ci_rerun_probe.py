import importlib.util
import json
import unittest
from pathlib import Path
from unittest.mock import Mock, call, patch

SPEC = importlib.util.spec_from_file_location("sandbox_driver", Path(__file__).with_name("sandbox.py"))
driver = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(driver)


class SameHeadRerunProbeTests(unittest.TestCase):
    head = "a" * 40
    repo = "/repos/schlange-sandbox/queue-lab"

    def probe(self):
        run = object.__new__(driver.Run)
        client = Mock()
        run.org, run.repo, run.r = "schlange-sandbox", "queue-lab", self.repo
        run.actors = {"human": client}
        run.prs = {"a": {"number": 31}}
        run.started = "2026-11-23T08:00:00Z"
        run.checkpoints = [{"kind": "queued", "prs": ["a"]}]
        run.save_state = Mock()
        pr = {"state": "open", "draft": False, "head": {"sha": self.head}}
        first = {"id": 90, "head_sha": self.head, "status": "completed", "run_attempt": 1}
        new = {**first, "status": "in_progress", "run_attempt": 2}
        prior_check = {"id": 44, "name": "ci-ok", "head_sha": self.head, "status": "completed",
                       "conclusion": "success", "app": {"slug": "github-actions"}}
        pending_job = {"name": "ci-ok", "head_sha": self.head, "status": "queued", "run_id": 90,
                       "check_run_url": f"{driver.API}{self.repo}/check-runs/45"}
        green_job = {**pending_job, "status": "completed", "conclusion": "success"}
        pending_check = {**prior_check, "id": 45, "status": "queued", "conclusion": None}
        green_check = {**prior_check, "id": 45}
        client.call.side_effect = [
            pr, first, None, pr, new, pending_check,
            pr, {**new, "status": "completed"}, green_check,
        ]
        client.paged.side_effect = [[prior_check], [pending_job], [green_job]]
        return run, client

    def test_rerun_records_pending_and_green_on_the_same_head_and_attempt(self):
        run, client = self.probe()
        with patch.object(driver, "workflow_check_identity", return_value=90),              patch.object(driver.time, "sleep"):
            run.step_rerun_ci({"rerun_ci": "a"})
        self.assertEqual([point["kind"] for point in run.checkpoints],
                         ["queued", "ci_rerun_requested", "ci_rerun_pending", "ci_rerun_green"])
        self.assertTrue(all(point["head"] == self.head for point in run.checkpoints[1:]))
        self.assertEqual(run.checkpoints[-2]["run_attempt"], 2)
        self.assertEqual(run.checkpoints[-2]["check_id"], 45)
        self.assertEqual(run.checkpoints[-1]["check_id"], 45)
        client.call.assert_has_calls([call("POST", f"{self.repo}/actions/runs/90/rerun", retry=False)])
        client.paged.assert_has_calls([call(f"{self.repo}/actions/runs/90/attempts/2/jobs")] * 2)
        client.call.assert_has_calls([call("GET", f"{self.repo}/check-runs/45")] * 2, any_order=True)

    def test_wrong_app_rerun_check_is_refused(self):
        run, client = self.probe()
        responses = list(client.call.side_effect)
        responses[5] = {**responses[5], "app": {"slug": "another-app"}}
        client.call.side_effect = responses
        with patch.object(driver, "workflow_check_identity", return_value=90), \
             self.assertRaisesRegex(RuntimeError, "named check on the original head"):
            run.step_rerun_ci({"rerun_ci": "a"})
        self.assertEqual(run.checkpoints[-1]["kind"], "ci_rerun_requested")

    def test_head_change_after_rerun_cannot_be_certified(self):
        run, client = self.probe()
        pr = {"state": "open", "draft": False, "head": {"sha": self.head}}
        moved = {**pr, "head": {"sha": "b" * 40}}
        client.call.side_effect = [
            pr, {"id": 90, "head_sha": self.head, "status": "completed", "run_attempt": 1}, None, moved,
        ]
        with patch.object(driver, "workflow_check_identity", return_value=90),              self.assertRaisesRegex(RuntimeError, "head changed"):
            run.step_rerun_ci({"rerun_ci": "a"})
        self.assertEqual([point["kind"] for point in run.checkpoints],
                         ["queued", "ci_rerun_requested"])

    def test_without_confirmed_entry_it_never_requests_a_rerun(self):
        run, client = self.probe()
        run.checkpoints.clear()
        with self.assertRaisesRegex(RuntimeError, "queue entry"):
            run.step_rerun_ci({"rerun_ci": "a"})
        client.call.assert_not_called()

    def test_probe_uses_delayed_ci_and_reaches_quiet_checkpoint(self):
        fixture = Path(__file__).resolve().parents[1] / "scenarios/s20-queued-ci-rerun.json"
        scenario = json.loads(fixture.read_text())
        self.assertEqual(scenario["steps"][1]["wait"], "queued")
        self.assertEqual(scenario["steps"][2]["rerun_ci"], "a")
        self.assertEqual(scenario["steps"][-1]["wait"], "quiet")


if __name__ == "__main__":
    unittest.main()
