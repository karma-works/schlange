---
id: commands-other
title: "Commands `backport`, `copy`, `rebase`, `squash`, `update`"
priority: P2
arztagent: false
status: missing
inventory:
  - "cmd:backport"
  - "cmd:copy"
  - "cmd:rebase"
  - "cmd:squash"
  - "cmd:update"
problems:
  - "MQ-50"
source: "https://docs.mergify.com/commands"
---

# Commands `backport`, `copy`, `rebase`, `squash`, `update`

Further comment commands that trigger the actions of the same name once.

## Sub-features

- `recognised`: the parser knows all five; after the restriction check (their documented
  defaults included) the queue answers that the command is not supported yet and does nothing.
- `backport`, `copy`, `rebase`, `squash`, `update`: missing (the actions are not built).

## How to get to it (user POV)

- Comment on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet** as actions. The "not supported yet" answer: `target/release/schlange sim run scenarios/p1-commands.yaml` → `Schlange does not support it yet`.

## Gotchas

- Permissions, see `commands-restrictions`.
