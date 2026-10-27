---
id: queue-branch-protection
title: "Required checks from branch protection and rulesets"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.branch_protection_injection_mode"
  - "default:queue_rules.branch_protection_injection_mode"
  - "enum:branch_protection_injection_mode:*"
problems:
  - "MQ-13"
  - "MQ-15"
  - "W-05"
  - "W-11"
source: "https://docs.mergify.com/merge-queue/github-rulesets"
---

# Required checks from branch protection and rulesets

The queue takes the checks that branch protection or rulesets of the base require into its own
conditions. By default they become part of the queue conditions. In Arztagent the ruleset on
`main` requires the check `Risk class and data protection` (PR gate).

## Sub-features

- `mode-queue`: default, required checks join the conditions.
- `mode-merge`, `mode-none`: another place, or off.
- `read-fresh`: ruleset changes take effect at once (MQ-13).
- `self-block`: the queue's own check must never be required by its own gate (W-05, deliberately better).

## How to get to it (user POV)

- Ruleset in the repository settings; `schlange doctor` shows the derived checks (planned).

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check names can come from two apps; rulesets can pin an app (MQ-15).
