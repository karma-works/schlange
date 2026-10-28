import os
import shutil
import subprocess
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
        (root / 'sandbox' / 'scenarios' / 's01.json').write_text('{}')
        self.marker = root / 'driver-called'
        for name, body in {'curl': 'exit 0', 'python3': 'touch "$TEST_MARKER"\n[ "${TEST_KNOWN:-}" = 1 ] && echo "KNOWN DEVIATION (documented)"\nexit 0'}.items():
            path = root / 'bin' / name
            path.write_text('#!/bin/sh\n' + body + '\n')
            path.chmod(0o755)
        self.root = root
        self.env = {**os.environ, 'PATH': str(root / 'bin') + ':' + os.environ['PATH'],
                    'TEST_MARKER': str(self.marker), 'GITHUB_STEP_SUMMARY': str(root / 'summary')}

    def run_scenario(self, repo):
        return subprocess.run(['bash', str(self.root / 'sandbox' / 'ci' / 'run-scenarios.sh'), repo, 's01'],
                              env=self.env, capture_output=True, text=True)

    def test_missing_baseline_fails_before_driver_on_schlange(self):
        result = self.run_scenario('schlange-e2e')
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
        (self.root / 'sandbox' / 'baselines' / 's01.json').write_text('{}')
        result = self.run_scenario('schlange-e2e')
        self.assertEqual(result.returncode, 0)
        self.assertTrue(self.marker.exists())

    def test_known_deviation_is_not_reported_as_identical(self):
        (self.root / 'sandbox' / 'baselines' / 's01.json').write_text('{}')
        self.env['TEST_KNOWN'] = '1'
        result = self.run_scenario('schlange-e2e')
        self.assertEqual(result.returncode, 0)
        self.assertIn('known deviation', (self.root / 'summary').read_text())
        self.assertNotIn('same as baseline', (self.root / 'summary').read_text())


if __name__ == '__main__':
    unittest.main()
