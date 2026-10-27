---
id: action-comment
title: "Action `comment`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:comment"
  - "config:pull_request_rules.actions.comment"
  - "config:pull_request_rules.actions.comment.*"
  - "config:defaults.actions.comment"
  - "config:defaults.actions.comment.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/comment"
---

# Action `comment`

Write a comment on the pull request. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `comment`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
