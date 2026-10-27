---
id: queue-speculation
title: "Serial, cumulative speculation"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:merge_queue"
  - "config:merge_queue.mode"
  - "default:merge_queue.mode"
  - "enum:mode:serial"
  - "config:merge_queue.max_parallel_checks"
  - "default:merge_queue.max_parallel_checks"
  - "default:merge_queue.skip_intermediate_results"
problems:
  - "MQ-23"
  - "MQ-25"
  - "MQ-28"
  - "MQ-44"
source: "https://docs.mergify.com/merge-queue/queue-modes"
---

# Serial, cumulative speculation

In serial mode the queue tests several states at once: the first entry, the first plus the second,
and so on, up to `max_parallel_checks` (default 5). When an entry fails, every state behind it is
discarded and rebuilt. Intermediate results are not skipped by default.

## Sub-features

- `cumulative`: every state holds all entries before it plus the current base.
- `max-parallel`: at most 5 running states (I4); slots are freed on every exit.
- `cascade`: a failing entry discards every state behind it.
- `intermediate-results`: default `skip_intermediate_results: false`.

## How to get to it (user POV)

- Several queue pull requests at once, each holding one entry more.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Property test over random event sequences with I4.

## Gotchas

- Slots leak easily on abort or dequeue (MQ-25).
- A slow first entry blocks all others (MQ-44).
