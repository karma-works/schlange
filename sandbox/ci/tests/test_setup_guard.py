import os
import subprocess
import tempfile
import unittest
import json
from pathlib import Path

SCRIPT = Path(__file__).resolve().parents[2] / 'setup.sh'
START = Path(__file__).resolve().parents[1] / 'start-schlange.sh'


class SetupGuardTests(unittest.TestCase):
    def test_fixture_reset_retains_owner_preflight(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'preflight'
            program = '''source "$SETUP_SCRIPT"
preflight() { echo owner >> "$TEST_PREFLIGHT"; }
preflight_isolated() { echo isolated >> "$TEST_PREFLIGHT"; }
reset_isolated_fixture() { :; }
main fixture-isolated
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_PREFLIGHT': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(marker.read_text().splitlines(), ['owner'])

    def test_isolated_preflight_does_not_require_membership_api(self):
        program = '''source "$SETUP_SCRIPT"
gh() {
  case "$*" in
    'auth status') return 0;;
    'api user --jq .login') echo sandbox-user;;
    'api orgs/schlange-sandbox') return 0;;
    'api repos/schlange-sandbox/schlange-e2e --jq .full_name') echo "${TEST_REPO_NAME:-schlange-sandbox/schlange-e2e}";;
    'api repos/schlange-sandbox/schlange-e2e --jq .permissions.admin') echo "${TEST_REPO_ADMIN:-true}";;
    *memberships*) echo forbidden >&2; return 1;;
    *) return 1;;
  esac
}
preflight_isolated
test "$ME" = sandbox-user
'''
        env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
               'SANDBOX_HUMAN_TOKEN': 'sandbox-test-token'}
        result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        denied = subprocess.run(['bash', '-c', program], env={**env, 'TEST_REPO_ADMIN': 'false'},
                                capture_output=True, text=True)
        self.assertNotEqual(denied.returncode, 0)
        wrong = subprocess.run(['bash', '-c', program], env={**env, 'TEST_REPO_NAME': 'other/repo'},
                               capture_output=True, text=True)
        self.assertNotEqual(wrong.returncode, 0)

    def test_isolated_creation_refusal_stops_before_any_fixture_write(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'fixture-called'
            program = '''source "$SETUP_SCRIPT"
app_field() { case "$2" in id) echo 123;; bot_login) echo bot;; esac; }
gh() { return 1; }
initialize_isolated_fixture() { touch "$TEST_FIXTURE_CALLED"; }
cmd_repos_isolated
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_FIXTURE_CALLED': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())

    def test_new_isolated_fixture_never_clones_or_removes_existing_files(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'git-calls'
            program = '''source "$SETUP_SCRIPT"
ME=fixture-owner
render_fixture() { printf 'fixture\\n' > "$1/file"; }
gh() { echo 'Git Repository is empty. (HTTP 409)' >&2; return 1; }
git() {
  printf '%s\\n' "$*" >> "$TEST_GIT_CALLS"
  if [ "$1" = init ]; then mkdir -p "${@: -1}"; fi
}
initialize_isolated_fixture
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_GIT_CALLS': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            calls = marker.read_text()
            self.assertNotIn('clone', calls)
            self.assertNotIn(' rm ', calls)
            self.assertNotIn('--force', calls)
            self.assertIn('push -q origin main', calls)

    def test_new_isolated_fixture_refuses_nonempty_repo(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'git-called'
            program = '''source "$SETUP_SCRIPT"
gh() { return 0; }
git() { touch "$TEST_GIT_CALLED"; }
initialize_isolated_fixture
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_GIT_CALLED': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())

    def test_new_isolated_fixture_cannot_overwrite_concurrent_initial_commit(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            bare = root / 'remote.git'
            seed = root / 'seed'
            subprocess.run(['git', 'init', '-q', '--bare', str(bare)], check=True)
            subprocess.run(['git', 'init', '-q', '-b', 'main', str(seed)], check=True)
            (seed / 'existing').write_text('keep\n')
            subprocess.run(['git', '-C', str(seed), 'add', '-A'], check=True)
            subprocess.run(['git', '-C', str(seed), '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                            'commit', '-qm', 'existing'], check=True)
            subprocess.run(['git', '-C', str(seed), 'push', '-q', str(bare), 'main'], check=True)
            before = subprocess.check_output(['git', '--git-dir', str(bare), 'rev-parse', 'refs/heads/main'], text=True).strip()
            program = '''source "$SETUP_SCRIPT"
ME=fixture-owner
gh() { echo 'Git Repository is empty. (HTTP 409)' >&2; return 1; }
render_fixture() { printf 'fixture\\n' > "$1/file"; }
initialize_isolated_fixture
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'GIT_CONFIG_COUNT': '1', 'GIT_CONFIG_KEY_0': f'url.file://{bare}.insteadOf',
                   'GIT_CONFIG_VALUE_0': 'https://github.com/schlange-sandbox/schlange-e2e-isolated.git'}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertNotIn('command not found', result.stderr)
            after = subprocess.check_output(['git', '--git-dir', str(bare), 'rev-parse', 'refs/heads/main'], text=True).strip()
            self.assertEqual(before, after)

    def test_isolated_fixture_reset_checks_all_open_prs_before_push(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'push-called'
            program = '''source "$SETUP_SCRIPT"
python3() { return 1; }
push_fixture() { touch "$TEST_PUSH_CALLED"; }
reset_isolated_fixture
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_PUSH_CALLED': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())
            self.assertNotIn('command not found', result.stderr)

    def test_isolated_ruleset_requires_default_branch_and_queue_bypass(self):
        ruleset = {'target': 'branch', 'enforcement': 'active',
                   'conditions': {'ref_name': {'include': ['~DEFAULT_BRANCH'], 'exclude': []}},
                   'bypass_actors': [{'actor_type': 'Integration', 'actor_id': 123, 'bypass_mode': 'always'}],
                   'rules': [{'type': 'required_status_checks', 'parameters': {'required_status_checks': [
                       {'context': 'ci-ok'}, {'context': 'Risk class and data protection'}]}}]}
        program = '''source "$SETUP_SCRIPT"
gh() { if [[ "$*" == *'--jq'* ]]; then echo 7; else printf '%s' "$TEST_RULESET"; fi; }
verify_isolated_ruleset schlange-sandbox/schlange-e2e-isolated 123
'''
        env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
               'TEST_RULESET': json.dumps(ruleset)}
        self.assertEqual(subprocess.run(['bash', '-c', program], env=env, capture_output=True).returncode, 0)
        ruleset['conditions']['ref_name']['include'] = ['refs/heads/other']
        env['TEST_RULESET'] = json.dumps(ruleset)
        self.assertNotEqual(subprocess.run(['bash', '-c', program], env=env, capture_output=True).returncode, 0)

    def test_isolated_resume_dry_run_never_invokes_install(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'python-called'
            program = '''source "$SETUP_SCRIPT"
DRY=true
app_field() { case "$2" in id) echo 123;; bot_login) echo bot;; esac; }
verify_isolated_fixture() { return 0; }
python3() { touch "$TEST_PYTHON_CALLED"; }
configure_repo() { :; }
cmd_finish_isolated
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_PYTHON_CALLED': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse(marker.exists())
            self.assertNotIn('access to schlange-sandbox/schlange-e2e-isolated was verified', result.stdout)

    def test_resume_verifies_exact_fixture_git_tree(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture = root / 'fixture'
            fixture.mkdir()
            (fixture / 'file').write_text('expected\n')
            repo = root / 'remote'
            subprocess.run(['git', 'init', '-q', '-b', 'main', str(repo)], check=True)
            (repo / 'file').write_text('expected\n')
            subprocess.run(['git', '-C', str(repo), 'add', '-A'], check=True)
            subprocess.run(['git', '-C', str(repo), '-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
                            'commit', '-qm', 'fixture'], check=True)
            program = '''source "$SETUP_SCRIPT"
gh() { if [[ "$*" == *'.default_branch'* ]]; then echo "${TEST_DEFAULT_BRANCH:-main}"; else echo schlange-sandbox/schlange-e2e-isolated; fi; }
render_fixture() { cp -R "$TEST_FIXTURE/." "$1/"; }
git() {
  if [[ " $* " == *" clone "* ]]; then
    command git clone -q --branch main "$TEST_REPO" "${@: -1}"
  else
    command git "$@"
  fi
}
verify_isolated_fixture
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_FIXTURE': str(fixture), 'TEST_REPO': str(repo)}
            self.assertEqual(subprocess.run(['bash', '-c', program], env=env, capture_output=True).returncode, 0)
            wrong_default = {**env, 'TEST_DEFAULT_BRANCH': 'other'}
            self.assertNotEqual(subprocess.run(['bash', '-c', program], env=wrong_default, capture_output=True).returncode, 0)
            (fixture / 'file').write_text('different\n')
            self.assertNotEqual(subprocess.run(['bash', '-c', program], env=env, capture_output=True).returncode, 0)

    def test_isolated_resume_stops_before_install_if_fixture_not_verified(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'install-called'
            program = '''source "$SETUP_SCRIPT"
app_field() { case "$2" in id) echo 123;; bot_login) echo bot;; esac; }
verify_isolated_fixture() { return 1; }
python3() { touch "$TEST_INSTALL_CALLED"; }
cmd_finish_isolated
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_INSTALL_CALLED': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertFalse(marker.exists())
            self.assertIn('fixture', result.stderr)

    def test_disposable_configuration_never_falls_back_to_missing_bypass(self):
        with tempfile.TemporaryDirectory() as directory:
            marker = Path(directory) / 'ruleset-calls'
            program = '''source "$SETUP_SCRIPT"
run() { :; }
ruleset_apply() { printf '%s\\n' "$3" >> "$TEST_RULESET_CALLS"; return 1; }
configure_repo schlange-e2e-isolated bot 123 required
'''
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SETUP_SCRIPT': str(SCRIPT),
                   'TEST_RULESET_CALLS': str(marker)}
            result = subprocess.run(['bash', '-c', program], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertEqual(marker.read_text().splitlines(), ['true'])

    def test_start_rejects_arbitrary_repo_before_credentials(self):
        result = subprocess.run(['bash', str(START)],
                                env={**os.environ, 'SANDBOX_REPO': 'unrelated',
                                     'SCHLANGE_SANDBOX_CONFIG': '/nonexistent', 'RUNNER_TEMP': '/nonexistent'},
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('refused', result.stderr)

    def test_setup_requires_explicit_sandbox_token(self):
        with tempfile.TemporaryDirectory() as directory:
            env = {**os.environ, 'SANDBOX_ORG': 'schlange-sandbox', 'SCHLANGE_SANDBOX_CONFIG': directory}
            env.pop('SANDBOX_HUMAN_TOKEN', None)
            fake_gh = Path(directory) / 'gh'
            fake_gh.write_text('#!/bin/sh\ntouch "$TEST_GH_CALLED"\nexit 0\n')
            fake_gh.chmod(0o755)
            marker = Path(directory) / 'called'
            env.update(PATH=directory + ':' + os.environ['PATH'], TEST_GH_CALLED=str(marker))
            result = subprocess.run(['bash', str(SCRIPT), 'check'], env=env, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn('sandbox-scoped human token', result.stderr)
            self.assertFalse(marker.exists())

    def test_refuses_non_sandbox_before_any_setup(self):
        result = subprocess.run(['bash', str(SCRIPT), 'fixture-e2e'],
                                env={**os.environ, 'SANDBOX_ORG': 'not-schlange-sandbox'},
                                capture_output=True, text=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn('only schlange-sandbox', result.stderr)


if __name__ == '__main__':
    unittest.main()
