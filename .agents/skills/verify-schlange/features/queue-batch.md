---
id: queue-batch
title: "Batches (`batch_size`, `batch_max_wait_time`)"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.batch_size"
  - "config:queue_rules.batch_size.*"
  - "config:queue_rules.batch_max_wait_time"
  - "default:queue_rules.batch_size"
  - "default:queue_rules.batch_max_wait_time"
problems:
  - "MQ-26"
  - "MQ-21"
  - "MQ-28"
source: "https://docs.mergify.com/merge-queue/batches"
---

# Batches (`batch_size`, `batch_max_wait_time`)

Several pull requests are tested together in one queue pull request. A batch starts once it is
full or the wait time has passed. Arztagent: up to 5 pull requests, wait at most 5 minutes.
Without settings, the batch size is 1 and the wait time 30 seconds.

## Sub-features

- `size`: fixed size, or a minimum and a maximum.
- `wait-time`: duration as text (`5 minutes`), virtual clock in the simulation.
- `default`: size 1, wait time 30 seconds.
- `change-while-waiting`: configuration change while a batch waits.

## How to get to it (user POV)

- Several ready pull requests in quick succession; one shared draft pull request `mergify/merge-queue/…` appears.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Virtual clock: 4 pull requests, then 5 minutes of silence → a batch of 4 starts.

## Gotchas

- A pull request must not wait forever below the minimum (MQ-26).
- Batches raise CI cost when they fail (MQ-28).
