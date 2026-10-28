//! Reproductions of the critique of phases 1a–1c (`.critic/phase-1abc.md`), sections "Core and
//! conditions" and "Invariants", on the simulated forge with every invariant checked.

use schlange_forge::dsl::{Sim, ci};
use schlange_forge::runner::Simulation;
use schlange_forge::sim::Faults;

/// Every webhook lost; reconciliation every 30 s.
fn all_lost() -> Faults {
    Faults {
        seed: 1,
        drop: 1.0,
        duplicate: 0.0,
        delay: 0.0,
        max_delay: 0,
        reconcile_every: 30,
    }
}

fn builds(r: &schlange_forge::scenario::RunReport) -> Vec<String> {
    r.log
        .iter()
        .filter(|l| l.text.starts_with("build car"))
        .map(|l| l.text.clone())
        .collect()
}

/// R3: with `max_checks_retries`, a split test of a failed batch is retried before it counts
/// as red (docs: batches, `previous_check_retries` of a split).
#[test]
fn bisection_split_tests_are_retried() {
    let cfg = "merge_queue:\n  max_parallel_checks: 2\nqueue_rules:\n  - name: default\n    batch_size: 3\n    batch_max_wait_time: 5 min\n    max_checks_retries: 1\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let r = Sim::new("split-retries", cfg)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only().fails_on("BUG"))
        .faults(None)
        .pr(1, &[("a.rs", "1\n")])
        .pr(2, &[("b.rs", "BUG\n")])
        .pr(3, &[("c.rs", "3\n")])
        .open("0s", 1)
        .open("0s", 2)
        .open("0s", 3)
        .run();
    r.assert_invariants();
    let log = r.log_text();
    let split = builds(&r)
        .iter()
        .filter(|b| b.ends_with("(checks #1 #2)"))
        .count();
    assert_eq!(split, 2, "the red split #1 #2 is retried once:\n{log}");
    assert_eq!(r.outcome(2), "dequeued CHECKS_FAILED", "{log}");
    assert_eq!(r.outcome(1), "merged");
    assert_eq!(r.outcome(3), "merged");
}

/// R4: a batch is seeded with the next pull request and filled by similarity of the changed
/// directories (no scopes configured), not in plain queue order (docs: batches, "How Pull
/// Requests Are Grouped Into a Batch": #1 and #3 touch `api/`, #2 `docs/`, #4 `web/`).
#[test]
fn batches_are_grouped_by_changed_directories() {
    let cfg = "merge_queue:\n  max_parallel_checks: 2\nqueue_rules:\n  - name: default\n    batch_size: 2\n    batch_max_wait_time: 5 min\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    // Every webhook is lost and one reconciliation shows all four at once, so the batch is
    // assembled from all of them.
    let s = Sim::new("similarity", cfg)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only())
        .pr(1, &[("api/a.rs", "1\n")])
        .pr(2, &[("docs/b.md", "2\n")])
        .pr(3, &[("api/c.rs", "3\n")])
        .pr(4, &[("web/d.ts", "4\n")])
        .open("0s", 1)
        .open("0s", 2)
        .open("0s", 3)
        .open("0s", 4)
        .build();
    let mut sim = Simulation::new(&s, all_lost());
    sim.run(&s);
    assert!(!sim.checker.violated(), "{:?}", sim.checker.results);
    let b: Vec<String> = sim
        .log
        .iter()
        .filter(|l| l.text.starts_with("build car"))
        .map(|l| l.text.clone())
        .collect();
    assert!(b[0].ends_with("(checks #1 #3)"), "{b:#?}");
    assert!(b[1].ends_with("(checks #2 #4)"), "{b:#?}");
    for n in 1..=4 {
        assert_eq!(sim.outcome(n), "merged");
    }
}
