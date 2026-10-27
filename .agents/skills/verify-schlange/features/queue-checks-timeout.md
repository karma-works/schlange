---
id: queue-checks-timeout
title: "Check timeout and retries (defaults)"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.checks_timeout"
  - "default:queue_rules.checks_timeout"
  - "config:queue_rules.max_checks_retries"
  - "default:queue_rules.max_checks_retries"
problems:
  - "MQ-37"
  - "MQ-38"
  - "MQ-39"
  - "MQ-42"
  - "MQ-33"
source: "https://docs.mergify.com/merge-queue/rules"
---

# Check timeout and retries (defaults)

When a required check does not report in time, the run ends with a timeout. Arztagent sets nothing
and inherits the default `auto`: a value derived from the history of run times. Red checks are not
retried by default.

## Sub-features

- `fixed`: built. A duration (at least 60 s, smaller values are clamped) counted from the car's start; the car fails with `CHECKS_TIMEOUT`; the reason lists the checks still running and the referenced checks that never reported (MQ-37). A batch that times out is split like a red batch.
- `null`: built (no timeout).
- `auto`: built. No timeout until enough qualifying runs are in the history, then a timeout calibrated from the history of successful first-attempt runtimes. Mergify's safety margin is undocumented, so the exact value can differ.
- `confirm`: built. A red or timed-out verdict is acted on only after a fresh read of the draft pull request; a lost webhook must not dequeue anything (MQ-33). Found by the property test.
- `retries`: built. With `max_checks_retries > 0` a red car is retried by recreating its draft pull request, up to the limit, before the failure is handled; in-place checks are then off.
- `cancelled`: built. Cancelled runs without a hard failure give `CHECKS_INTERRUPTED`; a failure together with a cancellation gives `CHECKS_FAILED`.

## How to get to it (user POV)

- The status comment lists the missing checks.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **Hanging merge check.** Run `target/release/schlange sim run scenarios/p1-checks-timeout.yaml`. At +0:11 (10 minutes after the car started) `dequeue #1 … CHECKS_TIMEOUT: checks did not pass within 600s; still running: [full]; never reported: []`; #2 is rebuilt and lands. Exit 0.
- **Lost webhook does not time out a green car.** `cargo test -p schlange-forge --test regressions lost_webhooks_are_repaired_by_reconciliation`.

## Gotchas

- Rebuild `auto` only from its public description; compare the values in shadow mode (MQ-38).
- With `auto` and a check that never reports, the queue waits forever as long as the history is too short (as Mergify does). The property tests therefore generate hanging checks only with a fixed timeout; with `auto` liveness (I2) cannot hold for a hanging CI.
- A car fails without waiting for the timeout when the merge conditions are false and either no run is pending or a referenced check already failed. A referenced check that has not started yet while others are done counts as "nothing pending" only if no run at all is pending; a CI that creates its runs late could be judged too early (not seen in the simulation, where all runs start with the draft).
