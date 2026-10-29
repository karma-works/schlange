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
    prs = reference.get('prs')
    landed = reference.get('landed')
    if not expected or not isinstance(prs, dict) or set(prs) != expected:
        return False
    if not isinstance(landed, list) or not all(isinstance(k, str) for k in landed):
        return False
    deviations = reference.get('known_deviations', [])
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
    for pr in prs.values():
        if (not isinstance(pr, dict) or pr.get('end') not in ('open', 'closed', 'merged')
                or type(pr.get('queued')) is not bool or type(pr.get('enter_count')) is not int
                or pr['enter_count'] < 0 or not isinstance(pr.get('dequeue_codes'), list)
                or not all(isinstance(code, str) for code in pr['dequeue_codes'])):
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
