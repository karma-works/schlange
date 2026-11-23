#!/usr/bin/env python3
"""Validate one workflow run's reference lifecycle against its independent outcomes."""
import importlib.util
import json
import os
import re
import sys
from pathlib import Path

DRIVER_PATH = Path(__file__).resolve().parents[1] / 'driver' / 'sandbox.py'
DRIVER_SPEC = importlib.util.spec_from_file_location('schlange_sandbox_driver', DRIVER_PATH)
driver = importlib.util.module_from_spec(DRIVER_SPEC)
DRIVER_SPEC.loader.exec_module(driver)

RUNS_MAX = 32
PRS_MAX = 128
EVENTS_MAX = driver.REFERENCE_ACTIVITY_MAX_EVENTS
REPOSITORY = 'schlange-sandbox/queue-lab'
EVENT_TYPES = ['action.queue.enter', 'action.queue.leave', 'action.queue.merged']
INCOMPLETE_REASONS = frozenset({
    'source_missing', 'recording_invalid', 'activity_incomplete', 'binding_invalid',
    'event_invalid', 'lifecycle_inconsistent', 'outcome_mismatch', 'bound_exceeded',
})


class CheckError(Exception):
    pass


def _fail(reason):
    raise CheckError(reason)


def _time(value, reason='binding_invalid'):
    try:
        return driver._activity_time(value)
    except (driver._ReferenceActivityError, TypeError, ValueError, OverflowError):
        _fail(reason)


def _source(environ):
    run = environ.get('GITHUB_RUN_ID')
    attempt = environ.get('GITHUB_RUN_ATTEMPT')
    commit = environ.get('GITHUB_SHA')
    repository = environ.get('GITHUB_REPOSITORY')
    if (environ.get('GITHUB_ACTIONS') != 'true'
            or not isinstance(run, str) or not run.isascii() or not run.isdigit()
            or not 0 < int(run) <= 2**63 - 1
            or not isinstance(attempt, str) or not attempt.isascii() or not attempt.isdigit()
            or not 0 < int(attempt) <= 2**31 - 1
            or not isinstance(commit, str) or not re.fullmatch(r'[0-9a-f]{40}', commit)
            or not isinstance(repository, str)
            or not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository)):
        _fail('source_missing')
    return {'workflow_run_id': int(run), 'workflow_run_attempt': int(attempt),
            'workflow_commit': commit, 'workflow_repository': repository}


def _recording(state, outcome):
    if not isinstance(state, dict) or not isinstance(outcome, dict):
        _fail('recording_invalid')
    run_id = state.get('run_id')
    scenario = outcome.get('scenario')
    if (state.get('org') != 'schlange-sandbox' or state.get('repo') != 'queue-lab'
            or state.get('timed_out') is not False
            or outcome.get('repo') != 'queue-lab'
            or not isinstance(run_id, str) or not re.fullmatch(r's\d{2}-queue-lab-[a-z0-9-]+', run_id)
            or not isinstance(scenario, str) or not re.fullmatch(r'S\d{2}', scenario)
            or not run_id.startswith(scenario.lower() + '-')):
        _fail('recording_invalid')
    started, finished = _time(state.get('started')), _time(state.get('finished'))
    if finished < started:
        _fail('recording_invalid')
    raw_prs, outcome_prs = state.get('prs'), outcome.get('prs')
    if (not isinstance(raw_prs, dict) or not raw_prs or len(raw_prs) > PRS_MAX
            or not isinstance(outcome_prs, dict) or set(raw_prs) != set(outcome_prs)):
        _fail('recording_invalid')
    numbers = {}
    for alias, pull in raw_prs.items():
        if (not isinstance(alias, str) or not isinstance(pull, dict)
                or type(pull.get('number')) is not int or pull['number'] <= 0
                or pull['number'] in numbers.values()):
            _fail('recording_invalid')
        numbers[alias] = pull['number']
    return run_id, started, finished, numbers


def _event_shape(event, start, finish, selected):
    if (not isinstance(event, dict) or type(event.get('id')) is not int or event['id'] <= 0
            or type(event.get('pull_request')) is not int
            or event['pull_request'] not in selected or event.get('kind') not in {'enter', 'leave', 'merged'}):
        _fail('event_invalid')
    at = _time(event.get('received_at'), 'event_invalid')
    if not start <= at <= finish:
        _fail('event_invalid')
    common = {'id', 'received_at', 'pull_request', 'kind'}
    if event['kind'] == 'enter':
        if set(event) != common:
            _fail('event_invalid')
    elif event['kind'] == 'leave':
        if (set(event) != common | {'merged', 'reason_code'}
                or type(event.get('merged')) is not bool
                or not isinstance(event.get('reason_code'), str)
                or event['reason_code'] not in driver.REFERENCE_DEQUEUE_CODES):
            _fail('event_invalid')
    else:
        if (set(event) != common | {'merge_commit_sha'}
                or not isinstance(event.get('merge_commit_sha'), str)
                or not re.fullmatch(r'[0-9a-f]{40}', event['merge_commit_sha'])):
            _fail('event_invalid')
    return at


def verify_run(state, outcome, activity, environ=None):
    environ = os.environ if environ is None else environ
    expected_source = _source(environ)
    run_id, start, finish, numbers = _recording(state, outcome)
    if (not isinstance(activity, dict)
            or set(activity) != {'schema', 'repository', 'scenario_run_id',
                                 'scenario_finished_at', 'collected_at', 'source', 'query',
                                 'complete', 'incomplete_reason', 'events'}
            or type(activity.get('schema')) is not int or activity.get('schema') != 1
            or activity.get('repository') != REPOSITORY
            or activity.get('scenario_run_id') != run_id
            or activity.get('scenario_finished_at') != state['finished']
            or activity.get('complete') is not True or activity.get('incomplete_reason') is not None):
        _fail('activity_incomplete')
    source = activity.get('source')
    if (not isinstance(source, dict) or set(source) != set(expected_source)
            or type(source.get('workflow_run_id')) is not int
            or type(source.get('workflow_run_attempt')) is not int
            or source != expected_source):
        _fail('binding_invalid')
    collected = _time(activity.get('collected_at'))
    if collected < finish:
        _fail('binding_invalid')
    query = activity.get('query')
    if (not isinstance(query, dict)
            or not isinstance(query.get('pull_requests'), list)
            or any(type(number) is not int for number in query.get('pull_requests', []))
            or query != {'received_from': state['started'], 'received_to': state['finished'],
                         'pull_requests': sorted(numbers.values()),
                         'event_types': EVENT_TYPES}):
        _fail('binding_invalid')
    events = activity.get('events')
    if not isinstance(events, list) or len(events) > EVENTS_MAX:
        _fail('bound_exceeded' if isinstance(events, list) else 'event_invalid')
    lifecycle = {number: {'active': False, 'awaiting_merge': False, 'merged': False,
                          'entries': 0, 'dequeues': []} for number in numbers.values()}
    seen_ids = set()
    previous_order = None
    last_at = {}
    for event in events:
        at = _event_shape(event, start, finish, lifecycle)
        order = (at, event['id'])
        if (event['id'] in seen_ids or previous_order is not None and order <= previous_order
                or event['pull_request'] in last_at and at <= last_at[event['pull_request']]):
            _fail('event_invalid')
        seen_ids.add(event['id'])
        previous_order = order
        last_at[event['pull_request']] = at
        item = lifecycle[event['pull_request']]
        if item['merged'] or item['awaiting_merge'] and event['kind'] != 'merged':
            _fail('lifecycle_inconsistent')
        if event['kind'] == 'enter':
            if item['active']:
                _fail('lifecycle_inconsistent')
            item['active'] = True
            item['entries'] += 1
        elif event['kind'] == 'leave':
            if not item['active']:
                _fail('lifecycle_inconsistent')
            item['active'] = False
            if event['merged']:
                if event['reason_code'] not in {
                        'PR_MERGED', 'PR_MERGED_INTERMEDIATE_RESULTS_SKIPPED'}:
                    _fail('lifecycle_inconsistent')
                item['awaiting_merge'] = True
            else:
                if event['reason_code'] in {
                        'PR_MERGED', 'PR_MERGED_INTERMEDIATE_RESULTS_SKIPPED'}:
                    _fail('lifecycle_inconsistent')
                item['dequeues'].append(event['reason_code'])
        else:
            if not item['awaiting_merge']:
                _fail('lifecycle_inconsistent')
            item['awaiting_merge'] = False
            item['merged'] = True

    merged_aliases = []
    checked = []
    for alias, number in sorted(numbers.items(), key=lambda pair: pair[1]):
        actual = outcome['prs'].get(alias)
        item = lifecycle[number]
        if item['awaiting_merge'] or not isinstance(actual, dict):
            _fail('lifecycle_inconsistent')
        enter_count = actual.get('enter_count')
        codes = actual.get('dequeue_codes')
        end = actual.get('end')
        if (type(enter_count) is not int or enter_count < 0
                or type(actual.get('queued')) is not bool
                or actual['queued'] is not (enter_count > 0)
                or not isinstance(codes, list)
                or any(not isinstance(code, str) or code not in driver.REFERENCE_DEQUEUE_CODES
                       for code in codes)
                or end not in {'open', 'closed', 'merged'}
                or enter_count != item['entries'] or codes != item['dequeues']
                or (end == 'merged') is not item['merged']
                or end == 'closed' and item['active']):
            _fail('outcome_mismatch')
        if end == 'merged':
            if actual.get('merged_by_mergify') is not True:
                _fail('outcome_mismatch')
            merged_aliases.append(alias)
        checked.append({'number': number, 'enter_count': item['entries'],
                        'non_merge_dequeue_codes': item['dequeues'],
                        'terminal': 'merged' if item['merged'] else 'none'})
    landed = outcome.get('landed')
    if (not isinstance(landed, list) or any(not isinstance(alias, str) for alias in landed)
            or len(landed) != len(set(landed))
            or set(landed) != set(merged_aliases)):
        _fail('outcome_mismatch')
    return {'scenario_run_id': run_id, 'pull_requests': checked}


def _empty(reason=None):
    return {'schema': 1, 'kind': 'reference_history_check', 'repository': REPOSITORY,
            'source': {'workflow_run_id': None, 'workflow_run_attempt': None,
                       'workflow_commit': None, 'workflow_repository': None},
            'complete': False, 'incomplete_reason': reason, 'runs': []}


def check(recordings, environ=None):
    environ = os.environ if environ is None else environ
    result = _empty()
    try:
        result['source'] = _source(environ)
        if not isinstance(recordings, Path) or not recordings.is_dir():
            _fail('recording_invalid')
        directories = sorted(path for path in recordings.iterdir() if path.is_dir())
        if not directories or len(directories) > RUNS_MAX:
            _fail('bound_exceeded' if directories else 'recording_invalid')
        runs = []
        seen_run_ids = set()
        seen_prs = set()
        for directory in directories:
            try:
                state = json.loads((directory / 'run.json').read_text())
                outcome = json.loads((directory / 'outcome.json').read_text())
                activity = json.loads((directory / 'reference-activity.json').read_text())
            except (OSError, ValueError, TypeError, RecursionError):
                _fail('recording_invalid')
            checked = verify_run(state, outcome, activity, environ)
            numbers = {row['number'] for row in checked['pull_requests']}
            if checked['scenario_run_id'] in seen_run_ids or numbers & seen_prs:
                _fail('recording_invalid')
            seen_run_ids.add(checked['scenario_run_id'])
            seen_prs.update(numbers)
            runs.append(checked)
        result.update({'complete': True, 'runs': runs})
        return result
    except (CheckError, OSError, ValueError, TypeError, KeyError, OverflowError,
            RecursionError) as error:
        reason = (error.args[0] if isinstance(error, CheckError) and error.args
                  and error.args[0] in INCOMPLETE_REASONS else 'recording_invalid')
        result.update({'complete': False, 'incomplete_reason': reason, 'runs': []})
        return result


def main():
    if len(sys.argv) != 3:
        return 64
    destination = Path(sys.argv[2])
    result = check(Path(sys.argv[1]))
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.with_suffix(destination.suffix + '.tmp')
        temporary.write_text(json.dumps(result, indent=2, ensure_ascii=False) + '\n')
        os.replace(temporary, destination)
    except (OSError, TypeError, ValueError):
        return 1
    return 0 if result['complete'] else 1


if __name__ == '__main__':
    sys.exit(main())
