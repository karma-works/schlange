---
id: action-review
title: "Action `review`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:review"
  - "config:pull_request_rules.actions.review"
  - "config:pull_request_rules.actions.review.*"
  - "config:defaults.actions.review"
  - "config:defaults.actions.review.*"
  - "default:pull_request_rules.actions.review.*"
  - "default:defaults.actions.review.*"
  - "enum:type:*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/review"
---

# Action `review`

Submit a review in the bot's name. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `review`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
