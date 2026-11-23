import unittest
from pathlib import Path


ROOT = Path(__file__).resolve().parents[3]
WORKFLOW = ROOT / '.github' / 'workflows' / 'sandbox.yml'


class FixtureInspectionWorkflowTests(unittest.TestCase):
    def test_inspection_is_read_only_and_publishes_the_structural_report(self):
        workflow = WORKFLOW.read_text()
        self.assertIn('python3 -I sandbox/driver/sandbox.py verify-fixture --repo "$REPO"', workflow)
        self.assertIn("inputs.inspect_only && inputs.scenarios == 'verify-fixture'", workflow)
        self.assertIn('--source-repository "$GITHUB_REPOSITORY" --source-sha "$GITHUB_SHA"', workflow)
        self.assertIn('--run-id "$GITHUB_RUN_ID" --run-attempt "$GITHUB_RUN_ATTEMPT"', workflow)
        self.assertIn('$RUNNER_TEMP/fixture-inspection.json', workflow)
        self.assertIn('${{ runner.temp }}/fixture-inspection.json', workflow)
        self.assertIn("inputs.inspect_only && (!inputs.isolated_setup || inputs.isolated_setup == 'none')", workflow)
        self.assertIn("if: ${{ !inputs.inspect_only", workflow)


if __name__ == '__main__':
    unittest.main()
