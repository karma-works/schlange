---
id: action-rebase
title: "Action `rebase`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:rebase"
  - "config:pull_request_rules.actions.rebase"
  - "config:pull_request_rules.actions.rebase.*"
  - "config:defaults.actions.rebase"
  - "config:defaults.actions.rebase.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/rebase"
---

# Action `rebase`

Rebase the pull request onto the base. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `rebase`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
