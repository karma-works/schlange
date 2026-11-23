"""The sandbox diagnostic exports only bounded structural Schlange dequeue causes."""
import importlib.util
import json
import os
import sqlite3
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

SCRIPT = Path(__file__).resolve().parents[1] / 'export-shadow-causes.py'
SPEC = importlib.util.spec_from_file_location('export_shadow_causes', SCRIPT)
exporter = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(exporter)


class ShadowCauseExportTests(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        self.runs = self.root / 'runs'
        self.run = self.runs / 's03'
        self.run.mkdir(parents=True)
        self.state = {
            'org': 'schlange-sandbox', 'repo': 'queue-lab',
            'run_id': 's03-queue-lab-test', 'started': '2026-11-23T08:00:00Z',
            'finished': '2026-11-23T08:10:00Z', 'timed_out': False,
            'prs': {'a': {'number': 300}, 'b': {'number': 301}},
        }
        self.scenario = {'id': 'S03'}
        self.database = self.root / 'schlange.db'
        connection = sqlite3.connect(self.database)
        connection.execute('''CREATE TABLE shadow_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, side TEXT NOT NULL,
            pr INTEGER NOT NULL, kind TEXT NOT NULL, event_key TEXT NOT NULL,
            at INTEGER NOT NULL, data TEXT NOT NULL)''')
        connection.commit()
        connection.close()
        self.source = {
            'GITHUB_ACTIONS': 'true', 'GITHUB_RUN_ID': '45136842518',
            'GITHUB_RUN_ATTEMPT': '1', 'GITHUB_SHA': 'a' * 40,
            'GITHUB_REPOSITORY': 'karma-works/schlange',
        }

    def write_run(self):
        (self.run / 'run.json').write_text(json.dumps(self.state))
        (self.run / 'scenario.json').write_text(json.dumps(self.scenario))

    def insert(self, pr, at, data, *, side='schlange', kind='dequeued', repo='schlange-sandbox/queue-lab'):
        connection = sqlite3.connect(self.database)
        connection.execute(
            'INSERT INTO shadow_events (repo,side,pr,kind,event_key,at,data) VALUES (?,?,?,?,?,?,?)',
            (repo, side, pr, kind, f'event-{pr}-{at}', at, json.dumps(data)))
        connection.commit()
        connection.close()

    def capture(self):
        self.write_run()
        return exporter.capture(self.database, self.runs, self.source)

    def test_exports_exact_queue_condition_without_reason_text(self):
        self.insert(301, 1795421100, {
            'code': 'PR_DEQUEUED',
            'text': 'queue_conditions no longer hold (condition 2 is false)'})
        self.insert(300, 1795421160, {'code': 'CHECKS_FAILED', 'text': 'private detail'})
        self.insert(300, 1795421170, {'code': None, 'text': 'private uncoded detail'})

        result = self.capture()

        self.assertTrue(result['complete'], result)
        self.assertEqual(result['source']['workflow_run_id'], 45136842518)
        self.assertEqual(result['scenarios'], [{
            'run_id': 's03-queue-lab-test',
            'started_at': '2026-11-23T08:00:00Z',
            'finished_at': '2026-11-23T08:10:00Z',
            'pull_requests': [300, 301],
        }])
        self.assertEqual(result['events'], [{
            'id': 1, 'pull_request': 301, 'at': 1795421100,
            'cause': {'kind': 'queue_condition', 'condition_index': 2},
        }])
        encoded = json.dumps(result)
        self.assertNotIn('queue_conditions', encoded)
        self.assertNotIn('private detail', encoded)

    def test_classifies_only_established_finite_causes(self):
        cases = [
            ('the pull request does not merge onto the base branch', 'base_conflict'),
            ('the pull request was closed', 'closed'),
            ('a merge protection holds it (freeze): private detail', 'hold'),
            ('a future internal reason with private detail', 'other'),
        ]
        for offset, (text, _) in enumerate(cases):
            self.insert(301, 1795421100 + offset, {'code': 'PR_DEQUEUED', 'text': text})

        result = self.capture()

        self.assertTrue(result['complete'], result)
        self.assertEqual([event['cause']['kind'] for event in result['events']],
                         [kind for _, kind in cases])
        self.assertNotIn('private detail', json.dumps(result))

    def test_fails_closed_atomically_on_bad_binding_window_or_row(self):
        mutations = []

        def wrong_repo():
            self.state['repo'] = 'arzt-agent'
        mutations.append(wrong_repo)

        def wrong_scenario():
            self.scenario['id'] = 'S18'
        mutations.append(wrong_scenario)

        def malformed_data():
            connection = sqlite3.connect(self.database)
            connection.execute(
                'INSERT INTO shadow_events (repo,side,pr,kind,event_key,at,data) VALUES (?,?,?,?,?,?,?)',
                ('schlange-sandbox/queue-lab', 'schlange', 301, 'dequeued', 'bad',
                 1795421100, '{private'))
            connection.commit()
            connection.close()
        mutations.append(malformed_data)

        def missing_code():
            self.insert(301, 1795421100, {'text': 'private missing code'})
        mutations.append(missing_code)

        def mixed_pr_number_types():
            self.state['prs']['b']['number'] = '301'
        mutations.append(mixed_pr_number_types)

        def oversize_data():
            self.insert(301, 1795421100, {
                'code': 'PR_DEQUEUED', 'text': 'x' * (exporter.DATA_BYTES_MAX + 1)})
        mutations.append(oversize_data)

        def deeply_nested_data():
            connection = sqlite3.connect(self.database)
            raw = '[' * 5000 + '0' + ']' * 5000
            connection.execute(
                'INSERT INTO shadow_events (repo,side,pr,kind,event_key,at,data) VALUES (?,?,?,?,?,?,?)',
                ('schlange-sandbox/queue-lab', 'schlange', 301, 'dequeued', 'nested',
                 1795421100, raw))
            connection.commit()
            connection.close()
        mutations.append(deeply_nested_data)

        for mutate in mutations:
            with self.subTest(mutate=mutate.__name__):
                self.setUp()
                mutate()
                result = self.capture()
                self.assertFalse(result['complete'], result)
                self.assertEqual(result['events'], [])
                self.assertIn(result['incomplete_reason'], exporter.INCOMPLETE_REASONS)
                self.assertNotIn('private', json.dumps(result))

    def test_post_finish_cleanup_rows_are_outside_the_selected_interval(self):
        self.insert(301, 1795421100, {
            'code': 'PR_DEQUEUED',
            'text': 'queue_conditions no longer hold (condition 2 is false)'})
        self.insert(301, 1795422000, {
            'code': 'PR_DEQUEUED', 'text': 'the pull request was closed'})

        result = self.capture()

        self.assertTrue(result['complete'], result)
        self.assertEqual(len(result['events']), 1)
        self.assertEqual(result['events'][0]['cause'], {
            'kind': 'queue_condition', 'condition_index': 2})

    def test_json_recursion_failure_is_an_atomic_incomplete_artifact(self):
        self.insert(301, 1795421100, {'code': 'PR_DEQUEUED', 'text': 'other'})
        self.write_run()
        with patch.object(exporter.json, 'loads', side_effect=[
                self.state, self.scenario, RecursionError('nested')]):
            result = exporter.capture(self.database, self.runs, self.source)
        self.assertFalse(result['complete'])
        self.assertEqual(result['incomplete_reason'], 'row_invalid')
        self.assertEqual(result['events'], [])

    def test_reader_is_query_only_and_bounds_rows(self):
        self.write_run()
        connection = exporter.open_database(self.database)
        with self.assertRaises(sqlite3.OperationalError):
            connection.execute('DELETE FROM shadow_events')
        connection.close()

        connection = sqlite3.connect(self.database)
        rows = [('schlange-sandbox/queue-lab', 'schlange', 301, 'dequeued', f'e-{i}',
                 1795421100, json.dumps({'code': 'PR_DEQUEUED', 'text': 'unknown'}))
                for i in range(exporter.EVENTS_MAX + 1)]
        connection.executemany(
            'INSERT INTO shadow_events (repo,side,pr,kind,event_key,at,data) VALUES (?,?,?,?,?,?,?)',
            rows)
        connection.commit()
        connection.close()
        result = exporter.capture(self.database, self.runs, self.source)
        self.assertFalse(result['complete'])
        self.assertEqual(result['incomplete_reason'], 'bound_exceeded')
        self.assertEqual(result['events'], [])

    def test_missing_source_or_database_is_a_bounded_incomplete_artifact(self):
        self.write_run()
        for database, source in [(self.root / 'missing.db', self.source), (self.database, {})]:
            with self.subTest(database=database, source=source):
                result = exporter.capture(database, self.runs, source)
                self.assertFalse(result['complete'])
                self.assertEqual(result['events'], [])
                self.assertLess(len(json.dumps(result)), 4096)

    def test_cli_storage_failure_writes_marker_without_failing_comparison_step(self):
        self.write_run()
        output = self.root / 'evidence' / 'causes.json'
        result = subprocess.run(
            ['python3', '-I', str(SCRIPT), str(self.root / 'missing.db'),
             str(self.runs), str(output)], env={**os.environ, **self.source},
            capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        artifact = json.loads(output.read_text())
        self.assertFalse(artifact['complete'])
        self.assertEqual(artifact['incomplete_reason'], 'storage')
        self.assertEqual(artifact['events'], [])

    def test_workflow_exports_after_strict_failure_path_in_an_always_step(self):
        workflow = (SCRIPT.parents[2] / '.github' / 'workflows' / 'sandbox.yml').read_text()
        verify = workflow.index('- name: Verify shadow report against independent Mergify recordings')
        export = workflow.index('- name: Export structural Schlange dequeue causes')
        stop = workflow.index('- name: Stop schlange serve, show the end of its log')
        self.assertLess(verify, export)
        self.assertLess(export, stop)
        block = workflow[export:stop]
        self.assertIn('if: ${{ always() && inputs.shadow_mode', block)
        self.assertIn('export-shadow-causes.py', block)


if __name__ == '__main__':
    unittest.main()
