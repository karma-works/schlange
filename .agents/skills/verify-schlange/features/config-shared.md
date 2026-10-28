---
id: config-shared
title: "Shared anchors (`shared`)"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:shared"
problems:
  - "MQ-19"
source: "https://docs.mergify.com/configuration/file-format"
---

# Shared anchors (`shared`)

A free section for YAML anchors that are reused elsewhere through aliases.

## Sub-features

- `anchors`: built. The YAML stage resolves anchors and aliases; `shared` itself has no behaviour, its content is never validated as rules and never taken from an extended configuration.

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **CLI.** `schlange config validate` resolves anchors and aliases (YAML stage).
- **Scenario.** `target/release/schlange sim run scenarios/p2-config-extends.yaml`: the queue conditions reuse an anchor from `shared`.

## Gotchas

- Error positions inside aliases point at the use.
