---
id: queue-bisection
title: "Bisection of red batches"
priority: P1
arztagent: true
status: partial
inventory:
  - "config:queue_rules.batch_max_failure_resolution_attempts"
  - "default:queue_rules.batch_max_failure_resolution_attempts"
problems:
  - "MQ-21"
  - "MQ-22"
source: "https://docs.mergify.com/merge-queue/batches"
---

# Bisection of red batches

When a batch fails, the queue splits it to find the guilty pull request. The others are merged,
the guilty one is dequeued. Without a setting there is no limit on attempts (Arztagent sets none).

## Sub-features

- `split`: built (`crates/schlange-core/src/bisect.rs`). The newest pull request of the red batch is set aside (the batch already tested it); the others are grouped in order into `max(max_parallel_checks, 2)` groups of `ceil((n − 1) / groups)`, and test car i stacks groups 1 to i on the base. Unit test `splits_as_documented` reproduces the documented 13/6/5 examples.
- `culprit`: built. All tests green → the last pull request is the culprit; first red test f → parts before f land (from test f − 1's car), parts after f and the last go back to the queue, part f is split again; a red part of one is the culprit. The culprit leaves with the code of the test that isolated it; the last pull request, when every test passed, with the code of the failed batch.
- `land-turns-red`: built. A test chosen to land the passing parts that turns red on the fresh read before landing counts as a failure of the parts it holds: they become the next round (or the culprit, or leave at the limit), the part waiting to be split goes back to the queue. Critic reproduction: `scenarios/regressions/critic-stuck-bisection.yaml`, `cargo test -p schlange-forge --test regressions bisection_with_a_merge_condition_label_never_wedges_or_blames_an_innocent`.
- `attributes`: built. A pull request whose own attributes fail the merge conditions (a label, the title) is no CI culprit: it leaves with `PR_DEQUEUED` (code assumed) and is queued again only when its attributes change; the split it was in is abandoned and its pull requests go back to the queue.
- `innocence`: property test `innocent_pull_requests_are_never_blamed`: with one deterministic culprit no other pull request is dequeued for `CHECKS_FAILED`, `CHECKS_TIMEOUT` or (without a limit) `DROPPED_BY_BISECTION_ELIMINATION`, also with webhook faults, forge faults and crashes.
- `limit`: built. Documented rule: a part with several pull requests that fails in the last round leaves (`BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS`), parts before it that passed merge, parts after it go back with a fresh count. `batch_max_failure_resolution_attempts: 0` dequeues the whole batch with `BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS`; a red multi-PR part in the last round leaves with that code; with one parallel check the last round tests only the first part and drops the rest with `DROPPED_BY_BISECTION_ELIMINATION`.
- `termination`: every round splits a strict prefix, so it ends without a limit (MQ-21); the property tests run random batches with and without limits.
- `interaction`: two pull requests red only together → the later one leaves (`p1-semantic-conflict`, property tests with `SEM_A`/`SEM_B`).
- `split-retries`: built. With `max_checks_retries`, a red split test is retried on a fresh draft before it counts as red, like any car (docs: a split's `previous_check_retries`). A split test whose build is in doubt is rebuilt with the same attempt. Test: `cargo test -p schlange-forge --test critic_1abc bisection_split_tests_are_retried`.
- `rerun-failed-only`, `skip_intermediate_results`: missing. Retries (`max_checks_retries`) happen before a red batch is split; see `queue-checks-timeout`.

## How to get to it (user POV)

- The status comment of the guilty pull request names the reason; the others are merged.

## Driving it with schlange

Preconditions:

- Release binary built (`.agents/skills/verify-schlange/scripts/verify.sh launch`) and `schlange doctor` without FAIL.

- **One culprit in a batch of four.** Run `target/release/schlange sim run scenarios/p1-bisection.yaml`. `build car … (checks #1 #2 #3 #4)` fails at +0:11; split tests `(checks #1 #2)` and `(checks #1 #2 #3)`; at +0:21 #3 leaves with `CHECKS_FAILED`, #4 goes back (`BATCH_AHEAD_FAILED`, code assumed), car 6 lands #1 and #2; #4 lands at +0:31. Exit 0.
- **Arztagent.** Run `target/release/schlange sim run scenarios/p1-arztagent-two-step.yaml`: the batch of three fails the full `ci-ok`, the split finds #2.
- **Limits and two rounds.** `cargo test -p schlange-forge --test dsl bisection` (limit 0, limit 1 with one check, two rounds).

## Gotchas

- Flaky tests blow up batches (MQ-22): a flake in a batch makes the last pull request the "culprit" when every split passes (`scenarios/p1-flaky.yaml` shows a flake dequeuing an innocent pull request).
- With fewer slots than tests (one parallel check), a green test would hold the only slot the next test needs. Schlange then lands the green prefix and splits the rest again in the same round. The documentation describes the one-check case only for the last round; this is our choice for the other rounds (found by the property test, regression `bisection_with_one_check_and_a_held_pull_request_settles`).
- The code for pull requests sent back to the queue after a split (`BATCH_AHEAD_FAILED`) is assumed; the documentation only says they "go back to the queue".
- A pull request that leaves during a split (label, push) aborts the split; the rest wait for a new batch (`BATCH_REBUILT`).
