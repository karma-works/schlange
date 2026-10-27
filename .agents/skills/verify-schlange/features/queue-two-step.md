---
id: queue-two-step
title: "Two-step CI (`queue_conditions` and `merge_conditions`)"
priority: P1
arztagent: true
status: partial
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

- `enqueue`: built. `queue_conditions` on the pull request.
- `merge`: built. `merge_conditions` on the car: the pull request's own attributes with the check runs of the draft pull request's head (or of the pull request itself when checked in place). Without `merge_conditions`, the queue conditions are used (our reading of "single-step CI").
- `dequeue`: built. `queue_conditions` are evaluated after every event and once more on a fresh read before landing.
- `missing-check`: built with a fixed `checks_timeout` (`CHECKS_TIMEOUT` with the list of checks that never reported); with `auto` it waits forever (see `queue-checks-timeout`).
- `queue-only-ci`: built in the simulated CI: a script can run only on queue draft pull requests (`runs_on: queue`), like Arztagent's full `ci-ok` on `mergify/merge-queue/*` heads.

## How to get to it (user POV)

- A pull request becomes ready (not a draft), checks green → status "queued".
- The queue pull request runs the full suite → merge.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Arztagent's two steps.** Run `target/release/schlange sim run scenarios/p1-arztagent-two-step.yaml`. On the pull requests `ci-ok` completes at +0:03 (quick) and `KI-Review` at +0:05; the queue's draft `#1000` runs `ci-ok` for 20 minutes (`#1000 check `ci-ok` completed …: failure` at +0:30) and never `KI-Review`; #2 leaves with `CHECKS_FAILED`, #1, #3 and Dependabot's #4 land. Exit 0.
- **Label during the queue's CI.** Run `target/release/schlange sim run scenarios/p1-label-halt.yaml`: `dequeue #1 … PR_DEQUEUED (code assumed): queue_conditions no longer hold (condition 3 is false)` at +0:11; after the label is removed at +1:00, #1 is queued again and lands.

## Gotchas

- In Arztagent, `ci.yml` reacts only to `pull_request`; the queue pull request must be a real pull request (MQ-18). The simulated forge opens a draft pull request per car and runs the queue scripts on it.
- `ai-review.yml` never runs on queue pull requests; `KI-Review` may therefore only appear in `queue_conditions`.
- Checks on the draft are matched by name only; the `app` of a check run is not compared yet (MQ-15).
