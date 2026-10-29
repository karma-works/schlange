import os
import json
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / 'run-scenarios.sh'


class ScenarioRunnerTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = Path(self.temp.name)
        (root / 'sandbox' / 'ci').mkdir(parents=True)
        (root / 'sandbox' / 'scenarios').mkdir()
        (root / 'sandbox' / 'baselines').mkdir()
        (root / 'bin').mkdir()
        shutil.copy(SCRIPT, root / 'sandbox' / 'ci' / 'run-scenarios.sh')
        shutil.copy(SCRIPT.parent / 'validate-reference.py', root / 'sandbox' / 'ci' / 'validate-reference.py')
        (root / 'sandbox' / 'scenarios' / 's01.json').write_text('{"steps": [{"open": "a"}]}')
        self.marker = root / 'driver-called'
        for name, body in {'curl': 'exit 0', 'python3': 'case "${2:-}" in */validate-reference.py) exec "$REAL_PYTHON" "$@";; esac\ntouch "$TEST_MARKER"\n[ "${TEST_KNOWN:-}" = 1 ] && echo "KNOWN DEVIATION (documented)"\nexit 0'}.items():
            path = root / 'bin' / name
            path.write_text('#!/bin/sh\n' + body + '\n')
            path.chmod(0o755)
        self.root = root
        self.env = {**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH'],
                    'TEST_MARKER': str(self.marker), 'REAL_PYTHON': sys.executable,
                    'GITHUB_STEP_SUMMARY': str(root / 'summary')}

    def run_scenario(self, repo):
        return subprocess.run(['bash', str(self.root / 'sandbox' / 'ci' / 'run-scenarios.sh'), repo, 's01'],
                              env=self.env, capture_output=True, text=True)

    def write_baseline(self):
        (self.root / 'sandbox' / 'baselines' / 's01.json').write_text(json.dumps({
            'prs': {'a': {'end': 'merged', 'queued': True, 'enter_count': 1, 'dequeue_codes': []}},
            'landed': ['a'], 'batches': []}))

    def test_missing_baseline_fails_before_driver_on_schlange(self):
        result = self.run_scenario('schlange-e2e')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_provisional_baseline_fails_before_driver_on_schlange(self):
        (self.root / 'sandbox' / 'baselines' / 's01.json').write_text('{"reference_quality": "provisional"}')
        result = self.run_scenario('schlange-e2e-isolated')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_unverified_or_invalid_baseline_fails_before_driver(self):
        baseline = self.root / 'sandbox' / 'baselines' / 's01.json'
        for contents in ('{"reference_quality": "unreviewed"}', '{invalid json',
                         '{"reference_quality":\n"provisional"}'):
            with self.subTest(contents=contents):
                baseline.write_text(contents)
                result = self.run_scenario('schlange-e2e-isolated')
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.marker.exists())

    def test_isolated_repo_requires_reference_baseline(self):
        result = self.run_scenario('schlange-e2e-isolated')
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.marker.exists())
        self.write_baseline()
        self.assertEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)
        self.assertTrue(self.marker.exists())

    def test_arbitrary_same_org_repo_is_rejected(self):
        self.assertNotEqual(self.run_scenario('other-sandbox-repo').returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_arbitrary_scenario_path_is_rejected(self):
        (self.root / 'sandbox' / 'outside.json').write_text('{}')
        result = subprocess.run(['bash', str(self.root / 'sandbox' / 'ci' / 'run-scenarios.sh'),
                                 'schlange-e2e-isolated', '../outside'],
                                env=self.env, capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_missing_baseline_is_recorded_on_reference(self):
        result = self.run_scenario('queue-lab')
        self.assertEqual(result.returncode, 0)
        self.assertTrue(self.marker.exists())

    def test_reference_run_also_fails_on_timeout(self):
        path = self.root / 'bin' / 'python3'
        path.write_text('#!/bin/sh\n[ "${SANDBOX_FAIL_ON_TIMEOUT:-}" = 1 ]\n')
        path.chmod(0o755)
        self.assertEqual(self.run_scenario('queue-lab').returncode, 0)

    def test_existing_baseline_runs_compatibility_scenario(self):
        self.write_baseline()
        result = self.run_scenario('schlange-e2e')
        self.assertEqual(result.returncode, 0)
        self.assertTrue(self.marker.exists())

    def test_known_deviation_is_not_reported_as_identical(self):
        self.write_baseline()
        self.env['TEST_KNOWN'] = '1'
        result = self.run_scenario('schlange-e2e')
        self.assertEqual(result.returncode, 0)
        self.assertIn('known deviation', (self.root / 'summary').read_text())
        self.assertNotIn('same as baseline', (self.root / 'summary').read_text())

    def test_incomplete_baseline_fails_before_driver(self):
        baseline = self.root / 'sandbox' / 'baselines' / 's01.json'
        for data in ({}, {'prs': {'other': {}}, 'landed': []},
                     {'prs': {'a': {'end': 'merged'}}, 'landed': []}):
            with self.subTest(data=data):
                baseline.write_text(json.dumps(data))
                self.assertNotEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)
                self.assertFalse(self.marker.exists())

    def test_malformed_known_deviations_fail_before_driver(self):
        baseline = self.root / 'sandbox' / 'baselines' / 's01.json'
        for deviations in (None, [{}], [{'pr': 'a', 'field': 'end', 'observed': 'open'}],
                           [{'pr': 'a', 'field': 'queued', 'observed': 1, 'why': 'invalid'}],
                           [{'pr': 'a', 'field': 'queued', 'observed': False, 'why': 'one'},
                            {'pr': 'a', 'field': 'queued', 'observed': True, 'why': 'two'}]):
            with self.subTest(deviations=deviations):
                self.write_baseline()
                data = json.loads(baseline.read_text())
                data['known_deviations'] = deviations
                baseline.write_text(json.dumps(data))
                self.assertNotEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)
                self.assertFalse(self.marker.exists())

    def test_valid_landed_deviation_is_accepted(self):
        self.write_baseline()
        baseline = self.root / 'sandbox' / 'baselines' / 's01.json'
        data = json.loads(baseline.read_text())
        data['known_deviations'] = [{'pr': '*', 'field': 'landed', 'observed': [], 'why': 'reviewed'}]
        baseline.write_text(json.dumps(data))
        self.assertEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)

    def test_strict_batch_scenario_requires_batch_reference_before_driver(self):
        scenario = self.root / 'sandbox' / 'scenarios' / 's01.json'
        scenario.write_text('{"steps": [{"open": "a"}], "strict_batches": true}')
        self.write_baseline()
        baseline = self.root / 'sandbox' / 'baselines' / 's01.json'
        data = json.loads(baseline.read_text())
        data.pop('batches')
        baseline.write_text(json.dumps(data))
        self.assertNotEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_expected_batch_must_match_reference_before_driver(self):
        scenario = self.root / 'sandbox' / 'scenarios' / 's01.json'
        scenario.write_text('{"steps": [{"open": "a"}], "strict_batches": true, "expected_batches": [["a"]]}')
        self.write_baseline()
        self.assertNotEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)
        self.assertFalse(self.marker.exists())

    def test_strict_batch_reference_requires_ci_conclusion(self):
        scenario = self.root / 'sandbox' / 'scenarios' / 's01.json'
        scenario.write_text('{"steps": [{"open": "a"}], "strict_batches": true, "expected_batches": [["a"]]}')
        self.write_baseline()
        baseline = self.root / 'sandbox' / 'baselines' / 's01.json'
        data = json.loads(baseline.read_text())
        data['batches'] = [{'prs': ['a']}]
        baseline.write_text(json.dumps(data))
        self.assertNotEqual(self.run_scenario('schlange-e2e-isolated').returncode, 0)
        self.assertFalse(self.marker.exists())


if __name__ == '__main__':
    unittest.main()
