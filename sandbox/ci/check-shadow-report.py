#!/usr/bin/env python3
"""Check selected shadow facts against the driver's independent Mergify recording.

Only synthetic sandbox recordings are accepted. A CLI exit of zero alone is insufficient:
an empty, pending or inconclusive comparison is not evidence of parity.
"""
import json
import os
import re
import sys
from datetime import datetime
from pathlib import Path

DRAFT_CANDIDATES_MAX = 256
DRAFT_REFERENCE_NUMBERS_MAX = 64
DRAFT_BODY_LINES_MAX = 512
DRAFT_REFERENCE_LINES_MAX = 64


def _bounded_numbers(value, maximum):
    return (isinstance(value, list) and len(value) <= maximum
            and all(type(number) is int and number > 0 for number in value)
            and value == sorted(set(value)))


def _boolean(record, field):
    value = record.get(field, False)
    if type(value) is not bool:
        raise ValueError('invalid structural draft observer diagnostics')
    return value


def _validate_draft_diagnostics(diagnostics):
    if not isinstance(diagnostics, dict):
        raise ValueError('invalid structural draft observer diagnostics')
    fields = {'matching_queue_prefix', 'matching_configured_author',
              'structured_membership_parsed', 'structures', 'truncated'}
    if not set(diagnostics) <= fields:
        raise ValueError('invalid structural draft observer diagnostics')
    candidate_fields = ('matching_queue_prefix', 'matching_configured_author',
                        'structured_membership_parsed')
    for field in candidate_fields:
        if not _bounded_numbers(diagnostics.get(field, []), DRAFT_CANDIDATES_MAX):
            raise ValueError('invalid structural draft observer diagnostics')
    observed_candidates = set(diagnostics.get('matching_queue_prefix', [])) | set(
        diagnostics.get('matching_configured_author', []))
    parsed_candidates = set(diagnostics.get('structured_membership_parsed', []))
    if not parsed_candidates <= observed_candidates:
        raise ValueError('invalid structural draft observer diagnostics')
    structures = diagnostics.get('structures', [])
    if not isinstance(structures, list) or len(structures) > DRAFT_CANDIDATES_MAX:
        raise ValueError('invalid structural draft observer diagnostics')
    structure_numbers = []
    any_nested_truncated = False
    structure_fields = {
        'number', 'title_hash_references', 'title_hash_references_truncated',
        'title_pull_path_references', 'title_pull_path_references_truncated',
        'body_line_count', 'body_lines_truncated', 'body_reference_lines',
        'body_reference_lines_truncated', 'truncated'}
    line_fields = {
        'line', 'hash_references', 'hash_references_truncated',
        'pull_path_references', 'pull_path_references_truncated',
        'starts_with_list_marker', 'starts_with_hash_reference',
        'starts_with_markdown_link', 'contains_html_comment_delimiter'}
    for structure in structures:
        if (not isinstance(structure, dict) or not set(structure) <= structure_fields
                or not {'number', 'title_hash_references', 'title_pull_path_references',
                        'body_line_count', 'body_reference_lines'} <= set(structure)):
            raise ValueError('invalid structural draft observer diagnostics')
        number = structure['number']
        if type(number) is not int or number <= 0 or number not in observed_candidates:
            raise ValueError('invalid structural draft observer diagnostics')
        structure_numbers.append(number)
        for field in ('title_hash_references', 'title_pull_path_references'):
            if not _bounded_numbers(structure[field], DRAFT_REFERENCE_NUMBERS_MAX):
                raise ValueError('invalid structural draft observer diagnostics')
        line_count = structure['body_line_count']
        if type(line_count) is not int or not 0 <= line_count <= DRAFT_BODY_LINES_MAX:
            raise ValueError('invalid structural draft observer diagnostics')
        reference_lines = structure['body_reference_lines']
        if (not isinstance(reference_lines, list)
                or len(reference_lines) > DRAFT_REFERENCE_LINES_MAX):
            raise ValueError('invalid structural draft observer diagnostics')
        line_indexes = []
        line_truncated = False
        for line in reference_lines:
            required = {'line', 'hash_references', 'pull_path_references',
                        'starts_with_list_marker', 'starts_with_hash_reference',
                        'starts_with_markdown_link', 'contains_html_comment_delimiter'}
            if (not isinstance(line, dict) or not set(line) <= line_fields
                    or not required <= set(line)):
                raise ValueError('invalid structural draft observer diagnostics')
            index = line['line']
            if type(index) is not int or not 0 <= index < line_count:
                raise ValueError('invalid structural draft observer diagnostics')
            line_indexes.append(index)
            for field in ('hash_references', 'pull_path_references'):
                if not _bounded_numbers(line[field], DRAFT_REFERENCE_NUMBERS_MAX):
                    raise ValueError('invalid structural draft observer diagnostics')
            for field in ('starts_with_list_marker', 'starts_with_hash_reference',
                          'starts_with_markdown_link', 'contains_html_comment_delimiter'):
                _boolean(line, field)
            line_truncated |= (_boolean(line, 'hash_references_truncated')
                               or _boolean(line, 'pull_path_references_truncated'))
        if line_indexes != sorted(set(line_indexes)):
            raise ValueError('invalid structural draft observer diagnostics')
        nested_truncated = (
            _boolean(structure, 'title_hash_references_truncated')
            or _boolean(structure, 'title_pull_path_references_truncated')
            or _boolean(structure, 'body_lines_truncated')
            or _boolean(structure, 'body_reference_lines_truncated')
            or line_truncated)
        if _boolean(structure, 'truncated') != nested_truncated:
            raise ValueError('invalid structural draft observer diagnostics')
        any_nested_truncated |= nested_truncated
    if structure_numbers != sorted(set(structure_numbers)):
        raise ValueError('invalid structural draft observer diagnostics')
    if set(structure_numbers) != observed_candidates - parsed_candidates:
        raise ValueError('invalid structural draft observer diagnostics')
    truncated = _boolean(diagnostics, 'truncated')
    if any_nested_truncated and not truncated:
        raise ValueError('invalid structural draft observer diagnostics')
    if truncated:
        raise ValueError('structural draft observer diagnostics are truncated')


def verify(report, recordings, directories=None):
    if (type(report.get('history_incomplete', False)) is not bool
            or report.get('history_incomplete', False)):
        raise ValueError('repository shadow occurrence history is incomplete')
    if report.get('repository') != 'schlange-sandbox/queue-lab':
        raise ValueError('shadow report is not for the reference sandbox')
    diagnostics = report.get('draft_observer', {})
    _validate_draft_diagnostics(diagnostics)
    diagnostic_summary = (
        f"draft observer prefix={len(diagnostics.get('matching_queue_prefix', []))}, "
        f"author={len(diagnostics.get('matching_configured_author', []))}, "
        f"parsed={len(diagnostics.get('structured_membership_parsed', []))}")
    rows = report.get('pull_requests')
    if not isinstance(rows, list):
        raise ValueError('missing shadow comparisons')
    by_number = {}
    for row in rows:
        number = row.get('number')
        if type(number) is not int or number in by_number:
            raise ValueError('invalid or duplicate shadow PR number')
        by_number[number] = row
    selected = set()
    groups = []
    for directory in sorted(recordings.iterdir() if directories is None else directories):
        if not directory.is_dir():
            continue
        state = json.loads((directory / 'run.json').read_text())
        outcome = json.loads((directory / 'outcome.json').read_text())
        if ((state.get('org'), state.get('repo')) != ('schlange-sandbox', 'queue-lab')
                or outcome.get('repo') != 'queue-lab'):
            raise ValueError('recording is not from the reference sandbox')
        if state.get('timed_out') is not False or not isinstance(state.get('finished'), str) or not state['finished']:
            raise ValueError('recording is unfinished or lacks a successful timeout observation')
        prs = state.get('prs')
        if not isinstance(prs, dict) or not prs or set(prs) != set(outcome.get('prs', {})):
            raise ValueError('recording has incomplete PR evidence')
        numbers = {key: pr['number'] for key, pr in prs.items()}
        if any(type(n) is not int or n <= 0 or n in selected for n in numbers.values()):
            raise ValueError('recording has invalid or repeated PR numbers')
        if len(set(numbers.values())) != len(numbers):
            raise ValueError('recording reuses a PR number')
        ranks = {}
        for key, number in numbers.items():
            selected.add(number)
            row = by_number.get(number)
            if row is None:
                raise ValueError(f'PR #{number} is missing from the shadow report')
            if type(row.get('before_watching', False)) is not bool or row.get('before_watching', False):
                raise ValueError(f'PR #{number} was not confirmed while shadow mode was watching')
            if (type(row.get('history_incomplete', False)) is not bool
                    or row.get('history_incomplete', False)):
                raise ValueError(f'PR #{number} has incomplete shadow occurrence history')
            if row.get('verdict') != 'agree' or row.get('differences') != []:
                verdict = row.get('verdict')
                if verdict not in ('agree', 'disagree', 'pending', 'inconclusive'):
                    verdict = 'invalid'
                fields = ('queued', 'queue_entries', 'landed', 'order', 'batch',
                          'dequeue_reason', 'dequeue_reasons')
                different = [field for field in fields if isinstance(row.get(field), dict)
                             and row[field].get('agree') is False]
                before = row.get('before_watching')
                before = str(before).lower() if type(before) is bool else 'unknown'
                raise ValueError(f'PR #{number} has no complete agreement '
                                 f'(verdict={verdict}, before_watching={before}, '
                                 f'differing_fields={",".join(different) or "none"})')
            actual = outcome['prs'][key]
            for field in ('queued', 'queue_entries', 'landed', 'order', 'batch',
                          'dequeue_reason', 'dequeue_reasons'):
                comparison = row.get(field)
                if comparison is None:
                    continue
                if (comparison.get('agree') is not True
                        or comparison.get('schlange') != comparison.get('mergify')):
                    raise ValueError(f'PR #{number} has inconsistent {field} agreement')
            if type(actual.get('queued')) is not bool or actual.get('end') not in ('open', 'closed', 'merged'):
                raise ValueError('independent recording has no complete queue/landing evidence')
            codes = actual.get('dequeue_codes')
            if not isinstance(codes, list) or not all(isinstance(code, str) for code in codes):
                raise ValueError('independent recording has no complete dequeue evidence')
            entries = actual.get('enter_count')
            if type(entries) is not int or entries < int(actual['queued']) or len(codes) > entries:
                raise ValueError(f'PR #{number}: queue entry/dequeue history is not comparable')
            if row.get('queue_entries', {}).get('mergify') != entries:
                raise ValueError(f'PR #{number}: observer has a different queue-entry history; '
                                 f'{diagnostic_summary}')
            if row.get('dequeue_reasons', {}).get('mergify') != codes:
                raise ValueError(f'PR #{number}: observer has a different dequeue history')
            if (row.get('queued', {}).get('mergify') is not actual['queued']
                    or row.get('landed', {}).get('mergify') is not (actual['end'] == 'merged')):
                raise ValueError(f'PR #{number}: observer differs from the independent queue events')
            expected_reason = codes[-1] if codes else None
            if row.get('dequeue_reason', {}).get('mergify') != expected_reason:
                raise ValueError(f'PR #{number}: observer has a different dequeue reason')
            if actual['end'] == 'merged':
                if actual.get('merged_by_mergify') is not True:
                    raise ValueError(f'PR #{number}: independent recording does not prove a Mergify merge')
                rank = row.get('order', {}).get('mergify')
                if type(rank) is not int or rank <= 0:
                    raise ValueError(f'PR #{number}: missing landing rank')
                ranks[key] = rank
                reported_batch = row.get('batch', {}).get('mergify')
                if (not isinstance(reported_batch, list) or not reported_batch
                        or any(type(n) is not int or n <= 0 for n in reported_batch)
                        or len(set(reported_batch)) != len(reported_batch)
                        or number not in reported_batch):
                    raise ValueError(f'PR #{number}: invalid landing batch')
                successful_batches = [sorted(numbers[k] for k in batch['prs'])
                                      for batch in outcome.get('batches', [])
                                      if batch.get('ci_ok') == 'success']
                if sorted(reported_batch) not in successful_batches:
                    raise ValueError(f'PR #{number}: reported batch has no independent green evidence')
        if (len(set(ranks.values())) != len(ranks)
                or sorted(ranks, key=ranks.get) != outcome.get('landed')):
            raise ValueError('observer landing order differs from the independent recording')
        groups.append({'run_id': state['run_id'], 'compared_prs': len(numbers)})
    if not selected:
        raise ValueError('no scenario PRs were compared')
    return {'repository': report['repository'], 'compared_prs': len(selected), 'recordings': groups}


def _actions_source(environ):
    values = (environ.get('GITHUB_RUN_ID'), environ.get('GITHUB_RUN_ATTEMPT'),
              environ.get('GITHUB_SHA'), environ.get('GITHUB_REPOSITORY'))
    run_id, attempt, commit, repository = values
    if (not all(isinstance(value, str) for value in values)
            or not run_id.isdigit() or int(run_id) <= 0
            or not attempt.isdigit() or int(attempt) <= 0
            or not re.fullmatch(r'[0-9a-f]{40}', commit)
            or not re.fullmatch(r'[A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+', repository)):
        raise ValueError('workflow provenance is missing')
    return {'workflow_run_id': int(run_id), 'workflow_run_attempt': int(attempt),
            'workflow_commit': commit, 'workflow_repository': repository}


def _timestamp(value):
    if not isinstance(value, str) or len(value) > 64:
        raise ValueError('invalid snapshot timestamp')
    try:
        parsed = datetime.fromisoformat(value.replace('Z', '+00:00'))
    except ValueError:
        raise ValueError('invalid snapshot timestamp') from None
    if parsed.tzinfo is None:
        raise ValueError('invalid snapshot timestamp')
    return parsed


def verify_snapshots(recordings, environ=None):
    """Verify each run only against the shadow rows captured before its cleanup."""
    environ = os.environ if environ is None else environ
    expected_source = _actions_source(environ)
    groups, seen_runs, seen_prs = [], set(), set()
    directories = sorted(directory for directory in recordings.iterdir() if directory.is_dir())
    if not directories:
        raise ValueError('no scenario recordings')
    for directory in directories:
        state = json.loads((directory / 'run.json').read_text())
        snapshot = json.loads((directory / 'shadow-report.json').read_text())
        if (state.get('org'), state.get('repo')) != ('schlange-sandbox', 'queue-lab'):
            raise ValueError('recording is not from the reference sandbox')
        run_id = state.get('run_id')
        if not isinstance(run_id, str) or not run_id or run_id in seen_runs:
            raise ValueError('invalid or repeated scenario run id')
        seen_runs.add(run_id)
        prs = state.get('prs')
        if not isinstance(prs, dict) or not prs:
            raise ValueError('recording has incomplete PR evidence')
        selected = sorted(pr.get('number') for pr in prs.values() if isinstance(pr, dict))
        if (len(selected) != len(prs) or any(type(number) is not int or number <= 0
                                            or number in seen_prs for number in selected)
                or selected != sorted(set(selected))):
            raise ValueError('recording has invalid or repeated PR numbers')
        seen_prs.update(selected)
        expected_fields = {
            'schema', 'repository', 'scenario_run_id', 'scenario_finished_at', 'captured_at',
            'selected_pull_requests', 'source', 'complete', 'incomplete_reason', 'report'}
        if (not isinstance(snapshot, dict) or set(snapshot) != expected_fields
                or snapshot.get('schema') != 1
                or snapshot.get('repository') != 'schlange-sandbox/queue-lab'
                or snapshot.get('scenario_run_id') != run_id
                or snapshot.get('scenario_finished_at') != state.get('finished')
                or snapshot.get('selected_pull_requests') != selected
                or snapshot.get('source') != expected_source
                or snapshot.get('complete') is not True
                or snapshot.get('incomplete_reason') is not None):
            raise ValueError('pre-cleanup shadow snapshot is incomplete or unbound')
        if _timestamp(snapshot['captured_at']) < _timestamp(state.get('finished')):
            raise ValueError('pre-cleanup shadow snapshot predates scenario completion')
        report = snapshot.get('report')
        if not isinstance(report, dict) or set(report) != {
                'repository', 'history_incomplete', 'draft_observer', 'pull_requests'}:
            raise ValueError('pre-cleanup shadow report is malformed')
        report_numbers = [row.get('number') for row in report['pull_requests']
                          if isinstance(row, dict)] if isinstance(report.get('pull_requests'), list) else []
        if report_numbers != selected:
            raise ValueError('pre-cleanup shadow report does not contain the selected PRs')
        summary = verify(report, recordings, directories=[directory])
        groups.extend(summary['recordings'])
    return {'repository': 'schlange-sandbox/queue-lab',
            'compared_prs': len(seen_prs), 'recordings': groups}


def main():
    try:
        if len(sys.argv) == 3 and sys.argv[1] == '--snapshots':
            summary = verify_snapshots(Path(sys.argv[2]))
        else:
            report = json.loads(Path(sys.argv[1]).read_text())
            summary = verify(report, Path(sys.argv[2]))
    except (OSError, ValueError, KeyError, IndexError, TypeError, AttributeError) as error:
        print(json.dumps({'complete': False}))
        print(f'Shadow evidence incomplete: {error}', file=sys.stderr)
        return 1
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
