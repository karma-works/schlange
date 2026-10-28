//! P2 queue features against the simulated forge: cases the property tests found (kept as
//! regressions) and checks of the invariant checker for the new merge methods and modes.

use schlange_forge::dsl::{Sim, ci};
use schlange_forge::runner::Simulation;
use schlange_forge::sim::{Action, Faults};

fn log_of<G: schlange_forge::git::GitStore>(sim: &Simulation<G>) -> String {
    sim.log
        .iter()
        .map(|l| format!("{} {} {}\n", l.t, l.kind, l.text))
        .collect()
}

fn violations<G: schlange_forge::git::GitStore>(sim: &Simulation<G>) -> Vec<String> {
    sim.checker
        .results
        .iter()
        .flat_map(|(id, r)| r.violations.iter().map(move |v| format!("{id}: {v}")))
        .collect()
}

const ISOLATED: &str = "merge_queue:\n  max_parallel_checks: 2\n  mode: isolated\nqueue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: 1\n    checks_timeout: 20 min\n    queue_conditions:\n      - base = main\n      - -draft\n      - -label = halt\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n      - -label = wip\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

/// Property test (400 cases): `merge-batch` with `reset_on_external_merge: never`. The second
/// car is stacked on the first one's branch; after a push to main it has no tested change on
/// the base to carry over, so the queue resets instead of landing it on the moved base (I0).
#[test]
fn merge_batch_with_never_resets_after_an_external_push() {
    let config = "merge_queue:\n  max_parallel_checks: 2\n  reset_on_external_merge: never\nqueue_rules:\n  - name: default\n    merge_method: merge-batch\n    batch_size: 2\n    batch_max_wait_time: 1 min\n    queue_conditions:\n      - base = main\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let s = Sim::new("merge-batch-never", config)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only())
        .pr(1, &[("a.rs", "#1\n")])
        .pr(2, &[("b.rs", "#2\n")])
        .pr(3, &[("c.rs", "#3\n")])
        .pr(4, &[("d.rs", "#4\n")])
        .open("0s", 1)
        .open("0s", 2)
        .open("10s", 3)
        .open("10s", 4)
        .at(
            "5m",
            Action::PushBase {
                branch: "main".into(),
                files: vec![("e.rs".into(), "direct\n".into())],
            },
        )
        .faults(None)
        .build();
    let mut sim = Simulation::new(&s, Faults::none());
    sim.run(&s);
    let v = violations(&sim);
    assert!(v.is_empty(), "{v:?}\n{}", log_of(&sim));
    for n in 1..=4 {
        assert_eq!(sim.outcome(n), "merged", "#{n}\n{}", log_of(&sim));
    }
    assert!(
        log_of(&sim).contains("MERGE_QUEUE_RESET"),
        "{}",
        log_of(&sim)
    );
}

/// Property test (I5, isolated mode): a push to main whose webhook is lost while two isolated
/// cars run. Neither car may land on top of the unseen external commit as if it were another
/// lane's landing.
#[test]
fn isolated_car_never_lands_over_an_unseen_external_push() {
    let faults = Faults {
        seed: 2711553846008183219,
        drop: 0.2663164133413929,
        duplicate: 0.2599976877687983,
        delay: 0.0,
        max_delay: 1,
        reconcile_every: 39,
    };
    let s = Sim::new("iso-push", ISOLATED)
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only())
        .pr(1, &[("pr1.rs", "#1\n")])
        .pr_with(2, &[("pr2.rs", "#2\n")], |d| {
            d.labels.insert("wip".into());
        })
        .open("56s", 1)
        .open("0s", 2)
        .at(
            "0s",
            Action::Unlabel {
                number: 2,
                label: "wip".into(),
            },
        )
        .at(
            "91s",
            Action::PushBase {
                branch: "main".into(),
                files: vec![("direct0.rs".into(), "direct\n".into())],
            },
        )
        .faults(Some(faults.clone()))
        .build();
    let mut sim = Simulation::new(&s, faults);
    sim.run(&s);
    let v = violations(&sim);
    assert!(v.is_empty(), "{v:?}\n{}", log_of(&sim));
    assert_eq!(sim.outcome(1), "merged", "{}", log_of(&sim));
    assert_eq!(sim.outcome(2), "merged", "{}", log_of(&sim));
}
