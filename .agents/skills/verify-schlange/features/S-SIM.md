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

## Driving it

- `schlange sim run <file> [--json]`; exit 0 when every expectation holds.

## Gotchas

- Without fault injection a green scenario proves only the normal case.
