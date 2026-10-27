---
id: queue-rules
title: "Queue rules (`queue_rules`)"
priority: P1
arztagent: true
status: missing
inventory:
  - "config:queue_rules"
  - "config:queue_rules.name"
problems:
  - "MQ-10"
  - "MQ-19"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Queue rules (`queue_rules`)

A configuration defines one or more named queues. A pull request enters the first queue whose
queue conditions it meets, unless the `queue` action names one. Arztagent has exactly one queue,
`default`.

## Sub-features

- `list`: up to 50 queues; their order is the order of selection.
- `name`: unique name that actions and commands refer to.
- `selection`: without a name, the first queue that matches.

## How to get to it (user POV)

- The user sees the queue in the status comment and in the queue's check.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. The simulation reads `queue_rules` and evaluates the `queue_conditions` of queue `default`.
- **Planned.** The queue itself is a placeholder in phase 0.

## Gotchas

- When a pull request's base changes, it must leave the queue and be evaluated again (MQ-10).
