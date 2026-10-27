---
id: conditions-queue
title: "Queue attributes"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:queue-position"
  - "attr:queue-name"
  - "attr:queued-at"
  - "attr:queue-merge-started-at"
problems:
  - "MQ-43"
source: "https://docs.mergify.com/configuration/conditions"
---

# Queue attributes

Position, queue name and timestamps of a queued pull request as conditions.

## Sub-features

- `position`
- `name`
- `timestamps`

## How to get to it (user POV)

- Conditions.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- The value changes with every queue step.
