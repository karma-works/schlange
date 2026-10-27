---
id: action-assign
title: "Action `assign`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:assign"
  - "config:pull_request_rules.actions.assign"
  - "config:pull_request_rules.actions.assign.*"
  - "config:defaults.actions.assign"
  - "config:defaults.actions.assign.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/assign"
---

# Action `assign`

Assign people to a pull request or remove them. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `assign`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
