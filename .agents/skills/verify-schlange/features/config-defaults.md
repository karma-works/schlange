---
id: config-defaults
title: "Shared defaults (`defaults`)"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:defaults"
  - "config:defaults.actions"
  - "config:defaults.queue_rule"
  - "config:defaults.queue_rule.*"
  - "default:defaults.queue_rule.*"
problems:
  - "MQ-19"
source: "https://docs.mergify.com/configuration/file-format"
---

# Shared defaults (`defaults`)

Values that apply to every queue rule or action unless that rule or action says otherwise.

## Sub-features

- `queue-rule`
- `actions`

## How to get to it (user POV)

- Section `defaults`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Order: rule before defaults before schema default.
