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
- `newest-run`: built. One run counts per kind (check run or commit status), name and GitHub App on the head commit; check runs and statuses are ordered apart (their ids are never compared). When one check spans several workflow files or events, the newest run of each counts and a run that has not passed is kept even if another one passed later. The docs read checks that way only when the base branch requires them; the engine does not know which checks are required and reads every check that way (fail closed). The GitHub adapter does not fill in workflow and event yet, so there every check run of one name and app falls back to the newest run.
- `app-prefix`: built. `@<app-slug>/<name>` matches only that app's runs on every `check-*` attribute; the bare name matches any app; `config validate` rejects a value starting with `@` of another shape.
- `stale`: built. Only runs on the current head count (`PullRequest::current_checks`).

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
- The GitHub adapter's missing workflow/event identity can let a later same-name success hide a failure from another workflow. `sandbox/scenarios/s14-check-workflow-collision.json` is designed to observe this with real Actions jobs; `s15-review-manual-retry.json` examines a custom KI-Review failure followed by a synthetic manual success. Neither scenario has a reviewed sandbox baseline yet. The custom check's validated Actions run link is supplied by the app and is not proof of job provenance.
- A check-suite read failure now holds the queue instead of acting on the last checks (`check-identity` in `github-integration.md`); workflow/event association within a suite is still not implemented.
