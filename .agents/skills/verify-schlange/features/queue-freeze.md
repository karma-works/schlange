---
id: queue-freeze
title: "Scheduled freeze"
priority: P2
arztagent: false
status: missing
inventory:
  - "api:*/scheduled_freeze*"
  - "cli:mergify freeze *"
problems:
  - "MQ-46"
source: "https://docs.mergify.com/merge-protections/freeze"
---

# Scheduled freeze

For a time window, pull requests matching given conditions are not merged.

## Sub-features

- `create`
- `update`
- `delete`
- `list`

## How to get to it (user POV)

- CLI or API.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Time zones of the windows.
