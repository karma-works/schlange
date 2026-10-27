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
- `git`: built. Commits with trees and parents, deterministic ids, refs by compare-and-swap, file-level three-way merge with conflicts, squash (`git.rs`).
- `ci-script`: built. Conclusion per tree (markers, pull requests present together), seeded flakes keyed by tree, check and attempt, hanging checks, queue-only and pull-request-only checks (`ci.rs`).
- `fault-injection`: built for webhooks: drop, duplicate, delay (and so reorder); periodic reconciliation. 5xx, lying writes, crashes and delayed reads are missing.
- `invariants`: built. I0–I4 after every step, I2/I3 at quiescence, I5 by a second, disturbed run (`invariants.rs`, `runner.rs`). A violation fails the run.
- `dsl`: built (`dsl.rs`), with golden event logs (`tests/golden/*.log`) and property tests (`tests/properties.rs`).

## How to get to it (user POV)

- `schlange sim run scenarios/<name>.yaml [--json]`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Run every scenario.** For each file in `scenarios/*.yaml`: `target/release/schlange sim run scenarios/<file>`. Exit `0`; `result: PASS`; every line under `expectations` reads `PASS`; `I0` … `I4` read `checked`; `I5` reads `checked` with a note on dropped and duplicated deliveries (only `p1-flaky.yaml` reports `I5 not_checked`, by design).
- **JSON.** `--json` gives `passed`, `log`, `outcomes`, `merged_order`, `expectations`, `invariants` (status, checks, violations, notes), `end_state` and `disturbed`.
- **Tests.** `cargo test -p schlange-forge` (scenarios with golden logs, DSL tests, regressions, properties); `SCHLANGE_PBT_CASES=2000 cargo test -p schlange-forge --test properties` for a long property run.

## Gotchas

- I5 compares end states only where the outcome does not depend on timing. Queue order follows the moment the queue learns that a pull request is ready; with flaky CI, conflicts between pull requests or split limits, a disturbed delivery can legitimately change which pull requests merge. Such scenarios set `faults: none` and say why.
- Webhooks in the simulation are hints: the runner reads the forge after each delivery. A real adapter has to do the same.
- `max_time` (default 7 days) bounds a run; a run that does not settle reports an I2 violation.
