//! Cases the property tests found, kept as fixed regressions.

use schlange_forge::dsl::{Sim, ci};
use schlange_forge::git::GitStore;
use schlange_forge::runner::{Simulation, clock};
use schlange_forge::sim::{Action, Faults};

const ONE_CHECK: &str = "merge_queue:\n  max_parallel_checks: 1\nqueue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: 1\n    checks_timeout: 20 min\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

#[test]
fn lost_webhooks_are_repaired_by_reconciliation() {
    let faults = Faults {
        seed: 4166010031714336513,
        drop: 0.3800473649965589,
        duplicate: 0.0,
        delay: 0.0,
        max_delay: 0,
        reconcile_every: 30,
    };
    let s = Sim::new("lost", ONE_CHECK)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only())
        .pr(1, &[("pr1.rs", "#1\n")])
        .open("0s", 1)
        .faults(Some(faults.clone()))
        .build();
    let mut sim = Simulation::new(&s, faults);
    sim.run(&s);
    let log: String = sim
        .log
        .iter()
        .map(|l| format!("{} {} {}\n", l.t, l.kind, l.text))
        .collect();
    assert_eq!(sim.outcome(1), "merged", "{log}");
}

// ---------------------------------------------------------------------------------------------
// Reproductions of the critique of phase 1a (`.critic/phase-1a.md`).

fn builds(log: &[schlange_forge::scenario::LogEntry]) -> Vec<(u64, String)> {
    log.iter()
        .filter(|l| l.text.starts_with("build car"))
        .map(|l| (l.t, l.text.clone()))
        .collect()
}

/// The pull requests a `build car` line checks: `(checks #1 #2)`.
fn checked(line: &str) -> Vec<u64> {
    line.rsplit_once("(checks ")
        .map(|(_, r)| r.trim_end_matches(')'))
        .unwrap_or("")
        .split_whitespace()
        .filter_map(|s| s.trim_start_matches('#').parse().ok())
        .collect()
}

fn violations<G: GitStore>(sim: &Simulation<G>) -> Vec<String> {
    sim.checker
        .results
        .iter()
        .flat_map(|(id, r)| r.violations.iter().map(move |v| format!("{id}: {v}")))
        .collect()
}

fn log_text<G: GitStore>(sim: &Simulation<G>) -> String {
    sim.log
        .iter()
        .map(|l| format!("{} {:<8} {}\n", clock(l.t), l.kind, l.text))
        .collect()
}

const DYNAMIC: &str = "merge_queue:\n  max_parallel_checks: 5\nqueue_rules:\n  - name: default\n    batch_size: {min: 1, max: 2}\n    queue_conditions:\n      - label = go\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

/// Queues `n` pull requests that the engine sees in one read (every webhook dropped, one
/// reconciliation pass) and returns the batches of the first round of cars.
fn dynamic_first_round(n: u64) -> Vec<Vec<u64>> {
    let mut sim = Sim::new("dynamic", DYNAMIC).check(ci("full", "10m").queue_only());
    for k in 1..=n {
        let p = format!("f{k}");
        sim = sim
            .pr(k, &[(p.as_str(), "x\n")])
            .open("0s", k)
            .label("10s", k, "go");
    }
    let s = sim.build();
    let f = Faults {
        seed: 1,
        drop: 1.0,
        duplicate: 0.0,
        delay: 0.0,
        max_delay: 0,
        reconcile_every: 30,
    };
    let mut run = Simulation::new(&s, f);
    run.run(&s);
    assert!(violations(&run).is_empty(), "{}", log_text(&run));
    let all = builds(&run.log);
    let first = all.first().map(|b| b.0).expect("a car");
    all.iter()
        .filter(|(t, _)| *t == first)
        .map(|(_, l)| checked(l))
        .collect()
}

/// Docs (merge-queue/batches, dynamic batch sizing): with five parallel checks and a range of
/// 1 to 2, three queued pull requests become three batches of one, ten become five batches of
/// two. The size divides the backlog by the free slots, not by all slots.
#[test]
fn dynamic_batch_size_matches_the_documented_example() {
    assert_eq!(dynamic_first_round(3), vec![vec![1], vec![2], vec![3]]);
    assert_eq!(
        dynamic_first_round(10),
        vec![vec![1, 2], vec![3, 4], vec![5, 6], vec![7, 8], vec![9, 10]]
    );
}

/// The batch wait timer starts with the first queued pull request: batch of 3, wait 5 min, two
/// pull requests queued at 0 and 4 min leave together at 5 min.
#[test]
fn batch_wait_timer_starts_with_the_first_pull_request() {
    let cfg = "queue_rules:\n  - name: default\n    batch_size: 3\n    batch_max_wait_time: 5 min\n    queue_conditions:\n      - label = go\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let r = Sim::new("timer", cfg)
        .check(ci("full", "10m").queue_only())
        .faults(None)
        .pr(1, &[("a", "1\n")])
        .pr(2, &[("b", "2\n")])
        .open("0s", 1)
        .open("0s", 2)
        .label("0s", 1, "go")
        .label("4m", 2, "go")
        .run();
    r.assert_invariants();
    let b = builds(&r.log);
    assert_eq!(b[0].0, 300, "{}", r.log_text());
    assert_eq!(checked(&b[0].1), vec![1, 2]);
    assert_eq!(r.merged_order, vec![1, 2]);
}

/// MQ-42: the check named in `merge_conditions` never runs on the queue draft; another check
/// does and passes. Expected: no early CHECKS_FAILED with an empty list, but CHECKS_TIMEOUT
/// after the timeout, naming the check that never reported.
#[test]
fn a_merge_check_that_never_reports_on_the_draft_times_out_naming_it() {
    let cfg = "queue_rules:\n  - name: default\n    checks_timeout: 30 min\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let r = Sim::new("mq42", cfg)
        .check(ci("quick", "1m").pr_only())
        .check(ci("lint", "2m").queue_only())
        .faults(None)
        .pr(1, &[("a", "1\n")])
        .open("0s", 1)
        .run();
    r.assert_invariants();
    assert_eq!(r.outcome(1), "dequeued CHECKS_TIMEOUT", "{}", r.log_text());
    let line = r
        .log
        .iter()
        .find(|l| l.text.contains("CHECKS_TIMEOUT"))
        .expect("timeout line");
    assert!(
        line.text.contains("never reported: [full]"),
        "{}",
        line.text
    );
    assert_eq!(
        line.t,
        60 + 30 * 60,
        "the timeout runs from the car's start"
    );
}

const STUCK: &str = "merge_queue:\n  max_parallel_checks: 3\nqueue_rules:\n  - name: default\n    batch_size: 4\n    batch_max_wait_time: 1 min\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n      - -label = wip\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

/// The critic's seed sweep: delayed webhooks, a merge-condition-only label added to #1 around
/// the time the first split test goes green, culprit #4. Before the fix 138 of 1600 runs
/// wedged or dequeued innocent #3 while #4 stayed. Now every run settles, #4 is the only CI
/// culprit, and #1 leaves (if at all) with its own reason.
#[test]
fn bisection_with_a_merge_condition_label_never_wedges_or_blames_an_innocent() {
    let mut bad = Vec::new();
    for seed in 0..400u64 {
        for label_at in [690u64, 700, 710, 720] {
            let s = Sim::new("stuck", STUCK)
                .check(ci("quick", "1m").pr_only())
                .check(ci("full", "5m").queue_only().fails_on("BUG"))
                .pr(1, &[("a", "1\n")])
                .pr(2, &[("b", "2\n")])
                .pr(3, &[("c", "3\n")])
                .pr(4, &[("d", "BUG\n")])
                .open("0s", 1)
                .open("0s", 2)
                .open("0s", 3)
                .open("0s", 4)
                .label(&format!("{label_at}s"), 1, "wip")
                .build();
            let f = Faults {
                seed,
                drop: 0.0,
                duplicate: 0.0,
                delay: 1.0,
                max_delay: 60,
                reconcile_every: 0,
            };
            let mut sim = Simulation::new(&s, f);
            sim.run(&s);
            let mut v = violations(&sim);
            for n in [2, 3] {
                if sim.outcome(n) != "merged" {
                    v.push(format!("innocent #{n}: {}", sim.outcome(n)));
                }
            }
            if sim.outcome(4) != "dequeued CHECKS_FAILED" {
                v.push(format!("culprit #4: {}", sim.outcome(4)));
            }
            if !matches!(sim.outcome(1).as_str(), "merged" | "dequeued PR_DEQUEUED") {
                v.push(format!("#1: {}", sim.outcome(1)));
            }
            if !v.is_empty() {
                bad.push(format!("seed {seed} label {label_at}: {v:?}"));
                if bad.len() == 1 {
                    eprintln!("{}", log_text(&sim));
                }
            }
        }
    }
    assert!(bad.is_empty(), "{} bad runs, first: {}", bad.len(), bad[0]);
}

// ---------------------------------------------------------------------------------------------
// Options that phase 1a ignored silently.

fn rules_with(extra_queue: &str, extra_rule: &str) -> String {
    format!(
        "merge_queue:\n  max_parallel_checks: 2\n{extra_queue}queue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: 1\n    checks_timeout: 30 min\n{extra_rule}    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n"
    )
}

/// `reset_on_external_merge: never`: a direct push to main while #1 and #2 are tested does not
/// discard their cars; both land on the new tip, and I0 checks each landing as the tested
/// change applied to the actual parent. With `always` (default) the same push resets.
#[test]
fn reset_on_external_merge_never_keeps_the_cars() {
    let build = |cfg: String| {
        Sim::new("never", &cfg)
            .check(ci("quick", "1m").pr_only())
            .check(ci("full", "10m").queue_only())
            .pr(1, &[("a.rs", "1\n")])
            .pr(2, &[("b.rs", "2\n")])
            .open("0s", 1)
            .open("0s", 2)
            .push_base("5m", &[("hotfix.rs", "x\n")])
            .run()
    };
    let never = build(rules_with("  reset_on_external_merge: never\n", ""));
    never.assert_invariants();
    assert_eq!(never.merged_order, vec![1, 2], "{}", never.log_text());
    assert!(!never.log_text().contains("MERGE_QUEUE_RESET"));
    assert_eq!(builds(&never.log).len(), 2, "no car rebuilt");
    assert!(
        never
            .invariant("I0")
            .notes
            .iter()
            .any(|n| n.contains("actual parent")),
        "{:?}",
        never.invariant("I0")
    );
    let always = build(rules_with("", ""));
    always.assert_invariants();
    assert_eq!(always.merged_order, vec![1, 2]);
    assert!(always.log_text().contains("MERGE_QUEUE_RESET"));
    assert_eq!(builds(&always.log).len(), 4, "both cars rebuilt");
}

/// `max_checks_retries: 2` with a check that flakes half of the time: each retry recreates the
/// draft (a new car, a new draft number), the pull request is dequeued only after the last
/// retry failed. With 0 retries one failure dequeues it.
#[test]
fn max_checks_retries_recreates_the_draft() {
    let mut merged = 0;
    let mut retried = 0;
    for seed in 0..200u64 {
        let r = Sim::new("retries", &rules_with("", "    max_checks_retries: 2\n"))
            .check(ci("quick", "1m").pr_only())
            .check(ci("full", "10m").queue_only().flaky(0.5))
            .flake_seed(seed)
            .faults(None)
            .pr(1, &[("a.rs", "1\n")])
            .open("0s", 1)
            .run();
        r.assert_invariants();
        let retries = r
            .log
            .iter()
            .filter(|l| l.text.contains("CHECKS_RETRIED"))
            .count();
        let drafts: std::collections::BTreeSet<String> = r
            .log
            .iter()
            .filter(|l| l.kind == "reply")
            .filter_map(|l| l.text.split("draft #").nth(1).map(str::to_string))
            .collect();
        assert_eq!(builds(&r.log).len(), retries + 1, "{}", r.log_text());
        assert_eq!(drafts.len(), retries + 1, "a fresh draft per attempt");
        assert!(retries <= 2);
        retried += usize::from(retries > 0);
        if r.outcome(1) == "merged" {
            merged += 1;
        } else {
            assert_eq!(r.outcome(1), "dequeued CHECKS_FAILED");
            assert_eq!(retries, 2, "dequeued only after the last retry");
        }
    }
    assert!(
        retried > 50 && merged > 150,
        "{retried} retried, {merged}/200 merged"
    );
    let none = Sim::new("no-retries", &rules_with("", ""))
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only().flaky(1.0))
        .faults(None)
        .pr(1, &[("a.rs", "1\n")])
        .open("0s", 1)
        .run();
    assert_eq!(none.outcome(1), "dequeued CHECKS_FAILED");
}

/// `allow_queue_branch_edit: true` with `squash`: the option needs a method that lands the
/// batch branch itself (docs), so the engine does not accept the edit (`config validate`
/// rejects the combination too). Before, it accepted it, landed the pull request without the
/// edit although the checks ran on the edited head, and halted on the tree mismatch; the
/// strengthened I0 (green tied to the draft's head being the car's tested head) flags that
/// landing. Now the edit dequeues as without the option, and nothing untested lands.
#[test]
fn allow_queue_branch_edit_with_squash_refuses_the_edit() {
    let r = Sim::new(
        "edit",
        &rules_with("", "    allow_queue_branch_edit: true\n"),
    )
    .check(ci("quick", "1m").pr_only())
    .check(ci("full", "10m").queue_only())
    .faults(None)
    .pr(1, &[("a.rs", "1\n")])
    .pr(2, &[("b.rs", "2\n")])
    .open("0s", 1)
    .open("0s", 2)
    .at("5m", Action::PushQueueBranch(1))
    .run();
    let log = r.log_text();
    assert!(log.contains("DRAFT_PULL_REQUEST_CHANGED"), "{log}");
    assert!(!log.contains("HALT queue of main"), "{log}");
    assert_eq!(r.outcome(1), "dequeued DRAFT_PULL_REQUEST_CHANGED");
    assert_eq!(r.outcome(2), "merged");
    for id in ["I0", "I1", "I2", "I3", "I4"] {
        assert!(
            r.invariant(id).violations.is_empty(),
            "{id}\n{}",
            r.to_text()
        );
    }
}

const AUTO: &str = "merge_queue:\n  max_parallel_checks: 5\nqueue_rules:\n  - name: default\n    batch_size: 1\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

/// `checks_timeout: auto` (the default) applies no timeout until the queue has history: a
/// hanging check on a brand-new queue waits (documented; set a duration for a guaranteed
/// timeout).
#[test]
fn checks_timeout_auto_without_history_waits() {
    let mut s = Sim::new("auto-new", AUTO)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only().hangs_on("HANG"))
        .faults(None)
        .pr(1, &[("a.rs", "HANG\n")])
        .open("0s", 1)
        .build();
    s.max_time = 2 * 86400;
    let mut sim = Simulation::new(&s, Faults::none());
    sim.run(&s);
    assert_eq!(sim.outcome(1), "queued", "{}", log_text(&sim));
    assert!(!log_text(&sim).contains("CHECKS_TIMEOUT"));
}

/// With twenty successful first runs of ten minutes, `auto` derives a timeout from them
/// (p95 × 1.5 = 900 s) and a hanging check ends in CHECKS_TIMEOUT, saying the timeout is
/// automatic.
#[test]
fn checks_timeout_auto_with_history_times_out() {
    let mut sim = Sim::new("auto-history", AUTO)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only().hangs_on("HANG"))
        .faults(None);
    for n in 1..=20u64 {
        let path = format!("f{n}.rs");
        sim = sim
            .pr(n, &[(path.as_str(), "x\n")])
            .open(&format!("{}s", n * 60), n);
    }
    let r = sim.pr(21, &[("hang.rs", "HANG\n")]).open("4h", 21).run();
    r.assert_invariants();
    assert_eq!(r.outcome(21), "dequeued CHECKS_TIMEOUT", "{}", r.log_text());
    let line = r
        .log
        .iter()
        .find(|l| l.text.contains("CHECKS_TIMEOUT"))
        .expect("timeout");
    assert!(
        line.text.contains("gave the checks 900s, a limit it estimates"),
        "{}",
        line.text
    );
}

/// A pull request leaves a batch of two that has a car behind it: its batch is rebuilt
/// (BATCH_REBUILT for the one that stays) and the car behind gets BATCH_AHEAD_REBUILT.
#[test]
fn a_rebuilt_batch_ahead_is_reported_as_such() {
    let cfg = "merge_queue:\n  max_parallel_checks: 2\nqueue_rules:\n  - name: default\n    batch_size: 2\n    batch_max_wait_time: 1 min\n    queue_conditions:\n      - check-success = quick\n      - -label = halt\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let r = Sim::new("rebuilt", cfg)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only())
        .faults(None)
        .pr(1, &[("a.rs", "1\n")])
        .pr(2, &[("b.rs", "2\n")])
        .pr(3, &[("c.rs", "3\n")])
        .pr(4, &[("d.rs", "4\n")])
        .open("0s", 1)
        .open("0s", 2)
        .open("0s", 3)
        .open("0s", 4)
        .label("5m", 1, "halt")
        .run();
    r.assert_invariants();
    let log = r.log_text();
    assert!(
        log.contains("checks of #2 ended, stays queued: BATCH_REBUILT"),
        "{log}"
    );
    assert!(
        log.contains("checks of #3 ended, stays queued: BATCH_AHEAD_REBUILT"),
        "{log}"
    );
    assert_eq!(r.outcome(1), "dequeued PR_DEQUEUED");
    for n in 2..=4 {
        assert_eq!(r.outcome(n), "merged");
    }
}
