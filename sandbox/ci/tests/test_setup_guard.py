import os
import subprocess
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / 'setup.sh'


class SetupGuardTests(unittest.TestCase):
    def test_refuses_non_sandbox_before_any_setup(self):
        result = subprocess.run(['bash', str(SCRIPT), 'fixture-e2e'],
                                env={**os.environ, 'SANDBOX_ORG': 'not-schlange-sandbox'},
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('only schlange-sandbox', result.stderr)


if __name__ == '__main__':
    unittest.main()
