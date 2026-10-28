---
id: scopes
title: "Scopes for monorepos"
priority: P2
arztagent: false
status: partial
inventory:
  - "config:scopes"
  - "config:scopes.*"
  - "default:scopes.*"
  - "attr:scope"
  - "cli:mergify ci scopes"
  - "cli:mergify ci scopes-send"
  - "api:POST /repos/{owner}/{repository}/pulls/{number}/scopes"
  - "api:PUT /repos/{owner}/{repository}/commits/{sha}/scopes"
  - "api:PUT /repos/{owner}/{repository}/pulls/{number}/scopes"
unsupported:
  - "config:scopes.source.manual"
  - "config:scopes.capacities"
  - "config:scopes.default_capacity"
  - "attr:scope"
  - "cli:*"
  - "api:*"
problems:
  - "MQ-48"
source: "https://docs.mergify.com/merge-queue/scopes"
---

# Scopes for monorepos

Pull requests are grouped by the parts of a monorepo they touch; independent parts can be tested
in parallel.

## Sub-features

- `files`: built. `scopes.source.files`: a scope per name with `include` and optional `exclude` globs (the `files` glob syntax), matched against a pull request's changed files (`queue_extras::Scopes::of`).
- `batching`: built. Serial and isolated mode: batch candidates are ranked by priority, then by the share of scopes they have in common with the batch, then by changed directories, then by queue order (`queue::group_with_scopes`). Parallel mode: only pull requests with exactly the same scopes share a batch (`queue-modes`).
- `barrier`: built. `scopes.barrier_files`: a pull request changing a matching file is a barrier: it shares a batch only with barriers, starts only once every car ahead is done, and no car starts while it is in the queue (serial and parallel mode; isolated mode ignores it for ordering, as documented). At least one `include` is required.
- `merge_queue_scope`: parsed; Schlange's queue drafts carry no scopes, so it has no effect.
- `manual`: missing (`source.manual` is parsed but gives every pull request no scope; the scopes upload API and CLI are not built).
- `capacity`: missing (`capacities`, `default_capacity`).
- `attr-scope`: missing (the `scope` condition attribute).
- `direct-merge`: missing (direct merge of a pull request behind the base only by merges in other scopes).

## How to get to it (user POV)

- Section `scopes`; the queue status API shows `scopes` and `all_scopes` per pull request and batch.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- Run `target/release/schlange sim run scenarios/p2-parallel-scopes.yaml` (parallel mode: scope lanes, a pull request in two scopes on top of both, a barrier alone). Exit 0. Also on real git.
- `cargo test -p schlange-config --lib queue_extras` (scope matching, barrier).

## Gotchas

- Overlapping scopes (MQ-48): a pull request in two scopes is tested on top of the last batch of each.
