#!/usr/bin/env python3
"""Export prose-free Schlange dequeue causes for selected queue-lab scenarios.

This diagnostic is not an input to the shadow comparison. It reads the local shadow occurrence
table after the comparison attempt and never publishes the stored reason text.
"""
import json
import os
import re
import sqlite3
import sys
from datetime import datetime
from pathlib import Path

EVENTS_MAX = 1024
SCENARIOS_MAX = 32
PRS_MAX = 128
DATA_BYTES_MAX = 16_384
CONDITION_INDEX_MAX = 4096
INCOMPLETE_REASONS = frozenset({
    'source_missing', 'binding_invalid', 'storage', 'schema_invalid',
    'bound_exceeded', 'row_invalid', 'window_invalid',
})
REPOSITORY = 'schlange-sandbox/queue-lab'


class ExportError(Exception):
    pass


def _time(value):
    if not isinstance(value, str) or not re.fullmatch(
            r'\d{4}-\d\d-\d\dT\d\d:\d\d:\d\d(?:\.\d{1,6})?(?:Z|[+-]\d\d:\d\d)', value):
        raise ExportError('binding_invalid')
    try:
        parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
    except ValueError:
        raise ExportError('binding_invalid') from None
    if parsed.tzinfo is None:
        raise ExportError('binding_invalid')
    return int(parsed.timestamp())


def _source(environ):
    values = tuple(environ.get(name) for name in (
        'GITHUB_RUN_ID', 'GITHUB_RUN_ATTEMPT', 'GITHUB_SHA', 'GITHUB_REPOSITORY'))
    run, attempt, commit, repository = values
    if (environ.get('GITHUB_ACTIONS') != 'true'
            or not all(isinstance(value, str) for value in values)
            or not run.isdigit() or int(run) <= 0 or len(run) > 32
            or not attempt.isdigit() or int(attempt) <= 0 or len(attempt) > 16
            or not re.fullmatch(r'[0-9a-f]{40}', commit or '')
            or not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository or '')):
        raise ExportError('source_missing')
    return {'workflow_run_id': int(run), 'workflow_run_attempt': int(attempt),
            'workflow_commit': commit, 'workflow_repository': repository}


def _bindings(recordings):
    if not isinstance(recordings, Path) or not recordings.is_dir():
        raise ExportError('binding_invalid')
    directories = sorted(path for path in recordings.iterdir() if path.is_dir())
    if not directories or len(directories) > SCENARIOS_MAX:
        raise ExportError('bound_exceeded' if directories else 'binding_invalid')
    scenarios = []
    by_pr = {}
    for directory in directories:
        try:
            state = json.loads((directory / 'run.json').read_text())
            scenario = json.loads((directory / 'scenario.json').read_text())
        except (OSError, ValueError, TypeError):
            raise ExportError('binding_invalid') from None
        if not isinstance(state, dict) or not isinstance(scenario, dict):
            raise ExportError('binding_invalid')
        run_id, scenario_id = state.get('run_id'), scenario.get('id')
        if (state.get('org') != 'schlange-sandbox' or state.get('repo') != 'queue-lab'
                or state.get('timed_out') is not False
                or not isinstance(run_id, str) or len(run_id) > 104
                or not re.fullmatch(r's\d{2}-queue-lab-[a-z0-9-]+', run_id)
                or not isinstance(scenario_id, str) or not re.fullmatch(r'S\d{2}', scenario_id)
                or not run_id.startswith(scenario_id.lower() + '-')):
            raise ExportError('binding_invalid')
        started_at, finished_at = state.get('started'), state.get('finished')
        started, finished = _time(started_at), _time(finished_at)
        if finished < started:
            raise ExportError('binding_invalid')
        raw_prs = state.get('prs')
        if not isinstance(raw_prs, dict) or not raw_prs or len(raw_prs) > PRS_MAX:
            raise ExportError('binding_invalid')
        prs = [pr.get('number') for pr in raw_prs.values() if isinstance(pr, dict)]
        if (len(prs) != len(raw_prs) or any(type(number) is not int or number <= 0 for number in prs)
                or len(prs) != len(set(prs))):
            raise ExportError('binding_invalid')
        prs.sort()
        for number in prs:
            if number in by_pr:
                raise ExportError('binding_invalid')
            by_pr[number] = (started, finished)
        scenarios.append({'run_id': run_id, 'started_at': started_at,
                          'finished_at': finished_at, 'pull_requests': prs})
    if len(by_pr) > PRS_MAX:
        raise ExportError('bound_exceeded')
    return scenarios, by_pr


def open_database(path):
    if not isinstance(path, Path) or not path.is_file():
        raise ExportError('storage')
    connection = sqlite3.connect(path.resolve().as_uri() + '?mode=ro', uri=True)
    connection.execute('PRAGMA query_only = ON')
    return connection


def _cause(text):
    match = re.fullmatch(r'queue_conditions no longer hold \(condition (\d+) is false\)', text)
    if match:
        index = int(match.group(1))
        if not 1 <= index <= CONDITION_INDEX_MAX:
            raise ExportError('row_invalid')
        return {'kind': 'queue_condition', 'condition_index': index}
    if text == 'the pull request does not merge onto the base branch':
        return {'kind': 'base_conflict'}
    if text == 'the pull request was closed':
        return {'kind': 'closed'}
    if re.fullmatch(r'a merge protection holds it \([^\r\n()]+\): [^\r\n]+', text):
        return {'kind': 'hold'}
    return {'kind': 'other'}


def _rows(database, by_pr):
    connection = open_database(database)
    try:
        columns = {row[1] for row in connection.execute('PRAGMA table_info(shadow_events)')}
        required = {'id', 'repo', 'side', 'pr', 'kind', 'at', 'data'}
        if not required <= columns:
            raise ExportError('schema_invalid')
        windows = ' OR '.join('(pr = ? AND at >= ? AND at <= ?)' for _ in by_pr)
        query = (f'SELECT id, pr, at, typeof(data), length(CAST(data AS BLOB)), '
                 f'CASE WHEN length(CAST(data AS BLOB)) <= ? THEN data ELSE NULL END '
                 f'FROM shadow_events WHERE repo = ? AND side = ? AND kind = ? '
                 f'AND ({windows}) ORDER BY id LIMIT ?')
        window_values = [value for pr in sorted(by_pr) for value in (pr, *by_pr[pr])]
        values = [DATA_BYTES_MAX, REPOSITORY, 'schlange', 'dequeued',
                  *window_values, EVENTS_MAX + 1]
        rows = connection.execute(query, values).fetchall()
    finally:
        connection.close()
    if len(rows) > EVENTS_MAX:
        raise ExportError('bound_exceeded')
    events = []
    previous_id = 0
    for row_id, pr, at, data_type, data_bytes, raw_data in rows:
        if (type(row_id) is not int or row_id <= previous_id or type(pr) is not int
                or pr not in by_pr or type(at) is not int or at < 0
                or data_type != 'text' or type(data_bytes) is not int
                or not 0 <= data_bytes <= DATA_BYTES_MAX or not isinstance(raw_data, str)):
            raise ExportError('row_invalid')
        previous_id = row_id
        try:
            data = json.loads(raw_data)
        except (ValueError, TypeError, RecursionError):
            raise ExportError('row_invalid') from None
        if (not isinstance(data, dict) or 'code' not in data
                or (data.get('code') is not None and not isinstance(data.get('code'), str))
                or not isinstance(data.get('text'), str)):
            raise ExportError('row_invalid')
        if data['code'] == 'PR_DEQUEUED':
            events.append({'id': row_id, 'pull_request': pr, 'at': at,
                           'cause': _cause(data['text'])})
    return events


def _incomplete_result(reason=None):
    return {'schema': 1, 'repository': REPOSITORY,
            'source': {'workflow_run_id': None, 'workflow_run_attempt': None,
                       'workflow_commit': None, 'workflow_repository': None},
            'complete': False, 'incomplete_reason': reason, 'scenarios': [], 'events': []}


def capture(database, recordings, environ=None):
    environ = os.environ if environ is None else environ
    result = _incomplete_result()
    try:
        result['source'] = _source(environ)
        result['scenarios'], by_pr = _bindings(recordings)
        result['events'] = _rows(database, by_pr)
        result['complete'] = True
        return result
    except (ExportError, OSError, sqlite3.Error, OverflowError, TypeError, ValueError,
            KeyError, UnicodeError, RecursionError) as error:
        reason = (error.args[0] if isinstance(error, ExportError) and error.args
                  and error.args[0] in INCOMPLETE_REASONS else 'storage')
        result.update({'complete': False, 'incomplete_reason': reason, 'events': []})
        return result


def main():
    if len(sys.argv) != 4:
        return 64
    destination = Path(sys.argv[3])
    try:
        result = capture(Path(sys.argv[1]), Path(sys.argv[2]))
    except Exception:
        result = _incomplete_result('storage')
    try:
        destination.parent.mkdir(parents=True, exist_ok=True)
        temporary = destination.with_suffix(destination.suffix + '.tmp')
        temporary.write_text(json.dumps(result, indent=2, ensure_ascii=False) + '\n')
        os.replace(temporary, destination)
    except Exception:
        # The optional diagnostic never changes the established comparison gate.
        return 0
    return 0


if __name__ == '__main__':
    sys.exit(main())
