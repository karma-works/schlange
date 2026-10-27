---
id: queue-conflicts
title: "Conflicts and a moving base"
priority: P1
arztagent: true
status: missing
inventory:
  - "attr:conflict"
  - "config:merge_queue.reset_on_external_merge"
  - "default:merge_queue.reset_on_external_merge"
  - "enum:reset_on_external_merge:*"
problems:
  - "MQ-27"
  - "MQ-06"
  - "MQ-47"
  - "MQ-01"
source: "https://docs.mergify.com/merge-queue/lifecycle"
---

# Conflicts and a moving base

A pull request can conflict with the base or with a pull request ahead of it in the queue. When
the base moves outside the queue (direct push, admin merge), every running state is discarded and
rebuilt by default. Landing happens only while the base still is the expected state.

## Sub-features

- `conflict-base`: conflict with `main` → dequeue with a reason.
- `conflict-ahead`: conflict with a pull request ahead → hold until that one is merged or dequeued.
- `external-merge`: default `always`, rebuild all states.
- `semantic-conflict`: red together, green alone; the queue finds it by testing on top of the predecessor.

## How to get to it (user POV)

- Status comment "conflict"; after the author rebases, the pull request can be queued again.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- Simulated forge with a foreign push between "green" and "set ref".

## Gotchas

- An emergency bypass on `main` is the same case as MQ-06.
