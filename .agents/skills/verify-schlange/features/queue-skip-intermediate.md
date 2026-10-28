---
id: queue-skip-intermediate
title: "Skip intermediate results"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:merge_queue.skip_intermediate_results"
problems:
  - "MQ-23"
source: "https://docs.mergify.com/merge-queue/performance"
---

# Skip intermediate results

When a later cumulative state is green, earlier states may count as passed without waiting for
their own result.

## Sub-features

- `option`: built for serial mode with `squash`, `merge` and `rebase` (`queue::skip_intermediate`). When a car further back is green, the cars ahead of it (same queue rule) are absorbed into it: their drafts are closed, their pull requests get `INTERMEDIATE_RESULTS_SKIPPED` and land with the green car, whose branch carries them. A red front car whose failure could be stood in for waits while a car behind it that carries it still runs; when none is left, its failure is handled as usual (retry, split, dequeue).
- `I0`: with the option, I0 uses the batch semantics (each commit is a tested prefix of the green car's branch; the checks ran on its tip), also for batch size 1 (KONZEPT.md section 6).
- `bisection`: missing. A part of a split batch is not stood in for by a later part.
- `parallel`: missing (the option is only applied in serial mode); `isolated` refuses it as documented (not applied).
- `fast-forward`, `merge-batch`: not applied (stacked cars).

## How to get to it (user POV)

- Option in `merge_queue`.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- Run `target/release/schlange sim run scenarios/p2-skip-intermediate.yaml`: car 1 (#1) fails, car 2 (#1 + #2) passes; `checks of #1 ended, stays queued: INTERMEDIATE_RESULTS_SKIPPED`, both land, nothing is dequeued. Exit 0. Also on real git (`localgit` test).
- The property tests draw the option (`skip`).

## Gotchas

- Known risk with flaky tests: a real failure of an earlier state that a later change hides lands anyway (that is the option's contract).
