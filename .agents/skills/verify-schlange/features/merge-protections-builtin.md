---
id: merge-protections-builtin
title: "Built-in protections (`Depends-On`, configuration changed)"
priority: P2
arztagent: false
status: missing
inventory:
  - "attr:depends-on"
  - "attr:mergify-configuration-changed"
problems:
  - "MQ-19"
  - "MQ-49"
source: "https://docs.mergify.com/merge-protections/builtin"
---

# Built-in protections (`Depends-On`, configuration changed)

A pull request can name other pull requests that must be merged first; a change of the
configuration is recognised as its own state.

## Sub-features

- `depends-on`
- `config-changed`

## How to get to it (user POV)

- Line `Depends-On: #n` in the pull request body.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- According to PLAN-MERGIFY.md Arztagent uses `Depends-On` instead of stacks; candidate for P1 once used.
