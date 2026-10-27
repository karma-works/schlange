---
id: queue-update-method
title: "Update method of a pull request"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:queue_rules.update_method"
  - "enum:update_method:*"
problems:
  - "W-08"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Update method of a pull request

When a pull request is tested in place and is outdated, the queue brings it up to date with the
base by merge or rebase.

## Sub-features

- `merge`
- `rebase`

## How to get to it (user POV)

- Option in the queue rule.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- The queue's own rebase must not dequeue the pull request (W-08).
