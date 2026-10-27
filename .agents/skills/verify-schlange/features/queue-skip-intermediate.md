---
id: queue-skip-intermediate
title: "Skip intermediate results"
priority: P2
arztagent: false
status: missing
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

- `option`

## How to get to it (user POV)

- Option in `merge_queue`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Known risk with flaky tests.
