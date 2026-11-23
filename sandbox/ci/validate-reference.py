#!/usr/bin/env python3
"""Reject incomplete reference evidence before a compatibility scenario changes GitHub."""
import json
import sys


def valid(reference, scenario):
    if not isinstance(reference, dict) or reference.get('reference_quality', 'verified') != 'verified':
        return False
    if not isinstance(scenario, dict) or not isinstance(scenario.get('steps'), list):
        return False
    expected = {step['open'] for step in scenario['steps']
                if isinstance(step, dict) and isinstance(step.get('open'), str)}
    def valid_outcome(outcome, complete_batches=False):
        prs = outcome.get('prs')
        landed = outcome.get('landed')
        if not expected or not isinstance(prs, dict) or set(prs) != expected:
            return False
        if (not isinstance(landed, list)
                or not all(isinstance(k, str) and k in expected for k in landed)):
            return False
        for pr in prs.values():
            if (not isinstance(pr, dict) or pr.get('end') not in ('open', 'closed', 'merged')
                    or type(pr.get('queued')) is not bool or type(pr.get('enter_count')) is not int
                    or pr['enter_count'] < 0 or not isinstance(pr.get('dequeue_codes'), list)
                    or not all(isinstance(code, str) for code in pr['dequeue_codes'])):
                return False
        if complete_batches or scenario.get('strict_batches', False):
            batches = outcome.get('batches')
            conclusions = {'success', 'failure', 'neutral', 'cancelled', 'skipped',
                           'timed_out', 'action_required', 'stale'}
            if not isinstance(batches, list) or not all(
                    isinstance(b, dict) and isinstance(b.get('prs'), list)
                    and all(isinstance(pr, str) and pr in expected for pr in b['prs'])
                    and isinstance(b.get('ci_ok'), str) and b['ci_ok'] in conclusions for b in batches):
                return False
            if (scenario.get('strict_batches', False) and 'expected_batches' in scenario
                    and [b['prs'] for b in batches] != scenario['expected_batches']):
                return False
        return True

    if not valid_outcome(reference):
        return False
    variants = reference.get('accepted_variants', [])
    if not isinstance(variants, list):
        return False
    names = set()
    for variant in variants:
        if (not isinstance(variant, dict) or not isinstance(variant.get('name'), str)
                or not variant['name'] or variant['name'] in names
                or not isinstance(variant.get('why'), str) or not variant['why']
                or not isinstance(variant.get('source_runs'), list) or not variant['source_runs']
                or not all(isinstance(run, str) and run.isascii() and run.isdigit()
                           and not run.startswith('0') for run in variant['source_runs'])
                or not valid_outcome(variant, complete_batches=True)):
            return False
        names.add(variant['name'])
    if variants and not valid_outcome(reference, complete_batches=True):
        return False
    deviations = reference.get('known_deviations', [])
    if variants and deviations:
        return False
    if not isinstance(deviations, list) or not all(
            isinstance(d, dict) and isinstance(d.get('pr'), str)
            and isinstance(d.get('field'), str) and 'observed' in d
            and isinstance(d.get('why'), str) for d in deviations):
        return False
    seen = set()
    for d in deviations:
        pr, field, value = d['pr'], d['field'], d['observed']
        if (pr, field) in seen:
            return False
        seen.add((pr, field))
        if pr == '*' and field == 'landed':
            if not isinstance(value, list) or not all(isinstance(k, str) for k in value):
                return False
        elif pr not in expected or not (
                (field == 'end' and value in ('open', 'closed', 'merged'))
                or (field == 'queued' and type(value) is bool)
                or (field == 'enter_count' and type(value) is int and value >= 0)
                or (field == 'dequeue_codes' and isinstance(value, list)
                    and all(isinstance(code, str) for code in value))):
            return False
    return True


try:
    with open(sys.argv[1]) as f:
        reference = json.load(f)
    with open(sys.argv[2]) as f:
        scenario = json.load(f)
    sys.exit(0 if valid(reference, scenario) else 1)
except (OSError, ValueError, IndexError, TypeError):
    sys.exit(1)
