---
id: action-merge
title: "Action `merge`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:merge"
  - "config:pull_request_rules.actions.merge"
  - "config:pull_request_rules.actions.merge.*"
  - "config:defaults.actions.merge"
  - "config:defaults.actions.merge.*"
  - "default:pull_request_rules.actions.merge.*"
  - "default:defaults.actions.merge.*"
  - "enum:method:*"
problems:
  - "MQ-06"
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/merge"
---

# Action `merge`

Merge the pull request directly, without a queue. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `merge`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
