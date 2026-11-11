"""Offline contract tests for schlange-setup; every gh invocation is intercepted."""
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

SCRIPT = Path(__file__).resolve().parents[1] / 'schlange-setup.sh'
MOCK = '''#!/usr/bin/env python3
import json, os, subprocess, sys
from pathlib import Path
p = Path(os.environ['MOCK_STATE'])
s = json.loads(p.read_text())
a = sys.argv[1:]
if a[:2] == ['auth', 'status']: sys.exit(0)
if a[:2] == ['label', 'view']: print('halt'); sys.exit(0)
if a[:2] == ['label', 'create']: sys.exit(0)
assert a.pop(0) == 'api', a
method = 'GET'
if '-X' in a:
 i=a.index('-X'); method=a[i+1]; del a[i:i+2]
endpoint=a.pop(0)
if endpoint.endswith('/rulesets') and os.environ.get('MOCK_FAIL_RULESET_LIST'):
 print('unavailable',file=sys.stderr); sys.exit(1)
body = None
if '--input' in a: body = json.load(sys.stdin)
if method != 'GET':
 s['writes'].append([method, endpoint, body])
 if '/rulesets/' in endpoint:
  rid=endpoint.rsplit('/',1)[1]; body['id']=int(rid); s['rulesets'][rid]=body
 elif endpoint.endswith('/rulesets'):
  rid='99'; body['id']=99; s['rulesets'][rid]=body
 elif endpoint.endswith('/restrictions/apps'):
  slug=next(x[7:] for x in a if x.startswith('apps[]='))
  apps=s['classic']['apps']
  if method=='POST': apps.append({'slug':slug})
  elif method=='DELETE': s['classic']['apps']=[x for x in apps if x['slug']!=slug]
 p.write_text(json.dumps(s)); print('{}'); sys.exit(0)
if endpoint == 'repos/test/repo': out={'permissions':{'admin':True}, 'default_branch':'main'}
elif endpoint == 'user': out={'login':'admin'}
elif endpoint == 'apps/schlange': out={'slug':'schlange','id':42}
elif endpoint == 'apps/mergify': out={'slug':'mergify','id':7}
elif endpoint.endswith('/actions/workflows'): out={'workflows':[]}
elif endpoint.endswith('/rulesets'): out=list(s['rulesets'].values())
elif '/rulesets/' in endpoint: out=s['rulesets'][endpoint.rsplit('/',1)[1]]
elif endpoint.endswith('/protection/restrictions'): out=s['classic']
elif endpoint.endswith('/protection'): out={'restrictions':s['classic']}
else: raise AssertionError(endpoint)
if '--jq' in a:
 q=a[a.index('--jq')+1]
 r=subprocess.run(['jq','-r',q],input=json.dumps(out),text=True,capture_output=True)
 sys.stdout.write(r.stdout); sys.stderr.write(r.stderr); sys.exit(r.returncode)
print(json.dumps(out))
'''


def ruleset(name='Merge nur über Mergify', rid=1, include=None, exclude=None):
    return dict(id=rid, name=name, target='branch', enforcement='active',
                conditions={'ref_name':{'include':include or ['~DEFAULT_BRANCH'], 'exclude':exclude or []}},
                rules=[{'type':'update'}], bypass_actors=[
                    {'actor_type':'Integration','actor_id':7,'bypass_mode':'exempt'},
                    {'actor_type':'RepositoryRole','actor_id':5,'bypass_mode':'pull_request'}])


class Setup(unittest.TestCase):
    def setUp(self):
        self.tmp = tempfile.TemporaryDirectory()
        self.addCleanup(self.tmp.cleanup)
        self.root = Path(self.tmp.name)
        (self.root/'gh').write_text(MOCK)
        (self.root/'gh').chmod(0o755)
        self.state=self.root/'state.json'
        self.save([ruleset()])
        self.env={**os.environ,'PATH':str(self.root)+':'+os.environ['PATH'],
                  'SCHLANGE_REPO':'test/repo','SCHLANGE_APP_ID':'42',
                  'SCHLANGE_APP_SLUG':'schlange',
                  'SCHLANGE_BACKUP':str(self.root/'backup'),'MOCK_STATE':str(self.state)}

    def save(self, rs):
        self.state.write_text(json.dumps({'rulesets':{str(x['id']):x for x in rs},
                                        'classic':{'apps':[{'slug':'mergify'}],'users':[],'teams':[]},'writes':[]}))

    def run_script(self,*args,code=0):
        r=subprocess.run(['bash',str(SCRIPT),*args],env=self.env,text=True,capture_output=True)
        self.assertEqual(r.returncode,code,r.stdout+r.stderr)
        return r

    def read(self): return json.loads(self.state.read_text())

    def test_yes_before_or_after_command(self):
        self.run_script('--yes','setup')
        self.run_script('setup','--yes')

    def test_unknown_arguments_rejected_without_writes(self):
        self.run_script('status','--wat',code=64)
        self.assertEqual(self.read()['writes'],[])

    def test_lock_refuses_legacy_lock(self):
        self.run_script('lock-main','--yes',code=1)
        self.assertEqual(self.read()['writes'],[])

    def test_failed_ruleset_read_never_creates_competing_lock(self):
        self.env['MOCK_FAIL_RULESET_LIST']='1'
        self.run_script('lock-main','--yes',code=1)
        self.assertEqual(self.read()['writes'],[])

    def test_cutover_migrates_in_place_removes_old_writer_and_is_idempotent(self):
        self.run_script('cutover-from-mergify','--revoke-app','7','--yes')
        s=self.read()
        self.assertEqual(list(s['rulesets']),['1'])
        lock=s['rulesets']['1']
        self.assertEqual(lock['name'],'Merge only via Schlange')
        self.assertEqual(lock['bypass_actors'],[
            {'actor_id':42,'actor_type':'Integration','bypass_mode':'exempt'},
            {'actor_id':5,'actor_type':'RepositoryRole','bypass_mode':'pull_request'}])
        self.assertEqual(s['classic']['apps'],[{'slug':'schlange'}])
        first=s['writes'][0]
        self.assertEqual(first[:2],['PUT','repos/test/repo/rulesets/1'])
        self.assertEqual([a['actor_id'] for a in first[2]['bypass_actors']
                          if a['actor_type']=='Integration'],[42])
        self.assertTrue(list((self.root/'backup').glob('*/rulesets/1.json')))
        self.run_script('--yes','cutover-from-mergify','--revoke-app','7')
        self.assertEqual(list(self.read()['rulesets']),['1'])

    def test_cutover_rejects_ambiguous_locks_before_writing(self):
        self.save([ruleset(),ruleset('Other update lock',rid=2)])
        self.run_script('cutover-from-mergify','--revoke-app','7','--yes',code=1)
        self.assertEqual(self.read()['writes'],[])

    def test_cutover_does_not_revoke_same_app(self):
        self.run_script('cutover-from-mergify','--revoke-app','42','--yes',code=1)
        self.assertEqual(self.read()['writes'],[])

    def test_excluded_main_ruleset_is_not_granted_bypass(self):
        rs=ruleset('Release only',include=['~ALL'],exclude=['refs/heads/main'])
        self.save([rs])
        self.run_script('setup','--yes')
        self.assertEqual(self.read()['rulesets']['1'],rs)

    def test_wildcard_main_lock_prevents_duplicate_lock(self):
        self.save([ruleset(include=['refs/heads/*'])])
        self.run_script('lock-main','--yes',code=1)
        self.assertEqual(self.read()['writes'],[])

    def test_cutover_removes_unexpected_integration_from_exclusive_lock(self):
        rs=ruleset()
        rs['bypass_actors'].append({'actor_id':99,'actor_type':'Integration','bypass_mode':'always'})
        self.save([rs])
        self.run_script('cutover-from-mergify','--revoke-app','7','--yes')
        actors=self.read()['rulesets']['1']['bypass_actors']
        self.assertEqual([x['actor_id'] for x in actors if x['actor_type']=='Integration'],[42])

    def test_cutover_removes_old_bypass_from_other_main_protections(self):
        checks=ruleset('Checks',rid=2)
        checks['rules']=[{'type':'required_status_checks','parameters':{'required_status_checks':[{'context':'ci'}]}}]
        self.save([ruleset(),checks])
        self.run_script('cutover-from-mergify','--revoke-app','7','--yes')
        rs=self.read()['rulesets']['2']
        self.assertEqual(rs['rules'],checks['rules'])
        self.assertEqual([x['actor_id'] for x in rs['bypass_actors'] if x['actor_type']=='Integration'],[42])

    def test_status_shows_all_integration_actors_and_classic_restrictions(self):
        out=self.run_script('status').stdout
        self.assertIn('7',out)
        self.assertIn('exempt',out)
        self.assertIn('mergify',out)
        self.assertEqual(self.read()['writes'],[])


if __name__ == '__main__': unittest.main()
