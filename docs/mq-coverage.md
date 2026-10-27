# Coverage of the problem catalogue (MQ-01 … MQ-53)

Which case from [research/merge-queue-probleme.md](research/merge-queue-probleme.md) has which
test, as of phase 1c (simulated forge with git in memory, `localgit` with real git
repositories on disk, and the GitHub adapter against the fake GitHub over HTTP). "Covered" means a scenario or test asserts the expected behaviour;
"partial" means only part of the expectation is tested; "open" means no test yet. "Real git"
means the test runs on `localgit`: real three-way merges, `git update-ref` compare-and-swap,
every check a process in a checkout, I0–I4 judged on the repository on disk.
Scenario commands: `target/release/schlange sim run [--forge localgit] scenarios/<file>`
(`--forge localgit` needs a build with the `localgit` feature). Tests:
`cargo test -p <crate> --test <file> <name>`. "Fake GitHub" means `schlange serve` against
`crates/schlange-github-fake` over HTTP: REST calls, signed webhooks, the fake's git state
checked for I0, double landings and I3.

| ID | Status | Where |
|---|---|---|
| MQ-01 semantic conflict | covered | `p1-semantic-conflict.yaml`; `p1-semantic-conflict-compile.yaml` (a real compile script runs in a checkout: a rename plus a call of the old name, red only together; on both forges); property tests (`SEM_A`/`SEM_B`, also on real git) |
| MQ-02 landed ≠ tested | covered for `squash` (sim and real git) | engine tree check: `schlange-core --test queue landed_tree_differs_from_tested_tree_halts_the_queue`; I0 per commit in every scenario and property case; real git: `schlange-forge --test localgit` (six scenarios, matrix) and `--test localgit_properties`; the checker reads every landed tree from the repository |
| MQ-03 squash of a batch on a moving parent | covered for `squash` × batch 1–5 (real git) | `schlange-forge --test localgit matrix_squash_by_batch_size_one_to_five_keeps_i0_on_real_git`: every pull request edits one file on different lines, plus a culprit and a line conflict; I0 per commit, main's content line by line, a one-parent history, `git fsck`. `localgit_properties`: overlapping lines and base pushes. Methods `merge`, `rebase`, `fast-forward` and `merge-batch`: open, not built in the core |
| MQ-04, MQ-05 fast-forward | open | method not built |
| MQ-06 base moves outside the queue | covered | `p1-base-moved.yaml` (also on real git); `landing_refused_because_the_base_moved_resets_the_queue`; random base pushes in properties (also on real git) |
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
| MQ-18 events that trigger queue CI | partial (sim, fake GitHub) | queue draft per car, `runs_on: queue` in `p1-arztagent-two-step.yaml`; fake GitHub: the queue creates the branch, pushes merge commits and opens a draft pull request, which emit `push` and `pull_request.opened` (`e2e arztagent_one_pull_request_through_the_draft_to_a_squash_landing`); which workflows GitHub really starts: open (needs a sandbox) |
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
| MQ-29 webhooks duplicated, reordered, lost | covered (sim, fake GitHub) | I5 in every scenario with faults; `delivery_does_not_change_the_end_state`; fake GitHub: `e2e duplicated_and_reordered_webhooks_change_nothing`, delivery ids deduplicated (`client the_webhook_endpoint_checks_signatures_and_drops_repeated_deliveries`) |
| MQ-30 without webhooks | covered (sim, fake GitHub) | `every_scenario_also_holds_without_webhooks_but_with_reconciliation`; `lost_webhooks_are_repaired_by_reconciliation`; fake GitHub: `e2e lost_webhooks_are_recovered_by_reconciliation` (every webhook lost) |
| MQ-31 crash mid-merge | partial (fake GitHub) | no journal yet; `e2e a_restart_recovers_from_github_and_sweeps_orphans`: a restart while a car runs recovers by reconciliation, sweeps the orphaned branch and draft, lands |
| MQ-32 two instances | open | |
| MQ-33 eventual consistency | partial | red/timeout verdicts confirmed by a fresh read; fake GitHub: 404/5xx storms on reads never dequeue (`e2e storms_of_404_and_5xx_never_dequeue`); a 404 for a branch counts only after a canary read; delayed (stale) reads not injected |
| MQ-34 rate limits | covered (fake GitHub) | `e2e rate_limits_are_waited_out_and_landing_still_happens` (429 with `Retry-After`, secondary limit on the merge); `client rate_limits_block_everyone_and_cosmetics_are_deferred`; unit tests of the budget (landing before cosmetics) |
| MQ-35 5xx after a successful write | covered (fake GitHub) | `e2e a_5xx_after_the_merge_happened_does_not_land_twice` (each pull request squashed once, no halt); `e2e a_lying_5xx_on_draft_creation_and_queue_branch_merges_is_absorbed`; `client reads_retry_5xx_and_404_but_writes_report_an_unknown_outcome` |
| MQ-36 leftover branches | covered | I3 (strict pattern, after every step and at quiescence); fake GitHub: I3 at the end of every e2e test, orphan sweep after a restart |
| MQ-37 a check never reports | covered | `p1-checks-timeout.yaml` (with a fixed timeout) |
| MQ-38 `checks_timeout: auto` | open | not built |
| MQ-39 cancelled runs | partial | code `CHECKS_INTERRUPTED` in the verdict; no scenario |
| MQ-40 forge recomputes the merge ref | covered (sim, real git) | cars are frozen commits on queue branches; on `localgit` they are real merge commits on real refs |
| MQ-41 CI side effects on queue branches | open | |
| MQ-42 merge checks never run on the queue PR | covered | `p1-checks-timeout.yaml`; reason lists checks that never reported |
| MQ-43 queue hangs without a reason | partial | I2 at quiescence in every run; `pr explain` missing |
| MQ-44 head-of-line blocking | open | not measured |
| MQ-45 dequeue reasons | partial | codes per path in `queue-dequeue-reasons.md`, golden event logs; some codes assumed |
| MQ-46 … MQ-53 | open | except MQ-47 (= MQ-06, covered) and MQ-53 (= MQ-08, covered) |
