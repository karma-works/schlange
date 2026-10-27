//! Cases the property tests found, kept as fixed regressions.

use schlange_forge::dsl::{Sim, ci};
use schlange_forge::git::GitStore;
use schlange_forge::runner::{Simulation, clock};
use schlange_forge::sim::Faults;

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
