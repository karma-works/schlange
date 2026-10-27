---
id: action-request-reviews
title: "Action `request_reviews`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:request_reviews"
  - "config:pull_request_rules.actions.request_reviews"
  - "config:pull_request_rules.actions.request_reviews.*"
  - "config:defaults.actions.request_reviews"
  - "config:defaults.actions.request_reviews.*"
  - "default:pull_request_rules.actions.request_reviews.*"
  - "default:defaults.actions.request_reviews.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/request-reviews"
---

# Action `request_reviews`

Request reviews from people or teams. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `request_reviews`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
