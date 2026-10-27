---
id: queue-branch-edit
title: "Foreign pushes to queue branches"
priority: P1
arztagent: true
status: partial
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

- `default-off`: built. The engine knows the head it built on each queue branch; any other head read from the forge is a foreign push.
- `dequeue`: built. The car's own pull requests leave with `DRAFT_PULL_REQUEST_CHANGED`; cars behind are rebuilt with `BATCH_AHEAD_BRANCH_EDITED`; the branch is deleted.
- `allow-true`: the engine accepts the edit and keeps the car. But Mergify accepts `allow_queue_branch_edit: true` only with `merge_method: merge-batch` or `fast-forward`, and Schlange builds neither, so there is no configuration in which the option is both valid and supported.
- `validation`: `config validate` rejects `true` with any other method or with `merge_method` unset (stage 4, error), and flags `merge-batch` and `fast-forward` as unsupported (feature `queue-merge-methods-other`). The option itself is therefore not listed as `unsupported`: `false` is supported, and every valid `true` is already flagged through its merge method.

## How to get to it (user POV)

- Status comment with the reason.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Foreign push.** Run `target/release/schlange sim run scenarios/p1-queue-branch-push.yaml`. At +0:04 `foreign push to mergify/merge-queue/…`, then `dequeue #1 … DRAFT_PULL_REQUEST_CHANGED` and `checks of #2 ended, stays queued: BATCH_AHEAD_BRANCH_EDITED`; #2 lands; the foreign file never reaches main (I0 checked). Exit 0.
- **Engine level.** `cargo test -p schlange-core --test queue a_foreign_push_to_the_queue_branch_dequeues`.

## Gotchas

- Mergify deprecates the option (2026-12-31); the default behaviour stays.
- "Every pull request embarked in the branch": in a cumulative car the branch also carries the pull requests of cars ahead. Schlange dequeues only the car's own pull requests and rebuilds the cars behind; whether Mergify also dequeues those ahead is not documented.
- With faults, the foreign push may be noticed only at the next reconciliation; the fresh read before landing makes sure the edited car never lands.
