---
id: action-copy
title: "Action `copy`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:copy"
  - "config:pull_request_rules.actions.copy"
  - "config:pull_request_rules.actions.copy.*"
  - "config:defaults.actions.copy"
  - "config:defaults.actions.copy.*"
  - "default:pull_request_rules.actions.copy.*"
  - "default:defaults.actions.copy.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/copy"
---

# Action `copy`

Create a copy of the pull request for other branches, also before the merge. Arztagent does not
use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `copy`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
- The action's default texts appear in the inventory as a hash only (clean room).
