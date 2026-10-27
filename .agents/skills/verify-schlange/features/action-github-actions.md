---
id: action-github-actions
title: "Action `github_actions` (deprecated)"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:github_actions"
  - "config:pull_request_rules.actions.github_actions"
  - "config:pull_request_rules.actions.github_actions.*"
  - "config:defaults.actions.github_actions"
  - "config:defaults.actions.github_actions.*"
  - "default:pull_request_rules.actions.github_actions.*"
  - "default:defaults.actions.github_actions.*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/github-actions"
---

# Action `github_actions` (deprecated)

Start workflows by dispatch. Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `github_actions`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
