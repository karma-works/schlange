---
id: queue-priority
title: "Priorities (`priority_rules`)"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:priority_rules"
  - "config:priority_rules.*"
  - "default:priority_rules.*"
  - "enum:priority:*"
problems:
  - "MQ-24"
source: "https://docs.mergify.com/merge-queue/priority"
---

# Priorities (`priority_rules`)

Rules give pull requests a priority; higher ones move ahead in the queue and may optionally
interrupt running checks.

## Sub-features

- `levels`: built. `low`, `medium`, `high` are 1000, 2000, 3000 (`crates/schlange-config/src/queue_extras.rs`); without a matching rule a pull request is `medium`. The first matching rule wins.
- `number`: built. Numeric priorities, mixed with the keywords.
- `order`: built. The waiting pull requests of a base are ordered by priority (highest first), then by the order of the queue rules, then by arrival (`queue::waiting_order`). A higher priority goes right after the cars that already run (no interruption).
- `batching`: built. A batch is seeded with the next pull request in that order; its free places go to the same queue rule's candidates by priority first, then scopes, then changed directories (`queue::group_with_scopes`): similarity never pulls a lower priority ahead.
- `interrupt`: built. `allow_checks_interruption: true` on the matching rule: when that pull request is next to embark and a running car holds only pull requests of lower priority, the car and the cars built on it are discarded; their pull requests stay queued with `PR_WITH_HIGHER_PRIORITY_QUEUED` and the urgent one is checked first (`queue::preempt`). A car whose landing is in flight, a car checked in place and a running bisection are never interrupted.
- `schedule-yield`: missing. The documented exception (checks of a batch whose merge schedule is closed give up their slot) needs merge schedules, which Schlange does not build.

## How to get to it (user POV)

- Rule in the configuration; the queue status API shows `priority_alias` and `priority_rule_name` per pull request.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Interruption.** Run `target/release/schlange sim run scenarios/p2-priority.yaml`. #3 (medium) is ahead of #2 (low) although it came later; at +0:14 `checks of #3 ended, stays queued: PR_WITH_HIGHER_PRIORITY_QUEUED` and `build car … (checks #4)`; merged in order 1, 4, 3, 2. Exit 0.
- **Without interruption.** Run `target/release/schlange sim run scenarios/p2-priority-no-interrupt.yaml`: #3 (high) goes right after the running #1, ahead of #2; nothing is cancelled. Exit 0.
- **Real git.** `cargo test -p schlange-forge --features localgit --test localgit` runs both scenarios on real repositories.
- **Property tests.** `cargo test -p schlange-forge --test properties` draws priority rules (with and without interruption), `urgent` label toggles and a second queue rule; I0–I5 and the innocence oracle hold.

## Gotchas

- Cancel displaced runs and log the reason (MQ-24): the displaced pull requests get `PR_WITH_HIGHER_PRIORITY_QUEUED`.
- Our reading, not verified against Mergify: the priority of a waiting pull request follows its attributes while it waits (the docs say rules are evaluated when it enters the queue); priority comes before the order of queue rules.
