//! Property tests over random scenarios: random queue settings, pull requests with conflicting
//! files, markers that break or hang the full suite, semantic conflicts, flaky checks, human
//! actions (labels read by the queue conditions and by the merge conditions only, pushes,
//! closes, drafts, manual merges, direct pushes to main, pushes to queue branches, comment
//! commands `queue`, `dequeue`, `refresh` and `rebase` by a maintainer or by someone the
//! restrictions refuse, pushes by someone else than the author; every third pull request is
//! a human's that only a `queue` command queues; queue settings include
//! `reset_on_external_merge: never`, `max_checks_retries`, `allow_queue_branch_edit` and an
//! explicit `checks_timeout: auto`), webhook
//! faults, forge faults (lost, late, duplicated and lying replies, base races, lagging reads, CI
//! timing) and engine crashes. The runner checks I0–I4 after every step and I2/I3 at
//! quiescence; any violation fails the case.
//!
//! - `invariants_hold_in_every_step`: perfect and disturbed webhook delivery.
//! - `invariants_hold_under_forge_faults_and_crashes`: the same plans with forge faults and
//!   random crashes (restart from the journal).
//! - `innocent_pull_requests_are_never_blamed`: one deterministic culprit (a broken or a
//!   hanging pull request), no flakes, no semantic conflicts: no other pull request is ever
//!   dequeued for CHECKS_FAILED or CHECKS_TIMEOUT, and without a resolution limit none for
//!   DROPPED_BY_BISECTION_ELIMINATION or BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS (with a limit
//!   the docs drop pull requests untested on purpose).
//! - `delivery_does_not_change_the_end_state` (I5): when the outcome does not depend on timing
//!   (no flakes, no conflicts between pull requests, label toggles only around a pull
//!   request's own life), disturbed webhooks, forge faults and crashes reach the same end
//!   state as perfect delivery, dequeue reasons included.
//!
//! Deterministic: a fixed seed. `SCHLANGE_PBT_CASES` sets the number of cases (default 64 per
//! property); e.g. `SCHLANGE_PBT_CASES=2000 cargo test --release -p schlange-forge --test
//! properties` for a long run.

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

use schlange_core::engine::{Control, Freeze};
use schlange_forge::Scenario;
use schlange_forge::dsl::{Sim, ci};
use schlange_forge::faults::{CrashPlan, ForgeFaults};
use schlange_forge::runner::{EndState, Simulation, clock};
use schlange_forge::sim::{Action, CiTiming, Faults};

fn config() -> Config {
    let cases = std::env::var("SCHLANGE_PBT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(64);
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
    /// `-label = wip` in the merge conditions only (an attribute the queue conditions do not
    /// read).
    wip: bool,
    /// `reset_on_external_merge: never`.
    never: bool,
    /// `max_checks_retries`.
    retries: u32,
    /// `allow_queue_branch_edit: true` (with `squash` the engine refuses the edit, as the
    /// option needs a method that lands the batch branch).
    branch_edit: bool,
    /// `checks_timeout: auto` written out (the default; without history it applies no
    /// timeout, so a case with it is the same as without a timeout).
    auto: bool,
    /// `merge_method`.
    method: &'static str,
    /// `merge_queue.mode`: serial, isolated, or parallel with file-pattern scopes.
    mode: &'static str,
    /// `priority_rules`: `urgent` is high (and may interrupt checks when `interrupt`), `later`
    /// low.
    priority: bool,
    interrupt: bool,
    /// `skip_intermediate_results: true`.
    skip: bool,
    /// A second queue rule `hotfix` (queue conditions `label = urgent`) listed first.
    two_queues: bool,
}

impl Settings {
    fn yaml(&self) -> String {
        let batch = match self.batch {
            (n, None) => format!("{n}"),
            (min, Some(max)) => format!("{{min: {min}, max: {max}}}"),
        };
        let method = self.method;
        let mut rule = format!(
            "  - name: default\n    merge_method: {method}\n    batch_size: {batch}\n    batch_max_wait_time: 1 min\n"
        );
        if let Some(l) = self.limit {
            rule.push_str(&format!("    batch_max_failure_resolution_attempts: {l}\n"));
        }
        if self.timeout {
            rule.push_str("    checks_timeout: 20 min\n");
        } else if self.auto {
            rule.push_str("    checks_timeout: auto\n");
        }
        if self.retries > 0 {
            rule.push_str(&format!("    max_checks_retries: {}\n", self.retries));
        }
        if self.branch_edit {
            rule.push_str("    allow_queue_branch_edit: true\n");
        }
        let queue = "      - base = main\n      - -draft\n      - -label = halt\n      - check-success = quick\n";
        rule.push_str("    queue_conditions:\n");
        rule.push_str(queue);
        match (self.single_step, self.wip) {
            (false, false) => {
                rule.push_str("    merge_conditions:\n      - check-success = full\n")
            }
            (false, true) => rule.push_str(
                "    merge_conditions:\n      - check-success = full\n      - -label = wip\n",
            ),
            (true, true) => {
                rule.push_str("    merge_conditions:\n");
                rule.push_str(queue);
                rule.push_str("      - -label = wip\n");
            }
            (true, false) => {}
        }
        let mut mq = String::new();
        if self.never {
            mq.push_str("  reset_on_external_merge: never\n");
        }
        if self.mode != "serial" {
            mq.push_str(&format!("  mode: {}\n", self.mode));
        }
        if self.skip {
            mq.push_str("  skip_intermediate_results: true\n");
        }
        let mut extra = String::new();
        if self.mode == "parallel" {
            extra.push_str(
                "scopes:\n  source:\n    files:\n      left:\n        include: [a.rs, b.rs]\n      right:\n        include: [c.rs, d.rs]\n  barrier_files:\n    include: [f.rs]\n",
            );
        }
        if self.priority {
            extra.push_str(&format!(
                "priority_rules:\n  - name: urgent\n    conditions: [label = urgent]\n    priority: high\n    allow_checks_interruption: {}\n  - name: later\n    conditions: [label = later]\n    priority: low\n",
                self.interrupt
            ));
        }
        let hotfix = if self.two_queues {
            format!(
                "  - name: hotfix\n    merge_method: {method}\n    queue_conditions:\n      - base = main\n      - -draft\n      - -label = halt\n      - label = urgent\n      - check-success = quick\n    merge_conditions:\n      - check-success = {}\n",
                if self.single_step { "quick" } else { "full" }
            )
        } else {
            String::new()
        };
        format!(
            "merge_queue:\n  max_parallel_checks: {}\n{mq}{extra}queue_rules:\n{hotfix}{rule}pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n",
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
        prop::bool::weighted(0.4),
        (
            prop::bool::weighted(0.3),
            prop::sample::select(vec![0u32, 0, 1, 2]),
            prop::bool::weighted(0.15),
            prop::bool::weighted(0.3),
        ),
        (
            prop::sample::select(vec![
                "squash",
                "squash",
                "merge",
                "rebase",
                "fast-forward",
                "merge-batch",
            ]),
            prop::sample::select(vec!["serial", "serial", "serial", "isolated", "parallel"]),
            prop::bool::weighted(0.4),
            any::<bool>(),
            prop::bool::weighted(0.25),
            prop::bool::weighted(0.2),
        ),
    )
        .prop_map(
            |(
                batch,
                max_parallel,
                limit,
                timeout,
                single_step,
                wip,
                (never, retries, branch_edit, auto),
                (method, mode, priority, interrupt, skip, two_queues),
            )| {
                // Mergify refuses fast-forward in parallel and isolated mode (docs: merge
                // strategies); such a configuration is never run.
                let mode = if method == "fast-forward" {
                    "serial"
                } else {
                    mode
                };
                Settings {
                    batch,
                    max_parallel,
                    limit,
                    timeout,
                    single_step,
                    wip,
                    never,
                    retries,
                    branch_edit,
                    auto,
                    method,
                    mode,
                    priority,
                    interrupt,
                    skip: skip && mode != "isolated",
                    two_queues,
                }
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
    Wip(usize, bool),
    Push(usize, bool),
    Close(usize),
    Draft(usize),
    Ready(usize),
    MergeByHand(usize),
    PushBase(usize),
    PushQueueBranch(usize),
    /// A comment command: `queue`, `dequeue`, `refresh` or `rebase` (answered as not
    /// supported), by the maintainer (write permission) or by mallory (read, refused).
    Command(usize, u8, bool),
    /// A push by mallory (someone else than the author): cancels a waiting `queue` command.
    PushByOther(usize),
    /// The `urgent` label on or off (priority rules, the `hotfix` queue).
    Urgent(usize, bool),
    /// An operator pauses (true) or unpauses (false) the queue.
    Pause(bool),
    /// A freeze of main for this many seconds, with `label = urgent` as the exception.
    Freeze(u64),
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

fn forge_faults() -> impl Strategy<Value = ForgeFaults> {
    (
        (
            any::<u64>(),
            0.0f64..0.3,
            0.0f64..0.3,
            0.0f64..0.6,
            0u64..150,
        ),
        (0.0f64..0.2, 0.0f64..0.2, 0.0f64..0.3, 0.0f64..0.5, 0u64..60),
        (0.0f64..0.4, 0u64..120, 0u64..90),
    )
        .prop_map(
            |(
                (seed, reply_drop, reply_duplicate, reply_delay, max_reply_delay),
                (error, lie, base_race, stale_read, max_lag),
                (jitter, max_create_delay, max_cancel_delay),
            )| ForgeFaults {
                seed,
                reply_drop,
                reply_duplicate,
                reply_delay,
                max_reply_delay,
                error,
                lie,
                base_race,
                stale_read,
                max_lag,
                ci: CiTiming {
                    seed,
                    jitter,
                    max_create_delay,
                    max_cancel_delay,
                },
            },
        )
}

fn human() -> impl Strategy<Value = Human> {
    prop_oneof![
        (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Label(n, on)),
        (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Wip(n, on)),
        (0usize..6, any::<bool>()).prop_map(|(n, fix)| Human::Push(n, fix)),
        (0usize..6).prop_map(Human::Close),
        (0usize..6).prop_map(Human::Draft),
        (0usize..6).prop_map(Human::Ready),
        (0usize..6).prop_map(Human::MergeByHand),
        (0usize..6).prop_map(Human::PushBase),
        (0usize..6).prop_map(Human::PushQueueBranch),
        (0usize..6, 0u8..4, prop::bool::weighted(0.8))
            .prop_map(|(n, c, trusted)| Human::Command(n, c, trusted)),
        (0usize..6).prop_map(Human::PushByOther),
        (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Urgent(n, on)),
        any::<bool>().prop_map(Human::Pause),
        (60u64..2400).prop_map(Human::Freeze),
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
        prop::collection::vec((0u64..3600, human()), 0..=6),
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

fn human_action(p: &Plan, k: usize, h: &Human) -> Action {
    let n = p.prs.len();
    let pr = |i: &usize| (*i % n) as u64 + 1;
    match h {
        Human::Label(i, true) => Action::Label {
            number: pr(i),
            label: "halt".into(),
        },
        Human::Label(i, false) => Action::Unlabel {
            number: pr(i),
            label: "halt".into(),
        },
        Human::Wip(i, true) => Action::Label {
            number: pr(i),
            label: "wip".into(),
        },
        Human::Wip(i, false) => Action::Unlabel {
            number: pr(i),
            label: "wip".into(),
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
        Human::Command(i, c, trusted) => Action::Comment {
            number: pr(i),
            by: if *trusted { "maintainer" } else { "mallory" }.into(),
            body: format!(
                "@mergifyio {}",
                ["queue", "dequeue", "refresh", "rebase"][*c as usize % 4]
            ),
        },
        Human::PushByOther(i) => Action::PushAs {
            number: pr(i),
            by: "mallory".into(),
            files: vec![(
                PATHS[p.prs[*i % n].path].to_string(),
                format!("pushed by mallory {k}\n"),
            )],
        },
        Human::Urgent(i, true) => Action::Label {
            number: pr(i),
            label: "urgent".into(),
        },
        Human::Urgent(i, false) => Action::Unlabel {
            number: pr(i),
            label: "urgent".into(),
        },
        Human::Pause(true) => Action::Control(Control::Pause {
            reason: format!("incident {k}"),
            by: None,
        }),
        Human::Pause(false) => Action::Control(Control::Unpause),
        Human::Freeze(len) => Action::Control(Control::Freeze {
            freeze: Freeze {
                id: k as u64,
                reason: format!("freeze {k}"),
                start: 0,
                end: Some(*len + 3600),
                matching: vec!["base = main".into()],
                exclude: vec!["label = urgent".into()],
            },
        }),
    }
}

/// Every third pull request is a human's: no rule queues it, only a `queue` command.
fn author(num: u64) -> &'static str {
    if num.is_multiple_of(3) {
        "human"
    } else {
        "bot"
    }
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
        .permission("maintainer", "write")
        .permission("mallory", "read")
        .check(quick)
        .flake_seed(p.faults.seed)
        .faults(Some(p.faults.clone()));
    if !p.settings.single_step {
        sim = sim.check(full);
    }
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
            .pr_with(num, &[(PATHS[pr.path], content.as_str())], |d| {
                d.author = author(num).into()
            })
            .at(&format!("{}s", pr.open_at), Action::Open(num));
    }
    for (k, (t, h)) in p.humans.iter().enumerate() {
        sim = sim.at(&format!("{t}s"), human_action(p, k, h));
    }
    // A pause always ends: an operator unpauses after the last action.
    if p.humans
        .iter()
        .any(|(_, h)| matches!(h, Human::Pause(true)))
    {
        sim = sim.at("3700s", Action::Control(Control::Unpause));
    }
    let mut s = sim.build();
    s.max_time = 3 * 86400;
    s
}

fn outcome_of(
    s: &Scenario,
    faults: Faults,
    forge: Option<ForgeFaults>,
    crash: Option<CrashPlan>,
) -> (Vec<String>, Simulation, String) {
    let mut sim = Simulation::new(s, faults);
    if let Some(f) = forge {
        sim = sim.with_forge_faults(f);
    }
    if let Some(c) = crash {
        sim = sim.with_crashes(c);
    }
    sim.run(s);
    let mut out = Vec::new();
    for (id, r) in &sim.checker.results {
        for v in &r.violations {
            out.push(format!("{id}: {v}"));
        }
    }
    let log = sim
        .log
        .iter()
        .map(|l| format!("{} {} {}", clock(l.t), l.kind, l.text))
        .collect::<Vec<_>>()
        .join("\n");
    (out, sim, log)
}

fn violations(s: &Scenario, faults: Faults) -> Vec<String> {
    let (mut v, _, log) = outcome_of(s, faults, None, None);
    if !v.is_empty() {
        v.push(log);
    }
    v
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

    /// I0–I4 hold, the queue settles and nothing lands twice when the forge's replies are lost,
    /// late, duplicated or lie, pushes race the landings, reads lag, CI timing varies, and the
    /// engine crashes at random side effects and restarts from its journal.
    #[test]
    fn invariants_hold_under_forge_faults_and_crashes(
        p in plan(),
        f in forge_faults(),
        crash_rate in prop_oneof![Just(0.0f64), 0.0f64..0.08],
    ) {
        let s = scenario(&p);
        let crash = CrashPlan::random(f.seed, crash_rate, 4);
        let (v, sim, log) = outcome_of(&s, p.faults.clone(), Some(f), Some(crash));
        prop_assert!(v.is_empty(), "{}\n{}\nwire: {:?}", v.join("\n"), log, sim.wire.stats);
    }
}

#[derive(Clone, Debug)]
struct OraclePlan {
    settings: Settings,
    /// Per pull request: path and open time.
    prs: Vec<(usize, u64)>,
    culprit: usize,
    hang: bool,
    humans: Vec<(u64, Human)>,
    faults: Faults,
}

fn oracle_plan() -> impl Strategy<Value = OraclePlan> {
    (
        settings(),
        prop::collection::vec((0usize..6, 0u64..1800), 2..=6),
        any::<prop::sample::Index>(),
        any::<bool>(),
        prop::collection::vec(
            (
                0u64..3600,
                prop_oneof![
                    (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Label(n, on)),
                    (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Wip(n, on)),
                    // Only fixing pushes: a push that adds a bug makes a second culprit.
                    (0usize..6).prop_map(|n| Human::Push(n, true)),
                    (0usize..6).prop_map(Human::Close),
                    (0usize..6).prop_map(Human::Draft),
                    (0usize..6).prop_map(Human::Ready),
                    (0usize..6).prop_map(Human::MergeByHand),
                    (0usize..6).prop_map(Human::PushBase),
                    (0usize..6).prop_map(Human::PushQueueBranch),
                    (0usize..6, 0u8..4, prop::bool::weighted(0.8))
                        .prop_map(|(n, c, trusted)| Human::Command(n, c, trusted)),
                    (0usize..6).prop_map(Human::PushByOther),
                    (0usize..6, any::<bool>()).prop_map(|(n, on)| Human::Urgent(n, on)),
                    any::<bool>().prop_map(Human::Pause),
                    (60u64..2400).prop_map(Human::Freeze),
                ],
            ),
            0..=5,
        ),
        faults(),
    )
        .prop_map(|(settings, prs, culprit, hang, humans, faults)| {
            let culprit = culprit.index(prs.len());
            let hang = hang && settings.timeout;
            OraclePlan {
                settings,
                prs,
                culprit,
                hang,
                humans,
                faults,
            }
        })
}

fn oracle_scenario(p: &OraclePlan) -> Scenario {
    let full = ci("full", "10m")
        .queue_only()
        .fails_on("BUG")
        .hangs_on("HANG");
    let quick = if p.settings.single_step {
        ci("quick", "2m").fails_on("BUG").hangs_on("HANG")
    } else {
        ci("quick", "1m").pr_only()
    };
    let mut sim = Sim::new("pbt-oracle", &p.settings.yaml())
        .permission("maintainer", "write")
        .permission("mallory", "read")
        .check(quick)
        .faults(Some(p.faults.clone()));
    if !p.settings.single_step {
        sim = sim.check(full);
    }
    let n = p.prs.len();
    for (i, (path, t)) in p.prs.iter().enumerate() {
        let num = i as u64 + 1;
        let marker = match (i == p.culprit, p.hang) {
            (true, true) => " HANG",
            (true, false) => " BUG",
            _ => "",
        };
        let content = format!("#{num}{marker}\n");
        sim = sim
            .pr_with(num, &[(PATHS[*path], content.as_str())], |d| {
                d.author = author(num).into()
            })
            .at(&format!("{t}s"), Action::Open(num));
    }
    let as_plan = Plan {
        settings: p.settings.clone(),
        prs: p
            .prs
            .iter()
            .map(|(path, t)| PrPlan {
                path: *path,
                marker: 0,
                open_at: *t,
            })
            .collect(),
        humans: vec![],
        flake: false,
        faults: p.faults.clone(),
    };
    for (k, (t, h)) in p.humans.iter().enumerate() {
        // Merging the culprit by hand puts the bug on main: every later batch fails.
        if matches!(h, Human::MergeByHand(i) if *i % n == p.culprit) {
            continue;
        }
        sim = sim.at(&format!("{t}s"), human_action(&as_plan, k, h));
    }
    if p.humans
        .iter()
        .any(|(_, h)| matches!(h, Human::Pause(true)))
    {
        sim = sim.at("3700s", Action::Control(Control::Unpause));
    }
    let mut s = sim.build();
    s.max_time = 3 * 86400;
    s
}

/// The dequeue codes in a log, per pull request: `dequeue #N from `q`: CODE …`.
fn dequeues(log: &str) -> Vec<(u64, String)> {
    log.lines()
        .filter_map(|l| {
            let rest = l.split("dequeue #").nth(1)?;
            let (n, rest) = rest.split_once(' ')?;
            let code = rest.split(": ").nth(1)?.split([' ', ':']).next()?;
            Some((n.parse().ok()?, code.to_string()))
        })
        .collect()
}

proptest! {
    #![proptest_config(config())]

    /// Innocence oracle: with one deterministic culprit, no other pull request is dequeued for a
    /// CI failure, a timeout or (without a limit) bisection elimination. Perfect delivery,
    /// disturbed webhooks, and forge faults with crashes.
    #[test]
    fn innocent_pull_requests_are_never_blamed(p in oracle_plan(), f in forge_faults()) {
        let s = oracle_scenario(&p);
        let culprit = p.culprit as u64 + 1;
        let mut forbidden = vec!["CHECKS_FAILED", "CHECKS_TIMEOUT"];
        if p.settings.limit.is_none() {
            forbidden.extend(["DROPPED_BY_BISECTION_ELIMINATION", "BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS"]);
        }
        let crash = CrashPlan::random(f.seed, 0.02, 2);
        for (name, faults, forge, crash) in [
            ("perfect", Faults::none(), None, None),
            ("webhook faults", p.faults.clone(), None, None),
            ("forge faults and crashes", p.faults.clone(), Some(f.clone()), Some(crash)),
        ] {
            let (v, _, log) = outcome_of(&s, faults, forge, crash);
            prop_assert!(v.is_empty(), "{name}: {}\n{log}", v.join("\n"));
            for (n, code) in dequeues(&log) {
                prop_assert!(
                    n == culprit || !forbidden.contains(&code.as_str()),
                    "{name}: innocent #{n} dequeued with {code} (culprit #{culprit})\n{log}"
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
enum Toggle {
    None,
    /// `halt` from the start, removed later: merges eventually.
    HaltUntil(u64),
    /// `halt` for good: never queued.
    HaltForever,
    /// `wip` (merge conditions only) from the start, removed later.
    WipUntil(u64),
}

#[derive(Clone, Debug)]
struct CalmPlan {
    settings: Settings,
    /// Per pull request: open time, marker (0 none, 1 BUG, 2 HANG) and a label toggle.
    prs: Vec<(u64, u8, Toggle)>,
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
            (
                0u64..1800,
                prop::sample::select(vec![0u8, 0, 0, 1, 2]),
                prop_oneof![
                    4 => Just(Toggle::None),
                    1 => (0u64..3600).prop_map(Toggle::HaltUntil),
                    1 => Just(Toggle::HaltForever),
                    1 => (0u64..3600).prop_map(Toggle::WipUntil),
                ],
            ),
            1..=6,
        ),
        prop::collection::vec(0u64..3600, 0..=3),
        faults(),
    )
        .prop_map(|(mut settings, prs, base_pushes, faults)| {
            settings.wip |= prs.iter().any(|p| matches!(p.2, Toggle::WipUntil(_)));
            CalmPlan {
                settings,
                prs,
                base_pushes,
                faults,
            }
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
    for (i, (t, m, toggle)) in p.prs.iter().enumerate() {
        let n = i as u64 + 1;
        // Every pull request has its own file: no conflicts between them.
        let path = format!("pr{n}.rs");
        let content = format!("#{n}{}\n", marker_text(*m));
        let label = match toggle {
            Toggle::None => None,
            Toggle::HaltUntil(_) | Toggle::HaltForever => Some("halt"),
            Toggle::WipUntil(_) => Some("wip"),
        };
        sim = sim
            .pr_with(n, &[(path.as_str(), content.as_str())], |d| {
                if let Some(l) = label {
                    d.labels.insert(l.to_string());
                }
            })
            .at(&format!("{t}s"), Action::Open(n));
        match toggle {
            Toggle::HaltUntil(off) | Toggle::WipUntil(off) => {
                sim = sim.at(
                    &format!("{}s", t + off),
                    Action::Unlabel {
                        number: n,
                        label: label.expect("label").into(),
                    },
                );
            }
            _ => {}
        }
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
    /// duplicated, delayed and reordered webhooks, repaired by reconciliation), forge faults and
    /// crashes reach the same end state as perfect delivery, dequeue reasons included.
    #[test]
    fn delivery_does_not_change_the_end_state(p in calm_plan(), f in forge_faults()) {
        let s = calm_scenario(&p);
        let report = schlange_forge::run(&s);
        let i5 = report.invariant("I5");
        prop_assert_eq!(i5.status, "checked", "{}", report.to_text());
        for i in &report.invariants {
            prop_assert!(i.violations.is_empty(), "{}", report.to_text());
        }
        // Sanity: every pull request without a marker and not halted for good merged.
        for (i, (_, m, toggle)) in p.prs.iter().enumerate() {
            let n = i as u64 + 1;
            let expected = match (m, toggle) {
                (_, Toggle::HaltForever) => "open",
                (0, _) => "merged",
                _ => continue,
            };
            prop_assert_eq!(report.outcome(n), expected, "{}", report.to_text());
        }
        // The same end state with forge faults and crashes on top.
        let crash = CrashPlan::random(f.seed, 0.03, 3);
        let (v, sim, log) = outcome_of(&s, p.faults.clone(), Some(f), Some(crash));
        prop_assert!(v.is_empty(), "{}\n{log}", v.join("\n"));
        let mut end: EndState = sim.end_state();
        if sim.wire.stats.base_races > 0 {
            // Races push commits of their own to main: the trees differ by design.
            end.base_trees = report.end_state.base_trees.clone();
        }
        prop_assert_eq!(&end, &report.end_state, "forge faults and crashes\n{}", log);
    }
}
