---
id: queue-merge-method-squash
title: "Merge method `squash`"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.merge_method"
  - "enum:merge_method:squash"
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

- `one-commit-per-pr`: built. A batch lands as one squash commit per pull request, in queue order, each with the single parent of the commit before (`SimForge::land`).
- `tree-equality`: built. Engine: last landed tree = car tree, else halt. Checker (I0): every landed commit's tree equals the tree the queue built for exactly that prefix of the car, and the first commit's parent has the tree the car was built on.
- `commit-message`: missing; the simulated squash message is `<title> (#N)`, see `queue-commit-message`.
- `other methods`: `merge_method` is parsed; any value lands as squash in the simulation (`queue-merge-methods-other` is missing).

## How to get to it (user POV)

- A merged pull request shows up on `main` as one commit with the pull request's title and number.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Batch of five.** Run `target/release/schlange sim run scenarios/p1-batch-five.yaml`: `car 11 landed #4 as …, #5 as …, #6 as …, #7 as …, #8 as …` (five commits) and `I0  checked (… checks)`.
- **Random batches 1–4.** `cargo test -p schlange-forge --test properties` runs I0 per commit for random batch sizes.

## Gotchas

- GitHub's own queue landed wrong states for squash batches on 2026-04-23 (MQ-02). The test must check every single commit, not only the tip (MQ-03).
- Inside a batch, the intermediate commits (base + first k pull requests) were never tested as such: CI ran only on the batch's tip. I0 therefore checks that each intermediate commit equals the queue branch's prefix commit for that pull request, and that the tip had green checks. That is Mergify's semantics for `squash` with batches too; strict per-commit testing holds only for batch size 1.
- The simulated git merges at file level; a squash and a merge of the same change give the same tree there. Real git can differ (MQ-03), which is why the tree check exists.
