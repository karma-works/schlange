---
id: config-extends
title: "Inherit a configuration (`extends`)"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:extends"
problems:
  - "MQ-52"
source: "https://docs.mergify.com/configuration/sharing"
---

# Inherit a configuration (`extends`)

A configuration can inherit another one from a repository of the same organisation, one level
deep.

## Sub-features

- `one-level`
- `same-org`

## How to get to it (user POV)

- Key `extends`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Check the source (MQ-52).
