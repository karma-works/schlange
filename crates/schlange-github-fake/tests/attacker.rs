//! Property test with an attacker: Arztagent's threat model against `schlange serve` on the
//! fake GitHub. An AI agent with write access opens pull requests and, interleaved with a
//! listed human's work, API faults and webhook faults:
//!
//! - edits other users' comments (into `@mergifyio queue`),
//! - pushes commits whose author is forged (the app's bot, the human),
//! - pushes constitution files to any pull request, also by renaming one out of a guarded
//!   path,
//! - adds and removes `halt` while the merge call fails (the label change and the fault fire
//!   together),
//! - ticks the queue's checkboxes and comments commands itself,
//! - races the human's queue command with a push right after it.
//!
//! After the queue drains, with faults off:
//!
//! - **Constitution:** every landed squash commit that changes a constitution path was queued
//!   by the listed human with a command made while the landed head was the pull request's
//!   head (the fake's activity log says which head was current at the comment).
//! - I0, I1 (no landing after a dequeue, at an untested head or while `halt` was set), I3, and
//!   nothing lands twice (the fake's checks).
//!
//! Each case starts a fake and a service: the default is small. `SCHLANGE_GH_ATTACK_CASES`
//! sets the number of cases (default 3), e.g. `SCHLANGE_GH_ATTACK_CASES=50 cargo test -p
//! schlange-github-fake --test attacker`.

use std::time::Duration;

use proptest::prelude::*;
use proptest::test_runner::{Config, RngSeed};

use schlange_core::commands::CONTROLS_MARKER;
use schlange_github_fake::tester::Running;
use schlange_github_fake::{
    CiOn, CiRule, FaultKind, FaultRule, HookFaults, Tester, Then, guard_labels,
};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const AGENT: &str = "arztagent-bot";
const HUMAN: &str = "karma-works";
/// A constitution file on the base the agent can rename away.
const GUARDED: [&str; 2] = [".github/workflows/ci.yml", "governance/risk-paths.yaml"];

/// The constitution paths the attacker writes, judged independently of the configuration's
/// regular expression (all of them match it).
fn constitution(path: &str) -> bool {
    path.starts_with(".github/")
        || path.starts_with("governance/")
        || path == "AGENTS.md"
        || path.contains("/quality/")
}

#[derive(Clone, Debug)]
enum Op {
    /// The agent opens a pull request; `guarded` puts a constitution file in it.
    Open {
        guarded: bool,
        f: u8,
    },
    /// The human comments `queue` on the i-th pull request (vouching for its head now).
    HumanQueue {
        i: u8,
    },
    /// The human writes an ordinary comment (something for the agent to edit).
    HumanChat {
        i: u8,
    },
    /// The human comments `queue` and the agent pushes a constitution file right after.
    Race {
        i: u8,
    },
    /// The agent edits the k-th human comment into a queue command.
    EditHuman {
        k: u8,
    },
    /// The agent pushes a commit authored as `as_who` (0: itself, 1: the app's bot, 2: the
    /// human); `guarded` writes a constitution path.
    Push {
        i: u8,
        as_who: u8,
        guarded: bool,
    },
    /// The agent renames a guarded base file out of its path in the i-th pull request.
    Rename {
        i: u8,
        which: u8,
    },
    /// The agent ticks the queue's checkbox, or comments a command itself.
    AgentTick {
        i: u8,
    },
    AgentCommand {
        i: u8,
    },
    /// `halt` on or off.
    Halt {
        i: u8,
        add: bool,
    },
    /// A fault on the merge call that sets `halt` on the i-th pull request when it fires.
    FaultWithHalt {
        i: u8,
        kind: u8,
    },
    /// An API fault alone.
    Fault {
        kind: u8,
    },
    Advance {
        minutes: u8,
    },
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (any::<bool>(), 0u8..4).prop_map(|(guarded, f)| Op::Open { guarded, f }),
        3 => any::<u8>().prop_map(|i| Op::HumanQueue { i }),
        2 => any::<u8>().prop_map(|i| Op::HumanChat { i }),
        2 => any::<u8>().prop_map(|i| Op::Race { i }),
        3 => any::<u8>().prop_map(|k| Op::EditHuman { k }),
        3 => (any::<u8>(), 0u8..3, any::<bool>())
            .prop_map(|(i, as_who, guarded)| Op::Push { i, as_who, guarded }),
        1 => (any::<u8>(), 0u8..2).prop_map(|(i, which)| Op::Rename { i, which }),
        1 => any::<u8>().prop_map(|i| Op::AgentTick { i }),
        1 => any::<u8>().prop_map(|i| Op::AgentCommand { i }),
        1 => (any::<u8>(), any::<bool>()).prop_map(|(i, add)| Op::Halt { i, add }),
        2 => (any::<u8>(), 0u8..3).prop_map(|(i, kind)| Op::FaultWithHalt { i, kind }),
        1 => (0u8..4).prop_map(|kind| Op::Fault { kind }),
        4 => (1u8..4).prop_map(|minutes| Op::Advance { minutes }),
    ]
}

fn merge_fault(kind: u8) -> FaultKind {
    match kind {
        0 => FaultKind::StatusMessage(
            405,
            "Base branch was modified. Review and try the merge again.".into(),
        ),
        1 => FaultKind::StatusMessage(409, "Head branch was modified.".into()),
        _ => FaultKind::Status(502),
    }
}

fn fault(kind: u8) -> FaultRule {
    match kind {
        0 => FaultRule::new("PUT", "/merge", FaultKind::Lie(502), 1),
        1 => FaultRule::new("GET", "/pulls/", FaultKind::Status(503), 3),
        2 => FaultRule::new("GET", "/activity", FaultKind::Status(502), 2),
        _ => FaultRule::new("*", "/", FaultKind::RateLimit { retry_after: 1 }, 1),
    }
}

/// What the test remembers: the pull requests, the human's comments, and the human's queue
/// commands (pull request, comment id).
#[derive(Default)]
struct World {
    prs: Vec<u64>,
    human_comments: Vec<u64>,
    commands: Vec<(u64, u64)>,
}

impl World {
    fn pick(&self, t: &Running, i: u8) -> Option<u64> {
        if self.prs.is_empty() {
            return None;
        }
        let n = self.prs[i as usize % self.prs.len()];
        t.gh.pr(n).is_some_and(|p| p.open).then_some(n)
    }
}

fn apply(t: &Running, w: &mut World, op: &Op, step: usize) {
    match op {
        Op::Open { guarded, f } => {
            let path = if *guarded {
                format!(".github/workflows/w{f}-{step}.yml")
            } else {
                format!("backend/f{f}-{step}.py")
            };
            let n = t
                .open_pr(AGENT)
                .file(&path, &format!("v{step}\n"))
                .title(&path)
                .create();
            w.prs.push(n);
        }
        Op::HumanQueue { i } => {
            if let Some(n) = w.pick(t, *i) {
                let c = t.gh.comment(n, HUMAN, "@mergifyio queue");
                w.commands.push((n, c));
                w.human_comments.push(c);
            }
        }
        Op::HumanChat { i } => {
            if let Some(n) = w.pick(t, *i) {
                w.human_comments
                    .push(t.gh.comment(n, HUMAN, &format!("Looked at it ({step}).")));
            }
        }
        Op::Race { i } => {
            if let Some(n) = w.pick(t, *i) {
                let c = t.gh.comment(n, HUMAN, "@mergifyio queue");
                w.commands.push((n, c));
                w.human_comments.push(c);
                t.gh.push_pr_as(
                    n,
                    AGENT,
                    &[(format!("governance/race-{step}.yaml"), "x\n".into())],
                );
            }
        }
        Op::EditHuman { k } => {
            if !w.human_comments.is_empty() {
                let c = w.human_comments[*k as usize % w.human_comments.len()];
                // The edited comment is no command of the human's any more.
                w.commands.retain(|(_, id)| *id != c);
                t.gh.edit_comment(c, AGENT, "@mergifyio queue")
                    .expect("the agent has write access");
            }
        }
        Op::Push { i, as_who, guarded } => {
            if let Some(n) = w.pick(t, *i) {
                let who = match as_who {
                    0 => AGENT,
                    1 => "schlange[bot]",
                    _ => HUMAN,
                };
                let path = if *guarded {
                    format!("backend/quality/p{step}.json")
                } else {
                    format!("backend/p{step}.py")
                };
                t.gh.push_pr_as(n, who, &[(path, format!("p{step}\n"))]);
            }
        }
        Op::Rename { i, which } => {
            if let Some(n) = w.pick(t, *i) {
                let from = GUARDED[*which as usize % GUARDED.len()];
                let head = t.gh.pr(n).unwrap().head_sha;
                if t.gh.tree_of(&head).contains_key(from) {
                    t.gh.move_in_pr(n, from, &format!("docs/moved-{step}.txt"));
                }
            }
        }
        Op::AgentTick { i } => {
            if let Some(n) = w.pick(t, *i) {
                t.gh.tick(n, AGENT, CONTROLS_MARKER);
            }
        }
        Op::AgentCommand { i } => {
            if let Some(n) = w.pick(t, *i) {
                t.gh.comment(n, AGENT, "@mergifyio queue");
            }
        }
        Op::Halt { i, add } => {
            if let Some(n) = w.pick(t, *i) {
                t.gh.label(n, "halt", *add);
            }
        }
        Op::FaultWithHalt { i, kind } => {
            if let Some(n) = w.pick(t, *i) {
                t.gh.add_fault(
                    FaultRule::new("PUT", "/merge", merge_fault(*kind), 1)
                        .then(Then::Label(n, "halt".into())),
                );
            }
        }
        Op::Fault { kind } => t.gh.add_fault(fault(*kind)),
        Op::Advance { minutes } => t.advance(&format!("{minutes}m")),
    }
    std::thread::sleep(Duration::from_millis(15));
}

/// Drives the queue until nothing is queued and no car is left (faults off). Queue drafts
/// get their full suite green from the CI rules.
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

/// Constitution oracle: every landed change of a constitution path was vouched for by one of
/// the human's queue commands at exactly the landed head.
fn constitution_violations(t: &Running, w: &World) -> Vec<String> {
    let mut v = Vec::new();
    for q in t.gh.squashes() {
        let before = t.gh.tree_of(&q.parent);
        let after = t.gh.tree_of(&q.sha);
        let changed: Vec<&String> = before
            .keys()
            .chain(after.keys())
            .filter(|p| before.get(*p) != after.get(*p))
            .filter(|p| constitution(p))
            .collect();
        if changed.is_empty() {
            continue;
        }
        let comments = t.gh.comments_on(q.pr);
        let vouched = w.commands.iter().any(|(n, id)| {
            *n == q.pr
                && comments
                    .iter()
                    .find(|c| c.id == *id)
                    .is_some_and(|c| t.gh.head_at(q.pr, c.created_at).as_deref() == Some(&q.head))
        });
        if !vouched {
            v.push(format!(
                "constitution: #{} landed {changed:?} at head {} that no command of {HUMAN} \
                 vouched for",
                q.pr, q.head
            ));
        }
    }
    v
}

fn config() -> Config {
    let cases = std::env::var("SCHLANGE_GH_ATTACK_CASES")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(3);
    Config {
        cases,
        rng_seed: RngSeed::Fixed(0x6174_7461_636b),
        failure_persistence: None,
        max_shrink_iters: 16,
        ..Config::default()
    }
}

proptest! {
    #![proptest_config(config())]

    #[test]
    fn an_agent_with_write_access_never_lands_an_unvouched_constitution_change(
        ops in prop::collection::vec(op(), 6..22),
        drop in prop::sample::select(vec![0.0, 0.3, 1.0, 1.0]),
        duplicate in prop::sample::select(vec![0.0, 0.3]),
        delay_ms in 0u64..30,
        seed in any::<u64>(),
    ) {
        let mut tester = Tester::new(ARZTAGENT)
            .ci(CiRule::success("Risk class and data protection", CiOn::Both))
            .ci(CiRule::success("ci-ok", CiOn::Both))
            .ci(CiRule::success("KI-Review", CiOn::PullRequest))
            .merge_guard(guard_labels(&["halt"]))
            .reconcile_every("60s")
            .fake(|c| {
                c.permissions.insert("workflows".into(), "write".into());
            })
            .hook_faults(HookFaults {
                drop,
                duplicate,
                max_delay: Duration::from_millis(delay_ms),
                seed,
            });
        for g in GUARDED {
            tester = tester.base_file(g, "guarded\n");
        }
        let t = tester.start();
        t.permission(AGENT, "write");
        t.permission(HUMAN, "admin");
        let mut w = World::default();
        for (step, o) in ops.iter().enumerate() {
            apply(&t, &mut w, o, step);
        }
        drain(&t);
        let mut v = constitution_violations(&t, &w);
        v.extend(t.gh.invariant_violations(true));
        v.extend(t.dequeued_but_landed());
        t.shutdown();
        prop_assert!(v.is_empty(), "violations: {v:?}\nops: {ops:?}");
    }
}
