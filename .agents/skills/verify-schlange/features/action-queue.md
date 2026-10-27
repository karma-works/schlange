---
id: action-queue
title: "Action `queue`"
priority: P1
arztagent: true
status: partial
inventory:
  - "action:queue"
  - "config:pull_request_rules.actions.queue"
  - "config:pull_request_rules.actions.queue.*"
  - "config:defaults.actions.queue"
  - "config:defaults.actions.queue.*"
  - "default:pull_request_rules.actions.queue.*"
  - "default:defaults.actions.queue.*"
unsupported:
  - "config:defaults.actions.queue"
  - "config:defaults.actions.queue.*"
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

- `without-name`: built. First queue whose `queue_conditions` hold.
- `with-name`: built. Only the named queue.
- `idempotent`: built. The same event twice queues once (MQ-12).
- `no-loop`: built. After a failure-type dequeue the action does not queue the same head again; a new push does.
- `level`: open. Once queued, Schlange keeps the pull request while its `queue_conditions` hold, even if the rule's own conditions stop holding; see Gotchas.
- `options`: the schema now has only `name` under `actions.queue` (built). `defaults.actions.queue` is missing (listed as `unsupported`; the `defaults` section is not read).

## How to get to it (user POV)

- Status "queued" on the pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Decisions `enqueue #N into `default` (rule `…`)` for #1, #3, #6, #9; they land. Exit 0.
- **No requeue loop.** `cargo test -p schlange-forge --test dsl failed_pull_request_is_not_requeued_until_it_changes`.

## Gotchas

- A pull request queued by command behaves differently from one queued by rule; check dequeueing when the rule's conditions stop holding. Schlange does not dequeue when only the rule's conditions (not the queue conditions) stop holding; whether Mergify does is not verified.
