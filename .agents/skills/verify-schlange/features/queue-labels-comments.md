---
id: queue-labels-comments
title: "Queue labels and status comments"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:merge_queue.queued_label"
  - "config:merge_queue.dequeued_label"
  - "config:merge_queue.checking_label"
  - "config:merge_queue.status_comments"
  - "config:merge_queue.queue_controls_comment"
  - "default:merge_queue.queued_label"
  - "default:merge_queue.dequeued_label"
  - "default:merge_queue.status_comments"
  - "default:merge_queue.queue_controls_comment"
  - "enum:status_comments:*"
  - "default:merge_queue.checking_label"
problems:
  - "MQ-45"
  - "MQ-43"
source: "https://docs.mergify.com/merge-queue/monitoring"
---

# Queue labels and status comments

The queue shows a pull request's state with labels (queued, dequeued) and status comments.
Arztagent sets none of this and gets the defaults: labels `queued` and `dequeued`, all status
comments, and the checkboxes.

## Sub-features

- `labels`: built (`crates/schlange-core/src/cosmetics.rs`). `queued_label` (default `queued`)
  while queued; `checking_label` (default none) while in a batch or parked in a bisection, off
  while waiting or held; `dequeued_label` (default `dequeued`) after leaving unmerged (also on a
  closed pull request); `null` turns a label off. The engine records what it applied and changes
  only the difference; the queue's own labels never count as the pull request changing.
- `status-comments`: built. One status comment per pull request, created and then edited:
  `all` (default) reports waiting, being checked (with the draft), held, split, merged and
  dequeued with the reason; `outcomes` only merged and dequeued; `none` nothing. The texts are
  Schlange's own. The "Nothing landed" section is not built (no `rebase` method).
- `checkboxes`: built (`queue_controls_comment`, default on). A comment with an "add to the
  merge queue" checkbox when a pull request is open, never queued, nothing holds it, and some
  queue's conditions hold, but no rule queued it; the status comment carries a "put back into
  the queue" checkbox after a dequeue of an open pull request. Ticking equals `queue` by the
  person who ticked, under the same restrictions; the box is shown unticked again afterwards.
  On GitHub the tick arrives as `issue_comment.edited` with the ticker as sender; the
  reconciliation cannot recover a lost tick (the API does not say who edited).
- `cosmetic`: built. Never a condition for landing; GitHub sends it with the lowest priority and
  defers it when the budget is low (`e2e_commands cosmetics_wait_for_the_budget_and_never_delay_landing`).

## How to get to it (user POV)

- Labels on the pull request, the bot's comments in its timeline.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Defaults.** Any scenario, e.g. `target/release/schlange sim run scenarios/p1-label-halt.yaml`: `labels #N +queued`, `status comment #N: …`, `labels #N +dequeued -queued`.
- **Options.** `target/release/schlange sim run scenarios/p1-labels-status.yaml` (`checking_label`, a renamed `dequeued_label`, `status_comments: outcomes`).
- **Checkboxes.** `target/release/schlange sim run scenarios/p1-checkbox.yaml`.
- **Fake GitHub.** `cargo test -p schlange-github-fake --test e2e_commands ticking_the_checkboxes_queues_and_requeues cosmetics_wait_for_the_budget_and_never_delay_landing`.

## Gotchas

- Cosmetics (labels, comments) must never delay landing (MQ-34).
- Exactly when Mergify posts the controls comment ("ready to be queued") is our reading of the
  docs; observe it before claiming conformance.
