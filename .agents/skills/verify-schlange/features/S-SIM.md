---
id: S-SIM
title: "Scenarios in the simulated forge"
priority: P1
arztagent: false
status: partial
inventory: []
problems:
  - "MQ-29"
  - "MQ-31"
  - "MQ-12"
  - "MQ-30"
  - "MQ-36"
source: "KONZEPT.md, section 8"
---

# Scenarios in the simulated forge

YAML scenarios (and a Rust DSL for tests) run against a simulated forge with an in-memory git and
a virtual clock; the output is an event log, expectations and an invariant report I0–I5.

## Sub-features

- `scenario-format`: built; the phase-0 format still parses. New: `repo`, `seed`, `faults`, `max_time`, per-check `runs_on`, `fail_when_contains`, `fail_when_all`, `fail_when_prs`, `hang_when_contains`, `flake_rate`; files as a list or a path → content map; events `push_base`, `merge_manually`, `push_queue_branch`; expectations `state`, `reason`, `expect_merged_order`, `expect_base`, `expect_log` (`crates/schlange-forge/src/scenario.rs`).
- `git`: built. Commits with trees and parents, deterministic ids, refs by compare-and-swap, file-level three-way merge with conflicts, squash (`git.rs`). The forge logic, runner and checker are generic over the `GitStore` trait.
- `localgit`: built (cargo feature `localgit`, default for `schlange-forge`, off in the release binary and the image). The same forge logic on real git: a bare repository in a temporary directory, objects written with git plumbing, merges and squashes by `git merge-tree --write-tree` (line-level, git's conflicts), every ref write a `git update-ref <ref> <new> <old>` compare-and-swap, all reads from the repository (`localgit.rs`). Pull request metadata, the clock and webhooks stay the adapter's own. `schlange sim run --forge localgit <scenario>` in a build with `--features localgit`; `localgit::run` / `run_with` from Rust.
- `ci-script`: built. Conclusion per tree (markers, pull requests present together), seeded flakes keyed by tree, check and attempt, hanging checks, queue-only and pull-request-only checks (`ci.rs`). New: `run: <command>` makes a check a real process, `sh -c` in a checkout of the tested commit (exit 0: the conclusion, 125: never reports, other: failure). On `localgit` every check is a process in a `git worktree` checkout: marker rules are compiled into a `grep` script; `fail_when_prs` stays a rule the forge evaluates on the real tree.
- `fault-injection`: built for webhooks: drop, duplicate, delay (and so reorder); periodic reconciliation. 5xx, lying writes, crashes and delayed reads are missing.
- `invariants`: built. I0–I4 after every step, I2/I3 at quiescence, I5 by a second, disturbed run (`invariants.rs`, `runner.rs`). A violation fails the run.
- `dsl`: built (`dsl.rs`), with golden event logs (`tests/golden/*.log`) and property tests (`tests/properties.rs`).

## How to get to it (user POV)

- `schlange sim run scenarios/<name>.yaml [--json] [--forge sim|localgit]`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Run every scenario.** For each file in `scenarios/*.yaml`: `target/release/schlange sim run scenarios/<file>`. Exit `0`; `result: PASS`; every line under `expectations` reads `PASS`; `I0` … `I4` read `checked`; `I5` reads `checked` with a note on dropped and duplicated deliveries (only `p1-flaky.yaml` reports `I5 not_checked`, by design).
- **JSON.** `--json` gives `passed`, `log`, `outcomes`, `merged_order`, `expectations`, `invariants` (status, checks, violations, notes), `end_state` and `disturbed`.
- **Tests.** `cargo test -p schlange-forge` (scenarios with golden logs, DSL tests, regressions, properties); `SCHLANGE_PBT_CASES=2000 cargo test -p schlange-forge --test properties` for a long property run.
- **Real git.** Needs git ≥ 2.38. `cargo build --release -p schlange --features localgit`, then `target/release/schlange sim run --forge localgit scenarios/<file>` for `p1-batch-five`, `p1-bisection`, `p1-semantic-conflict`, `p1-semantic-conflict-compile`, `p1-base-moved`, `p1-arztagent-two-step`: header `forge localgit`, `result: PASS`, `I0` … `I4` `checked`. A build without the feature answers `--forge localgit` with exit 3. Tests: `cargo test -p schlange-forge --test localgit` (those six scenarios with the same outcomes as in memory, the squash × batch 1–5 matrix, line-level merges, a negative I0/I3 test); `SCHLANGE_LOCALGIT_PBT_CASES=50 cargo test --release -p schlange-forge --test localgit_properties` for a longer real-git property run (default 4 cases).

## Gotchas

- I5 compares end states only where the outcome does not depend on timing. Queue order follows the moment the queue learns that a pull request is ready; with flaky CI, conflicts between pull requests or split limits, a disturbed delivery can legitimately change which pull requests merge. Such scenarios set `faults: none` and say why.
- Webhooks in the simulation are hints: the runner reads the forge after each delivery. A real adapter has to do the same.
- `max_time` (default 7 days) bounds a run; a run that does not settle reports an I2 violation.
- Golden event logs exist for the in-memory forge only: on `localgit` the commit ids differ (they are git's), so the localgit test compares outcomes and merged order with the in-memory run instead.
- On `localgit`, check durations are still virtual time: the process runs when the check starts, its result is reported after `duration`.
- A `run` command also works on the in-memory forge (the tree is written to a temporary directory), so `p1-semantic-conflict-compile` needs `sh`, `grep` and `sed` on both forges.
- A `localgit` run starts git hundreds of times (about 1–3 s per scenario); `SCHLANGE_KEEP_TEMP=1` keeps the repositories for inspection.
