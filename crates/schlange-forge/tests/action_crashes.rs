//! Rule actions under crashes (MQ-31): the engine dies before and right after every side effect
//! of the action scenarios and restarts from its journal. Besides I0–I4 and the end state of the
//! run without a crash, what the actions left on the forge is the same: every comment the
//! actions posted exists exactly once, the same copies exist, labels, assignees, review
//! requests and checks match, and workflow dispatches never run twice.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schlange_forge::faults::{CrashPlan, ForgeFaults};
use schlange_forge::git::GitStore;
use schlange_forge::runner::{Scenario, Simulation, clock};
use schlange_forge::scenario::parse;
use schlange_forge::sim::{BOT, Faults};

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

/// What the actions left on the forge.
fn traces<G: GitStore>(sim: &Simulation<G>) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut comments: BTreeMap<String, usize> = BTreeMap::new();
    for n in sim.forge.numbers() {
        for c in sim.forge.comments_on(n) {
            if c.author == BOT && c.body.contains("schlange-action") {
                *comments.entry(format!("#{n}: {}", c.body)).or_default() += 1;
            }
        }
        let Some(p) = sim.forge.read_pull_request(n) else {
            continue;
        };
        if p.author == BOT && !schlange_forge::sim::is_copy(&p) {
            continue;
        }
        if schlange_forge::sim::is_copy(&p) {
            out.insert(
                format!("copy {}", p.head_ref),
                format!("{} {:?} {:?}", p.base, p.labels, p.assignees),
            );
            continue;
        }
        let mut labels = p.labels.clone();
        for own in ["queued", "dequeued"] {
            labels.remove(own);
        }
        out.insert(
            format!("#{n}"),
            format!(
                "labels {labels:?} assignees {:?} requested {:?} {:?} draft {} closed {}",
                p.assignees, p.requested_reviewers, p.requested_teams, p.draft, p.closed
            ),
        );
    }
    for (body, k) in comments {
        out.insert(format!("comment {body}"), k.to_string());
    }
    let mut checks = BTreeMap::new();
    for c in sim.forge.posted_checks.iter() {
        checks.insert((c.number, c.name.clone()), c.conclusion.clone());
    }
    for ((n, name), c) in checks {
        out.insert(format!("check #{n} {name}"), c);
    }
    out.insert("dispatches".into(), sim.forge.dispatches.len().to_string());
    out
}

const ACTION_SCENARIOS: &[&str] = &[
    "p2-actions-label-comment.yaml",
    "p2-actions-people.yaml",
    "p2-actions-branches.yaml",
    "p2-actions-copies.yaml",
    "p2-config-extends.yaml",
];

#[test]
fn a_crash_at_every_side_effect_never_posts_twice() {
    let mut total = 0;
    for name in ACTION_SCENARIOS {
        let s = load(name);
        let mut clean = Simulation::new(&s, Faults::none());
        clean.run(&s);
        assert!(
            violations(&clean).is_empty(),
            "{name}: {:?}",
            violations(&clean)
        );
        let expected = clean.end_state();
        let expected_traces = traces(&clean);
        for k in 0..clean.effects {
            for after in [false, true] {
                let mut sim =
                    Simulation::new(&s, Faults::none()).with_crashes(CrashPlan::at(k, after));
                sim.run(&s);
                total += 1;
                let when = if after { "after" } else { "before" };
                let v = violations(&sim);
                assert!(
                    v.is_empty(),
                    "{name}, crash {when} side effect {k}: {v:?}\n{}",
                    log(&sim)
                );
                assert_eq!(
                    sim.end_state(),
                    expected,
                    "{name}, crash {when} side effect {k}\n{}",
                    log(&sim)
                );
                let t = traces(&sim);
                for (key, value) in &t {
                    if key.starts_with("comment ") {
                        assert_eq!(
                            value,
                            "1",
                            "{name}, crash {when} {k}: {key} posted {value} times\n{}",
                            log(&sim)
                        );
                    }
                }
                let dispatches: usize = t["dispatches"].parse().unwrap();
                assert!(
                    dispatches <= expected_traces["dispatches"].parse().unwrap(),
                    "{name}: a dispatch ran twice"
                );
                let strip = |m: &BTreeMap<String, String>| -> BTreeMap<String, String> {
                    m.iter()
                        .filter(|(k, _)| *k != "dispatches")
                        .map(|(k, v)| (k.clone(), v.clone()))
                        .collect()
                };
                assert_eq!(
                    strip(&t),
                    strip(&expected_traces),
                    "{name}, crash {when} side effect {k}\n{}",
                    log(&sim)
                );
            }
        }
    }
    eprintln!("{total} crash runs");
    assert!(total > 100);
}

#[test]
fn action_scenarios_keep_their_invariants_under_forge_faults_and_crashes() {
    for name in ACTION_SCENARIOS {
        let s = load(name);
        for seed in 0..20u64 {
            let mut sim = Simulation::new(&s, Faults::standard(seed))
                .with_forge_faults(ForgeFaults::standard(seed))
                .with_crashes(CrashPlan::random(seed, 0.05, 3));
            sim.run(&s);
            let v = violations(&sim);
            assert!(v.is_empty(), "{name} seed {seed}: {v:?}\n{}", log(&sim));
            for (key, value) in traces(&sim) {
                if key.starts_with("comment ") {
                    assert_eq!(value, "1", "{name} seed {seed}: {key}\n{}", log(&sim));
                }
            }
        }
    }
}
