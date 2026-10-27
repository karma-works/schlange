---
id: queue-merge-method-squash
title: "Merge method `squash`"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules.merge_method"
  - "enum:merge_method:squash"
  - "default:queue_rules.merge_method"
problems:
  - "MQ-02"
  - "MQ-03"
  - "MQ-20"
source: "https://docs.mergify.com/merge-queue/merge-strategies"
---

# Merge method `squash`

Every pull request lands as exactly one commit on the base. In a batch that gives one commit per
pull request; each of these commits must have a tree that was tested as such (invariant I0).

## Sub-features

- `one-commit-per-pr`: also in a batch, one squash commit per pull request, in queue order.
- `tree-equality`: after landing, the landed tree must equal the tested tree; otherwise halt the queue.
- `commit-message`: format, see `queue-commit-message`.

## How to get to it (user POV)

- A merged pull request shows up on `main` as one commit with the pull request's title and number.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.
- **Planned.** Real repositories: `localgit`, batch size 1 to 5, I0 after every commit.

## Gotchas

- GitHub's own queue landed wrong states for squash batches on 2026-04-23 (MQ-02). The test must check every single commit, not only the tip (MQ-03).
