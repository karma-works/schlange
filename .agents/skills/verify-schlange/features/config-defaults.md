---
id: config-defaults
title: "Shared defaults (`defaults`)"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:defaults"
  - "config:defaults.actions"
  - "config:defaults.queue_rule"
  - "config:defaults.queue_rule.*"
  - "default:defaults.queue_rule.*"
unsupported:
  - "config:defaults.queue_rule"
  - "config:defaults.queue_rule.*"
  - "default:defaults.queue_rule.*"
problems:
  - "MQ-19"
source: "https://docs.mergify.com/configuration/file-format"
---

# Shared defaults (`defaults`)

Values that apply to every action unless the rule's action sets them (`crates/schlange-config/src/actions.rs`, `with_defaults`).

## Sub-features

- `actions`: built. `defaults.actions.<action>` is layered under each rule's action: the rule's own key wins, then `defaults`, then the schema default; an action written without options (`comment:`) takes the defaults whole.
- `queue-rule`: not built (`defaults.queue_rule` is not applied to queue rules yet).

## How to get to it (user POV)

- A rule in `pull_request_rules` with the action; the action's result shows on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p2-config-extends.yaml`.
- **Unit.** `cargo test -p schlange-config actions` and `cargo test -p schlange-core --test actions defaults`.

## Gotchas

- Order: rule before defaults before schema default.
- With `extends`, `defaults` merge key by key, the local value winning.
