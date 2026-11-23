import hashlib
import hmac
import importlib.util
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).resolve().parents[1] / 'signed-webhook-probe.py'
WORKFLOW = Path(__file__).resolve().parents[3] / '.github' / 'workflows' / 'sandbox.yml'
spec = importlib.util.spec_from_file_location('signed_webhook_probe', SCRIPT)
probe = importlib.util.module_from_spec(spec)
spec.loader.exec_module(probe)


class SignedWebhookProbeTests(unittest.TestCase):
    def event(self, body, signature):
        return {
            'body': body,
            'query': {},
            'x-github-event': 'pull_request',
            'x-github-delivery': '11111111-2222-3333-4444-555555555555',
            'x-hub-signature-256': signature,
            'content-type': 'application/json',
        }

    def payload(self):
        return {
            'action': 'opened',
            'number': 42,
            'repository': {'full_name': 'schlange-sandbox/queue-lab'},
            'pull_request': {
                'number': 42,
                'draft': True,
                'user': {'login': 'sandbox-driver[bot]'},
                'head': {'ref': 'sbx/ingress-123-1/a'},
                'base': {'ref': 'main'},
            },
            'sender': {'login': 'sandbox-driver[bot]'},
        }

    def test_compact_json_is_accepted_only_when_it_is_the_original_signed_representation(self):
        secret = b'test-webhook-secret-long-enough'
        payload = self.payload()
        original = json.dumps(payload, ensure_ascii=False, separators=(',', ':')).encode()
        signature = 'sha256=' + hmac.new(secret, original, hashlib.sha256).hexdigest()
        selected = probe.select_signed_event(
            self.event(payload, signature), secret, 'schlange-sandbox/queue-lab',
            'sbx/ingress-123-1/a', 42, 'sandbox-driver[bot]')
        self.assertEqual(selected.body, original)
        self.assertEqual(selected.representation, 'json_compact')
        self.assertNotIn(signature, repr(selected))

        changed = json.loads(json.dumps(payload))
        changed['action'] = 'closed'
        changed_body = json.dumps(changed, ensure_ascii=False, separators=(',', ':')).encode()
        changed_signature = 'sha256=' + hmac.new(
            secret, changed_body, hashlib.sha256).hexdigest()
        with self.assertRaisesRegex(probe.ProbeError, 'event_scope_invalid'):
            probe.select_signed_event(
                self.event(changed, changed_signature), secret, 'schlange-sandbox/queue-lab',
                'sbx/ingress-123-1/a', 42, 'sandbox-driver[bot]')

    def test_unmatched_relay_representation_and_malformed_headers_fail_closed(self):
        secret = b'test-webhook-secret-long-enough'
        payload = self.payload()
        event = self.event(payload, 'sha256=' + '0' * 64)
        with self.assertRaisesRegex(probe.ProbeError, 'representation_unverified'):
            probe.select_signed_event(
                event, secret, 'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a',
                42, 'sandbox-driver[bot]')
        event['x-github-delivery'] = ['not', 'scalar']
        with self.assertRaisesRegex(probe.ProbeError, 'headers_invalid'):
            probe.select_signed_event(
                event, secret, 'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a',
                42, 'sandbox-driver[bot]')
        event = self.event(payload, 'sha256=' + '0' * 64)
        event['content-type'] = 'application/x-www-form-urlencoded'
        with self.assertRaisesRegex(probe.ProbeError, 'headers_invalid'):
            probe.select_signed_event(
                event, secret, 'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a',
                42, 'sandbox-driver[bot]')

    def test_exact_text_body_is_verified_without_reserializing_it(self):
        secret = b'test-webhook-secret-long-enough'
        original = json.dumps(self.payload(), ensure_ascii=False).encode()
        signature = 'sha256=' + hmac.new(secret, original, hashlib.sha256).hexdigest()
        selected = probe.select_signed_event(
            self.event(original.decode(), signature), secret,
            'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a', 42,
            'sandbox-driver[bot]')
        self.assertEqual(selected.body, original)
        self.assertEqual(selected.representation, 'string')

    def test_signed_malformed_payload_fails_scope_without_type_errors(self):
        secret = b'test-webhook-secret-long-enough'
        payload = self.payload()
        payload['repository'] = []
        original = json.dumps(payload, separators=(',', ':')).encode()
        signature = 'sha256=' + hmac.new(secret, original, hashlib.sha256).hexdigest()
        with self.assertRaisesRegex(probe.ProbeError, 'event_scope_invalid'):
            probe.select_signed_event(
                self.event(payload, signature), secret,
                'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a', 42,
                'sandbox-driver[bot]')

    def test_untrusted_prefilter_skips_unrelated_frames_before_header_validation(self):
        unrelated = self.event(self.payload(), 'not-a-signature')
        unrelated['body']['number'] = 41
        unrelated['body']['pull_request']['number'] = 41
        self.assertFalse(probe.matches_target_untrusted(
            unrelated, 'schlange-sandbox/queue-lab',
            'sbx/ingress-123-1/a', 42, 'sandbox-driver[bot]'))

        target = self.event(self.payload(), 'not-a-signature')
        self.assertTrue(probe.matches_target_untrusted(
            target, 'schlange-sandbox/queue-lab',
            'sbx/ingress-123-1/a', 42, 'sandbox-driver[bot]'))
        with self.assertRaisesRegex(probe.ProbeError, 'headers_invalid'):
            probe.select_signed_event(
                target, b'test-webhook-secret-long-enough',
                'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a', 42,
                'sandbox-driver[bot]')

    def test_verified_non_draft_target_is_rejected(self):
        secret = b'test-webhook-secret-long-enough'
        payload = self.payload()
        payload['pull_request']['draft'] = False
        original = json.dumps(payload, separators=(',', ':')).encode()
        signature = 'sha256=' + hmac.new(secret, original, hashlib.sha256).hexdigest()
        with self.assertRaisesRegex(probe.ProbeError, 'event_scope_invalid'):
            probe.select_signed_event(
                self.event(payload, signature), secret,
                'schlange-sandbox/queue-lab', 'sbx/ingress-123-1/a', 42,
                'sandbox-driver[bot]')

    def test_missing_relay_variable_reads_existing_hook_configuration_in_memory(self):
        with mock.patch.object(probe.driver, 'app_hook_url', return_value='https://smee.io/abcdefgh') as read:
            self.assertEqual(probe._configured_relay_url(None), 'https://smee.io/abcdefgh')
        read.assert_called_once_with('queue')

    def test_driver_hook_configuration_read_is_exact_bounded_and_redirect_disabled(self):
        class Response:
            def __enter__(self):
                return self

            def __exit__(self, *_args):
                return False

            def read(self, limit):
                self.limit = limit
                return b'{"url":"https://smee.io/abcdefgh"}'

        response = Response()
        with mock.patch.object(probe.driver, 'app_jwt', return_value='private-jwt'), \
                mock.patch.object(probe.driver.NO_REDIRECT_OPENER, 'open', return_value=response) as opened:
            self.assertEqual(
                probe.driver.app_hook_url('queue'), 'https://smee.io/abcdefgh')
        request = opened.call_args.args[0]
        self.assertEqual(request.full_url, 'https://api.github.com/app/hook/config')
        self.assertEqual(response.limit, 16_385)

    def test_zombie_process_is_not_considered_running(self):
        with tempfile.TemporaryDirectory() as directory:
            proc = Path(directory)
            process = proc / '7'
            process.mkdir()
            (process / 'stat').write_text('7 (schlange) Z 1 2 3\n')
            self.assertFalse(probe._process_running(7, proc))

    def test_service_identity_is_bound_to_binary_repo_data_and_loopback(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            proc = root / 'proc'
            process = proc / '7'
            process.mkdir(parents=True)
            binary = root / 'schlange'
            binary.write_bytes(b'binary')
            (process / 'exe').symlink_to(binary)
            data = root / 'data'
            data.mkdir()
            command = [
                str(binary), 'serve', '--shadow', '--repo',
                'schlange-sandbox/queue-lab', '--data-dir', str(data),
                '--listen', '127.0.0.1:8080', '--no-lease',
            ]
            (process / 'cmdline').write_bytes(b'\0'.join(
                item.encode() for item in command) + b'\0')
            probe._verify_service_identity(7, binary, data, proc)

            command[4] = 'schlange-sandbox/schlange-e2e'
            (process / 'cmdline').write_bytes(b'\0'.join(
                item.encode() for item in command) + b'\0')
            with self.assertRaisesRegex(probe.ProbeError, 'service_identity_invalid'):
                probe._verify_service_identity(7, binary, data, proc)

    def test_sse_parser_is_bounded_and_requires_one_json_data_event(self):
        event = probe.parse_sse_data([
            b'event: message\n',
            b'data: {"body":"{}","query":{}}\n',
            b'\n',
        ])
        self.assertEqual(event, {'body': '{}', 'query': {}})
        with self.assertRaisesRegex(probe.ProbeError, 'relay_event_invalid'):
            probe.parse_sse_data([b'data: []\n', b'\n'])

    def test_sse_reader_ignores_comment_only_heartbeats(self):
        class Response:
            def __init__(self):
                self.lines = iter((
                    b': heartbeat\n', b'\n',
                    b'data: {"body":"{}","query":{}}\n', b'\n'))

            def readline(self, _limit):
                return next(self.lines, b'')

        self.assertEqual(
            probe._next_sse(Response()),
            {'body': '{}', 'query': {}})

    def test_persistent_store_shape_counts_one_processed_delivery_without_ids(self):
        with tempfile.TemporaryDirectory() as directory:
            database = Path(directory) / 'schlange.db'
            db = sqlite3.connect(database)
            db.executescript('''
                CREATE TABLE inbox (guid TEXT PRIMARY KEY, repo TEXT, event TEXT,
                    body BLOB, at INTEGER, processed INTEGER);
                CREATE TABLE deliveries (guid TEXT PRIMARY KEY, at INTEGER);
                INSERT INTO inbox VALUES ('private-id', 'schlange-sandbox/queue-lab',
                    'pull_request', x'7b7d', 1, 1);
                INSERT INTO deliveries VALUES ('private-id', 2);
            ''')
            db.commit()
            db.close()
            self.assertEqual(
                probe.delivery_shape(database, 'schlange-sandbox/queue-lab'),
                {'inbox_rows': 1, 'pending_rows': 0, 'delivery_rows': 1})

    def test_workflow_is_fixed_to_actions_loopback_and_has_no_downloaded_forwarder(self):
        workflow = WORKFLOW.read_text()
        self.assertIn("if: ${{ inputs.scenarios == 'verify-signed-ingress' }}", workflow)
        self.assertIn("if: ${{ inputs.scenarios != 'verify-signed-ingress' }}", workflow)
        self.assertIn('SANDBOX_SMEE_URL: ${{ vars.SANDBOX_SMEE_URL }}', workflow)
        self.assertIn('SANDBOX_REPO: queue-lab', workflow)
        self.assertIn('SCHLANGE_PORT: 8080', workflow)
        self.assertIn('signed-webhook-probe.py', workflow)
        self.assertIn("group: sandbox-${{ inputs.repo || 'queue-lab' }}", workflow)
        self.assertNotIn('signed_ingress:', workflow)
        self.assertNotIn('npx', workflow)
        self.assertNotIn('0.0.0.0', workflow)


if __name__ == '__main__':
    unittest.main()
