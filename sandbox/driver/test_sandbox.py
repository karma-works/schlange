import importlib.util
import io
import unittest
import json
import tempfile
from pathlib import Path
from unittest.mock import patch

spec = importlib.util.spec_from_file_location('sandbox_driver', Path(__file__).with_name('sandbox.py'))
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)


class CheckpointTests(unittest.TestCase):
    def test_isolated_repo_is_explicitly_allowed_but_other_repo_is_not(self):
        with patch.object(driver.sys, 'argv', ['sandbox.py', 'wait-idle', '--repo', 'schlange-e2e-isolated']), \
             patch.object(driver, 'wait_idle') as wait:
            driver.main()
        wait.assert_called_once_with('schlange-sandbox', 'schlange-e2e-isolated', 1800, all_open=False)
        with patch.object(driver.sys, 'argv', ['sandbox.py', 'wait-idle', '--repo', 'unrelated']):
            with self.assertRaises(SystemExit):
                driver.main()

    def test_disposable_repo_cannot_skip_idle_check(self):
        with tempfile.TemporaryDirectory() as directory:
            scenario = Path(directory) / 'scenario.json'
            scenario.write_text(json.dumps({'id': 'S99', 'title': 'test', 'steps': []}))
            args = unittest.mock.Mock(scenario=str(scenario), repo='schlange-e2e-isolated', no_wait=True)
            with patch.object(driver, 'Run') as run:
                with self.assertRaises(SystemExit):
                    driver.cmd_run(args)
                run.assert_not_called()

    def test_driver_installation_token_is_scoped_to_selected_repo(self):
        jwt = unittest.mock.Mock()
        jwt.call.side_effect = [{'id': 9}, {'token': 'test-token'}]
        with patch.object(driver, 'app_meta', return_value={'owner': 'schlange-sandbox', 'owner_type': 'org'}), \
             patch.object(driver, 'Client', return_value=jwt), patch.object(driver, 'app_jwt', return_value='test-jwt'):
            driver.installation_client('driver', 'schlange-sandbox', 'schlange-e2e-isolated')
            fetch = driver.Client.call_args.args[0]
            self.assertEqual(fetch(), ('test-token', 3000))
        jwt.call.assert_any_call('POST', '/app/installations/9/access_tokens',
                                 {'repositories': ['schlange-e2e-isolated']})

    def test_isolated_app_installation_is_exact_and_never_retried(self):
        human = unittest.mock.Mock()
        scoped = unittest.mock.Mock()
        scoped.call.return_value = {'full_name': 'schlange-sandbox/schlange-e2e-isolated'}
        installation = {'id': 9, 'app_id': 10, 'account': {'login': 'schlange-sandbox'},
                        'repository_selection': 'selected'}
        with patch.object(driver, 'installation_client', return_value=scoped):
            driver.install_isolated_app(human, 'queue', 42, 10, installation)
        human.call.assert_called_once_with('PUT', '/user/installations/9/repositories/42', retry=False)
        scoped.call.assert_called_once_with('GET', '/repos/schlange-sandbox/schlange-e2e-isolated')
        human.reset_mock()
        installation['account']['login'] = 'other-org'
        with self.assertRaises(RuntimeError):
            driver.install_isolated_app(human, 'queue', 42, 10, installation)
        human.call.assert_not_called()

    def test_install_isolated_fails_closed_on_permission_error(self):
        human = unittest.mock.Mock()
        human.call.side_effect = RuntimeError('HTTP 403')
        installation = {'id': 9, 'app_id': 10, 'account': {'login': 'schlange-sandbox'},
                        'repository_selection': 'selected'}
        with patch.object(driver, 'installation_client') as scoped:
            with self.assertRaisesRegex(RuntimeError, 'installation repository access write'):
                driver.install_isolated_app(human, 'queue', 42, 10, installation)
            scoped.assert_not_called()
        human.call.assert_called_once()

    def test_install_existing_all_access_does_not_expand_selection(self):
        human = unittest.mock.Mock()
        scoped = unittest.mock.Mock()
        scoped.call.return_value = {'full_name': 'schlange-sandbox/schlange-e2e-isolated'}
        installation = {'id': 9, 'app_id': 10, 'account': {'login': 'schlange-sandbox'},
                        'repository_selection': 'all'}
        with patch.object(driver, 'installation_client', return_value=scoped):
            driver.install_isolated_app(human, 'queue', 42, 10, installation)
        human.call.assert_not_called()

    def test_unchanged_installation_is_accepted_only_after_scoped_read(self):
        human = unittest.mock.Mock()
        human.call.side_effect = RuntimeError('HTTP 304')
        scoped = unittest.mock.Mock()
        scoped.call.return_value = {'full_name': 'schlange-sandbox/schlange-e2e-isolated'}
        installation = {'id': 9, 'app_id': 10, 'account': {'login': 'schlange-sandbox'},
                        'repository_selection': 'selected'}
        with patch.object(driver, 'installation_client', return_value=scoped):
            driver.install_isolated_app(human, 'queue', 42, 10, installation)
        scoped.call.assert_called_once()

    def test_doctor_fails_when_app_cannot_access_selected_repo(self):
        with tempfile.TemporaryDirectory() as directory:
            config = Path(directory)
            (config / 'apps').mkdir()
            for role in ('queue', 'driver'):
                (config / 'apps' / f'{role}.json').write_text('{}')
            client = unittest.mock.Mock()
            client.call.side_effect = lambda method, path, *args, **kwargs: (
                {'id': 9} if path.endswith('/installation') else
                {'repositories': [{'name': 'schlange-e2e'}]} if path == '/installation/repositories' else
                {'token': 'test-token'})
            client.paged.return_value = [{'name': 'schlange-e2e'}]
            with patch.object(driver, 'CONFIG_DIR', config), patch.object(driver, 'Client', return_value=client), \
                 patch.object(driver, 'app_jwt', return_value='test-jwt'):
                with self.assertRaises(SystemExit) as result:
                    driver.cmd_doctor(unittest.mock.Mock(org='schlange-sandbox', repo='schlange-e2e-isolated'))
            self.assertEqual(result.exception.code, 1)

    def test_human_client_requires_sandbox_scoped_token(self):
        with tempfile.TemporaryDirectory() as directory, \
             patch.object(driver, 'CONFIG_DIR', Path(directory)), \
             patch.dict(driver.os.environ, {}, clear=True), \
             patch.object(driver.subprocess, 'run') as run:
            with self.assertRaises(RuntimeError):
                driver.human_client().token()
            run.assert_not_called()

    def test_open_pr_inventory_reports_only_structured_sandbox_facts(self):
        client = unittest.mock.Mock()
        client.paged.return_value = [
            {'number': 1, 'draft': False, 'user': {'login': 'dependabot[bot]'}, 'head': {'ref': 'dependabot/x'}},
            {'number': 2, 'draft': True, 'user': {'login': 'other'}, 'head': {'ref': 'sbx/run/a'}},
        ]
        with patch.object(driver, 'human_client', return_value=client):
            self.assertEqual(driver.open_pr_inventory('schlange-sandbox', 'schlange-e2e'), [
                {'number': 1, 'draft': False, 'actor': 'dependabot', 'head_kind': 'other'},
                {'number': 2, 'draft': True, 'actor': 'other', 'head_kind': 'scenario'},
            ])

    def test_disposable_dependabot_cleanup_rechecks_identity_and_never_touches_other_prs(self):
        def pr(number, login='dependabot[bot]', ref='dependabot/github_actions/example', sha='a' * 40):
            return {'number': number, 'state': 'open', 'user': {'login': login, 'id': 49699333, 'type': 'Bot'},
                    'head': {'ref': ref, 'sha': sha, 'repo': {'full_name': 'schlange-sandbox/schlange-e2e-isolated'}},
                    'base': {'ref': 'main', 'repo': {'full_name': 'schlange-sandbox/schlange-e2e-isolated'}}}
        client = unittest.mock.Mock()
        client.paged.return_value = [pr(1), pr(2)]
        client.call.side_effect = [pr(1), {'number': 1, 'state': 'closed'}, pr(2), {'number': 2, 'state': 'closed'}]
        with patch.object(driver, 'human_client', return_value=client):
            driver.close_isolated_dependabot('schlange-sandbox', 'schlange-e2e-isolated')
        writes = [c for c in client.call.call_args_list if c.args[0] == 'PATCH']
        self.assertEqual(len(writes), 2)
        self.assertTrue(all(c.kwargs.get('retry') is False for c in writes))
        self.assertFalse(any(c.args[0] == 'DELETE' for c in client.call.call_args_list))
        client.reset_mock()
        client.paged.return_value = [pr(1), pr(3, login='other')]
        with patch.object(driver, 'human_client', return_value=client):
            with self.assertRaises(RuntimeError):
                driver.close_isolated_dependabot('schlange-sandbox', 'schlange-e2e-isolated')
        client.call.assert_not_called()
        client.reset_mock(side_effect=True)
        client.paged.return_value = [pr(1)]
        client.call.return_value = pr(1, sha='b' * 40)
        with patch.object(driver, 'human_client', return_value=client):
            with self.assertRaises(RuntimeError):
                driver.close_isolated_dependabot('schlange-sandbox', 'schlange-e2e-isolated')
        self.assertFalse(any(c.args[0] == 'PATCH' for c in client.call.call_args_list))
        client.reset_mock(side_effect=True)
        bad = pr(1)
        bad['user']['id'] = 1
        client.paged.return_value = [bad]
        with patch.object(driver, 'human_client', return_value=client):
            with self.assertRaises(RuntimeError):
                driver.close_isolated_dependabot('schlange-sandbox', 'schlange-e2e-isolated')
        client.call.assert_not_called()
        with self.assertRaises(RuntimeError):
            driver.close_isolated_dependabot('schlange-sandbox', 'queue-lab')
        with self.assertRaises(RuntimeError):
            driver.close_isolated_dependabot('other', 'schlange-e2e-isolated')

    def test_wait_idle_cli_targets_only_selected_sandbox_repo(self):
        with patch.object(driver.sys, 'argv', ['sandbox.py', 'wait-idle', '--repo', 'schlange-e2e', '--all-open']), \
             patch.object(driver, 'wait_idle') as wait:
            driver.main()
        wait.assert_called_once_with('schlange-sandbox', 'schlange-e2e', 1800, all_open=True)

    def test_all_open_idle_check_includes_non_scenario_pull_requests(self):
        client = unittest.mock.Mock()
        client.paged.return_value = [{'number': 1, 'head': {'ref': 'ordinary'}}]
        with patch.object(driver, 'human_client', return_value=client):
            self.assertEqual(driver.busy('schlange-sandbox', 'schlange-e2e', all_open=True), [1])
            self.assertEqual(driver.busy('schlange-sandbox', 'schlange-e2e'), [])

    def test_api_error_does_not_expose_response_text(self):
        error = driver.urllib.error.HTTPError('https://example.invalid', 400, 'bad', {},
                                               io.BytesIO(b'private upstream response'))
        with patch.object(driver.urllib.request, 'urlopen', side_effect=error):
            with self.assertRaises(RuntimeError) as caught:
                driver.Client(lambda: ('token', 100), 'test').call('GET', '/test')
        self.assertNotIn('private upstream response', str(caught.exception))
        self.assertIn('400', str(caught.exception))

    def test_dispatch_http_failure_is_not_retried(self):
        error = driver.urllib.error.HTTPError('https://example.invalid', 502, 'bad', {}, io.BytesIO(b''))
        with patch.object(driver.urllib.request, 'urlopen', side_effect=error) as urlopen:
            with self.assertRaises(RuntimeError):
                driver.Client(lambda: ('token', 100), 'test').call('POST', '/dispatch', {}, retry=False)
        self.assertEqual(urlopen.call_count, 1)

    def test_public_record_summary_excludes_comment_text(self):
        summary = {'prs': {'a': {'number': 1, 'end': 'open', 'labels': [],
                                 'bot_comments': ['private upstream text']}},
                   'queue_prs': [{'number': 2, 'commits': 1, 'ci_ok': 'private conclusion'}],
                   'main_commits': ['private upstream commit text']}
        rendered = json.dumps(driver.public_record_summary(summary))
        self.assertNotIn('private upstream text', rendered)
        self.assertNotIn('private upstream commit text', rendered)
        self.assertNotIn('private conclusion', rendered)
        self.assertIn('open', rendered)

    def test_compatibility_comparison_rejects_missing_queue_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            baseline = Path(directory) / 'baseline.json'
            actual = Path(directory) / 'actual.json'
            pr = {'end': 'open', 'queued': False, 'enter_count': 0, 'dequeue_codes': []}
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': []}))
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'queued': None}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual), 1)
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'enter_count': False}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual), 1)

    def test_explicit_compatibility_mode_rejects_invalid_top_level(self):
        with tempfile.TemporaryDirectory() as directory:
            baseline = Path(directory) / 'baseline.json'
            actual = Path(directory) / 'actual.json'
            pr = {'end': 'open', 'queued': False, 'enter_count': 0, 'dequeue_codes': []}
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': []}))
            actual.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 1)

            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {}, 'landed': [], 'batches': []}))
            baseline.write_text(json.dumps({'prs': {}, 'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 1)
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': [],
                                            'known_deviations': [{'pr': 'a', 'field': 'end',
                                                                  'observed': 'merged', 'why': 'documented'}]}))
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'end': 'merged'}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 0)
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True,
                                                     expected_prs={'a', 'b'}), 1)

    def test_provisional_reference_cannot_pass_compatibility(self):
        with tempfile.TemporaryDirectory() as directory:
            baseline = Path(directory) / 'baseline.json'
            actual = Path(directory) / 'actual.json'
            pr = {'end': 'open', 'queued': False, 'enter_count': 0, 'dequeue_codes': []}
            baseline.write_text(json.dumps({'reference_quality': 'provisional', 'prs': {'a': pr}, 'landed': [], 'batches': []}))
            actual.write_text(json.dumps({'repo': 'schlange-e2e-isolated', 'prs': {'a': pr}, 'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 1)

    def test_numeric_deviation_does_not_match_boolean_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            baseline = Path(directory) / 'baseline.json'
            actual = Path(directory) / 'actual.json'
            pr = {'end': 'open', 'queued': False, 'enter_count': 0, 'dequeue_codes': []}
            baseline.write_text(json.dumps({'prs': {'a': pr}, 'landed': [], 'batches': [],
                                            'known_deviations': [{'pr': 'a', 'field': 'queued',
                                                                  'observed': 1, 'why': 'invalid'}]}))
            actual.write_text(json.dumps({'repo': 'schlange-e2e', 'prs': {'a': {**pr, 'queued': True}},
                                          'landed': [], 'batches': []}))
            self.assertEqual(driver.compare_outcomes(baseline, actual, strict_observation=True), 1)

    def test_failed_base_push_persists_start_checkpoint(self):
        run = object.__new__(driver.Run)
        run.run_id = 'run-123'
        run.checkpoints = []
        run.actors = {'human': object()}
        run.save_state = unittest.mock.Mock()
        run.commit_files = unittest.mock.Mock(side_effect=RuntimeError('write failed'))
        with self.assertRaises(RuntimeError):
            run.step_main_commit({'main_commit': {'x': '{run}'}})
        self.assertEqual(run.checkpoints[0]['kind'], 'base_push_started')
        run.save_state.assert_called_once_with()

    def test_successful_base_push_records_commit(self):
        run = object.__new__(driver.Run)
        run.run_id = 'run-123'
        run.checkpoints = []
        run.actors = {'human': object()}
        run.save_state = unittest.mock.Mock()
        run.commit_files = unittest.mock.Mock(return_value='new-base')
        run.step_main_commit({'main_commit': {'x': '{run}'}})
        self.assertEqual(run.checkpoints[-1]['sha'], 'new-base')

    def test_wait_queued_requires_an_unmatched_entry_and_fails_on_timeout(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.checkpoints = []
        run.timed_out = False
        run.save_state = unittest.mock.Mock()
        run.pr_state = lambda _: ('open', '')
        run.queue_events = lambda: [{'pull_request': 42, 'type': 'action.queue.enter', 'received_at': '2026-01-01T00:00:00Z'}]
        run.step_wait({'wait': 'queued', 'prs': ['a'], 'timeout': 1})
        self.assertEqual(run.checkpoints[0]['events'][0]['received_at'], '2026-01-01T00:00:00Z')
        run.queue_events = lambda: [
            {'pull_request': 42, 'type': 'action.queue.enter'},
            {'pull_request': 42, 'type': 'action.queue.leave'},
        ]
        self.assertFalse(driver.currently_queued(run.queue_events(), 42))
        self.assertTrue(driver.currently_queued(run.queue_events() + [
            {'pull_request': 42, 'type': 'action.queue.enter'}], 42))
        with self.assertRaises(RuntimeError):
            run.step_wait({'wait': 'queued', 'prs': ['a'], 'timeout': 0})
        self.assertTrue(run.timed_out)
        run.save_state.assert_called_once_with()

    def test_wait_dequeued_requires_a_prior_entry_and_leave(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.checkpoints = [{'kind': 'queued', 'prs': ['a'], 'events': [
            {'pull_request': 42, 'type': 'action.queue.enter'}]}]
        run.timed_out = False
        run.save_state = unittest.mock.Mock()
        run.pr_state = lambda _: ('open', '')
        run.queue_events = lambda: [
            {'pull_request': 42, 'type': 'action.queue.enter', 'received_at': '2026-01-01T00:00:00Z'},
            {'pull_request': 42, 'type': 'action.queue.leave', 'received_at': '2026-01-01T00:01:00Z',
             'metadata': {'dequeue_code': 'PR_MANUALLY_DEQUEUED'}},
        ]
        run.step_wait({'wait': 'dequeued', 'prs': ['a'], 'reason': 'PR_MANUALLY_DEQUEUED', 'timeout': 1})
        self.assertEqual(run.checkpoints[-1]['kind'], 'dequeued')
        self.assertEqual(len(run.checkpoints[-1]['events']), 2)
        self.assertFalse(driver.dequeued_since_checkpoint(
            [run.queue_events()[0], {**run.queue_events()[1], 'metadata': {'dequeue_code': 'CHECKS_FAILED'}}],
            42, 'PR_MANUALLY_DEQUEUED', run.checkpoints[0]['events']))
        self.assertFalse(driver.dequeued_since_checkpoint(
            run.queue_events(), 42, 'PR_MANUALLY_DEQUEUED', run.queue_events()))
        run.queue_events = lambda: []
        with self.assertRaises(RuntimeError):
            run.step_wait({'wait': 'dequeued', 'prs': ['a'], 'timeout': 0})

    def test_reference_driver_exits_nonzero_after_timeout(self):
        with tempfile.TemporaryDirectory() as directory:
            scenario = Path(directory) / 'scenario.json'
            scenario.write_text(json.dumps({'id': 'S99', 'title': 'test', 'steps': []}))
            args = unittest.mock.Mock(scenario=str(scenario), no_wait=True, org='schlange-sandbox',
                                      repo='queue-lab', keep=True, baseline=None)
            run = unittest.mock.Mock(timed_out=True)
            with patch.object(driver, 'RUNS', Path(directory)), patch.object(driver, 'Run', return_value=run), \
                 patch.object(driver, 'record'), patch.object(driver, 'write_outcome'), \
                 patch.dict(driver.os.environ, {'SANDBOX_FAIL_ON_TIMEOUT': '1'}):
                with self.assertRaises(SystemExit) as caught:
                    driver.cmd_run(args)
            self.assertEqual(caught.exception.code, 1)

    def test_incomplete_event_log_fails_closed(self):
        class Client:
            def call(self, *_):
                return {'next_cursor': 'more'}
        with self.assertRaises(RuntimeError):
            driver.fetch_events(Client(), 'url', strict=True)
        class MalformedClient:
            def call(self, *_):
                return {'events': [{}]}
        with self.assertRaises(RuntimeError):
            driver.fetch_events(MalformedClient(), 'url', strict=True)

    def test_rejects_queued_or_closed_pull_request(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.checkpoints = []
        run.pr_state = lambda _: ('open', '')
        run.queue_events = lambda: []
        run.step_assert_unqueued({'assert_unqueued': 'a'})
        self.assertEqual(run.checkpoints[0]['kind'], 'unqueued')
        self.assertEqual(run.checkpoints[0]['pr'], 'a')
        self.assertTrue(run.checkpoints[0]['at'])
        run.queue_events = lambda: [{'pull_request': 42, 'type': 'queue.enter'}]
        with self.assertRaises(RuntimeError):
            run.step_assert_unqueued({'assert_unqueued': 'a'})
        self.assertEqual(len(run.checkpoints), 1)
        run.queue_events = lambda: []
        run.pr_state = lambda _: ('merged', '')
        with self.assertRaises(RuntimeError):
            run.step_assert_unqueued({'assert_unqueued': 'a'})

    def test_dispatch_uses_pr_branch_or_main_and_resolves_pr_number(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42, 'branch': 'sbx/test/a'}}
        run.r = '/repos/schlange-sandbox/queue-lab'
        run.checkpoints = []
        client = unittest.mock.Mock()
        run.actor = lambda _: client
        run.step_dispatch_workflow({'dispatch_workflow': 'check-red.yml', 'pr': 'a', 'ref': 'pr'})
        client.call.assert_called_with('POST', run.r + '/actions/workflows/check-red.yml/dispatches',
                                       {'ref': 'sbx/test/a', 'inputs': {}}, retry=False)
        run.step_dispatch_workflow({'dispatch_workflow': 'ai-review.yml', 'pr': 'a', 'ref': 'main',
                                    'inputs': {'pr': '{pr}'}})
        client.call.assert_called_with('POST', run.r + '/actions/workflows/ai-review.yml/dispatches',
                                       {'ref': 'main', 'inputs': {'pr': '42'}}, retry=False)

    def test_prepare_branch_then_open_preserves_pre_push_head(self):
        run = object.__new__(driver.Run)
        run.run_id = 'run-123'
        run.r = '/repos/schlange-sandbox/queue-lab'
        run.scenario = {'id': 'S16'}
        run.prs = {}
        run.prepared = {}
        run.checkpoints = []
        client = unittest.mock.Mock()
        client.call.side_effect = [
            {'object': {'sha': 'old-base'}}, None,
            {'number': 42, 'node_id': 'pr-node'},
        ]
        run.actor = lambda _: client
        run.write_changes = unittest.mock.Mock(return_value='prepared-head')
        run.step_prepare({'prepare': 'a', 'conflict': True})
        run.step_open({'open': 'a', 'prepared': True})
        self.assertEqual(run.prs['a']['branch'], 'sbx/run-123/a')
        self.assertEqual(run.checkpoints[0]['base_sha'], 'old-base')
        self.assertEqual(run.checkpoints[0]['head_sha'], 'prepared-head')
        run.write_changes.assert_called_once()
        self.assertEqual(client.call.call_count, 3)

    def test_preexisting_conflict_checkpoint_requires_head_and_base(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.r = '/repos/schlange-sandbox/queue-lab'
        run.checkpoints = [{'kind': 'prepared', 'pr': 'a', 'head_sha': 'old-head', 'base_sha': 'old-base'},
                           {'kind': 'base_push_completed', 'sha': 'new-base'}]
        client = unittest.mock.Mock()
        client.call.return_value = {'head': {'sha': 'old-head'}, 'base': {'sha': 'new-base'}}
        run.actors = {'human': client}
        run.step_assert_prepared_base({'assert_prepared_base': 'a'})
        client.call.return_value['head']['sha'] = 'changed-head'
        with self.assertRaises(RuntimeError):
            run.step_assert_prepared_base({'assert_prepared_base': 'a'})
        client.call.return_value['head']['sha'] = 'old-head'
        client.call.return_value['base']['sha'] = 'old-base'
        with self.assertRaises(RuntimeError):
            run.step_assert_prepared_base({'assert_prepared_base': 'a'})

    def test_wait_check_records_id_and_rejects_old_result(self):
        run = object.__new__(driver.Run)
        run.prs = {'a': {'number': 42}}
        run.r = '/repos/schlange-sandbox/queue-lab'
        run.checkpoints = [{'kind': 'check', 'name': 'KI-Review', 'pr': 'a', 'id': 5}]
        run.timed_out = False
        run.save_state = unittest.mock.Mock()
        client = unittest.mock.Mock()
        client.call.return_value = {'head': {'sha': 'abc'}}
        run.actors = {'human': client}
        client.paged.return_value = [
            {'id': 5, 'name': 'KI-Review', 'head_sha': 'abc', 'status': 'completed', 'conclusion': 'success'},
            {'id': 6, 'name': 'KI-Review', 'head_sha': 'abc', 'status': 'completed', 'conclusion': 'success'},
        ]
        run.step_wait_check({'wait_check': 'a', 'name': 'KI-Review', 'conclusion': 'success',
                             'after_checkpoint': True, 'timeout': 1})
        self.assertEqual(run.checkpoints[-1]['id'], 6)
        client.paged.return_value = client.paged.return_value[:1]
        with self.assertRaises(RuntimeError):
            run.step_wait_check({'wait_check': 'a', 'name': 'KI-Review', 'conclusion': 'success',
                                 'after_checkpoint': True, 'timeout': 0})
        self.assertTrue(run.timed_out)

    def test_check_pair_requires_same_head_and_app_and_distinct_suites(self):
        run = object.__new__(driver.Run)
        run.checkpoints = [
            {'kind': 'check', 'pr': 'a', 'name': 'ci-ok', 'id': 5, 'head': 'abc',
             'app': 'github-actions', 'suite_id': 7, 'conclusion': 'failure'},
            {'kind': 'check', 'pr': 'a', 'name': 'ci-ok', 'id': 6, 'head': 'abc',
             'app': 'github-actions', 'suite_id': 8, 'conclusion': 'success'},
        ]
        run.step_assert_check_pair({'assert_check_pair': 'a', 'name': 'ci-ok', 'distinct_suites': True})
        run.checkpoints[-1]['head'] = 'other'
        with self.assertRaises(RuntimeError):
            run.step_assert_check_pair({'assert_check_pair': 'a', 'name': 'ci-ok', 'distinct_suites': True})
        run.checkpoints[-1]['head'] = 'abc'
        run.checkpoints[-1]['suite_id'] = 7
        with self.assertRaises(RuntimeError):
            run.step_assert_check_pair({'assert_check_pair': 'a', 'name': 'ci-ok', 'distinct_suites': True})

    def test_workflow_check_identity_requires_real_job_and_workflow(self):
        client = unittest.mock.Mock()
        client.call.side_effect = [
            {'run_id': 123, 'check_run_url': 'https://api.github.com/repos/schlange-sandbox/queue-lab/check-runs/6'},
            {'id': 123, 'path': '.github/workflows/collision-red.yml', 'event': 'workflow_dispatch',
             'head_sha': 'abc', 'created_at': '2026-01-01T00:00:01Z'},
        ]
        check = {'id': 6, 'details_url': 'https://github.com/schlange-sandbox/queue-lab/actions/runs/123/job/456'}
        self.assertEqual(driver.workflow_check_identity(client, '/repos/schlange-sandbox/queue-lab', check,
                         'collision-red.yml', 'workflow_dispatch', 'abc', '2026-01-01T00:00:00Z'), 123)
        check['details_url'] = 'https://evil.invalid/schlange-sandbox/queue-lab/actions/runs/123/job/456'
        self.assertIsNone(driver.workflow_check_identity(client, '/repos/schlange-sandbox/queue-lab', check,
                          'collision-red.yml', 'workflow_dispatch', 'abc', '2026-01-01T00:00:00Z'))
        check['details_url'] = 'https://github.com/schlange-sandbox/queue-lab/actions/runs/123/job/456'
        client.call.side_effect = [
            {'run_id': 123, 'check_run_url': 'https://api.github.com/repos/schlange-sandbox/queue-lab/check-runs/6'},
            {'id': 123, 'path': '.github/workflows/collision-green.yml', 'event': 'workflow_dispatch',
             'head_sha': 'abc', 'created_at': '2026-01-01T00:00:01Z'},
        ]
        self.assertIsNone(driver.workflow_check_identity(client, '/repos/schlange-sandbox/queue-lab', check,
                          'collision-red.yml', 'workflow_dispatch', 'abc', '2026-01-01T00:00:00Z'))

    def test_custom_check_records_only_validated_manual_run_hint(self):
        client = unittest.mock.Mock()
        client.call.return_value = {'id': 123, 'path': '.github/workflows/ai-review.yml',
                                    'event': 'workflow_dispatch', 'head_branch': 'main',
                                    'head_repository': {'full_name': 'schlange-sandbox/queue-lab'},
                                    'created_at': '2026-01-01T00:00:01Z'}
        check = {'id': 6, 'details_url': 'https://github.com/schlange-sandbox/queue-lab/actions/runs/123'}
        self.assertEqual(driver.custom_workflow_hint(client, '/repos/schlange-sandbox/queue-lab', check,
                         'ai-review.yml', '2026-01-01T00:00:00Z'), 123)
        check['details_url'] = 'https://evil.invalid/schlange-sandbox/queue-lab/actions/runs/123'
        self.assertIsNone(driver.custom_workflow_hint(client, '/repos/schlange-sandbox/queue-lab', check,
                          'ai-review.yml', '2026-01-01T00:00:00Z'))
    def test_file_contents_are_unique_per_run(self):
        run = object.__new__(driver.Run)
        run.run_id = 'run-123'
        run.flag_path = lambda _: 'ci/flags/test'
        run.flag_lines = lambda _: ''
        with patch.object(run, 'commit_files') as commit:
            run.write_changes(None, 'branch', 'a', {'files': {'governance/rule.txt': 'change {run}'}}, 1)
        self.assertEqual(commit.call_args.args[2]['governance/rule.txt'], 'change run-123')


if __name__ == '__main__':
    unittest.main()
