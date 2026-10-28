---
id: S-EXPLAIN
title: "`schlange explain`"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-43"
  - "MQ-45"
  - "W-13"
source: "KONZEPT.md, section 8"
---

# `schlange explain`

Explains per pull request why it is (not) queued, not merged, or was dequeued, from the engine
state: the conditions with their values, the holds, its place in the queue, its batch and the
last dequeue reason.

## Sub-features

- `cause-tree`: built (`crates/schlange-core/src/explain.rs`). Every `pull_request_rules` rule
  with a `queue` action and every queue rule: each condition as a tree, each node with its
  result (`[ok]`, `[no]`, `[??]` when it cannot be evaluated) and each atom with the value the
  engine saw; the summary names the failing sub-conditions of `and`/`or`/`not`. Merge
  conditions are judged on the batch draft's checks while the pull request rides in one.
- `holds`: built. `Depends-On`, `Merge-After`, configuration change, waiting command, manual
  dequeue (`State::holds`), pause, freeze, merge protections, halted queue.
- `place`: built. Queue, rule, position, the pull requests ahead, the batch (car, pull
  requests, draft, status), who queued it.
- `last-dequeue`: built (code, text, whether the code is assumed; the code of the last time its
  checks ended).
- `status-comment`: built. The status comment carries the short form: the position and the
  pull requests ahead while waiting, the failing queue conditions while a `queue` command
  waits.
- `json`: built (`--json`).
- `config-line`: missing (conditions carry no source line yet; critic e8a0022 M3).
- `mcp`: missing (S-MCP).

## How to get to it (user POV)

- `schlange explain <n> --repo owner/name [--url …] [--json]` (against `serve`)
- `schlange explain <n> --scenario scenarios/<file>.yaml` (end of a simulated run)
- `GET /repos/{owner}/{repo}/pulls/{n}/explain` (admin token)

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Scenario.** Run `target/release/schlange explain 2 --scenario scenarios/p0-queue-conditions.yaml`. Exit `0`; `#2 … not-queued`, the rule whose `-files ~= …` condition fails with the changed files as its value.
- **Golden files.** `cargo test -p schlange-core --test explain`: not queued (failing `or`/`not`), a waiting command, queued behind two, in a batch, held by `Depends-On`, paused, dequeued, unknown (`crates/schlange-core/tests/golden/explain/`).
- **Against serve.** `cargo test -p schlange --test serve`: `schlange explain <n>` through the admin API.

## Gotchas

- The explanation is the engine's view at the last read; a check reported a moment ago may not
  be in it yet.
