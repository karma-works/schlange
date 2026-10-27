---
id: queue-batch
title: "Batches (`batch_size`, `batch_max_wait_time`)"
priority: P1
arztagent: true
status: partial
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

- `size`: built for a fixed size. A `{min, max}` range is built as: size = `ceil(waiting / free slots)` clamped to `[min, max]` (free slots: `max_parallel_checks` minus the cars and the cars still being closed), waiting only while fewer than `min` are waiting. The documented example holds: five checks, 1..2, three queued → three batches of one, ten queued → five batches of two.
- `wait-time`: built. Duration text parsed (`5 minutes`, `1 min`, seconds); the timer runs on the virtual clock from the earliest `waiting_since` of the batch; a pull request put back after a discarded car keeps its original wait start, so it is not delayed twice.
- `default`: built. Size 1, wait 30 seconds (`QueueRule::new`).
- `grouping`: partial (`crates/schlange-core/src/queue.rs`, `group_by_similarity`). The batch is seeded with the next pull request in queue order; its free places go one at a time to the most similar candidates in a window at the front of the queue. Without scopes (not built) every candidate ties on scopes, so the changed directories decide: the share of directories a candidate has in common with the batch, out of all directories either side changes; ties go to the one queued first. The batch keeps queue order. The window (batch size plus `max_parallel_checks`) and the measure are our reading: the docs say "a little wider than `batch_size`, wider as `max_parallel_checks` grows" and "compares the directories", without numbers. Priorities and stacks are not built. Test: `cargo test -p schlange-forge --test critic_1abc batches_are_grouped_by_changed_directories` (the documented #1 #3 / #2 #4 example).
- `change-while-waiting`: missing (configuration is fixed per run in the simulation).

## How to get to it (user POV)

- Several ready pull requests in quick succession; one shared draft pull request `mergify/merge-queue/…` appears.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Wait, then a partial batch; a full batch at once.** Run `target/release/schlange sim run scenarios/p1-batch-five.yaml`. Three pull requests are queued at +0:05 and `build car … (checks #1 #2 #3)` appears at +0:10 (after 5 minutes); five queued at +0:35 start at once: `(checks #4 #5 #6 #7 #8)` at +0:35. Each batch lands as one squash commit per pull request. Exit 0.
- **Dynamic size.** `cargo test -p schlange-forge --test regressions dynamic_batch_size_matches_the_documented_example` (the full documented example), `cargo test -p schlange-core batch_size` (the formula), `cargo test -p schlange-forge --test dsl dynamic_batch_size_spreads_over_slots`.
- **Wait timer.** `cargo test -p schlange-forge --test regressions batch_wait_timer_starts_with_the_first_pull_request`.

## Gotchas

- A pull request must not wait forever below the minimum (MQ-26): the wait timer always fires a `Tick`, and I2 checks at quiescence that nothing is left queued.
- Batches raise CI cost when they fail (MQ-28); not measured yet.
- Mergify fills batches by similarity (scopes, directories), not strictly in queue order; Schlange's batches can therefore differ from Mergify's for the same queue. Not tested against Mergify.
- The dynamic-size formula is our reading of the documented examples (3 pull requests and 5 checks → three batches of 1; 10 → five of 2); the exact algorithm is not public.
