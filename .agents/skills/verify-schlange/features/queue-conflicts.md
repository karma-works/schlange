---
id: queue-conflicts
title: "Conflicts and a moving base"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:conflict"
  - "config:merge_queue.reset_on_external_merge"
  - "default:merge_queue.reset_on_external_merge"
  - "enum:reset_on_external_merge:*"
unsupported:
  - "attr:conflict"
problems:
  - "MQ-27"
  - "MQ-06"
  - "MQ-47"
  - "MQ-01"
  - "MQ-09"
source: "https://docs.mergify.com/merge-queue/lifecycle"
---

# Conflicts and a moving base

A pull request can conflict with the base or with a pull request ahead of it in the queue. When
the base moves outside the queue (direct push, admin merge), every running state is discarded and
rebuilt by default. Landing happens only while the base still is the expected state.

## Sub-features

- `conflict-base`: built. The forge reports that the pull request does not merge onto the base alone → dequeue with `CONFLICT_WITH_BASE_BRANCH`.
- `conflict-ahead`: built. It merges onto the base but not on top of pull requests ahead → held with `CONFLICT_WITH_PULL_AHEAD` (stays queued, keeps its place); released when those leave; when one of them merges, the next build finds the conflict with the base and dequeues with `CONFLICT_WITH_BASE_BRANCH`.
- `external-merge`: built for `always` (default): a base head the queue did not land discards every car and bisection (`MERGE_QUEUE_RESET`, stays queued) and rebuilds on the new base. A pull request merged by hand leaves with `PR_MANUALLY_MERGED`.
- `never`: built. With `reset_on_external_merge: never` an external merge does not discard the running cars; they land on the new tip (still a compare-and-swap). I0 then checks each landed commit as the tested change applied to the actual parent (`cargo test -p schlange-forge --test regressions reset_on_external_merge_never`).
- `semantic-conflict`: built through speculation: the later pull request is tested on top of the earlier one (`p1-semantic-conflict`; with a real compile script in a checkout, on both forges: `p1-semantic-conflict-compile`).
- `line-level` (real git): built on `localgit`. Conflicts are git's own (`git merge-tree --write-tree`): edits to different lines of one file merge, edits to the same lines conflict (`cargo test -p schlange-forge --test localgit line_level_merge_differs_from_the_in_memory_model`, the matrix test).
- `conflict` attribute: missing (listed as `unsupported`; the evaluator has no `conflict` attribute).

## How to get to it (user POV)

- Status comment "conflict"; after the author rebases, the pull request can be queued again.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Conflicts.** Run `target/release/schlange sim run scenarios/p1-conflict-ahead.yaml`. `car …: #5 conflicts with the base branch` → `dequeue #5 … CONFLICT_WITH_BASE_BRANCH`; `#2 conflicts with #1` → `checks of #2 ended, stays queued: CONFLICT_WITH_PULL_AHEAD`; after #1 lands, `dequeue #2 … CONFLICT_WITH_BASE_BRANCH`; #4 held behind #3 is released when #3 is labeled `halt` and lands. Exit 0.
- **Base moved.** Run `target/release/schlange sim run scenarios/p1-base-moved.yaml`. `direct push to main` at +0:05, then `checks of #1 ended, stays queued: MERGE_QUEUE_RESET (code assumed)` for both, rebuilt cars on the new base; main ends with `hotfix.txt`, `a.txt`, `b.txt`. Exit 0.
- **Semantic conflict.** Run `target/release/schlange sim run scenarios/p1-semantic-conflict.yaml`.
- **Real git.** With a build that has the `localgit` feature (`cargo build --release -p schlange --features localgit`): `target/release/schlange sim run --forge localgit scenarios/p1-semantic-conflict-compile.yaml` (header `forge localgit`; `#2 … CHECKS_FAILED`; main has `fn bar() {}`; exit 0) and the same for `p1-base-moved.yaml`. Tests: `cargo test -p schlange-forge --test localgit`.
- **Manual merge.** `cargo test -p schlange-forge --test dsl merged_by_hand_while_queued`.
- **Race.** A base push between "green" and "land" makes the compare-and-swap fail: `cargo test -p schlange-core --test queue landing_refused_because_the_base_moved_resets_the_queue`; the property tests push to main at random times.

## Gotchas

- An emergency bypass on `main` is the same case as MQ-06.
- The code for a reset after an external push is assumed: `MERGE_QUEUE_RESET` ("the merge queue has been reset"); `BASE_BRANCH_CHANGED` exists too, but its description is about the pull request's base branch, so Schlange uses it when a pull request is retargeted (MQ-10).
- Conflicts are file-level in the in-memory git: two different changes to one file always conflict. Real git (`localgit`) merges lines, so the same scenario can have fewer conflicts there; scenarios meant for both forges give each pull request its own file or its own lines.
- Which pull request ahead a conflict is with is found by merging pairs; if only the combination conflicts, all pull requests ahead are named.
