---
id: action-update
title: "Action `update`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:update"
  - "config:pull_request_rules.actions.update"
  - "config:pull_request_rules.actions.update.*"
  - "config:defaults.actions.update"
  - "config:defaults.actions.update.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/update"
---

# Action `update`

Merge the base into the pull request. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `update`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
