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
- `queue`: optionally with a queue name. Stays pending until `queue_conditions` match; a second command with the same (or no) name is ignored with a reply, one with another name replaces it (and takes the pull request out of its current queue first). Also re-queues a dequeued pull request (`requeue` is a deprecated alias).
- `pending-push`: a push by someone other than the queue while the command waits cancels it, with a reply naming the pusher; the queue's own updates do not. After entering the queue, a push dequeues instead.
- `dequeue`: takes a queued pull request out, or cancels a pending `queue` command; otherwise answers that nothing is queued. A dequeued pull request never rejoins on its own.
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
