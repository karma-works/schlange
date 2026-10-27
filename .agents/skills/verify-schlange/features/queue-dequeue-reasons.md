---
id: queue-dequeue-reasons
title: "Dequeue with a reason"
priority: P1
arztagent: true
status: missing
inventory:
  - "attr:queue-dequeue-reason"
  - "enum:queue-dequeue-reason:*"
problems:
  - "MQ-45"
  - "MQ-09"
  - "MQ-11"
  - "MQ-07"
  - "W-08"
  - "W-13"
source: "https://docs.mergify.com/merge-queue/lifecycle"
---

# Dequeue with a reason

When a pull request leaves the queue, a reason from a fixed list of codes always comes with it,
for example because the queue conditions no longer hold, a check failed, a conflict appeared or a
human dequeued it. The code can also be read as a condition attribute.

## Sub-features

- `automatic`: queue conditions stop holding (label `halt`, draft, new push, red check).
- `codes`: exactly the codes from the schema; a golden file per dequeue path.
- `attribute`: `queue-dequeue-reason` in conditions.
- `own-rebase`: an update by the queue itself does not dequeue the pull request (W-08, deliberately better).

## How to get to it (user POV)

- Status comment and label `dequeued` on the pull request; `schlange pr explain` names the reason.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Golden per path: label `halt`, draft, push, red check, command `dequeue`.

## Gotchas

- Many codes concern features outside phase 1 (scopes, partitions); they stay in this file's inventory so that no code gets lost.
