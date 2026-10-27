---
id: events
title: "Event log"
priority: P2
arztagent: false
status: missing
inventory:
  - "api:GET /repos/{owner}/{repository}/logs"
  - "api:GET /repos/{owner}/{repository}/logs/aggregate"
  - "cli:mergify events"
problems:
  - "MQ-43"
  - "W-03"
source: "https://docs.mergify.com/merge-queue/monitoring"
---

# Event log

Every decision of the queue as a log that can be queried.

## Sub-features

- `list`
- `aggregate`

## How to get to it (user POV)

- `schlange events` (planned).

## Driving it

- `schlange sim run` already prints the simulation's event log.

## Gotchas

- Keep the format stable.
