---
id: queue-update-method
title: "Update method of a pull request"
priority: P2
arztagent: false
status: missing
inventory:
  - "config:queue_rules.update_method"
  - "enum:update_method:*"
  - "default:queue_rules.update_method"
problems:
  - "W-08"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Update method of a pull request

When a pull request is tested in place and is outdated, the queue brings it up to date with the
base by merge or rebase.

## Sub-features

- `parse`: built. `update_method` is read (`QueueRule::update`, default `merge`, `rebase` for `fast-forward`), but nothing uses it yet.
- `merge`: missing. Schlange checks a pull request in place only when it is already up to date; a pull request behind the base is checked through a draft instead of being updated.
- `rebase`: missing.
- Why not built yet: after the queue's own update the pull request's checks restart on the new head, and the queue conditions (`check-success = …`) are false until they pass; the engine would dequeue the pull request (W-08). That needs a hold for the queue's own updates first.

## How to get to it (user POV)

- Option in the queue rule.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Not drivable yet.** Planned: a scenario under `scenarios/` that shows the flow in the simulated forge, then `localgit`.

## Gotchas

- The queue's own rebase must not dequeue the pull request (W-08).
