"""The shadow observer must agree with the independent driver, not just with itself."""
import copy
import json
import os
import signal
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / 'check-shadow-report.py'
START = SCRIPT.parent / 'start-schlange.sh'


class ShadowEvidenceTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.recording = self.root / 'runs' / 's01'
        self.recording.mkdir(parents=True)
        self.state = {'org': 'schlange-sandbox', 'repo': 'queue-lab',
                      'run_id': 's01-queue-lab-test', 'finished': '2026-11-03T06:00:00Z',
                      'timed_out': False, 'prs': {'a': {'number': 42}}}
        self.outcome = {'repo': 'queue-lab', 'scenario': 'S01', 'landed': ['a'],
                        'prs': {'a': {'end': 'merged', 'queued': True,
                                      'enter_count': 1, 'dequeue_codes': [],
                                      'merged_by_mergify': True}},
                        'batches': [{'prs': ['a'], 'ci_ok': 'success'}]}
        same = lambda value: {'schlange': value, 'mergify': value, 'agree': True}
        self.report = {'repository': 'schlange-sandbox/queue-lab', 'agreements': 1,
                       'disagreements': 0, 'inconclusive': 0, 'pending': 0,
                       'pull_requests': [{'number': 42, 'verdict': 'agree',
                                          'queued': same(True), 'landed': same(True),
                                          'order': same(1), 'batch': same([42]),
                                          'differences': []}]}

    def check(self):
        (self.recording / 'run.json').write_text(json.dumps(self.state))
        (self.recording / 'outcome.json').write_text(json.dumps(self.outcome))
        (self.root / 'report.json').write_text(json.dumps(self.report))
        return subprocess.run(['python3', '-I', str(SCRIPT), str(self.root / 'report.json'),
                               str(self.root / 'runs')], capture_output=True, text=True)

    def test_agreement_matches_independent_mergify_recording(self):
        self.assertEqual(self.check().returncode, 0)

    def test_empty_report_cannot_pass(self):
        self.report['pull_requests'] = []
        self.assertNotEqual(self.check().returncode, 0)

    def test_never_queued_pr_without_a_report_is_not_negative_evidence(self):
        self.outcome['prs']['a'].update(end='open', queued=False, dequeue_codes=[])
        self.outcome.update(landed=[], batches=[])
        self.report['pull_requests'] = []
        self.assertNotEqual(self.check().returncode, 0)

    def test_pending_inconclusive_and_disagree_are_not_agreement(self):
        for verdict in ('pending', 'inconclusive', 'disagree'):
            with self.subTest(verdict=verdict):
                self.report['pull_requests'][0]['verdict'] = verdict
                self.assertNotEqual(self.check().returncode, 0)

    def test_failure_reports_safe_verdict_and_disagreeing_dimensions(self):
        row = self.report['pull_requests'][0]
        row['verdict'] = 'disagree'
        row['queued']['agree'] = False
        row['differences'] = ['untrusted diagnostic text']
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('PR #42', result.stderr)
        self.assertIn('verdict=disagree', result.stderr)
        self.assertIn('queued', result.stderr)
        self.assertNotIn('untrusted diagnostic text', result.stderr)

    def test_failure_does_not_print_unrecognized_verdict_text(self):
        self.report['pull_requests'][0]['verdict'] = 'untrusted diagnostic text'
        result = self.check()
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('verdict=invalid', result.stderr)
        self.assertNotIn('untrusted diagnostic text', result.stderr)

    def test_shared_observer_mistake_does_not_pass(self):
        row = self.report['pull_requests'][0]
        row['queued'] = {'schlange': False, 'mergify': False, 'agree': True}
        self.assertNotEqual(self.check().returncode, 0)

    def test_shared_dequeue_reason_not_in_recording_does_not_pass(self):
        self.outcome['prs']['a'].update(end='open', queued=False, enter_count=0, dequeue_codes=[])
        self.outcome.update(landed=[], batches=[])
        row = self.report['pull_requests'][0]
        row['queued'] = {'schlange': False, 'mergify': False, 'agree': True}
        row['landed'] = {'schlange': False, 'mergify': False, 'agree': True}
        row.pop('order')
        row.pop('batch')
        row['dequeue_reason'] = {'schlange': 'checks_failed', 'mergify': 'checks_failed', 'agree': True}
        self.assertNotEqual(self.check().returncode, 0)

    def test_observed_dequeue_can_match_independent_history(self):
        self.outcome['prs']['a'].update(end='open', dequeue_codes=['PR_MANUALLY_DEQUEUED'])
        self.outcome.update(landed=[], batches=[])
        row = self.report['pull_requests'][0]
        row['landed'] = {'schlange': False, 'mergify': False, 'agree': True}
        row.pop('order')
        row.pop('batch')
        row['dequeue_reason'] = {'schlange': 'PR_MANUALLY_DEQUEUED',
                                 'mergify': 'PR_MANUALLY_DEQUEUED', 'agree': True}
        self.assertEqual(self.check().returncode, 0)

    def test_missing_independent_dequeue_codes_are_not_evidence(self):
        del self.outcome['prs']['a']['dequeue_codes']
        self.assertNotEqual(self.check().returncode, 0)

    def test_landed_pr_with_prior_dequeue_needs_a_history_comparison(self):
        self.outcome['prs']['a']['dequeue_codes'] = ['PR_MANUALLY_DEQUEUED']
        self.assertNotEqual(self.check().returncode, 0)

    def test_landed_pr_cannot_invent_a_dequeue_reason(self):
        self.report['pull_requests'][0]['dequeue_reason'] = {
            'schlange': 'checks_failed', 'mergify': 'checks_failed', 'agree': True}
        self.assertNotEqual(self.check().returncode, 0)

    def test_repeated_queue_entries_need_a_history_comparison(self):
        self.outcome['prs']['a']['enter_count'] = 2
        self.assertNotEqual(self.check().returncode, 0)

    def test_inconsistent_or_missing_queue_entry_count_is_not_evidence(self):
        for count in (0, None, True):
            with self.subTest(count=count):
                self.outcome['prs']['a']['enter_count'] = count
                self.assertNotEqual(self.check().returncode, 0)

    def test_inconsistent_agree_flag_does_not_pass(self):
        self.report['pull_requests'][0]['landed']['schlange'] = False
        self.assertNotEqual(self.check().returncode, 0)

    def test_closed_before_watching_cannot_be_agreement(self):
        self.report['pull_requests'][0]['before_watching'] = True
        self.assertNotEqual(self.check().returncode, 0)

    def test_foreign_or_unknown_merge_cannot_prove_mergify_landing(self):
        for merger in (False, None, 'mergify[bot]'):
            with self.subTest(merger=merger):
                self.outcome['prs']['a']['merged_by_mergify'] = merger
                self.assertNotEqual(self.check().returncode, 0)
        del self.outcome['prs']['a']['merged_by_mergify']
        self.assertNotEqual(self.check().returncode, 0)

    def test_batch_must_be_in_independent_recording(self):
        self.report['pull_requests'][0]['batch'] = {
            'schlange': [42, 99], 'mergify': [42, 99], 'agree': True}
        self.assertNotEqual(self.check().returncode, 0)

    def test_green_batch_for_another_pr_does_not_prove_this_landing(self):
        self.state['prs']['b'] = {'number': 43}
        self.outcome['prs']['b'] = copy.deepcopy(self.outcome['prs']['a'])
        self.outcome['landed'] = ['a', 'b']
        self.outcome['batches'] = [{'prs': ['b'], 'ci_ok': 'success'}]
        row = self.report['pull_requests'][0]
        row['batch'] = {'schlange': [43], 'mergify': [43], 'agree': True}
        other = copy.deepcopy(row)
        other['number'] = 43
        other['order'] = {'schlange': 2, 'mergify': 2, 'agree': True}
        self.report['pull_requests'].append(other)
        self.assertNotEqual(self.check().returncode, 0)

    def test_green_batch_cannot_repeat_a_pull_request(self):
        self.outcome['batches'] = [{'prs': ['a', 'a'], 'ci_ok': 'success'}]
        self.report['pull_requests'][0]['batch'] = {
            'schlange': [42, 42], 'mergify': [42, 42], 'agree': True}
        self.assertNotEqual(self.check().returncode, 0)

    def test_missing_timeout_and_unfinished_recordings_are_not_proof(self):
        for field, value in [('timed_out', True), ('timed_out', None), ('finished', None)]:
            with self.subTest(field=field, value=value):
                state = copy.deepcopy(self.state)
                self.state[field] = value
                self.assertNotEqual(self.check().returncode, 0)
                self.state = state

    def test_wrong_repository_is_refused(self):
        self.state['repo'] = 'arzt-agent'
        self.assertNotEqual(self.check().returncode, 0)

    def test_shadow_start_passes_read_only_mode_and_waits_for_readiness(self):
        for repo, mode in [('queue-lab', 'shadow'), ('schlange-e2e-isolated', 'active')]:
            with self.subTest(repo=repo, mode=mode), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                (root / '.research/mergify').mkdir(parents=True)
                (root / '.research/mergify/schema.json').write_text('{}')
                (root / 'config/apps').mkdir(parents=True)
                (root / 'config/apps/queue.json').write_text('{"id": 123}')
                binary = root / 'fake-schlange'
                binary.write_text('#!/bin/sh\nprintf "%s\\n" "$@" > "$TEST_ARGS"\nexec sleep 30\n')
                binary.chmod(0o755)
                (root / 'bin').mkdir()
                curl = root / 'bin/curl'
                curl.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$TEST_CURL"\ntest -f "$TEST_ARGS"\n')
                curl.chmod(0o755)
                env = {**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH'],
                       'SANDBOX_ORG': 'schlange-sandbox', 'SANDBOX_REPO': repo, 'SANDBOX_MODE': mode,
                       'SCHLANGE_SANDBOX_CONFIG': str(root / 'config'), 'RUNNER_TEMP': str(root),
                       'GITHUB_ENV': str(root / 'env'), 'TEST_ARGS': str(root / 'args'),
                       'TEST_CURL': str(root / 'curl')}
                try:
                    result = subprocess.run(['/bin/bash', str(START), str(binary)], cwd=root,
                                            env=env, capture_output=True, text=True, timeout=10)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    args = (root / 'args').read_text().splitlines()
                    self.assertEqual('--shadow' in args, mode == 'shadow')
                    self.assertIn('schlange-sandbox/' + repo, args)
                    self.assertIn('/readyz', (root / 'curl').read_text())
                    self.assertNotIn('/healthz', (root / 'curl').read_text())
                finally:
                    pid_file = root / 'schlange.pids'
                    if pid_file.exists():
                        try:
                            os.kill(int(pid_file.read_text()), signal.SIGTERM)
                        except ProcessLookupError:
                            pass

    def test_shadow_start_is_restricted_to_reference_repository(self):
        for repo, mode in [('queue-lab', 'active'), ('schlange-e2e', 'shadow'),
                           ('arzt-agent', 'shadow'), ('queue-lab', 'unknown')]:
            with self.subTest(repo=repo, mode=mode):
                result = subprocess.run(['/bin/bash', str(START)], capture_output=True, text=True,
                    env={**os.environ, 'SANDBOX_REPO': repo, 'SANDBOX_MODE': mode})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn('refused:', result.stderr)

    def test_shadow_start_does_not_publish_private_log_on_failure(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / '.research/mergify').mkdir(parents=True)
            (root / '.research/mergify/schema.json').write_text('{}')
            (root / 'config/apps').mkdir(parents=True)
            (root / 'config/apps/queue.json').write_text('{"id": 123}')
            binary = root / 'fake-schlange'
            binary.write_text('#!/bin/sh\necho "error PRIVATE-SANDBOX-TEXT"\nexit 1\n')
            binary.chmod(0o755)
            (root / 'bin').mkdir()
            curl = root / 'bin/curl'
            curl.write_text('#!/bin/sh\nexit 1\n')
            curl.chmod(0o755)
            env = {**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH'],
                   'SANDBOX_ORG': 'schlange-sandbox', 'SANDBOX_REPO': 'queue-lab',
                   'SANDBOX_MODE': 'shadow', 'SCHLANGE_SANDBOX_CONFIG': str(root / 'config'),
                   'RUNNER_TEMP': str(root), 'GITHUB_ENV': str(root / 'env')}
            result = subprocess.run(['/bin/bash', str(START), str(binary)], cwd=root,
                                    env=env, capture_output=True, text=True, timeout=10)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('schlange serve exited', result.stdout)
            self.assertNotIn('PRIVATE-SANDBOX-TEXT', result.stdout + result.stderr)


if __name__ == '__main__':
    unittest.main()
