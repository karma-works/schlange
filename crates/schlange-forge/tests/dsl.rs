//! Scenarios written with the Rust DSL: the paths that are awkward in YAML or need exact
//! assertions on the event log.

use schlange_core::model::Conclusion;
use schlange_forge::dsl::{Sim, ci};
use schlange_forge::sim::Action;

const TWO_STEP: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - base = main\n      - -draft\n      - -label = halt\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

fn two_step_batch(extra_rule: &str, max_parallel: u32) -> String {
    format!(
        "merge_queue:\n  max_parallel_checks: {max_parallel}\nqueue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: 4\n    batch_max_wait_time: 1 min\n{extra_rule}    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n"
    )
}

fn checks() -> (schlange_forge::ci::CiScript, schlange_forge::ci::CiScript) {
    (
        ci("quick", "1m").pr_only(),
        ci("full", "10m").queue_only().fails_on("BUG"),
    )
}

fn log_has(report: &schlange_forge::RunReport, needle: &str) -> bool {
    report.log.iter().any(|l| l.text.contains(needle))
}

#[test]
fn merged_by_hand_while_queued() {
    let (q, f) = checks();
    let r = Sim::new("manual", TWO_STEP)
        .check(q)
        .check(f)
        .pr(1, &[("a", "1\n")])
        .pr(2, &[("b", "2\n")])
        .open("0s", 1)
        .open("0s", 2)
        .at("3m", Action::MergeManually(1))
        .run();
    r.assert_invariants();
    assert_eq!(r.outcome(1), "dequeued PR_MANUALLY_MERGED");
    assert_eq!(r.outcome(2), "merged");
    assert!(log_has(&r, "MERGE_QUEUE_RESET"), "{}", r.to_text());
}

#[test]
fn closed_and_drafted_pull_requests_leave() {
    let (q, f) = checks();
    let r = Sim::new("close", TWO_STEP)
        .check(q)
        .check(f)
        .pr(1, &[("a", "1\n")])
        .pr(2, &[("b", "2\n")])
        .open("0s", 1)
        .open("0s", 2)
        .at("3m", Action::Close { number: 1 })
        .at("3m", Action::Draft { number: 2 })
        .run();
    r.assert_invariants();
    assert_eq!(r.outcome(1), "dequeued PR_DEQUEUED");
    assert_eq!(r.outcome(2), "dequeued PR_DEQUEUED");
    assert_eq!(r.merged_order, Vec::<u64>::new());
}

#[test]
fn bisection_limit_zero_dequeues_the_whole_batch() {
    let (q, f) = checks();
    let r = Sim::new(
        "limit0",
        &two_step_batch("    batch_max_failure_resolution_attempts: 0\n", 2),
    )
    .check(q)
    .check(f)
    .pr(1, &[("a", "1\n")])
    .pr(2, &[("b", "BUG\n")])
    .pr(3, &[("c", "3\n")])
    .open("0s", 1)
    .open("0s", 2)
    .open("0s", 3)
    .run();
    r.assert_invariants();
    for n in 1..=3 {
        assert_eq!(
            r.outcome(n),
            "dequeued BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS"
        );
    }
}

#[test]
fn bisection_needs_two_rounds() {
    // Batch of 4 with 2 checks: parts [1,2] [3]; #2 is the culprit. Round 1: [1,2] fails, so
    // [1,2] is split again (round 2): test [1] passes, so #2 is the culprit; #1 lands.
    let (q, f) = checks();
    let r = Sim::new("rounds", &two_step_batch("", 2))
        .check(q)
        .check(f)
        .pr(1, &[("a", "1\n")])
        .pr(2, &[("b", "BUG\n")])
        .pr(3, &[("c", "3\n")])
        .pr(4, &[("d", "4\n")])
        .open("0s", 1)
        .open("0s", 2)
        .open("0s", 3)
        .open("0s", 4)
        .run();
    r.assert_invariants();
    assert_eq!(r.outcome(2), "dequeued CHECKS_FAILED");
    assert_eq!(r.merged_order, vec![1, 3, 4], "{}", r.to_text());
}

#[test]
fn bisection_with_limit_one_and_one_check_drops_the_rest() {
    // One parallel check, limit 1: the last round tests only the first part. It passes, so it
    // lands and every pull request after it leaves untested.
    let (q, f) = checks();
    let r = Sim::new(
        "eliminate",
        &two_step_batch("    batch_max_failure_resolution_attempts: 1\n", 1),
    )
    .check(q)
    .check(f)
    .pr(1, &[("a", "1\n")])
    .pr(2, &[("b", "2\n")])
    .pr(3, &[("c", "BUG\n")])
    .pr(4, &[("d", "4\n")])
    .open("0s", 1)
    .open("0s", 2)
    .open("0s", 3)
    .open("0s", 4)
    // Which pull requests are dropped depends on queue order, and queue order on when the
    // queue learns that each one is ready: a disturbed delivery may order them differently,
    // so I5 (same end state) is not a property of this case.
    .faults(None)
    .run();
    r.assert_invariants();
    assert_eq!(r.outcome(1), "merged", "{}", r.to_text());
    assert_eq!(r.outcome(2), "merged");
    assert_eq!(r.outcome(3), "dequeued DROPPED_BY_BISECTION_ELIMINATION");
    assert_eq!(r.outcome(4), "dequeued DROPPED_BY_BISECTION_ELIMINATION");
}

#[test]
fn dynamic_batch_size_spreads_over_slots() {
    // min 1, max 2, five checks: three pull requests become three batches of one.
    let cfg = "queue_rules:\n  - name: default\n    batch_size: {min: 1, max: 2}\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let (q, f) = checks();
    let r = Sim::new("dynamic", cfg)
        .check(q)
        .check(f)
        .pr(1, &[("a", "1\n")])
        .pr(2, &[("b", "2\n")])
        .pr(3, &[("c", "3\n")])
        .open("0s", 1)
        .open("0s", 2)
        .open("0s", 3)
        .run();
    r.assert_invariants();
    assert!(log_has(&r, "(checks #3)"), "{}", r.to_text());
    assert_eq!(r.merged_order, vec![1, 2, 3]);
}

#[test]
fn red_quick_check_never_queues_and_check_rerun_requeues() {
    let (q, f) = checks();
    let r = Sim::new("never", TWO_STEP)
        .check(q)
        .check(f)
        .pr(1, &[("a", "1\n")])
        .pr_check(1, "quick", Some(Conclusion::Failure))
        .open("0s", 1)
        .run();
    r.assert_invariants();
    assert_eq!(r.outcome(1), "open");
}

#[test]
fn failed_pull_request_is_not_requeued_until_it_changes() {
    // #1 fails the full suite; the queue action does not queue the same head again (no loop),
    // a fix pushed later is queued and lands.
    let (q, f) = checks();
    let r = Sim::new("requeue", TWO_STEP)
        .check(q)
        .check(f)
        .pr(1, &[("a", "BUG\n")])
        .open("0s", 1)
        .push("1h", 1, &[("a", "fixed\n")])
        .run();
    r.assert_invariants();
    assert_eq!(r.outcome(1), "merged", "{}", r.to_text());
    let enqueues = r
        .log
        .iter()
        .filter(|l| l.text.starts_with("enqueue #1"))
        .count();
    assert_eq!(enqueues, 2);
}

#[test]
fn every_scenario_also_holds_without_webhooks_but_with_reconciliation() {
    // All webhooks dropped (MQ-30): the periodic reconciliation still makes progress.
    let (q, f) = checks();
    let faults = schlange_forge::sim::Faults {
        seed: 9,
        drop: 1.0,
        duplicate: 0.0,
        delay: 0.0,
        max_delay: 0,
        reconcile_every: 60,
    };
    let r = Sim::new("no-webhooks", TWO_STEP)
        .check(q)
        .check(f)
        .pr(1, &[("a", "1\n")])
        .pr(2, &[("b", "2\n")])
        .open("0s", 1)
        .open("0s", 2)
        .faults(Some(faults))
        .run();
    r.assert_invariants();
    assert_eq!(r.invariant("I5").status, "checked");
    let d = r.disturbed.as_ref().expect("disturbed run");
    assert!(d.dropped > 0);
    assert_eq!(d.end_state.merged.len(), 2);
}
