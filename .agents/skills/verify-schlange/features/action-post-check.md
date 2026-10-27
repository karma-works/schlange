---
id: action-post-check
title: "Action `post_check`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:post_check"
  - "config:pull_request_rules.actions.post_check"
  - "config:pull_request_rules.actions.post_check.*"
  - "config:defaults.actions.post_check"
  - "config:defaults.actions.post_check.*"
  - "default:pull_request_rules.actions.post_check.*"
  - "default:defaults.actions.post_check.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/post-check"
---

# Action `post_check`

Publish a custom check with title and summary on the pull request. Arztagent does not use the
action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `post_check`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
- The action's default texts appear in the inventory as a hash only (clean room).
