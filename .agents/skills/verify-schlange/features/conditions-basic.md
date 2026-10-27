---
id: conditions-basic
title: "Attributes `base`, `draft`, `label`, `author`, `sender`"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:base"
  - "attr:draft"
  - "attr:label"
  - "attr:author"
  - "attr:sender"
problems:
  - "MQ-09"
  - "MQ-10"
  - "MQ-11"
source: "https://docs.mergify.com/configuration/conditions"
---

# Attributes `base`, `draft`, `label`, `author`, `sender`

The simple pull request attributes Arztagent uses: target branch, draft state, labels, author and
(in `commands_restrictions`) the sender of a command.

## Sub-features

- `base`: built. Target branch as text (`crates/schlange-core/src/eval.rs`, `attribute`).
- `draft`: built. Boolean, negatable (`-draft`).
- `label`: built. List; label `halt` as stop.
- `author`: built. Login, bots too (`dependabot[bot]`).
- `sender`: built in the evaluator (login of the commenter, empty without a command); only read by `commands_restrictions`, which is missing, so it is evaluated nowhere yet.
- `login-ids`: missing. Logins are compared as text, not resolved to IDs (W-06).

## How to get to it (user POV)

- Set label `halt` → the pull request leaves the queue.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; `#8` (author not in the rule) stays out, `#5` leaves the queue after `labeled halt`, `#6` enters only after `ready for review`.

## Gotchas

- Resolve logins by ID rather than name, otherwise renames break (W-06).
