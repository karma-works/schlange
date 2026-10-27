---
id: queue-labels-comments
title: "Queue labels and status comments"
priority: P1
arztagent: true
status: missing
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
comments.

## Sub-features

- `labels`: default `queued` and `dequeued`; a label while checks run only when `checking_label` is set (default none).
- `status-comments`: all, outcomes only, or none.
- `checkboxes`: on by default (`queue_controls_comment`): a comment with a "queue this pull request" checkbox when it is ready but not queued, and a "requeue" checkbox in the status comment after it left unmerged. Ticking equals `@mergifyio queue` by the person who ticked, under the same command restrictions.

## How to get to it (user POV)

- Labels on the pull request, the bot's comment in its timeline.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Cosmetics (labels, comments) must never delay landing (MQ-34).
