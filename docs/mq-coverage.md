# Coverage of the problem catalogue (MQ-01 … MQ-53)

Which case from [research/merge-queue-probleme.md](research/merge-queue-probleme.md) has which
test, as of phase 1a (simulated forge). "Covered" means a scenario or test asserts the expected
behaviour; "partial" means only part of the expectation is tested; "open" means no test yet.
Scenario commands: `target/release/schlange sim run scenarios/<file>`. Tests:
`cargo test -p <crate> --test <file> <name>`.

| ID | Status | Where |
|---|---|---|
| MQ-01 semantic conflict | covered | `p1-semantic-conflict.yaml`; property tests (`SEM_A`/`SEM_B`) |
| MQ-02 landed ≠ tested | covered (sim) | engine tree check: `schlange-core --test queue landed_tree_differs_from_tested_tree_halts_the_queue`; I0 per commit in every scenario and property case. Real git: open (`localgit`) |
| MQ-03 squash of a batch on a moving parent | partial | I0 per commit against prefix trees (`p1-batch-five.yaml`, properties); file-level git only |
| MQ-04, MQ-05 fast-forward | open | method not built |
| MQ-06 base moves outside the queue | covered | `p1-base-moved.yaml`; `landing_refused_because_the_base_moved_resets_the_queue`; random base pushes in properties |
| MQ-07 push while queued, lost webhook | covered | `p1-push-to-queued-pr.yaml`; `landing_refused_because_the_head_moved_dequeues`; properties with dropped webhooks |
| MQ-08 push to a queue branch | covered | `p1-queue-branch-push.yaml`; `a_foreign_push_to_the_queue_branch_dequeues`; properties |
| MQ-09 closed, draft, merged by hand | partial | `closed_and_drafted_pull_requests_leave`, `merged_by_hand_while_queued`; reopen not tested |
| MQ-10 base branch changed | covered (engine) | `a_retargeted_pull_request_leaves_its_queue`; no forge action yet |
| MQ-11 halt / condition false during CI | covered | `p1-label-halt.yaml`; `a_label_seen_only_in_the_fresh_read_prevents_landing`; I1 |
| MQ-12 flapping conditions | partial | idempotence tests (`the_same_snapshot_twice_changes_nothing`, phase-0 test); duplicated webhooks in properties |
| MQ-13 changing branch protection | open | |
| MQ-14 stale check result | partial | newest run per head (phase-0 unit test); flakes keyed per tree |
| MQ-15 check from another app | open | app not compared |
| MQ-16 neutral/skipped as success | covered | phase-0 table test `check_semantics_table` |
| MQ-17 check rerun | partial | `a_stale_green_is_rechecked_before_landing` |
| MQ-18 events that trigger queue CI | partial (sim) | queue draft per car, `runs_on: queue` in `p1-arztagent-two-step.yaml`; real events: open |
| MQ-19 config change in the PR | open | |
| MQ-20 commit message | open | |
| MQ-21 bisection | covered | `p1-bisection.yaml`; `bisection_*` DSL tests; properties with and without limits |
| MQ-22 flaky test in a batch | covered | `p1-flaky.yaml`; flakes in properties (no red tree lands: I0/I1) |
| MQ-23 cascade | covered | `p1-cascade.yaml`; I0 in properties |
| MQ-24 priority | open | priorities not built |
| MQ-25 slot leaks | covered | I3 and I4 after every step in every scenario and property case |
| MQ-26 batch timer, minimum | covered | `p1-batch-five.yaml`; `dynamic_batch_size_spreads_over_slots`; I2 at quiescence |
| MQ-27 conflict with a PR ahead | covered | `p1-conflict-ahead.yaml` |
| MQ-28 CI cost | open | not measured |
| MQ-29 webhooks duplicated, reordered, lost | covered | I5 in every scenario with faults; `delivery_does_not_change_the_end_state` |
| MQ-30 without webhooks | covered | `every_scenario_also_holds_without_webhooks_but_with_reconciliation`; `lost_webhooks_are_repaired_by_reconciliation` |
| MQ-31 crash mid-merge | open | no journal yet |
| MQ-32 two instances | open | |
| MQ-33 eventual consistency | partial | red/timeout verdicts confirmed by a fresh read; delayed reads not injected |
| MQ-34 rate limits | open | |
| MQ-35 5xx after a successful write | open | |
| MQ-36 leftover branches | covered | I3 (strict pattern, after every step and at quiescence) |
| MQ-37 a check never reports | covered | `p1-checks-timeout.yaml` (with a fixed timeout) |
| MQ-38 `checks_timeout: auto` | open | not built |
| MQ-39 cancelled runs | partial | code `CHECKS_INTERRUPTED` in the verdict; no scenario |
| MQ-40 forge recomputes the merge ref | covered (sim) | cars are frozen commits on queue branches |
| MQ-41 CI side effects on queue branches | open | |
| MQ-42 merge checks never run on the queue PR | covered | `p1-checks-timeout.yaml`; reason lists checks that never reported |
| MQ-43 queue hangs without a reason | partial | I2 at quiescence in every run; `pr explain` missing |
| MQ-44 head-of-line blocking | open | not measured |
| MQ-45 dequeue reasons | partial | codes per path in `queue-dequeue-reasons.md`, golden event logs; some codes assumed |
| MQ-46 … MQ-53 | open | except MQ-47 (= MQ-06, covered) and MQ-53 (= MQ-08, covered) |
