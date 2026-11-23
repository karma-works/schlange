"""Tests for the scenario-scoped Mergify checks-start diagnostic."""
import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch
from urllib.parse import parse_qs, urlsplit

import sandbox as driver


class ReferenceCheckStartsTests(unittest.TestCase):
    def setUp(self):
        self.started = '2026-01-01T00:00:00Z'
        self.finished = '2026-01-01T00:10:00Z'
        self.state = {
            'org': 'schlange-sandbox', 'repo': 'queue-lab', 'run_id': 's03-queue-lab-test',
            'started': self.started, 'finished': self.finished, 'timed_out': False,
            'prs': {'a': {'number': 7}, 'b': {'number': 8}},
        }
        self.source = {
            'GITHUB_ACTIONS': 'true', 'GITHUB_RUN_ID': '123', 'GITHUB_RUN_ATTEMPT': '1',
            'GITHUB_SHA': 'a' * 40, 'GITHUB_REPOSITORY': 'karma-works/schlange',
        }
        self.run_id = '11111111-1111-4111-8111-111111111111'
        self.batch_id = '22222222-2222-4222-8222-222222222222'
        self.event = {
            'id': 1, 'pull_request': 7, 'repository': 'queue-lab',
            'type': 'action.queue.checks_start', 'received_at': '2026-01-01T00:01:00Z',
            'metadata': {
                'session_id': '33333333-3333-4333-8333-333333333333',
                'batch_id': self.batch_id, 'run_id': self.run_id, 'parent_batch_ids': [],
                'speculative_check_pull_request': {
                    'number': 10, 'head_sha': 'b' * 40, 'in_place': False,
                    'pull_request_numbers': [8, 7],
                },
                'start_reason': 'private prose',
            },
        }

    def capture(self, events):
        queries = []

        class Client:
            def activity_page(self, url):
                queries.append(parse_qs(urlsplit(url).query))
                return {'size': len(events), 'per_page': 100, 'events': events}, {}

        result = driver.capture_reference_check_starts(
            self.state, Client(), '2026-01-01T00:10:01Z', self.source)
        self.assertEqual(queries, [{
            'per_page': ['100'], 'received_from': [self.started],
            'received_to': [self.finished], 'pull_request': ['7', '8'],
            'event_type': ['action.queue.checks_start'],
        }])
        return result

    def test_preserves_source_member_order_and_allowlists_fields(self):
        result = self.capture([self.event])
        self.assertTrue(result['complete'], result)
        self.assertEqual(result['source']['workflow_run_id'], 123)
        self.assertEqual(result['events'][0]['pull_request_numbers'], [8, 7])
        self.assertEqual(result['events'][0]['head_sha'], 'b' * 40)
        self.assertEqual(result['events'][0]['draft_pull_request'], 10)
        self.assertEqual(result['events'][0]['batch_id'], self.batch_id)
        self.assertEqual(result['events'][0]['run_id'], self.run_id)
        self.assertNotIn('private prose', json.dumps(result))

    def test_reports_missing_bound_evidence_as_incomplete(self):
        metadata = self.event['metadata']
        metadata.pop('run_id')
        metadata['parent_batch_ids'] = None
        metadata['batch_id'] = None
        metadata['speculative_check_pull_request'].update(number=None, head_sha=None)
        result = self.capture([self.event])
        self.assertFalse(result['complete'], result)
        self.assertEqual(result['incomplete_reason'], 'check_identity_missing')
        self.assertEqual(result['events'], [])

    def test_rejects_unselected_or_inconsistent_members(self):
        for invalid_members in ([7, 99], [8], [7, 7], [True, 7], []):
            with self.subTest(invalid_members=invalid_members):
                self.event['metadata']['speculative_check_pull_request']['pull_request_numbers'] = invalid_members
                result = self.capture([self.event])
                self.assertFalse(result['complete'])
                self.assertEqual(result['incomplete_reason'], 'event_invalid')
                self.assertEqual(result['events'], [])

    def test_accepts_batch_rows_with_null_pr_and_multiple_sources_for_one_run(self):
        second = {**self.event, 'id': 2, 'pull_request': None}
        result = self.capture([second, self.event])
        self.assertTrue(result['complete'], result)
        self.assertEqual([item['pull_request'] for item in result['events']], [7, None])
        self.assertEqual([item['run_id'] for item in result['events']], [self.run_id, self.run_id])

    def test_rejects_conflicting_heads_for_one_run(self):
        other_head = {**self.event['metadata']['speculative_check_pull_request'], 'head_sha': 'c' * 40}
        second = {**self.event, 'id': 2, 'pull_request': 8,
                  'metadata': {**self.event['metadata'], 'speculative_check_pull_request': other_head}}
        result = self.capture([self.event, second])
        self.assertFalse(result['complete'], result)
        self.assertEqual(result['incomplete_reason'], 'event_invalid')
        self.assertEqual(result['events'], [])

    def test_distinguishes_in_place_pr_checks_from_draft_checks(self):
        check = self.event['metadata']['speculative_check_pull_request']
        check.update(in_place=True, pull_request_numbers=[7], number=7)
        result = self.capture([self.event])
        self.assertTrue(result['complete'], result)
        self.assertTrue(result['events'][0]['in_place'])
        self.assertIsNone(result['events'][0]['draft_pull_request'])
        self.assertEqual(result['events'][0]['head_sha'], 'b' * 40)


    def test_writes_separate_artifact_when_lifecycle_read_fails(self):
        def activity_page(url):
            types = parse_qs(urlsplit(url).query)['event_type']
            if types != ['action.queue.checks_start']:
                raise RuntimeError('private transport error')
            return {'size': 1, 'per_page': 100, 'events': [self.event]}, {}

        client = Mock()
        client.activity_page.side_effect = activity_page
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            (out / 'run.json').write_text(json.dumps(self.state))
            env = {**self.source, 'SANDBOX_REFERENCE_ACTIVITY': '1'}
            with patch.dict(driver.os.environ, env, clear=True), \
                 patch.object(driver, 'mergify_client', return_value=client), \
                 patch.object(driver, 'now_iso', return_value='2026-01-01T00:10:01Z'):
                driver.write_reference_activity(out)
            starts = json.loads((out / 'reference-check-starts.json').read_text())
            lifecycle = json.loads((out / 'reference-activity.json').read_text())
        self.assertTrue(starts['complete'])
        self.assertEqual(starts['events'][0]['run_id'], self.run_id)
        self.assertFalse(lifecycle['complete'])
        self.assertNotIn('private transport error', json.dumps(lifecycle))

if __name__ == '__main__':
    unittest.main()
