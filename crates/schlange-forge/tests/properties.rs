//! Property tests over random scenarios: random queue settings, pull requests with conflicting
//! files, markers that break or hang the full suite, semantic conflicts, flaky checks, human
//! actions (labels, pushes, closes, drafts, manual merges, direct pushes to main, pushes to
//! queue branches) and webhook faults. The runner checks I0–I4 after every step and I2/I3 at
//! quiescence; any violation fails the case.
//!
//! I5 (same end state with perfect and disturbed delivery) only holds when the outcome does not
//! depend on timing; `delivery_does_not_change_the_end_state` generates such scenarios (no
//! flakes, no conflicts between pull requests, no human actions that race the queue).
//!
//! Deterministic: a fixed seed. `SCHLANGE_PBT_CASES` sets the number of cases (default 40 per
//! property); e.g. `SCHLANGE_PBT_CASES=2000 cargo test --release -p schlange-forge --test
//! properties` for a long run.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

use schlange_forge::Scenario;
use schlange_forge::dsl::{Sim, ci};
use schlange_forge::runner::Simulation;
use schlange_forge::sim::{Action, Faults};

fn config() -> Config {
    let cases = std::env::var("SCHLANGE_PBT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(40);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x5c41_a46e),
        failure_persistence: None,
        max_shrink_iters: 200,
        ..Config::default()
    }
}

#[derive(Clone, Debug)]
struct Settings {
    batch: (u32, Option<u32>),
    max_parallel: u32,
    limit: Option<u32>,
    timeout: bool,
    single_step: bool,
}

impl Settings {
    fn yaml(&self) -> String {
        let batch = match self.batch {
            (n, None) => format!("{n}"),
            (min, Some(max)) => format!("{{min: {min}, max: {max}}}"),
        };
        let mut rule = format!(
            "  - name: default\n    merge_method: squash\n    batch_size: {batch}\n    batch_max_wait_time: 1 min\n"
        );
        if let Some(l) = self.limit {
            rule.push_str(&format!("    batch_max_failure_resolution_attempts: {l}\n"));
        }
        if self.timeout {
            rule.push_str("    checks_timeout: 20 min\n");
        }
        rule.push_str(
            "    queue_conditions:\n      - base = main\n      - -draft\n      - -label = halt\n      - check-success = quick\n",
        );
        if !self.single_step {
            rule.push_str("    merge_conditions:\n      - check-success = full\n");
        }
        format!(
            "merge_queue:\n  max_parallel_checks: {}\nqueue_rules:\n{rule}pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n",
            self.max_parallel
        )
    }
}

fn settings() -> impl Strategy<Value = Settings> {
    (
        prop_oneof![
            (1u32..=4).prop_map(|n| (n, None)),
            (1u32..=2, 2u32..=4).prop_map(|(a, b)| (a, Some(b)))
        ],
        1u32..=4,
        prop_oneof![Just(None), (0u32..=2).prop_map(Some)],
        any::<bool>(),
        prop::bool::weighted(0.2),
    )
        .prop_map(
            |(batch, max_parallel, limit, timeout, single_step)| Settings {
                batch,
                max_parallel,
                limit,
                timeout,
                single_step,
            },
        )
}

#[derive(Clone, Debug)]
struct PrPlan {
    path: usize,
    /// 0 none, 1 BUG, 2 HANG, 3 SEM_A, 4 SEM_B.
    marker: u8,
    open_at: u64,
}

#[derive(Clone, Debug)]
enum Human {
    Label(usize, bool),
    Push(usize, bool),
    Close(usize),
    Draft(usize),
    Ready(usize),
    MergeByHand(usize),
    PushBase(usize),
    PushQueueBranch(usize),
}

#[derive(Clone, Debug)]
struct Plan {
    settings: Settings,
    prs: Vec<PrPlan>,
    humans: Vec<(u64, Human)>,
    flake: bool,
    faults: Faults,
}

const PATHS: [&str; 6] = ["a.rs", "b.rs", "c.rs", "d.rs", "e.rs", "f.rs"];

fn marker_text(m: u8) -> &'static str {
    match m {
        1 => " BUG",
        2 => " HANG",
        3 => " SEM_A",
        4 => " SEM_B",
        _ => "",
    }
}

fn faults() -> impl Strategy<Value = Faults> {
    (
        any::<u64>(),
        0.0f64..0.4,
        0.0f64..0.4,
        0.0f64..0.6,
        0u64..120,
        30u64..180,
    )
        .prop_map(
            |(seed, drop, duplicate, delay, max_delay, reconcile_every)| Faults {
                seed,
                drop,
                duplicate,
                delay,
                max_delay,
                reconcile_every,
            },
        )
}

fn human() -> impl Strategy<Value = Human> {
    prop_oneof![
        (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Label(n, on)),
        (0usize..6, any::<bool>()).prop_map(|(n, fix)| Human::Push(n, fix)),
        (0usize..6).prop_map(Human::Close),
        (0usize..6).prop_map(Human::Draft),
        (0usize..6).prop_map(Human::Ready),
        (0usize..6).prop_map(Human::MergeByHand),
        (0usize..6).prop_map(Human::PushBase),
        (0usize..6).prop_map(Human::PushQueueBranch),
    ]
}

fn plan() -> impl Strategy<Value = Plan> {
    (
        settings(),
        prop::collection::vec(
            (
                0usize..6,
                prop::sample::select(vec![0u8, 0, 0, 1, 2, 3, 4]),
                0u64..1800,
            )
                .prop_map(|(path, marker, open_at)| PrPlan {
                    path,
                    marker,
                    open_at,
                }),
            1..=6,
        ),
        prop::collection::vec((0u64..3600, human()), 0..=5),
        prop::bool::weighted(0.3),
        faults(),
    )
        .prop_map(|(settings, prs, humans, flake, faults)| Plan {
            settings,
            prs,
            humans,
            flake,
            faults,
        })
}

fn scenario(p: &Plan) -> Scenario {
    let mut full = ci("full", "10m")
        .queue_only()
        .fails_on("BUG")
        .fails_with_all(&["SEM_A", "SEM_B"])
        .hangs_on("HANG");
    if p.flake {
        full = full.flaky(0.2);
    }
    let mut quick = ci("quick", "1m").pr_only();
    if p.settings.single_step {
        // Single-step CI: the merge conditions are the queue conditions, so the quick check
        // runs everywhere and carries the content rules.
        quick = ci("quick", "2m")
            .fails_on("BUG")
            .fails_with_all(&["SEM_A", "SEM_B"])
            .hangs_on("HANG");
        if p.flake {
            quick = quick.flaky(0.2);
        }
    }
    let mut sim = Sim::new("pbt", &p.settings.yaml())
        .check(quick)
        .flake_seed(p.faults.seed)
        .faults(Some(p.faults.clone()));
    if !p.settings.single_step {
        sim = sim.check(full);
    }
    let n = p.prs.len();
    for (i, pr) in p.prs.iter().enumerate() {
        let num = i as u64 + 1;
        // Without a fixed checks_timeout (`auto` has no history here, so no timeout), a hanging
        // check blocks the queue forever, by design and as in Mergify: I2 cannot hold, so
        // hangs only appear with a timeout.
        let marker = if pr.marker == 2 && !p.settings.timeout {
            0
        } else {
            pr.marker
        };
        let content = format!("#{num}{}\n", marker_text(marker));
        sim = sim
            .pr(num, &[(PATHS[pr.path], content.as_str())])
            .at(&format!("{}s", pr.open_at), Action::Open(num));
    }
    for (k, (t, h)) in p.humans.iter().enumerate() {
        let at = format!("{t}s");
        let pr = |i: &usize| (*i % n) as u64 + 1;
        let action = match h {
            Human::Label(i, true) => Action::Label {
                number: pr(i),
                label: "halt".into(),
            },
            Human::Label(i, false) => Action::Unlabel {
                number: pr(i),
                label: "halt".into(),
            },
            Human::Push(i, fix) => Action::Push {
                number: pr(i),
                files: vec![(
                    PATHS[p.prs[*i % n].path].to_string(),
                    if *fix {
                        format!("fixed by push {k}\n")
                    } else {
                        format!("push {k} BUG\n")
                    },
                )],
            },
            Human::Close(i) => Action::Close { number: pr(i) },
            Human::Draft(i) => Action::Draft { number: pr(i) },
            Human::Ready(i) => Action::Ready { number: pr(i) },
            Human::MergeByHand(i) => Action::MergeManually(pr(i)),
            Human::PushBase(i) => Action::PushBase {
                branch: "main".into(),
                files: vec![(PATHS[*i].to_string(), format!("direct {k}\n"))],
            },
            Human::PushQueueBranch(i) => Action::PushQueueBranch(pr(i)),
        };
        sim = sim.at(&at, action);
    }
    let mut s = sim.build();
    s.max_time = 3 * 86400;
    s
}

fn violations(s: &Scenario, faults: Faults) -> Vec<String> {
    let mut sim = Simulation::new(s, faults);
    sim.run(s);
    let mut out = Vec::new();
    for (id, r) in &sim.checker.results {
        for v in &r.violations {
            out.push(format!("{id}: {v}"));
        }
    }
    if !out.is_empty() {
        out.push(
            sim.log
                .iter()
                .map(|l| format!("{} {} {}", l.t, l.kind, l.text))
                .collect::<Vec<_>>()
                .join("\n"),
        );
    }
    out
}

proptest! {
    #![proptest_config(config())]

    /// I0–I4 hold after every step, with perfect and with disturbed delivery.
    #[test]
    fn invariants_hold_in_every_step(p in plan()) {
        let s = scenario(&p);
        let perfect = violations(&s, Faults::none());
        prop_assert!(perfect.is_empty(), "perfect delivery:\n{}", perfect.join("\n"));
        let disturbed = violations(&s, p.faults.clone());
        prop_assert!(disturbed.is_empty(), "disturbed delivery:\n{}", disturbed.join("\n"));
    }
}

#[derive(Clone, Debug)]
struct CalmPlan {
    settings: Settings,
    /// Per pull request: open time and whether it breaks the full suite (or hangs).
    prs: Vec<(u64, u8)>,
    base_pushes: Vec<u64>,
    faults: Faults,
}

fn calm_plan() -> impl Strategy<Value = CalmPlan> {
    (
        settings().prop_map(|mut s| {
            // No limit: with a limit, which pull requests are dropped depends on queue order.
            s.limit = None;
            s.timeout = true;
            s
        }),
        prop::collection::vec(
            (0u64..1800, prop::sample::select(vec![0u8, 0, 0, 1, 2])),
            1..=6,
        ),
        prop::collection::vec(0u64..3600, 0..=3),
        faults(),
    )
        .prop_map(|(settings, prs, base_pushes, faults)| CalmPlan {
            settings,
            prs,
            base_pushes,
            faults,
        })
}

fn calm_scenario(p: &CalmPlan) -> Scenario {
    let full = ci("full", "10m")
        .queue_only()
        .fails_on("BUG")
        .hangs_on("HANG");
    let quick = if p.settings.single_step {
        ci("quick", "2m").fails_on("BUG").hangs_on("HANG")
    } else {
        ci("quick", "1m").pr_only()
    };
    let mut sim = Sim::new("pbt-calm", &p.settings.yaml())
        .check(quick)
        .faults(Some(p.faults.clone()));
    if !p.settings.single_step {
        sim = sim.check(full);
    }
    for (i, (t, m)) in p.prs.iter().enumerate() {
        let n = i as u64 + 1;
        // Every pull request has its own file: no conflicts between them.
        let path = format!("pr{n}.rs");
        let content = format!("#{n}{}\n", marker_text(*m));
        sim = sim
            .pr(n, &[(path.as_str(), content.as_str())])
            .at(&format!("{t}s"), Action::Open(n));
    }
    for (k, t) in p.base_pushes.iter().enumerate() {
        sim = sim.at(
            &format!("{t}s"),
            Action::PushBase {
                branch: "main".into(),
                files: vec![(format!("direct{k}.rs"), "direct\n".into())],
            },
        );
    }
    let mut s = sim.build();
    s.max_time = 3 * 86400;
    s
}

proptest! {
    #![proptest_config(config())]

    /// I5: when the outcome does not depend on timing, disturbed delivery (dropped,
    /// duplicated, delayed and reordered webhooks, repaired by reconciliation) reaches the same
    /// end state as perfect delivery.
    #[test]
    fn delivery_does_not_change_the_end_state(p in calm_plan()) {
        let s = calm_scenario(&p);
        let report = schlange_forge::run(&s);
        let i5 = report.invariant("I5");
        prop_assert_eq!(i5.status, "checked", "{}", report.to_text());
        for i in &report.invariants {
            prop_assert!(i.violations.is_empty(), "{}", report.to_text());
        }
        // Sanity: every pull request without a marker merged.
        for (i, (_, m)) in p.prs.iter().enumerate() {
            let n = i as u64 + 1;
            if *m == 0 {
                prop_assert_eq!(report.outcome(n), "merged", "{}", report.to_text());
            }
        }
    }
}
