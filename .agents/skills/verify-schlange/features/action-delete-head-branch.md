---
id: action-delete-head-branch
title: "Action `delete_head_branch` (deprecated)"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:delete_head_branch"
  - "config:pull_request_rules.actions.delete_head_branch"
  - "config:pull_request_rules.actions.delete_head_branch.*"
  - "config:defaults.actions.delete_head_branch"
  - "config:defaults.actions.delete_head_branch.*"
  - "default:pull_request_rules.actions.delete_head_branch.*"
  - "default:defaults.actions.delete_head_branch.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/delete-head-branch"
---

# Action `delete_head_branch` (deprecated)

Delete the head branch after merge or close. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `delete_head_branch`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
