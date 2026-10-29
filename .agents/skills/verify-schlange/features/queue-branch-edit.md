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
- `allow-true`: built for `fast-forward` and `merge-batch` (the documented requirement). The engine keeps the car on the pushed head and rebuilds the cars behind (`BATCH_AHEAD_BRANCH_EDITED`); the full suite must pass on the pushed head before it lands. With `squash` (or any other method) the edit is refused like without the option, so nothing untested lands (`cargo test -p schlange-forge --test regressions allow_queue_branch_edit_with_squash_refuses_the_edit`). The simulation's checker models the accepted edit: the pushed head counts as tested only with the option and a method that lands the batch branch, only when it descends from the tested head, only with green merge conditions judged on it, and only when main's tip at landing carries the tested base's tree (`cargo test -p schlange-forge --test checker_negative`, each guard pinned by a test a mutation breaks). The property generator includes the combination. Fake GitHub: `cargo test -p schlange-github-fake --test e2e_p2 an_accepted_edit` (a pushed head that passes lands, one that breaks the suite never does). Engine: `cargo test -p schlange-core --test dequeue_codes an_accepted_branch_edit_reports_batch_ahead_branch_edited`. The option is deprecated (2026-12-31) and Arztagent does not use it.
- `validation`: `config validate` rejects `true` with any other method or with `merge_method` unset (stage 4, error); with `fast-forward` or `merge-batch` it is valid and supported (`config validate --strict` exits 0).

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
