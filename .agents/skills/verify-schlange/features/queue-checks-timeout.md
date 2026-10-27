---
id: queue-checks-timeout
title: "Check timeout and retries (defaults)"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.checks_timeout"
  - "default:queue_rules.checks_timeout"
  - "config:queue_rules.max_checks_retries"
  - "default:queue_rules.max_checks_retries"
problems:
  - "MQ-37"
  - "MQ-38"
  - "MQ-39"
  - "MQ-42"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Check timeout and retries (defaults)

When a required check does not report in time, the run ends with a timeout. Arztagent sets nothing
and inherits the default `auto`: a value derived from the history of run times. Red checks are not
retried by default.

## Sub-features

- `auto`: value from recent run times, with a lower bound; no timeout before enough history.
- `fixed`: duration as text.
- `retries`: default 0.
- `cancelled`: cancelled runs are reported with their own reason.

## How to get to it (user POV)

- The status comment lists the missing checks.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- Virtual clock: a check never starts → timeout with the list of missing checks.

## Gotchas

- Rebuild `auto` only from its public description; compare the values in shadow mode (MQ-38).
