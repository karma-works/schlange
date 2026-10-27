//! Property test on real git (`localgit`): random pull requests editing random lines of two
//! shared files (so they overlap: some merge line by line, some conflict the way git says),
//! random CI outcomes (markers that break the full suite, semantic-conflict pairs, flaky
//! checks), direct pushes to main, random queue settings. The runner checks I0–I4 against the
//! real repository after every step and I2/I3 at quiescence; any violation fails the case.
//!
//! Every case starts git hundreds of times, so the count is small: `SCHLANGE_LOCALGIT_PBT_CASES`
//! (default 4). Deterministic: a fixed seed.

#![cfg(feature = "localgit")]

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

use schlange_forge::Scenario;
use schlange_forge::dsl::{Sim, ci};
use schlange_forge::localgit::LocalGit;
use schlange_forge::runner::Simulation;
use schlange_forge::sim::{Action, Faults};

fn config() -> Config {
    let cases = std::env::var("SCHLANGE_LOCALGIT_PBT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(4);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x10ca_1617),
        failure_persistence: None,
        // Shrinking reruns whole scenarios on git; keep it short.
        max_shrink_iters: 16,
        ..Config::default()
    }
}

const FILES: [&str; 2] = ["a.txt", "b.txt"];
const LINES: usize = 12;

fn file_with(line: usize, text: &str) -> String {
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

#[derive(Clone, Debug)]
struct PrPlan {
    file: usize,
    line: usize,
    /// 0 none, 1 BUG, 2 SEM_A, 3 SEM_B.
    marker: u8,
    open_at: u64,
}

#[derive(Clone, Debug)]
struct Plan {
    batch: u32,
    max_parallel: u32,
    prs: Vec<PrPlan>,
    /// (time, file, line) of direct pushes to main.
    pushes: Vec<(u64, usize, usize)>,
    flake: bool,
    seed: u64,
}

fn plan() -> impl Strategy<Value = Plan> {
    (
        1u32..=4,
        1u32..=3,
        prop::collection::vec(
            (
                0usize..2,
                0usize..LINES,
                prop::sample::select(vec![0u8, 0, 0, 1, 2, 3]),
                0u64..1200,
            )
                .prop_map(|(file, line, marker, open_at)| PrPlan {
                    file,
                    line,
                    marker,
                    open_at,
                }),
            1..=6,
        ),
        prop::collection::vec((0u64..2400, 0usize..2, 0usize..LINES), 0..=2),
        prop::bool::weighted(0.3),
        any::<u64>(),
    )
        .prop_map(|(batch, max_parallel, prs, pushes, flake, seed)| Plan {
            batch,
            max_parallel,
            prs,
            pushes,
            flake,
            seed,
        })
}

fn scenario(p: &Plan) -> Scenario {
    let yaml = format!(
        "merge_queue:\n  max_parallel_checks: {}\nqueue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: {}\n    batch_max_wait_time: 1 min\n    queue_conditions:\n      - base = main\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\npull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n",
        p.max_parallel, p.batch
    );
    let mut full = ci("full", "10m")
        .queue_only()
        .fails_on("BUG")
        .fails_with_all(&["SEM_A", "SEM_B"]);
    if p.flake {
        full = full.flaky(0.2);
    }
    let mut sim = Sim::new("pbt-localgit", &yaml)
        .base_file(FILES[0], &file_with(LINES, ""))
        .base_file(FILES[1], &file_with(LINES, ""))
        .check(ci("quick", "1m").pr_only())
        .check(full)
        .flake_seed(p.seed)
        .faults(None);
    for (i, pr) in p.prs.iter().enumerate() {
        let n = i as u64 + 1;
        let marker = match pr.marker {
            1 => " BUG",
            2 => " SEM_A",
            3 => " SEM_B",
            _ => "",
        };
        let content = file_with(pr.line, &format!("edit by #{n}{marker}"));
        sim = sim
            .pr(n, &[(FILES[pr.file], content.as_str())])
            .at(&format!("{}s", pr.open_at), Action::Open(n));
    }
    for (k, (t, file, line)) in p.pushes.iter().enumerate() {
        sim = sim.at(
            &format!("{t}s"),
            Action::PushBase {
                branch: "main".into(),
                files: vec![(
                    FILES[*file].to_string(),
                    file_with(*line, &format!("direct push {k}")),
                )],
            },
        );
    }
    let mut s = sim.build();
    s.max_time = 3 * 86400;
    s
}

proptest! {
    #![proptest_config(config())]

    /// I0–I4 hold after every step on real git.
    #[test]
    fn invariants_hold_on_real_git(p in plan()) {
        let s = scenario(&p);
        let mut run = Simulation::on(&s, Faults::none(), LocalGit::new().expect("git"), true);
        run.run(&s);
        let mut violations = Vec::new();
        for (id, r) in &run.checker.results {
            for v in &r.violations {
                violations.push(format!("{id}: {v}"));
            }
        }
        let log: String = run
            .log
            .iter()
            .map(|l| format!("{} {} {}\n", l.t, l.kind, l.text))
            .collect();
        prop_assert!(violations.is_empty(), "{}\n{log}", violations.join("\n"));
        // I1 is checked per landing; a case where nothing lands has no I1 checks.
        for id in ["I0", "I2", "I3", "I4"] {
            prop_assert!(run.checker.results[id].checks > 0, "{id} never checked\n{log}");
        }
        if !run.merged_order.is_empty() {
            prop_assert!(run.checker.results["I1"].checks > 0, "I1 never checked\n{log}");
        }
        prop_assert!(run.forge.repo.fsck(), "git fsck failed");
        prop_assert!(run.forge.processes > 0);
    }
}
