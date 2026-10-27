---
id: action-queue
title: "Action `queue`"
priority: P1
arztagent: true
status: missing
inventory:
  - "action:queue"
  - "config:pull_request_rules.actions.queue"
  - "config:pull_request_rules.actions.queue.*"
  - "config:defaults.actions.queue"
  - "config:defaults.actions.queue.*"
  - "default:pull_request_rules.actions.queue.*"
  - "default:defaults.actions.queue.*"
problems:
  - "MQ-09"
  - "MQ-12"
source: "https://docs.mergify.com/workflow/actions/queue"
---

# Action `queue`

The action queues a pull request while the rule holds. Without a name it picks the first queue
whose queue conditions match.

## Sub-features

- `without-name`: first matching queue.
- `with-name`: fixed queue.
- `level`: when the rule stops holding, the pull request stays queued only through a command.

## How to get to it (user POV)

- Status "queued" on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Decision `enqueue` in the event log (placeholder queue).

## Gotchas

- A pull request queued by command behaves differently from one queued by rule; check dequeueing when the rule's conditions stop holding.
