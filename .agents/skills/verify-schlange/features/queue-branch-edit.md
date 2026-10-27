---
id: queue-branch-edit
title: "Foreign pushes to queue branches"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.allow_queue_branch_edit"
  - "default:queue_rules.allow_queue_branch_edit"
problems:
  - "MQ-08"
  - "MQ-53"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Foreign pushes to queue branches

When anyone but the queue pushes to a queue branch, the tested state no longer holds. By default
the queue then dequeues every affected pull request and reports the error.

## Sub-features

- `default-off`: foreign pushes are not accepted.
- `dequeue`: every pull request of the branch, with a reason.

## How to get to it (user POV)

- Status comment with the reason.

## Driving it

- No scenario yet. Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- Simulated forge with a foreign push to `mergify/merge-queue/*`.

## Gotchas

- Mergify deprecates the option (2026-12-31); the default behaviour stays.
