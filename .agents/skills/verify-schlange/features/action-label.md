---
id: action-label
title: "Action `label`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:label"
  - "config:pull_request_rules.actions.label"
  - "config:pull_request_rules.actions.label.*"
  - "config:defaults.actions.label"
  - "config:defaults.actions.label.*"
  - "default:pull_request_rules.actions.label.*"
  - "default:defaults.actions.label.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/label"
---

# Action `label`

Add, remove or toggle labels. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `label`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
