---
id: queue-merge-methods-other
title: "Other merge methods"
priority: P2
arztagent: false
status: partial
inventory:
  - "enum:merge_method:merge"
  - "enum:merge_method:rebase"
  - "enum:merge_method:fast-forward"
  - "enum:merge_method:merge-batch"
  - "config:queue_rules.queue_branch_merge_method"
  - "default:queue_rules.queue_branch_merge_method"
problems:
  - "MQ-03"
  - "MQ-04"
  - "MQ-05"
source: "https://docs.mergify.com/merge-queue/merge-strategies"
---

# Other merge methods

Besides squash, the queue can land with a merge commit, rebase, fast-forward, or as one merge of
the whole batch. Fast-forward needs a linear history and extra care with the pull request's
status.

## Sub-features

- `merge`: built. Each pull request lands as a merge commit of the base and its head (`Decision::Land { method }`); its own commits keep their SHAs. Sim/localgit: `GitStore::merge`; GitHub: `PUT /pulls/{n}/merge` with `merge_method: merge`.
- `rebase`: built. Each pull request's commits are replayed onto the base (`GitStore::cherry_pick`; GitHub: `merge_method: rebase`); `LandedCommit::extra` lists the replayed commits before the last one.
- `fast-forward`: built. The base moves onto the tested head (the queue branch, or the pull request itself in place); every car is built on the branch of the car ahead (`BuildCar::stack_on`), so landing it is a fast-forward. GitHub: `PATCH /git/refs` with `force: false` after a fresh read of the base. A retry rebuilds the cars stacked on the old attempt; `reset_on_external_merge: never` does not apply (a moved base can never be fast-forwarded onto a tested branch, so the queue resets; the same for `merge-batch`, whose stacked branches were built on the car ahead — our choice, the docs do not say).
- `merge-batch`: built. Cars are stacked as for fast-forward; the batch branch lands as one merge commit (sim/localgit: `GitStore::merge`; GitHub: the draft pull request merged with `merge_method: merge`, title `Merged #a, #b`).
- `queue-branch-fast-forward`: built. The deprecated `queue_branch_merge_method: fast-forward` means `merge_method: fast-forward`.
- `merged-state` (MQ-05): built. For `fast-forward` and `merge-batch` a pull request counts as merged only when its head is reachable from the new base (sim forge; the fake GitHub marks reachable pull requests merged as GitHub does).
- `allow_queue_branch_edit`: with these two methods an accepted edit of the batch branch lands with the batch (the cars behind are rebuilt).
- `I0`: per method (`crates/schlange-forge/src/invariants.rs`, KONZEPT.md section 6): `merge`, `fast-forward` (batch), `squash`: every first-parent commit is the tested prefix tree; `rebase` and `fast-forward` in place: the pull request's last commit is the tested tree, its earlier commits form one chain on the expected parent; `merge-batch`: one merge commit whose tree is the batch's tested tree.
- `unset`: Mergify uses the repository's first allowed method; Schlange squashes (`queue-merge-method-squash`, unsupported there).

## How to get to it (user POV)

- Commit graph on the base.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Simulation.** Run `target/release/schlange sim run scenarios/p2-method-merge.yaml` (and `-rebase`, `-fast-forward`, `-merge-batch`): five pull requests in batches of two, `land car …: <method> …`, I0 checked, exit 0. `scenarios/regressions/p2-fast-forward-in-place.yaml`: in place, with forge faults.
- **Real git.** `cargo test -p schlange-forge --features localgit --test localgit matrix_other_merge_methods_keep_i0_on_real_git`: every method × batch size 1–3 (merge-batch 2–3) with line-level edits of one file, a broken and a conflicting pull request: I0–I4 hold, the commit graph has the method's shape, `git fsck` passes.
- **GitHub (fake).** `cargo test -p schlange-github-fake --test e2e_p2 merge_method` (four tests).

## Gotchas

- Fast-forward: mark the pull request merged only after a reachability check (MQ-05).
- `merge-batch` with `batch_size: 1` is not refused by `config validate` (Mergify requires `batch_size > 1`); it lands like a merge of the single pull request's batch branch.
- `commit_message_format` is applied to squash commits only.
