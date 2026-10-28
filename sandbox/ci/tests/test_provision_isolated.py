import os
import subprocess
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[1] / 'provision-isolated.sh'
WORKFLOW = Path(__file__).resolve().parents[3] / '.github' / 'workflows' / 'sandbox.yml'


class IsolatedProvisionTests(unittest.TestCase):
    def test_provisioning_workflow_is_separate_from_scenarios(self):
        workflow = WORKFLOW.read_text()
        self.assertIn('isolated_setup:', workflow)
        self.assertIn('options: [none, create, resume]', workflow)
        self.assertIn('bash sandbox/ci/provision-isolated.sh "$REPO" "$SETUP" "$INSPECT" "$UPDATE"', workflow)
        for name in ('Start schlange serve', 'Scenarios', 'Reset selected sandbox fixture'):
            section = workflow.split('- name: ' + name, 1)[1].split('\n      - ', 1)[0]
            self.assertIn("inputs.isolated_setup == 'none'", section)

    def test_exact_repository_and_exclusive_mode_are_required(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            marker = root / 'called'
            fake = root / 'bash'
            fake.write_text('#!/bin/sh\nprintf "%s\\n" "$*" >> "$TEST_MARKER"\n')
            fake.chmod(0o755)
            env = {**os.environ, 'PATH': str(root) + ':' + os.environ['PATH'],
                   'TEST_MARKER': str(marker), 'SANDBOX_ORG': 'schlange-sandbox'}
            for args in [('queue-lab', 'create', 'false', 'false'),
                         ('schlange-e2e-isolated', 'create', 'true', 'false'),
                         ('schlange-e2e-isolated', 'resume', 'false', 'true'),
                         ('schlange-e2e-isolated', 'other', 'false', 'false')]:
                self.assertNotEqual(subprocess.run(['/bin/bash', str(SCRIPT), *args], env=env,
                                                   capture_output=True).returncode, 0)
                self.assertFalse(marker.exists())
            wrong_org = {**env, 'SANDBOX_ORG': 'other'}
            self.assertNotEqual(subprocess.run(['/bin/bash', str(SCRIPT), 'schlange-e2e-isolated',
                                                'create', 'false', 'false'], env=wrong_org,
                                               capture_output=True).returncode, 0)
            self.assertFalse(marker.exists())
            result = subprocess.run(['/bin/bash', str(SCRIPT), 'schlange-e2e-isolated', 'create',
                                     'false', 'false'], env=env, capture_output=True)
            self.assertEqual(result.returncode, 0)
            self.assertIn('setup.sh repos-isolated', marker.read_text())
            marker.unlink()
            result = subprocess.run(['/bin/bash', str(SCRIPT), 'schlange-e2e-isolated', 'resume',
                                     'false', 'false'], env=env, capture_output=True)
            self.assertEqual(result.returncode, 0)
            self.assertIn('setup.sh finish-isolated', marker.read_text())


if __name__ == '__main__':
    unittest.main()
