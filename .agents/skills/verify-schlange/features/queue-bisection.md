---
id: queue-bisection
title: "Bisection of red batches"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.batch_max_failure_resolution_attempts"
problems:
  - "MQ-21"
  - "MQ-22"
source: "https://docs.mergify.com/merge-queue/batches"
---

# Bisection of red batches

When a batch fails, the queue splits it to find the guilty pull request. The others are merged,
the guilty one is dequeued. Without a setting there is no limit on attempts (Arztagent sets none).

## Sub-features

- `split`: a red batch is split and tested again.
- `culprit`: only the deterministically guilty pull request is dequeued.
- `limit`: optional limit on attempts; after it, dequeue all.
- `interaction`: two pull requests red only together → at least one leaves.

## How to get to it (user POV)

- The status comment of the guilty pull request names the reason; the others are merged.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- Property test with a failure predicate over subsets.

## Gotchas

- Flaky tests blow up batches (MQ-22).
- Prove termination also without a limit (MQ-21).
