#!/usr/bin/env python3
"""Check selected shadow facts against the driver's independent Mergify recording.

Only synthetic sandbox recordings are accepted. A CLI exit of zero alone is insufficient:
an empty, pending or inconclusive comparison is not evidence of parity.
"""
import json
import sys
from pathlib import Path


def verify(report, recordings):
    if report.get('repository') != 'schlange-sandbox/queue-lab':
        raise ValueError('shadow report is not for the reference sandbox')
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
    for directory in sorted(recordings.iterdir()):
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
            if row.get('verdict') != 'agree' or row.get('differences') != []:
                verdict = row.get('verdict')
                if verdict not in ('agree', 'disagree', 'pending', 'inconclusive'):
                    verdict = 'invalid'
                fields = ('queued', 'landed', 'order', 'batch', 'dequeue_reason')
                different = [field for field in fields if isinstance(row.get(field), dict)
                             and row[field].get('agree') is False]
                before = row.get('before_watching')
                before = str(before).lower() if type(before) is bool else 'unknown'
                raise ValueError(f'PR #{number} has no complete agreement '
                                 f'(verdict={verdict}, before_watching={before}, '
                                 f'differing_fields={",".join(different) or "none"})')
            actual = outcome['prs'][key]
            for field in ('queued', 'landed', 'order', 'batch', 'dequeue_reason'):
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
            if type(entries) is not int or entries != int(actual['queued']) or len(codes) > entries:
                raise ValueError(f'PR #{number}: queue entry/dequeue history is not comparable')
            if (row.get('queued', {}).get('mergify') is not actual['queued']
                    or row.get('landed', {}).get('mergify') is not (actual['end'] == 'merged')):
                raise ValueError(f'PR #{number}: observer differs from the independent queue events')
            expected_reason = codes[-1] if codes else None
            if row.get('dequeue_reason', {}).get('mergify') != expected_reason:
                raise ValueError(f'PR #{number}: observer has a different dequeue reason')
            if actual['end'] == 'merged':
                if actual.get('merged_by_mergify') is not True:
                    raise ValueError(f'PR #{number}: independent recording does not prove a Mergify merge')
                if codes:
                    raise ValueError(f'PR #{number}: landed PR has an unverified dequeue history')
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


def main():
    try:
        report = json.loads(Path(sys.argv[1]).read_text())
        summary = verify(report, Path(sys.argv[2]))
    except (OSError, ValueError, KeyError, IndexError, TypeError, AttributeError) as error:
        print(f'Shadow evidence incomplete: {error}', file=sys.stderr)
        return 1
    print(json.dumps(summary, indent=2))
    return 0


if __name__ == '__main__':
    sys.exit(main())
