---
id: conditions-checks
title: "Check attributes and check semantics"
priority: P1
arztagent: true
status: partial
inventory:
  - "attr:check"
  - "attr:check-success"
  - "attr:check-failure"
  - "attr:check-neutral"
  - "attr:check-timed-out"
  - "attr:check-skipped"
  - "attr:check-cancelled"
  - "attr:check-pending"
  - "attr:check-stale"
problems:
  - "MQ-14"
  - "MQ-15"
  - "MQ-16"
  - "MQ-17"
source: "https://docs.mergify.com/configuration/conditions"
---

# Check attributes and check semantics

Check attributes read, per check name, the current run on the head commit. Success means `success`
only; `neutral` and `skipped` are not a success. `check-failure` also covers cancelled, timed-out
and action-required runs. With several runs the newest counts; an app prefix `@app/name` pins the
source.

## Sub-features

- `attributes`: built. All nine attributes are evaluated (`check`, `check-success`, `check-failure`, `check-neutral`, `check-skipped`, `check-cancelled`, `check-timed-out`, `check-stale`, `check-pending`; `crates/schlange-core/src/eval.rs`).
- `success`: only `success` (built in the simulation).
- `failure`: failure, cancelled, timed_out, action_required (built in the simulation).
- `newest-run`: the run created last per name wins (built in the simulation, without check suites).
- `app-prefix`: `@slug/name` (missing).
- `stale`: results for old SHAs do not count (missing).

## How to get to it (user POV)

- Checks on the pull request; `schlange pr explain` shows the run read per condition (planned).

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Tests.** `cargo test -p schlange-core` table test of all conclusions × attributes.
- **Scenario.** Run `target/release/schlange sim run scenarios/p0-queue-conditions.yaml`. Exit `0`; `#7` (`ci-ok` failure) and `#10` (cancelled check) are not queued; `#1` enters only after its last required check completes.

## Gotchas

- Required checks from rulesets are read per workflow file and event, not only per newest suite.
- A skipped GitHub Actions job reports no check; the queue then waits (Arztagent `ai-review.yml`).
