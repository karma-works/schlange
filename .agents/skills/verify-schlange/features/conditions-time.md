---
id: conditions-time
title: "Time and schedule"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:current-datetime"
  - "attr:schedule"
problems:
  - "MQ-46"
source: "https://docs.mergify.com/configuration/data-types"
---

# Time and schedule

Conditions over the current time and recurring time windows.

## Sub-features

- `datetime`
- `schedule`

## How to get to it (user POV)

- Conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Planned.** Virtual clock in the simulation.

## Gotchas

- Time zones.
