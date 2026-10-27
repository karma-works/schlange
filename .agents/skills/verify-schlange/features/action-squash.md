---
id: action-squash
title: "Action `squash`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:squash"
  - "config:pull_request_rules.actions.squash"
  - "config:pull_request_rules.actions.squash.*"
  - "config:defaults.actions.squash"
  - "config:defaults.actions.squash.*"
  - "default:pull_request_rules.actions.squash.*"
  - "default:defaults.actions.squash.*"
  - "enum:commit_message:*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/squash"
---

# Action `squash`

Squash the pull request's commits into one. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `squash`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
