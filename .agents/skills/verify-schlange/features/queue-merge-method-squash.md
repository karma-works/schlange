---
id: queue-merge-method-squash
title: "Merge method `squash`"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.merge_method"
  - "enum:merge_method:squash"
  - "default:queue_rules.merge_method"
unsupported:
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

- `one-commit-per-pr`: built. A batch lands as one squash commit per pull request, in queue order, each with the single parent of the commit before (`SimForge::land`).
- `tree-equality`: built. Engine: last landed tree = car tree, else halt. Checker (I0): every landed commit's tree equals the tree the queue built for exactly that prefix of the car, and the first commit's parent has the tree the car was built on.
- `real-git`: built on `localgit` (tests and `sim run --forge localgit`): queue branches are chains of real merge commits, each pull request lands as a real squash commit (`git merge-tree --write-tree` onto the base, `git commit-tree` with one parent), the base moves by `git update-ref` compare-and-swap. Matrix squash × batch 1–5 with every pull request editing the same file on different lines: I0 holds on every commit (`cargo test -p schlange-forge --test localgit matrix_squash_by_batch_size_one_to_five_keeps_i0_on_real_git`).
- `commit-message`: missing; the simulated squash message is `<title> (#N)`, see `queue-commit-message`.
- `other methods`: `merge`, `rebase`, `fast-forward`, `merge-batch` are `queue-merge-methods-other` (partial).
- `unset`: missing (listed as `unsupported`: `default:queue_rules.merge_method`). Without `merge_method`, Mergify uses the first method the repository allows; Schlange squashes regardless, so a configuration that leaves it unset is flagged.

## How to get to it (user POV)

- A merged pull request shows up on `main` as one commit with the pull request's title and number.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Batch of five.** Run `target/release/schlange sim run scenarios/p1-batch-five.yaml`: `car 11 landed #4 as …, #5 as …, #6 as …, #7 as …, #8 as …` (five commits) and `I0  checked (… checks)`.
- **Random batches 1–4.** `cargo test -p schlange-forge --test properties` runs I0 per commit for random batch sizes; `cargo test -p schlange-forge --test localgit_properties` does the same on real git with overlapping line edits.
- **Real git.** With a `localgit` build (`cargo build --release -p schlange --features localgit`): `target/release/schlange sim run --forge localgit scenarios/p1-batch-five.yaml`, header `forge localgit`, five squash commits in one landing, `I0  checked`.

## Gotchas

- GitHub's own queue landed wrong states for squash batches on 2026-04-23 (MQ-02). The test must check every single commit, not only the tip (MQ-03).
- Inside a batch, the intermediate commits (base + first k pull requests) were never tested as such: CI ran only on the batch's tip. I0 therefore checks that each intermediate commit equals the queue branch's prefix commit for that pull request, and that the tip had green checks. That is Mergify's semantics for `squash` with batches too; strict per-commit testing holds only for batch size 1.
- The in-memory git merges at file level; a squash and a merge of the same change give the same tree there. Real git can differ (MQ-03), which is why the tree check exists; on `localgit` the matrix and the real-git property test have not found a difference for squash so far (squash onto base + earlier squashes and the queue branch's merge chain share the merge base).
