---
id: queue-modes
title: "Queue modes `parallel` and `isolated`"
priority: P2
arztagent: false
status: missing
inventory:
  - "enum:mode:parallel"
  - "enum:mode:isolated"
problems:
  - "MQ-44"
  - "MQ-48"
source: "https://docs.mergify.com/merge-queue/queue-modes"
---

# Queue modes `parallel` and `isolated`

Besides the serial chain there is a parallel mode with independent scopes and an isolated mode in
which every pull request is tested on its own.

## Sub-features

- `parallel`
- `isolated`

## How to get to it (user POV)

- Option `merge_queue.mode`.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Never test overlapping scopes independently (MQ-48).
