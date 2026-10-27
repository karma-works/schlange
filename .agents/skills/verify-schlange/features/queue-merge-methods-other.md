---
id: queue-merge-methods-other
title: "Other merge methods"
priority: P2
arztagent: false
status: missing
inventory:
  - "enum:merge_method:merge"
  - "enum:merge_method:rebase"
  - "enum:merge_method:fast-forward"
  - "enum:merge_method:merge-batch"
  - "config:queue_rules.queue_branch_merge_method"
problems:
  - "MQ-03"
  - "MQ-04"
  - "MQ-05"
source: "https://docs.mergify.com/merge-queue/merge-strategies"
---

# Other merge methods

Besides squash, the queue can land with a merge commit, rebase, fast-forward, or as one merge of
the whole batch. Fast-forward needs a linear history and extra care with the pull request's
status.

## Sub-features

- `merge`
- `rebase`
- `fast-forward`
- `merge-batch`
- `queue-branch-fast-forward`

## How to get to it (user POV)

- Commit graph on the base.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- Fast-forward: mark the pull request merged only after a reachability check (MQ-05).
