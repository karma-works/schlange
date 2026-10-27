---
id: queue-rules
title: "Queue rules (`queue_rules`)"
priority: P1
arztagent: true
status: partial
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

- `list`: built. Every queue rule with its options is parsed (`crates/schlange-config/src/rules.rs`): conditions, `merge_method`, `batch_size`, `batch_max_wait_time`, `checks_timeout`, `batch_max_failure_resolution_attempts`, `queue_branch_prefix`, `allow_queue_branch_edit`, `max_checks_retries`, and `merge_queue.*` (`max_parallel_checks`, `mode`, `reset_on_external_merge`, `skip_intermediate_results`).
- `name`: built.
- `selection`: built. Without a name, the first queue whose `queue_conditions` hold.
- `order`: built. One queue per base branch; entries of earlier queue rules go first, a batch never mixes queue rules. Mergify's exact ordering across queue rules is not verified.
- `retarget`: built. A pull request whose base branch changes leaves with `BASE_BRANCH_CHANGED` (MQ-10).
- `config-from-base`: missing (the simulation uses one fixed configuration, MQ-19).

## How to get to it (user POV)

- The user sees the queue in the status comment and in the queue's check.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Arztagent's queue.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Pull requests #1, #3, #6, #9 are queued into `default` and land (`#3 merged as …`); the others never enter. Exit 0.
- **Options.** `cargo test -p schlange-config --lib rules` parses Arztagent's queue options and every default.

## Gotchas

- When a pull request's base changes, it must leave the queue and be evaluated again (MQ-10).
- Several queue rules share one queue per base branch in Schlange; whether Mergify orders them by rule order exactly like this is not verified (priorities are `queue-priority`, missing).
