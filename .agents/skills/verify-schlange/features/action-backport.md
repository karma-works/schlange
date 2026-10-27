---
id: action-backport
title: "Action `backport`"
priority: P2
arztagent: false
status: missing
inventory:
  - "action:backport"
  - "config:pull_request_rules.actions.backport"
  - "config:pull_request_rules.actions.backport.*"
  - "config:defaults.actions.backport"
  - "config:defaults.actions.backport.*"
  - "default:pull_request_rules.actions.backport.*"
  - "default:defaults.actions.backport.*"
  - "enum:merge_conflict_style:*"
  - "enum:ReportMode:*"
problems:
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/backport"
---

# Action `backport`

After the merge, create a copy of the pull request for other branches; conflicts are marked.
Arztagent does not use the action.

## Sub-features

- `run`
- `options`

## How to get to it (user POV)

- Rule with action `backport`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check whether the action is edge- or level-triggered.
- The action's default texts appear in the inventory as a hash only (clean room).
