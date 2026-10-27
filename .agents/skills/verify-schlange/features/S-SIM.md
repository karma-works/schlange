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
source: "KONZEPT.md, section 8"
---

# Scenarios in the simulated forge

YAML scenarios run in the simulated forge with a virtual clock; the output is an event log and an
invariant report.

## Sub-features

- `scenario-format`: built.
- `virtual-clock`: built.
- `ci-script`: built (duration and result per check).
- `invariants`: report built, checks I0–I5 missing.
- `fault-injection`: missing.
- `queue`: placeholder.

## How to get to it (user POV)

- `schlange sim run scenarios/p0-queue-conditions.yaml`

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Run the scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; `result: PASS`; every line under `expectations` reads `PASS`.
- **Event log.** The log shows virtual times (`+0:03:00`), check runs per pull request and decisions `enqueue #1 into `default``, `dequeue #5 …` after `#5 labeled halt`.
- **Invariant report.** Lines `I0` … `I5` read `not_checked` with the reason; placeholders are listed. That is the honest state of phase 0.
- **Proof.** `--json` gives `passed: true`, `log`, `expectations`, `invariants`.

## Gotchas

- Without fault injection a green scenario proves only the normal case.
