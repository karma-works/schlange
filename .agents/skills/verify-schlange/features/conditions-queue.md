---
id: conditions-queue
title: "Queue attributes"
priority: P2
arztagent: false
status: partial
inventory:
  - "attr:queue-position"
  - "attr:queue-name"
  - "attr:queued-at"
  - "attr:queue-merge-started-at"
problems:
  - "MQ-43"
source: "https://docs.mergify.com/configuration/conditions"
---

# Queue attributes

Position, queue name and timestamps of a queued pull request as conditions.

## Sub-features

- `position`: built. 0 for the first pull request of its queue on its base, by queue order
  (the order of entering); -1 when not queued.
- `name`: built. The queue's name; empty when not queued.
- `timestamps`: built. `queued-at` and `queue-merge-started-at` (when its current batch's checks
  started; none while waiting) as Unix time; only `<`, `<=`, `>`, `>=` against a timestamp or a
  relative `… ago`; unset values compare false.
- `queue-dequeue-reason` is in `queue-dequeue-reasons`.

## How to get to it (user POV)

- Conditions.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** `target/release/schlange sim run scenarios/p1-labels-status.yaml` (merge conditions with `queue-name`, `queue-position` and `queued-at`; the invariant checker evaluates them with the queue facts it observed before the landing).
- **Unit tests.** `cargo test -p schlange-core --lib eval::tests::queue_attributes`.

## Gotchas

- The value changes with every queue step.
- Position counts within one queue on one base branch; Mergify's ordering with priorities is not built.
