//! MQ-31: the engine crashes at every side effect, before it is carried out and right after,
//! loses its memory and everything on its way to it, and restarts from its journal plus a
//! fresh read of the forge. I0–I4 must hold in every run, nothing lands twice (I1), and the end
//! state (merged pull requests, outcomes with dequeue reasons, base trees) is the one of the run
//! without a crash.
//!
//! Also: every scenario under forge faults (lost, late, duplicated and lying replies, base
//! races, lagging reads, CI timing) keeps I0–I4 and settles.

use std::path::{Path, PathBuf};

use schlange_forge::faults::{CrashPlan, ForgeFaults};
use schlange_forge::git::GitStore;
use schlange_forge::runner::{Scenario, Simulation, clock};
use schlange_forge::scenario::parse;
use schlange_forge::sim::Faults;

fn load(name: &str) -> Scenario {
    let p: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios")
        .join(name);
    parse(&std::fs::read_to_string(&p).expect("scenario"), &p).expect("parses")
}

fn violations<G: GitStore>(sim: &Simulation<G>) -> Vec<String> {
    sim.checker
        .results
        .iter()
        .flat_map(|(id, r)| r.violations.iter().map(move |v| format!("{id}: {v}")))
        .collect()
}

fn log<G: GitStore>(sim: &Simulation<G>) -> String {
    sim.log
        .iter()
        .map(|l| format!("{} {:<8} {}\n", clock(l.t), l.kind, l.text))
        .collect()
}

/// Scenarios whose outcome does not depend on timing (no flakes, no human racing the queue).
const DETERMINISTIC: &[&str] = &[
    "p1-bisection.yaml",
    "p1-batch-five.yaml",
    "p1-arztagent-two-step.yaml",
    "p1-cascade.yaml",
    "p1-checks-timeout.yaml",
    "p1-in-place.yaml",
    "p1-semantic-conflict.yaml",
    "regressions/critic-stuck-bisection.yaml",
];

#[test]
fn a_crash_at_every_side_effect_changes_nothing() {
    let mut total = 0;
    for name in DETERMINISTIC {
        let s = load(name);
        let mut clean = Simulation::new(&s, Faults::none());
        clean.run(&s);
        assert!(
            violations(&clean).is_empty(),
            "{name}: {:?}",
            violations(&clean)
        );
        let expected = clean.end_state();
        for k in 0..clean.effects {
            for after in [false, true] {
                let mut sim =
                    Simulation::new(&s, Faults::none()).with_crashes(CrashPlan::at(k, after));
                sim.run(&s);
                total += 1;
                assert_eq!(sim.crashes, 1, "{name}: crash point {k}/{after} reached");
                let v = violations(&sim);
                assert!(
                    v.is_empty(),
                    "{name}, crash {} side effect {k}: {v:?}\n{}",
                    if after { "after" } else { "before" },
                    log(&sim)
                );
                assert_eq!(
                    sim.end_state(),
                    expected,
                    "{name}, crash {} side effect {k}\n{}",
                    if after { "after" } else { "before" },
                    log(&sim)
                );
            }
        }
    }
    eprintln!("{total} crash runs");
    assert!(total > 100);
}

#[test]
fn every_scenario_keeps_its_invariants_under_forge_faults_and_crashes() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    files.sort();
    for f in files {
        let s = parse(&std::fs::read_to_string(&f).unwrap(), &f).unwrap();
        for seed in 0..20u64 {
            let mut sim = Simulation::new(&s, Faults::standard(seed))
                .with_forge_faults(ForgeFaults::standard(seed))
                .with_crashes(CrashPlan::random(seed, 0.03, 3));
            sim.run(&s);
            let v = violations(&sim);
            assert!(
                v.is_empty(),
                "{} seed {seed}: {v:?}\n{}",
                f.display(),
                log(&sim)
            );
        }
    }
}
