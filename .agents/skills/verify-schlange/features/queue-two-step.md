---
id: queue-two-step
title: "Two-step CI (`queue_conditions` and `merge_conditions`)"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.queue_conditions"
  - "config:queue_rules.merge_conditions"
problems:
  - "MQ-42"
  - "MQ-18"
  - "MQ-11"
source: "https://docs.mergify.com/merge-queue/two-step"
---

# Two-step CI (`queue_conditions` and `merge_conditions`)

A pull request is queued when the queue conditions hold on the pull request itself (Arztagent:
quick checks, risk class, AI review). It is merged only when the merge conditions hold on the
queue pull request (Arztagent: full suite `ci-ok`). When the queue conditions stop holding, the
pull request leaves the queue.

## Sub-features

- `enqueue`: `queue_conditions` on the pull request.
- `merge`: `merge_conditions` on the queue pull request, or on the pull request when it is tested in place.
- `dequeue`: `queue_conditions` are evaluated continuously.
- `missing-check`: a merge check that never starts on the queue pull request ends as a timeout with a reason.

## How to get to it (user POV)

- A pull request becomes ready (not a draft), checks green → status "queued".
- The queue pull request runs the full suite → merge.

## Driving it

- `schlange sim run scenarios/p0-queue-conditions.yaml`: open a pull request, checks turn green, `queue_conditions` are evaluated, decision "enqueue" in the event log.
- `merge_conditions` are not evaluated yet in phase 0 (queue placeholder).

## Gotchas

- In Arztagent, `ci.yml` reacts only to `pull_request`; the queue pull request must be a real pull request (MQ-18).
- `ai-review.yml` never runs on queue pull requests; `KI-Review` may therefore only appear in `queue_conditions`.
