---
id: commands
title: "Commands `queue`, `dequeue`, `refresh`"
priority: P1
arztagent: true
status: missing
inventory:
  - "cmd:queue"
  - "cmd:dequeue"
  - "cmd:refresh"
problems:
  - "MQ-50"
source: "https://docs.mergify.com/commands"
---

# Commands `queue`, `dequeue`, `refresh`

Humans steer the queue with comments that start with the bot's name. Arztagent uses `queue` for
constitution pull requests, `dequeue` to stop and `refresh` in operations. The queue acknowledges
a command with a reaction and a reply.

## Sub-features

- `recognition`: command at the start of the comment, case-insensitive, both bot names.
- `queue`: optionally with a queue name.
- `dequeue`: leave the queue with a reason.
- `refresh`: evaluate again.
- `reply`: reaction plus reply comment.

## How to get to it (user POV)

- Comment `@mergifyio queue` on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Fuzz the command parser (Unicode, line breaks).

## Gotchas

- Schlange must understand `@mergifyio`, otherwise Arztagent's workflows change.
