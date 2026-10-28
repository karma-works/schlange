//! Property test over the fake GitHub: random pull requests, CI results, labels, pushes to
//! pull requests and to the base, time, API faults (5xx, lying 5xx, rate limits, transient
//! 405), webhook faults (lost, duplicated, reordered), `mergeable: null` and read lag, all
//! through HTTP against the real `schlange serve`. After the queue drains, the fake checks I0
//! (every base commit is a tested tree), I1 (no landing after a dequeue, at an untested head
//! or while `halt` was set; nothing lands twice) and I3 (no queue branch or draft left).
//!
//! Each case starts a fake and a service, so the default is small: `SCHLANGE_GH_PBT_CASES`
//! sets the number of cases (default 3), e.g. `SCHLANGE_GH_PBT_CASES=100 cargo test -p
//! schlange-github-fake --test properties` for a long run.

use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, HookFaults, Tester, guard_labels};

const CONFIG: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: 2\n    batch_max_wait_time: 1 minute\n    queue_conditions:\n      - base = main\n      - -label = halt\n      - check-success = ci\n    merge_conditions:\n      - check-success = ci\n\
                      pull_request_rules:\n  - name: all\n    conditions:\n      - base = main\n      - -label = halt\n    actions:\n      queue:\n";

#[derive(Clone, Debug)]
enum Op {
    /// A pull request changing file `f`; `breaks` makes the full suite on the draft fail.
    Open {
        f: u8,
        breaks: bool,
    },
    /// A new commit on the i-th pull request (modulo the number open).
    Push {
        i: u8,
        f: u8,
    },
    /// Add or remove `halt` on the i-th pull request.
    Halt {
        i: u8,
        add: bool,
    },
    /// A direct push to the base touching file `f`.
    BasePush {
        f: u8,
    },
    Advance {
        minutes: u8,
    },
    /// One API fault.
    Fault {
        kind: u8,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..5, prop::bool::weighted(0.2)).prop_map(|(f, breaks)| Op::Open { f, breaks }),
        1 => (any::<u8>(), 0u8..5).prop_map(|(i, f)| Op::Push { i, f }),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(i, add)| Op::Halt { i, add }),
        1 => (0u8..5).prop_map(|f| Op::BasePush { f }),
        3 => (1u8..4).prop_map(|minutes| Op::Advance { minutes }),
        2 => (0u8..6).prop_map(|kind| Op::Fault { kind }),
    ]
}

fn fault(kind: u8) -> FaultRule {
    match kind {
        0 => FaultRule::new("PUT", "/merge", FaultKind::Status(502), 2),
        1 => FaultRule::new("PUT", "/merge", FaultKind::Lie(502), 1),
        2 => FaultRule::new("POST", "/merges", FaultKind::Lie(504), 1),
        3 => FaultRule::new("GET", "/pulls/", FaultKind::Status(503), 3),
        4 => FaultRule::new("*", "/", FaultKind::RateLimit { retry_after: 1 }, 1),
        _ => FaultRule::new(
            "PUT",
            "/merge",
            FaultKind::StatusMessage(
                405,
                "Base branch was modified. Review and try the merge again.".into(),
            ),
            1,
        ),
    }
}

fn apply(t: &Running, prs: &mut Vec<u64>, op: &Op, n: usize) {
    match op {
        Op::Open { f, breaks } => {
            let content = if *breaks {
                format!("v{n} BREAKS_FULL_SUITE\n")
            } else {
                format!("v{n}\n")
            };
            let p = t
                .open_pr("someone")
                .file(&format!("src/f{f}-{n}.txt"), &content)
                .create();
            prs.push(p);
        }
        Op::Push { i, f } if !prs.is_empty() => {
            let p = prs[*i as usize % prs.len()];
            if t.gh.pr(p).is_some_and(|x| x.open) {
                t.gh.push_pr(p, &[(format!("src/push{f}-{n}.txt"), format!("p{n}\n"))]);
            }
        }
        Op::Halt { i, add } if !prs.is_empty() => {
            let p = prs[*i as usize % prs.len()];
            if t.gh.pr(p).is_some_and(|x| x.open) {
                t.gh.label(p, "halt", *add);
            }
        }
        Op::BasePush { f } => {
            t.gh.push_base(&[(format!("base/b{f}.txt"), format!("b{n}\n"))]);
        }
        Op::Advance { minutes } => t.advance(&format!("{}m", minutes)),
        Op::Fault { kind } => t.gh.add_fault(fault(*kind)),
        _ => {}
    }
    std::thread::sleep(Duration::from_millis(15));
}

/// Drives the queue until nothing is queued and no car is left (with faults off).
fn drain(t: &Running) {
    t.gh.clear_faults();
    t.gh.set_hook_faults(HookFaults::default());
    for _ in 0..60 {
        t.settle();
        let s = t.status().unwrap();
        if s.queued.is_empty() && s.cars == 0 && t.gh.open_queue_drafts().is_empty() {
            return;
        }
        t.advance("2m");
    }
    let s = t.status().unwrap();
    panic!(
        "the queue did not drain: queued {:?}, cars {}, pending {:?}\nlog:\n  {}",
        s.queued,
        s.cars,
        s.pending,
        s.log_tail.join("\n  ")
    );
}

fn config() -> Config {
    let cases = std::env::var("SCHLANGE_GH_PBT_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x6170_6969),
        failure_persistence: None,
        max_shrink_iters: 16,
        ..Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn the_queue_on_the_fake_github_keeps_its_invariants(
        ops in prop::collection::vec(op(), 4..18),
        drop in prop::sample::select(vec![0.0, 0.0, 0.3]),
        duplicate in prop::sample::select(vec![0.0, 0.3]),
        delay_ms in 0u64..30,
        mergeable_lag in 0u32..3,
        read_lag in 0u32..2,
        seed in any::<u64>(),
    ) {
        let t = Tester::new(CONFIG)
            .ci(CiRule::success("ci", CiOn::PullRequest))
            .ci(CiRule::fails_on("ci", CiOn::Queue, "BREAKS_FULL_SUITE"))
            .merge_guard(guard_labels(&["halt"]))
            .reconcile_every("60s")
            .hook_faults(HookFaults {
                drop,
                duplicate,
                max_delay: Duration::from_millis(delay_ms),
                seed,
            })
            .start();
        t.gh.set_mergeable_lag(mergeable_lag);
        t.gh.set_read_lag(read_lag);
        let mut prs = Vec::new();
        for (n, o) in ops.iter().enumerate() {
            apply(&t, &mut prs, o, n);
        }
        drain(&t);
        let mut v = t.gh.invariant_violations(true);
        v.extend(t.dequeued_but_landed());
        let s = t.status().unwrap();
        let halted = s.halted.clone();
        t.shutdown();
        prop_assert!(v.is_empty(), "violations: {v:?}\nops: {ops:?}");
        // A halt only after a tree mismatch (MQ-02), which no op here provokes.
        prop_assert!(halted.is_empty(), "halted: {halted:?}\nops: {ops:?}");
    }
}

// ---------------------------------------------------------------------------------------
// A bors-style stress test with Arztagent's real `.mergify.yml` (batch of five, `ci-ok` on the
// draft, `#check-failure = 0`, the constitution regex): many pull requests, a full suite that
// breaks for marked pull requests, checks that fail once and then pass (flakes), pushes while
// queued, a moving base, drafts, `halt`, API faults, webhook faults and restarts of `serve`
// (a crash: memory lost, the journal stays). Whatever happens, I0 (every commit on the base is
// a tree the full suite passed) and I1 hold, and nothing is left behind.

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");

#[derive(Clone, Debug)]
enum AOp {
    /// 0 plain, 1 breaks the suite, 2 flaky (fails once on the draft), 3 draft, 4 constitution path.
    Open { kind: u8, f: u8 },
    Push { i: u8, f: u8 },
    Halt { i: u8, add: bool },
    BasePush { f: u8 },
    Advance { minutes: u8 },
    Fault { kind: u8 },
    Restart,
}

fn aop() -> impl Strategy<Value = AOp> {
    prop_oneof![
        6 => (0u8..5, 0u8..6).prop_map(|(kind, f)| AOp::Open { kind, f }),
        2 => (any::<u8>(), 0u8..5).prop_map(|(i, f)| AOp::Push { i, f }),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(i, add)| AOp::Halt { i, add }),
        1 => (0u8..5).prop_map(|f| AOp::BasePush { f }),
        4 => (1u8..7).prop_map(|minutes| AOp::Advance { minutes }),
        2 => (0u8..6).prop_map(|kind| AOp::Fault { kind }),
        1 => Just(AOp::Restart),
    ]
}

fn arztagent_tester(drop: f64, duplicate: f64, delay_ms: u64, seed: u64) -> Tester {
    let flaked = std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let full_suite = CiRule {
        check: "ci-ok".into(),
        on: CiOn::Queue,
        decide: std::sync::Arc::new(move |c: &schlange_github_fake::CiCtx| {
            let mut flake = None;
            for v in c.tree.values() {
                if v.contains("BREAKS_FULL_SUITE") {
                    return Some("failure".into());
                }
                if let Some(m) = v.lines().find(|l| l.starts_with("FLAKY_ONCE")) {
                    flake = Some(m.to_string());
                }
            }
            if let Some(m) = flake
                && flaked.lock().unwrap().insert(m)
            {
                return Some("failure".into());
            }
            Some("success".into())
        }),
    };
    Tester::new(ARZTAGENT)
        .ci(CiRule::success("Risk class and data protection", CiOn::Both))
        .ci(CiRule::success("ci-ok", CiOn::PullRequest))
        .ci(full_suite)
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .merge_guard(guard_labels(&["halt"]))
        .persistent()
        .reconcile_every("60s")
        .hook_faults(HookFaults {
            drop,
            duplicate,
            max_delay: Duration::from_millis(delay_ms),
            seed,
        })
}

fn apply_a(t: &mut Running, prs: &mut Vec<u64>, op: &AOp, n: usize) {
    match op {
        AOp::Open { kind, f } => {
            let (path, content) = match kind {
                1 => (format!("backend/f{f}-{n}.py"), format!("v{n} BREAKS_FULL_SUITE\n")),
                2 => (format!("backend/f{f}-{n}.py"), format!("FLAKY_ONCE{n}\n")),
                4 => (format!(".github/workflows/w{n}.yml"), format!("on: [push] # {n}\n")),
                _ => (format!("backend/f{f}-{n}.py"), format!("v{n}\n")),
            };
            let author = ["arztagent-bot", "karma-works", "julianhaegele-create"][n % 3];
            let mut b = t.open_pr(author).file(&path, &content);
            if *kind == 3 {
                b = b.draft();
            }
            prs.push(b.create());
        }
        AOp::Push { i, f } if !prs.is_empty() => {
            let p = prs[*i as usize % prs.len()];
            if t.gh.pr(p).is_some_and(|x| x.open) {
                t.gh.push_pr(p, &[(format!("backend/push{f}-{n}.py"), format!("p{n}\n"))]);
            }
        }
        AOp::Halt { i, add } if !prs.is_empty() => {
            let p = prs[*i as usize % prs.len()];
            if t.gh.pr(p).is_some_and(|x| x.open) {
                t.gh.label(p, "halt", *add);
            }
        }
        AOp::BasePush { f } => {
            t.gh.push_base(&[(format!("base/b{f}.txt"), format!("b{n}\n"))]);
        }
        AOp::Advance { minutes } => t.advance(&format!("{minutes}m")),
        AOp::Fault { kind } => t.gh.add_fault(fault(*kind)),
        AOp::Restart => {
            t.stop_service();
            t.start_service();
        }
        _ => {}
    }
    std::thread::sleep(Duration::from_millis(15));
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn arztagent_configuration_under_bors_style_stress(
        ops in prop::collection::vec(aop(), 6..24),
        drop in prop::sample::select(vec![0.0, 0.0, 0.3]),
        duplicate in prop::sample::select(vec![0.0, 0.3]),
        delay_ms in 0u64..30,
        seed in any::<u64>(),
    ) {
        let mut t = arztagent_tester(drop, duplicate, delay_ms, seed).start();
        let mut prs = Vec::new();
        for (n, o) in ops.iter().enumerate() {
            apply_a(&mut t, &mut prs, o, n);
        }
        drain(&t);
        let mut v = t.gh.invariant_violations(true);
        v.extend(t.dequeued_but_landed());
        let halted = t.status().unwrap().halted.clone();
        t.shutdown();
        prop_assert!(v.is_empty(), "violations: {v:?}\nops: {ops:?}");
        prop_assert!(halted.is_empty(), "halted: {halted:?}\nops: {ops:?}");
    }
}

/// Not vacuous: with the same CI, seven agent pull requests (one breaks the suite, one flakes
/// once) and a restart in the middle, the broken one never lands, the pull requests that
/// share no batch with the flake land, and the base only ever holds tested trees. A flake in
/// a batch whose every split passes blames the batch's last pull request, as Mergify does
/// (`queue-bisection`, Gotchas): such a pull request (#7 here) may be dequeued, no other.
#[test]
fn arztagent_stress_scenario_lands_the_good_pull_requests() {
    let mut t = arztagent_tester(0.0, 0.0, 0, 1).start();
    let mut prs = Vec::new();
    let kinds = [0u8, 0, 1, 0, 2, 0, 0];
    for (n, kind) in kinds.iter().enumerate() {
        apply_a(&mut t, &mut prs, &AOp::Open { kind: *kind, f: n as u8 }, n);
    }
    t.advance("6m");
    apply_a(&mut t, &mut prs, &AOp::Restart, 99);
    drain(&t);
    let landed: Vec<u64> = t.gh.squashes().iter().map(|q| q.pr).collect();
    let v = t.gh.invariant_violations(true);
    t.shutdown();
    assert!(v.is_empty(), "{v:?}");
    for (i, kind) in kinds.iter().enumerate() {
        let n = prs[i];
        if *kind == 1 {
            assert!(!landed.contains(&n), "the broken #{n} landed");
        } else if i != 6 {
            assert!(landed.contains(&n), "#{n} (kind {kind}) did not land: {landed:?}");
        }
    }
}
