---
id: action-dismiss-reviews
title: "Action `dismiss_reviews`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:dismiss_reviews"
  - "config:pull_request_rules.actions.dismiss_reviews"
  - "config:pull_request_rules.actions.dismiss_reviews.*"
  - "config:defaults.actions.dismiss_reviews"
  - "config:defaults.actions.dismiss_reviews.*"
  - "default:pull_request_rules.actions.dismiss_reviews.*"
  - "default:defaults.actions.dismiss_reviews.*"
  - "enum:when:*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/dismiss-reviews"
---

# Action `dismiss_reviews`

Dismiss approvals or change requests on new pushes. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `dismiss_reviews`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
