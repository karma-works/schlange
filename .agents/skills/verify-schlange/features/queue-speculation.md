---
id: queue-speculation
title: "Serial, cumulative speculation"
priority: P1
arztagent: true
status: partial
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
  - "MQ-01"
source: "https://docs.mergify.com/merge-queue/queue-modes"
---

# Serial, cumulative speculation

In serial mode the queue tests several states at once: the first entry, the first plus the second,
and so on, up to `max_parallel_checks` (default 5). When an entry fails, every state behind it is
discarded and rebuilt. Intermediate results are not skipped by default.

## Sub-features

- `cumulative`: built. Car k holds the base plus the batches of cars 1..k (`crates/schlange-core/src/queue.rs`, `fill`). Landed pull requests drop out of the list of cars built later.
- `max-parallel`: built. At most `max_parallel_checks` cars; checked as I4 after every simulated step (cars in the engine and open drafts on the forge). Slots are freed on landing, failure, dequeue, reset and foreign push.
- `front-decides`: built. Only the front car lands or has its failure handled; a red car behind a running one waits, because the cause may lie ahead.
- `cascade`: built. A discarded car discards every car behind it (`BATCH_AHEAD_FAILED`, `PR_AHEAD_DEQUEUED`, `BATCH_REBUILT`, `MERGE_QUEUE_RESET`).
- `intermediate-results`: default `skip_intermediate_results: false`; `true` is `queue-skip-intermediate` (partial).
- `modes`: `serial` here; `parallel` and `isolated` are `queue-modes` (partial).

## How to get to it (user POV)

- Several queue pull requests at once, each holding one entry more.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Speculation and cascade.** Run `target/release/schlange sim run scenarios/p1-cascade.yaml`. Three `build car` lines at +0:01, the third reads `base … + #1 #2 #3 (checks #3)`; at +0:11 `checks of #2 ended, stays queued: BATCH_AHEAD_FAILED` and the same for #3, `dequeue #1 … CHECKS_FAILED`, then two rebuilt cars land #2 and #3. `I4  checked`. Exit 0.
- **Semantic conflict found by speculation.** Run `target/release/schlange sim run scenarios/p1-semantic-conflict.yaml`. Car 2 tests `#1 #2`; after #1 lands, #2 leaves with `CHECKS_FAILED`; `main has lib.rs = …` PASS. Exit 0.
- **Property test.** `cargo test -p schlange-forge --test properties` checks I4 after every step for random `max_parallel_checks` 1–4.

## Gotchas

- Slots leak easily on abort or dequeue (MQ-25); I3 and I4 run after every step for this reason.
- A slow first entry blocks all others (MQ-44); not measured yet.
- A non-front car that is red keeps its slot until it reaches the front or is discarded; Mergify's exact slot accounting for that case is not documented.
- While a bisection runs, Schlange builds no speculative cars behind it. Whether Mergify keeps speculating behind a split is not documented.
