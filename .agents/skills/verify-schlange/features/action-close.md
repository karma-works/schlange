---
id: action-close
title: "Action `close`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:close"
  - "config:pull_request_rules.actions.close"
  - "config:pull_request_rules.actions.close.*"
  - "config:defaults.actions.close"
  - "config:defaults.actions.close.*"
  - "default:pull_request_rules.actions.close.*"
  - "default:defaults.actions.close.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/close"
---

# Action `close`

Close the pull request with a message. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `close`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
- The action's default texts appear in the inventory as a hash only (clean room).
