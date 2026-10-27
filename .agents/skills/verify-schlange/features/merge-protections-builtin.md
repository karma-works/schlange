---
id: merge-protections-builtin
title: "Built-in protection: configuration changed"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:mergify-configuration-changed"
problems:
  - "MQ-19"
  - "MQ-49"
source: "https://docs.mergify.com/merge-protections/builtin"
---

# Built-in protection: configuration changed

A pull request can name other pull requests that must be merged first; a change of the
configuration is recognised as its own state.

## Sub-features

- `config-changed`

## How to get to it (user POV)

- Attribute `mergify-configuration-changed` in conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Arztagent changes its configuration only through constitution pull requests queued by a human.
