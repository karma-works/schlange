---
id: queue-priority
title: "Priorities (`priority_rules`)"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:priority_rules"
  - "config:priority_rules.*"
  - "default:priority_rules.*"
  - "enum:priority:*"
problems:
  - "MQ-24"
source: "https://docs.mergify.com/merge-queue/priority"
---

# Priorities (`priority_rules`)

Rules give pull requests a priority; higher ones move ahead in the queue and may optionally
interrupt running checks.

## Sub-features

- `levels`
- `number`
- `interrupt`

## How to get to it (user POV)

- Rule in the configuration.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Cancel displaced runs and log the reason (MQ-24).
