//! Cases the property tests found, kept as fixed regressions.

use schlange_forge::dsl::{Sim, ci};
use schlange_forge::runner::Simulation;
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
