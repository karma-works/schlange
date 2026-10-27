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

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; the `event log` section lists every event and decision with its virtual time.

## Gotchas

- Keep the format stable.
