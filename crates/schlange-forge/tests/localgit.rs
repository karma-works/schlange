//! The same scenarios on real git (`localgit`): a bare repository on disk, real three-way
//! merges, `git update-ref` compare-and-swap for every ref write, and every check as a process
//! in a checkout of the tested commit. Expectations and invariants I0–I4 are checked against
//! the real repository after every step; the outcome must match the in-memory forge.
//!
//! Also the merge-method × batch-size matrix of KONZEPT.md, section 9 (MQ-02, MQ-03) for the
//! method the core builds, `squash`, with batch sizes 1–5, against I0 on real git.

#![cfg(feature = "localgit")]

use std::path::{Path, PathBuf};

use schlange_forge::dsl::{Sim, ci};
use schlange_forge::git::GitStore;
use schlange_forge::localgit::{self, LocalGit};
use schlange_forge::runner::Simulation;
use schlange_forge::scenario::run_file;
use schlange_forge::sim::Faults;

fn scenario(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../scenarios")
        .join(format!("{name}.yaml"))
}

/// Scenarios that must pass on both forges (the critic's stuck bisection included).
const BOTH: [&str; 21] = [
    "regressions/critic-stuck-bisection",
    "p1-batch-five",
    "p1-bisection",
    "p1-semantic-conflict",
    "p1-semantic-conflict-compile",
    "p1-base-moved",
    "p1-arztagent-two-step",
    "p2-method-merge",
    "p2-method-rebase",
    "p2-method-fast-forward",
    "p2-method-merge-batch",
    "p2-isolated",
    "p2-parallel-scopes",
    "p2-pause",
    "p2-freeze",
    "p2-priority",
    "p2-priority-no-interrupt",
    "p2-skip-intermediate",
    "p2-merge-protections",
    "p2-required-checks",
    "regressions/p2-fast-forward-in-place",
];

fn check_scenario(name: &str) {
    let file = scenario(name);
    let real = localgit::run_file(&file).unwrap_or_else(|e| panic!("{name}: {e}"));
    assert_eq!(real.forge, "localgit");
    assert!(real.passed, "{name} on localgit\n{}", real.to_text());
    for id in ["I0", "I1", "I2", "I3", "I4"] {
        assert_eq!(real.invariant(id).status, "checked", "{name} {id}");
        assert!(real.invariant(id).checks > 0, "{name} {id}");
    }
    assert!(real.ci_processes > 0, "{name}: no check ran as a process");
    let sim = run_file(&file).expect("sim");
    assert!(sim.passed, "{name} in memory\n{}", sim.to_text());
    assert_eq!(real.outcomes, sim.outcomes, "{name}: outcomes differ");
    assert_eq!(real.merged_order, sim.merged_order, "{name}: order differs");
    // The disturbed run (I5) ran on real git too.
    if let Some(d) = &real.disturbed {
        assert!(d.invariant_violations.is_empty(), "{name}: {d:?}");
    }
}

#[test]
fn scenarios_pass_on_real_git_with_the_same_outcome_as_in_memory() {
    // One thread per scenario: the time goes into git processes.
    std::thread::scope(|s| {
        let handles: Vec<_> = BOTH
            .iter()
            .map(|name| s.spawn(move || check_scenario(name)))
            .collect();
        for h in handles {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });
}

const LINES: usize = 40;

fn base_text() -> String {
    (0..LINES).map(|i| format!("line {i}\n")).collect()
}

/// `shared.txt` with line `line` replaced by `text`.
fn edit(line: usize, text: &str) -> String {
    (0..LINES)
        .map(|i| {
            if i == line {
                format!("{text}\n")
            } else {
                format!("line {i}\n")
            }
        })
        .collect()
}

fn matrix_config(batch: u32) -> String {
    method_config("squash", batch)
}

fn method_config(method: &str, batch: u32) -> String {
    format!(
        "merge_queue:\n  max_parallel_checks: 2\nqueue_rules:\n  - name: default\n    merge_method: {method}\n    batch_size: {batch}\n    batch_max_wait_time: 1 min\n    queue_conditions:\n      - base = main\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\npull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n"
    )
}

/// One cell of the matrix for the other merge methods on real git: the same pull requests as
/// [`matrix_cell`] (same file, different lines, a broken one, a conflicting one). Every merged
/// change is on main line by line, I0–I4 hold for the method, and the commit graph has the
/// method's shape.
fn method_cell(method: &str, batch: u32) {
    let n = 2 * batch as u64 + 2;
    let mut sim = Sim::new(
        &format!("matrix-{method}-{batch}"),
        &method_config(method, batch),
    )
    .base_file("shared.txt", &base_text())
    .check(ci("quick", "1m").pr_only())
    .check(ci("full", "10m").queue_only().fails_on("BUG"))
    .faults(None);
    let line_of = |k: u64| if k == n { 3 } else { 3 * k as usize };
    for k in 1..=n {
        let text = match k {
            3 => format!("changed by #{k} BUG"),
            _ if k == n => format!("conflicting change by #{k}"),
            _ => format!("changed by #{k}"),
        };
        let content = edit(line_of(k), &text);
        sim = sim
            .pr(k, &[("shared.txt", content.as_str())])
            .open(if k <= n / 2 { "0s" } else { "5m" }, k);
    }
    let s = sim.build();
    let mut run = Simulation::on(&s, Faults::none(), LocalGit::new().expect("git"), true);
    run.run(&s);
    let log: String = run
        .log
        .iter()
        .map(|l| format!("{} {} {}\n", l.t, l.kind, l.text))
        .collect();
    let cell = format!("{method} batch {batch}");
    for (id, r) in &run.checker.results {
        assert!(
            r.violations.is_empty(),
            "{cell}: {id} violated: {:?}\n{log}",
            r.violations
        );
    }
    for id in ["I0", "I1", "I2", "I3", "I4"] {
        assert!(run.checker.results[id].checks > 0, "{cell}: {id}");
    }
    let mut merged = 0;
    for k in 1..=n {
        let outcome = run.outcome(k);
        match k {
            3 => assert_eq!(outcome, "dequeued CHECKS_FAILED", "{cell}\n{log}"),
            _ if k == n => assert!(
                outcome.starts_with("dequeued"),
                "{cell}: #{k} {outcome}\n{log}"
            ),
            _ => {
                assert_eq!(outcome, "merged", "{cell}: #{k}\n{log}");
                merged += 1;
            }
        }
    }
    let git = &run.forge.repo;
    let tip = git.ref_sha("main").expect("main");
    let shared = git.tree(&tip)["shared.txt"].clone();
    let mut expected: Vec<String> = (0..LINES).map(|i| format!("line {i}")).collect();
    for k in 1..n {
        if k != 3 {
            expected[line_of(k)] = format!("changed by #{k}");
        }
    }
    assert_eq!(shared, expected.join("\n") + "\n", "{cell}");
    let history = git.first_parent_history(&tip);
    let landed = &history[..history.len() - 1];
    let parents = |sha: &String| git.parents(sha).len();
    match method {
        // One commit per pull request on the first-parent line.
        "merge" | "fast-forward" => {
            assert_eq!(landed.len(), merged, "{cell}: {history:?}\n{log}");
            assert!(
                landed.iter().all(|c| parents(c) == 2),
                "{cell}: merge commits"
            );
        }
        "rebase" => {
            assert_eq!(landed.len(), merged, "{cell}: {history:?}\n{log}");
            assert!(landed.iter().all(|c| parents(c) == 1), "{cell}: linear");
        }
        // One merge commit per landed batch.
        "merge-batch" => {
            assert!(
                !landed.is_empty() && landed.len() <= merged,
                "{cell}: {history:?}"
            );
            assert!(
                landed.iter().all(|c| parents(c) == 2),
                "{cell}: merge commits"
            );
        }
        _ => unreachable!(),
    }
    // Every merged pull request's head is in main's history for the methods that keep SHAs.
    if matches!(method, "merge" | "fast-forward" | "merge-batch") {
        for k in 1..n {
            if k != 3 {
                let head = run.forge.read_pull_request(k).expect("pr").head_sha;
                assert!(
                    git.is_ancestor(&head, &tip),
                    "{cell}: #{k} head not in main"
                );
            }
        }
    }
    assert!(git.fsck(), "{cell}: git fsck");
}

#[test]
fn matrix_other_merge_methods_keep_i0_on_real_git() {
    let cells: Vec<(&str, u32)> = ["merge", "rebase", "fast-forward", "merge-batch"]
        .into_iter()
        .flat_map(|m| (1..=3u32).map(move |b| (m, b)))
        .filter(|(m, b)| !(*m == "merge-batch" && *b == 1))
        .collect();
    std::thread::scope(|s| {
        let handles: Vec<_> = cells
            .into_iter()
            .map(|(m, b)| s.spawn(move || method_cell(m, b)))
            .collect();
        for h in handles {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });
}

/// One cell of the matrix: squash with batch size `batch` on real git. `2 * batch + 2` pull
/// requests all edit the same file on different lines (git merges them line by line; the
/// in-memory model would call each pair a conflict). #3 breaks the full suite, the last one
/// edits #1's line differently (a real git conflict).
fn matrix_cell(batch: u32) {
    let n = 2 * batch as u64 + 2;
    let mut sim = Sim::new(&format!("matrix-squash-{batch}"), &matrix_config(batch))
        .base_file("shared.txt", &base_text())
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only().fails_on("BUG"))
        .faults(None);
    let line_of = |k: u64| if k == n { 3 } else { 3 * k as usize };
    for k in 1..=n {
        let text = match k {
            3 => format!("changed by #{k} BUG"),
            _ if k == n => format!("conflicting change by #{k}"),
            _ => format!("changed by #{k}"),
        };
        let content = edit(line_of(k), &text);
        sim = sim
            .pr(k, &[("shared.txt", content.as_str())])
            .open(if k <= n / 2 { "0s" } else { "5m" }, k);
    }
    let s = sim.build();
    let mut run = Simulation::on(&s, Faults::none(), LocalGit::new().expect("git"), true);
    run.run(&s);
    let log: String = run
        .log
        .iter()
        .map(|l| format!("{} {} {}\n", l.t, l.kind, l.text))
        .collect();
    for (id, r) in &run.checker.results {
        assert!(
            r.violations.is_empty(),
            "batch {batch}: {id} violated: {:?}\n{log}",
            r.violations
        );
    }
    for id in ["I0", "I1", "I2", "I3", "I4"] {
        assert!(run.checker.results[id].checks > 0, "batch {batch}: {id}");
    }
    for k in 1..=n {
        let outcome = run.outcome(k);
        match k {
            3 => assert_eq!(outcome, "dequeued CHECKS_FAILED", "batch {batch}\n{log}"),
            _ if k == n => assert!(
                outcome.starts_with("dequeued CONFLICT"),
                "batch {batch}: #{k} {outcome}\n{log}"
            ),
            _ => assert_eq!(outcome, "merged", "batch {batch}: #{k}\n{log}"),
        }
    }
    // The real repository: main holds every merged change, line by line, and nothing else;
    // its first-parent history is the initial commit plus one squash commit per merged pull
    // request; the repository is consistent.
    let git = &run.forge.repo;
    let tip = git.ref_sha("main").expect("main");
    let shared = git.tree(&tip)["shared.txt"].clone();
    let mut expected: Vec<String> = (0..LINES).map(|i| format!("line {i}")).collect();
    for k in 1..n {
        if k != 3 {
            expected[line_of(k)] = format!("changed by #{k}");
        }
    }
    assert_eq!(shared, expected.join("\n") + "\n", "batch {batch}");
    let history = git.first_parent_history(&tip);
    assert_eq!(
        history.len() as u64,
        1 + (n - 2),
        "batch {batch}: {history:?}"
    );
    for sha in &history[..history.len() - 1] {
        assert_eq!(git.parents(sha).len(), 1, "squash commits have one parent");
    }
    assert!(git.fsck(), "batch {batch}: git fsck");
    if std::env::var_os("SHOW_MATRIX").is_some() {
        eprintln!("=== batch {batch}\n{log}");
    }
    // Batches really formed: with batch size > 1 some car checked several pull requests.
    if batch > 1 {
        assert!(
            run.log.iter().any(|l| l.text.contains("(checks #1 #2")),
            "batch {batch}: no batch\n{log}"
        );
    }
}

#[test]
fn matrix_squash_by_batch_size_one_to_five_keeps_i0_on_real_git() {
    std::thread::scope(|s| {
        let handles: Vec<_> = (1..=5u32)
            .map(|b| s.spawn(move || matrix_cell(b)))
            .collect();
        for h in handles {
            if let Err(e) = h.join() {
                std::panic::resume_unwind(e);
            }
        }
    });
}

#[test]
fn line_level_merge_differs_from_the_in_memory_model() {
    // Two pull requests edit different lines of one file. On real git both land; the
    // in-memory model sees a file-level conflict and dequeues the second.
    let s = Sim::new("lines", &matrix_config(1))
        .base_file("shared.txt", &base_text())
        .check(ci("quick", "1m").pr_only())
        .check(ci("full", "10m").queue_only())
        .faults(None)
        .pr(1, &[("shared.txt", edit(5, "five").as_str())])
        .pr(2, &[("shared.txt", edit(30, "thirty").as_str())])
        .open("0s", 1)
        .open("0s", 2)
        .build();
    let real = localgit::run(&s).expect("git");
    real.assert_invariants();
    assert_eq!(real.outcome(1), "merged", "{}", real.to_text());
    assert_eq!(real.outcome(2), "merged", "{}", real.to_text());
    let mem = schlange_forge::run(&s);
    mem.assert_invariants();
    assert_eq!(mem.outcome(1), "merged");
    assert!(
        mem.outcome(2).starts_with("dequeued CONFLICT"),
        "{}",
        mem.outcome(2)
    );
}

#[test]
fn the_checker_sees_an_untested_commit_in_the_real_repository() {
    use schlange_config::rules::Rules;
    use schlange_core::State;
    use schlange_forge::invariants::Checker;
    use schlange_forge::sim::SimForge;

    let rules = Rules::from_yaml("queue_rules:\n  - name: default\n").unwrap();
    let mut forge = SimForge::with_git(
        LocalGit::new().expect("git"),
        "main",
        [("a".to_string(), "1\n".to_string())].into(),
        vec![],
        0,
    );
    let state = State::default();
    let mut c = Checker::new();
    c.after_step(&forge, &state, &rules);
    assert!(!c.violated());
    // Someone writes to the bare repository behind the queue's back.
    let tip = forge.read_branch("main").unwrap();
    let sneaky = forge.repo.commit(
        [("a".to_string(), "untested\n".to_string())].into(),
        vec![tip.clone()],
        "sneaky",
    );
    forge.repo.update_ref("main", &tip, &sneaky).unwrap();
    forge
        .repo
        .create_ref("mergify/merge-queue/0123456789", &sneaky)
        .unwrap();
    c.after_step(&forge, &state, &rules);
    assert!(!c.results["I0"].violations.is_empty());
    assert!(!c.results["I3"].violations.is_empty());
}

/// Forge faults and crashes on real git: lost, late, duplicated and lying replies, base races,
/// lagging reads, CI timing, and the engine crashing at random side effects and restarting
/// from its journal. I0–I4 hold and the queue settles (MQ-31, MQ-33, MQ-35).
#[test]
fn forge_faults_and_crashes_keep_the_invariants_on_real_git() {
    use schlange_forge::faults::{CrashPlan, ForgeFaults};
    for name in ["p1-bisection", "p1-batch-five"] {
        let file = scenario(name);
        let s = schlange_forge::scenario::parse(&std::fs::read_to_string(&file).unwrap(), &file)
            .unwrap();
        for seed in 0..3u64 {
            let git = LocalGit::new().expect("git");
            let mut sim = Simulation::on(&s, Faults::standard(seed), git, false)
                .with_forge_faults(ForgeFaults::standard(seed))
                .with_crashes(CrashPlan::random(seed, 0.05, 3));
            sim.run(&s);
            let v: Vec<String> = sim
                .checker
                .results
                .iter()
                .flat_map(|(id, r)| r.violations.iter().map(move |v| format!("{id}: {v}")))
                .collect();
            assert!(v.is_empty(), "{name} seed {seed} on localgit: {v:?}");
        }
    }
}
