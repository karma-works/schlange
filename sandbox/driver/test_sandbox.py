import importlib.util
import io
import unittest
import json
import tempfile
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('sandbox_driver', Path(__file__).with_name('sandbox.py'))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class CheckpointTests(unittest.TestCase):
    def test_api_error_does_not_expose_response_text(self):
        error = driver.urllib.error.HTTPError('https://example.invalid', 400, 'bad', {},
                                               io.BytesIO(b'private upstream response'))
        with patch.object(driver.urllib.request, 'urlopen', side_effect=error):
            with self.assertRaises(RuntimeError) as caught:
                driver.Client(lambda: ('token', 100), 'test').call('GET', '/test')
        self.assertNotIn('private upstream response', str(caught.exception))
        self.assertIn('400', str(caught.exception))

    def test_public_record_summary_excludes_comment_text(self):
        summary = {'prs': {'a': {'number': 1, 'end': 'open', 'labels': [],
                                 'bot_comments': ['private upstream text']}},
                   'queue_prs': [{'number': 2, 'commits': 1, 'ci_ok': 'private conclusion'}],
                   'main_commits': ['private upstream commit text']}
        rendered = json.dumps(driver.public_record_summary(summary))
        self.assertNotIn('private upstream text', rendered)
        self.assertNotIn('private upstream commit text', rendered)
        self.assertNotIn('private conclusion', rendered)
        self.assertIn('open', rendered)

    def test_compatibility_comparison_rejects_missing_queue_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            baseline = Path(directory) / 'baseline.json'
            actual = Path(directory) / 'actual.json'
            pr = {'end': 'open', 'queued': False, 'enter_count': 0, 'dequeue_codes': []}
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': []}))
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'queued': None}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual), 1)
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'enter_count': False}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual), 1)

    def test_explicit_compatibility_mode_rejects_invalid_top_level(self):
        with tempfile.TemporaryDirectory() as directory:
            baseline = Path(directory) / 'baseline.json'
            actual = Path(directory) / 'actual.json'
            pr = {'end': 'open', 'queued': False, 'enter_count': 0, 'dequeue_codes': []}
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': []}))
            actual.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 1)
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {}, 'landed': [], 'batches': []}))
            baseline.write_text(json.dumps({'prs': {}, 'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 1)
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': [],
                                            'known_deviations': [{'pr': 'a', 'field': 'end',
                                                                  'observed': 'merged', 'why': 'documented'}]}))
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'end': 'merged'}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 0)
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True,
                                                     expected_prs={'a', 'b'}), 1)

    def test_failed_base_push_persists_start_checkpoint(self):
        run = object.__new__(driver.Run)
        run.run_id = 'run-123'
        run.checkpoints = []
        run.actors = {'human': object()}
        run.save_state = unittest.mock.Mock()
        run.commit_files = unittest.mock.Mock(side_effect=RuntimeError('write failed'))
        with self.assertRaises(RuntimeError):
            run.step_main_commit({'main_commit': {'x': '{run}'}})
        self.assertEqual(run.checkpoints[0]['kind'], 'base_push_started')
        run.save_state.assert_called_once_with()

    def test_wait_queued_requires_an_unmatched_entry_and_fails_on_timeout(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.checkpoints = []
        run.timed_out = False
        run.save_state = unittest.mock.Mock()
        run.pr_state = lambda _: ('open', '')
        run.queue_events = lambda: [{'pull_request': 42, 'type': 'action.queue.enter', 'received_at': '2026-01-01T00:00:00Z'}]
        run.step_wait({'wait': 'queued', 'prs': ['a'], 'timeout': 1})
        self.assertEqual(run.checkpoints[0]['events'][0]['received_at'], '2026-01-01T00:00:00Z')
        run.queue_events = lambda: [
            {'pull_request': 42, 'type': 'action.queue.enter'},
            {'pull_request': 42, 'type': 'action.queue.leave'},
        ]
        self.assertFalse(driver.currently_queued(run.queue_events(), 42))
        self.assertTrue(driver.currently_queued(run.queue_events() + [
            {'pull_request': 42, 'type': 'action.queue.enter'}], 42))
        with self.assertRaises(RuntimeError):
            run.step_wait({'wait': 'queued', 'prs': ['a'], 'timeout': 0})
        self.assertTrue(run.timed_out)
        run.save_state.assert_called_once_with()

    def test_reference_driver_exits_nonzero_after_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            scenario = Path(directory) / 'scenario.json'
            scenario.write_text(json.dumps({'id': 'S99', 'title': 'test', 'steps': []}))
            args = unittest.mock.Mock(scenario=str(scenario), no_wait=True, org='schlange-sandbox',
                                      repo='queue-lab', keep=True, baseline=None)
            run = unittest.mock.Mock(timed_out=True)
            with patch.object(driver, 'RUNS', Path(directory)), patch.object(driver, 'Run', return_value=run), \
                 patch.object(driver, 'record'), patch.object(driver, 'write_outcome'), \
                 patch.dict(driver.os.environ, {'SANDBOX_FAIL_ON_TIMEOUT': '1'}):
                with self.assertRaises(SystemExit) as caught:
                    driver.cmd_run(args)
            self.assertEqual(caught.exception.code, 1)

    def test_incomplete_event_log_fails_closed(self):
        class Client:
            def call(self, *_):
                return {'next_cursor': 'more'}
        with self.assertRaises(RuntimeError):
            driver.fetch_events(Client(), 'url', strict=True)
        class MalformedClient:
            def call(self, *_):
                return {'events': [{}]}
        with self.assertRaises(RuntimeError):
            driver.fetch_events(MalformedClient(), 'url', strict=True)

    def test_rejects_queued_or_closed_pull_request(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.pr_state = lambda _: ('open', '')
        run.queue_events = lambda: []
        run.step_assert_unqueued({'assert_unqueued': 'a'})
        run.queue_events = lambda: [{'pull_request': 42, 'type': 'queue.enter'}]
        with self.assertRaises(RuntimeError):
            run.step_assert_unqueued({'assert_unqueued': 'a'})
        run.queue_events = lambda: []
        run.pr_state = lambda _: ('merged', '')
        with self.assertRaises(RuntimeError):
            run.step_assert_unqueued({'assert_unqueued': 'a'})

    def test_file_contents_are_unique_per_run(self):
        run = object.__new__(driver.Run)
        run.run_id = 'run-123'
        run.flag_path = lambda _: 'ci/flags/test'
        run.flag_lines = lambda _: ''
        with patch.object(run, 'commit_files') as commit:
            run.write_changes(None, 'branch', 'a', {'files': {'governance/rule.txt': 'change {run}'}}, 1)
        self.assertEqual(commit.call_args.args[2]['governance/rule.txt'], 'change run-123')


if __name__ == '__main__':
    unittest.main()
