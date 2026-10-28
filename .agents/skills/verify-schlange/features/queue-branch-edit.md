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
- `dequeue`: built. Every pull request embarked in the edited branch leaves with `DRAFT_PULL_REQUEST_CHANGED` (docs: "dequeues all pull requests embarked"), also those ahead that the branch carries; cars behind are rebuilt with `PR_AHEAD_DEQUEUED`; the branch is deleted (`cargo test -p schlange-core --test dequeue_codes an_edited_batch_dequeues_every_pull_request_embarked_in_it`).
- `allow-true` (model gap): the sim's invariant checker does not model an accepted edit (the engine lands the edited head, whose tree the car record does not know; the long property run raised a false alarm), so the property generator never combines `allow_queue_branch_edit` with `fast-forward` or `merge-batch`. The option is deprecated and Arztagent does not use it. The engine accepts the edit only with `merge_method: merge-batch` or `fast-forward` (the documented requirement): it keeps the car on the pushed head and rebuilds the cars behind (`BATCH_AHEAD_BRANCH_EDITED`). Schlange builds neither method, so the accepted path is exercised in the engine only (`cargo test -p schlange-core --test dequeue_codes an_accepted_branch_edit_reports_batch_ahead_branch_edited`). With `squash` the edit is refused like without the option, so nothing untested lands (before, the engine accepted it, landed the pull request without the edit and halted; `cargo test -p schlange-forge --test regressions allow_queue_branch_edit_with_squash_refuses_the_edit`). There is no configuration in which the option is both valid and supported.
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
