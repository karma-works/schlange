"""Offline reference history must be bound and lifecycle-complete."""
import copy
import importlib.util
import json
import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / 'check-reference-history.py'
SPEC = importlib.util.spec_from_file_location('check_reference_history', SCRIPT)
checker = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(checker)


class ReferenceHistoryTests(unittest.TestCase):
    def setUp(self):
        self.source = {
            'GITHUB_ACTIONS': 'true', 'GITHUB_RUN_ID': '45140670693',
            'GITHUB_RUN_ATTEMPT': '1',
            'GITHUB_SHA': 'fc3920aac0efd3ef3b42bbe0cf66165007dfb8c2',
            'GITHUB_REPOSITORY': 'karma-works/schlange',
        }
        self.state = {
            'org': 'schlange-sandbox', 'repo': 'queue-lab',
            'run_id': 's18-queue-lab-20261123-084422',
            'started': '2026-11-23T08:44:22Z',
            'finished': '2026-11-23T09:06:54Z', 'timed_out': False,
            'prs': {'a': {'number': 309}},
        }
        self.outcome = {
            'scenario': 'S18', 'repo': 'queue-lab',
            'prs': {'a': {'end': 'merged', 'queued': True, 'enter_count': 2,
                          'dequeue_codes': ['PR_MANUALLY_DEQUEUED'],
                          'merged_by_mergify': True}},
            'landed': ['a'], 'batches': [{'prs': ['a'], 'ci_ok': 'success'}],
        }
        self.activity = {
            'schema': 1, 'repository': 'schlange-sandbox/queue-lab',
            'scenario_run_id': self.state['run_id'],
            'scenario_finished_at': self.state['finished'],
            'collected_at': '2026-11-23T09:07:04Z',
            'source': {'workflow_run_id': 45140670693, 'workflow_run_attempt': 1,
                       'workflow_commit': self.source['GITHUB_SHA'],
                       'workflow_repository': self.source['GITHUB_REPOSITORY']},
            'query': {'received_from': self.state['started'],
                      'received_to': self.state['finished'], 'pull_requests': [309],
                      'event_types': ['action.queue.enter', 'action.queue.leave',
                                      'action.queue.merged']},
            'complete': True, 'incomplete_reason': None,
            'events': [
                {'id': 157063713, 'received_at': '2026-11-23T08:45:13.491495Z',
                 'pull_request': 309, 'kind': 'enter'},
                {'id': 157063844, 'received_at': '2026-11-23T08:45:41.729005Z',
                 'pull_request': 309, 'kind': 'leave', 'merged': False,
                 'reason_code': 'PR_MANUALLY_DEQUEUED'},
                {'id': 157064046, 'received_at': '2026-11-23T08:46:12.810411Z',
                 'pull_request': 309, 'kind': 'enter'},
                {'id': 157067190, 'received_at': '2026-11-23T08:56:19.375529Z',
                 'pull_request': 309, 'kind': 'leave', 'merged': True,
                 'reason_code': 'PR_MERGED'},
                {'id': 157067191, 'received_at': '2026-11-23T08:56:19.447652Z',
                 'pull_request': 309, 'kind': 'merged',
                 'merge_commit_sha': '2fc739928ec155cd08bb9434ff93167e5f349a58'},
            ],
        }

    def verify(self):
        return checker.verify_run(self.state, self.outcome, self.activity, self.source)

    def test_live_s18_merge_leave_and_merged_record_form_one_terminal_merge(self):
        result = self.verify()
        self.assertEqual(result, {
            'scenario_run_id': self.state['run_id'],
            'pull_requests': [{
                'number': 309, 'enter_count': 2,
                'non_merge_dequeue_codes': ['PR_MANUALLY_DEQUEUED'],
                'terminal': 'merged',
            }],
        })

    def test_ids_need_only_be_unique_and_equal_times_are_allowed_across_prs(self):
        self.state['prs']['b'] = {'number': 310}
        self.outcome['prs']['b'] = {
            'end': 'open', 'queued': True, 'enter_count': 1,
            'dequeue_codes': [], 'merged_by_mergify': False}
        self.activity['query']['pull_requests'] = [309, 310]
        self.activity['events'].insert(1, {
            'id': 5, 'received_at': self.activity['events'][0]['received_at'],
            'pull_request': 310, 'kind': 'enter'})
        for event, event_id in zip(self.activity['events'], [5, 50, 40, 60, 30, 70]):
            event['id'] = event_id

        result = self.verify()

        self.assertEqual([row['number'] for row in result['pull_requests']], [309, 310])
        self.assertEqual(result['pull_requests'][1]['enter_count'], 1)

    def test_rejects_missing_or_inconsistent_terminal_merge_evidence(self):
        mutations = [
            lambda a: a['events'].pop(),
            lambda a: a['events'].pop(-2),
            lambda a: a['events'][-2].update(merged=False),
            lambda a: a['events'][-1].update(kind='enter'),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                original = copy.deepcopy(self.activity)
                mutate(self.activity)
                with self.assertRaises(checker.CheckError):
                    self.verify()
                self.activity = original

    def test_rejects_unbound_source_repo_pr_set_interval_and_unfinished_run(self):
        mutations = [
            lambda: self.activity['source'].update(workflow_run_id=1),
            lambda: self.activity['source'].update(workflow_run_attempt=2),
            lambda: self.activity['source'].update(workflow_run_attempt=True),
            lambda: self.activity['source'].update(workflow_commit='a' * 40),
            lambda: self.activity['source'].update(workflow_repository='other/repo'),
            lambda: self.activity.update(schema=True),
            lambda: self.activity.update(repository='other/repo'),
            lambda: self.activity['query'].update(pull_requests=[310]),
            lambda: self.activity['query'].update(pull_requests=[True]),
            lambda: self.activity['query'].update(received_from='2026-11-23T08:44:23Z'),
            lambda: self.activity.update(scenario_run_id='s18-other'),
            lambda: self.state.update(timed_out=True),
            lambda: self.activity.update(complete=False, incomplete_reason='transport'),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                state, activity = copy.deepcopy(self.state), copy.deepcopy(self.activity)
                mutate()
                with self.assertRaises(checker.CheckError):
                    self.verify()
                self.state, self.activity = state, activity

    def test_rejects_malformed_or_impossible_lifecycle_and_outcome(self):
        mutations = [
            lambda: self.activity['events'][1].update(id=self.activity['events'][0]['id']),
            lambda: self.activity['events'][1].update(
                received_at=self.activity['events'][0]['received_at']),
            lambda: self.activity['events'].insert(0, self.activity['events'].pop(1)),
            lambda: self.activity['events'][2].update(kind='leave', merged=False,
                                                       reason_code='CHECKS_FAILED'),
            lambda: (self.activity['events'][1].update(reason_code='PR_MERGED'),
                     self.outcome['prs']['a'].update(dequeue_codes=['PR_MERGED'])),
            lambda: self.outcome['prs']['a'].update(enter_count=1),
            lambda: self.outcome['prs']['a'].update(dequeue_codes=[]),
            lambda: self.outcome['prs']['a'].update(end='open'),
            lambda: self.outcome['prs']['a'].pop('enter_count'),
            lambda: self.outcome.update(landed=[{}]),
        ]
        for mutate in mutations:
            with self.subTest(mutate=mutate):
                activity, outcome = copy.deepcopy(self.activity), copy.deepcopy(self.outcome)
                mutate()
                with self.assertRaises(checker.CheckError):
                    self.verify()
                self.activity, self.outcome = activity, outcome

    def test_closed_outcome_cannot_leave_the_pull_request_active(self):
        self.activity['events'] = self.activity['events'][:3]
        self.outcome['prs']['a'].update(end='closed', merged_by_mergify=False)
        self.outcome['landed'] = []
        with self.assertRaises(checker.CheckError):
            self.verify()

    def test_cli_writes_separate_structured_result_and_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            run = root / 'runs' / self.state['run_id']
            run.mkdir(parents=True)
            for name, value in [('run.json', self.state), ('outcome.json', self.outcome),
                                ('reference-activity.json', self.activity)]:
                (run / name).write_text(json.dumps(value))
            destination = root / 'reference-history-check.json'
            result = subprocess.run(
                ['python3', '-I', str(SCRIPT), str(root / 'runs'), str(destination)],
                env={**os.environ, **self.source}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            artifact = json.loads(destination.read_text())
            self.assertTrue(artifact['complete'])
            self.assertNotIn('agree', json.dumps(artifact).lower())
            self.assertEqual(artifact['runs'][0]['pull_requests'][0]['enter_count'], 2)

            self.activity['events'].pop()
            (run / 'reference-activity.json').write_text(json.dumps(self.activity))
            result = subprocess.run(
                ['python3', '-I', str(SCRIPT), str(root / 'runs'), str(destination)],
                env={**os.environ, **self.source}, capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            artifact = json.loads(destination.read_text())
            self.assertFalse(artifact['complete'])
            self.assertEqual(artifact['runs'], [])
            self.assertIn(artifact['incomplete_reason'], checker.INCOMPLETE_REASONS)

    def test_workflow_runs_offline_check_separately_before_shadow_verifier(self):
        workflow = (SCRIPT.parents[2] / '.github' / 'workflows' / 'sandbox.yml').read_text()
        history = workflow.index('- name: Check same-run reference histories')
        shadow = workflow.index('- name: Verify shadow report against independent Mergify recordings')
        self.assertLess(history, shadow)
        block = workflow[history:shadow]
        self.assertIn('if: ${{ always() && inputs.shadow_mode', block)
        self.assertIn('check-reference-history.py', block)
        self.assertIn('reference-history-check.json', block)
        self.assertIn('continue-on-error: true', block)

    def test_combined_check_rejects_duplicate_run_ids_and_prs_across_directories(self):
        with tempfile.TemporaryDirectory() as directory:
            runs = Path(directory)

            def write(name, state, outcome, activity):
                target = runs / name
                target.mkdir()
                for filename, value in [('run.json', state), ('outcome.json', outcome),
                                        ('reference-activity.json', activity)]:
                    (target / filename).write_text(json.dumps(value))

            write('first', self.state, self.outcome, self.activity)
            write('duplicate', self.state, self.outcome, self.activity)
            result = checker.check(runs, self.source)
            self.assertFalse(result['complete'])
            self.assertEqual(result['runs'], [])

            second_state = copy.deepcopy(self.state)
            second_outcome = copy.deepcopy(self.outcome)
            second_activity = copy.deepcopy(self.activity)
            second_state['run_id'] = 's19-queue-lab-20261123-094422'
            second_outcome['scenario'] = 'S19'
            second_activity['scenario_run_id'] = second_state['run_id']
            for path in (runs / 'duplicate').iterdir():
                path.unlink()
            (runs / 'duplicate').rmdir()
            write('same-pr', second_state, second_outcome, second_activity)
            result = checker.check(runs, self.source)
            self.assertFalse(result['complete'])
            self.assertEqual(result['runs'], [])


if __name__ == '__main__':
    unittest.main()
