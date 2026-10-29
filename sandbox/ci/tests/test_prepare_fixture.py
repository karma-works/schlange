import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / 'prepare-fixture.sh'


class FixturePreparationTests(unittest.TestCase):
    def test_rejects_non_sandbox_repository(self):
        result = subprocess.run(['/bin/bash', str(SCRIPT), 'arztagent'], capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)

    def test_waits_before_updating_only_e2e(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / 'calls'
            for name in ('python3', 'bash'):
                executable = root / name
                executable.write_text('#!/bin/sh\nprintf "%s %s\\n" "' + name + '" "$*" >> "$TEST_CALLS"\n')
                executable.chmod(0o755)
            env = {**os.environ, 'PATH': str(root) + ':' + os.environ['PATH'], 'TEST_CALLS': str(log)}
            result = subprocess.run(['/bin/bash', str(SCRIPT), 'schlange-e2e'], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text().splitlines()
            self.assertEqual(len(calls), 2)
            self.assertIn('wait-idle --repo schlange-e2e --all-open', calls[0])
            self.assertIn('setup.sh fixture-e2e', calls[1])

    def test_waits_before_updating_only_queue_lab(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            log = root / 'calls'
            for name in ('python3', 'bash'):
                executable = root / name
                executable.write_text('#!/bin/sh\nprintf "%s %s\\n" "' + name + '" "$*" >> "$TEST_CALLS"\n')
                executable.chmod(0o755)
            env = {**os.environ, 'PATH': str(root) + ':' + os.environ['PATH'], 'TEST_CALLS': str(log)}
            result = subprocess.run(['/bin/bash', str(SCRIPT), 'queue-lab'], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = log.read_text().splitlines()
            self.assertIn('wait-idle --repo queue-lab --all-open', calls[0])
            self.assertIn('setup.sh fixture-lab', calls[1])

    def test_failed_idle_check_never_runs_setup(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / 'python3').write_text('#!/bin/sh\nexit 1\n')
            (root / 'python3').chmod(0o755)
            marker = root / 'called'
            (root / 'bash').write_text('#!/bin/sh\ntouch "$TEST_MARKER"\n')
            (root / 'bash').chmod(0o755)
            env = {**os.environ, 'PATH': str(root) + ':' + os.environ['PATH'], 'TEST_MARKER': str(marker)}
            result = subprocess.run(['/bin/bash', str(SCRIPT), 'schlange-e2e'], env=env, capture_output=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())


if __name__ == '__main__':
    unittest.main()
