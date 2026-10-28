//! Critic e8a0022 reproductions against the fake GitHub. Copy into
//! crates/schlange-github-fake/tests/ and run `cargo test -p schlange-github-fake --test critic_e8a0022`.

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, Tester, Then, guard_labels};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const BOT: &str = "arztagent-bot";

fn arztagent() -> Tester {
    Tester::new(ARZTAGENT)
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::PullRequest))
        .ci(CiRule::fails_on("ci-ok", CiOn::Queue, "BREAKS_FULL_SUITE"))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .merge_guard(guard_labels(&["halt"]))
}

fn green_pr(t: &Running, path: &str, content: &str) -> u64 {
    t.open_pr(BOT).file(path, content).title(path).create()
}

/// C1: a transient 405 on the merge call; while it is answered, someone labels the pull
/// request `halt`. The adapter's transient retry re-reads only state and head, not the
/// conditions. Expectation (I1): the halted pull request never lands.
#[test]
fn c1_halt_label_during_a_transient_405_never_lands() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(
        FaultRule::new(
            "PUT",
            &format!("/pulls/{a}/merge"),
            FaultKind::StatusMessage(
                405,
                "Base branch was modified. Review and try the merge again.".into(),
            ),
            1,
        )
        .then(Then::Label(a, "halt".into())),
    );
    t.advance("5m");
    for _ in 0..6 {
        t.advance("1m");
        t.settle();
    }
    let merged = t.gh.pr(a).unwrap().merged;
    let v = t.gh.invariant_violations(true);
    assert!(!merged && v.is_empty(), "merged={merged} violations={v:?}");
    t.shutdown();
}

/// C2: same with a transient 409.
#[test]
fn c2_halt_label_during_a_transient_409_never_lands() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(
        FaultRule::new(
            "PUT",
            &format!("/pulls/{a}/merge"),
            FaultKind::StatusMessage(409, "Head branch was modified.".into()),
            1,
        )
        .then(Then::Label(a, "halt".into())),
    );
    t.advance("5m");
    for _ in 0..6 {
        t.advance("1m");
        t.settle();
    }
    let merged = t.gh.pr(a).unwrap().merged;
    let v = t.gh.invariant_violations(true);
    assert!(!merged && v.is_empty(), "merged={merged} violations={v:?}");
    t.shutdown();
}

/// C3: a batch of two; the first lands, then reading the second pull request fails (502 storm).
/// The landing reply is lost as an error. The queue must still land the second exactly once
/// and keep every invariant.
#[test]
fn c3_read_failure_between_batch_landings() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{b}"),
        FaultKind::Status(502),
        8,
    ));
    // Arm only after the first merge: a rule on the merge of `a` that lies (merged, 502).
    t.gh.add_fault(FaultRule::new(
        "PUT",
        &format!("/pulls/{a}/merge"),
        FaultKind::Lie(502),
        1,
    ));
    t.advance("5m");
    for _ in 0..12 {
        t.advance("2m");
        t.settle();
        if t.gh.pr(a).unwrap().merged && t.gh.pr(b).unwrap().merged {
            break;
        }
    }
    let q = t.gh.squashes();
    eprintln!("squashes: {q:?}");
    eprintln!("a code {:?} b code {:?}", t.dequeue_code(a), t.dequeue_code(b));
    t.assert_invariants();
    assert!(t.gh.pr(a).unwrap().merged);
    t.shutdown();
}


/// C7: a listed human queues a constitution pull request while `KI-Review` is still pending
/// (the command waits). The agent then pushes a commit whose author email maps to the app's
/// bot user (git author email = `<id>+schlange[bot]@users.noreply.github.com`; the fake's
/// `push_as` models GitHub's email → login mapping). `head_by_queue` takes the commit author
/// as the pusher, so the pending command is not cancelled and the unreviewed head is queued.
#[test]
fn c7_spoofed_bot_commit_keeps_a_pending_queue_command() {
    let t = Tester::new(ARZTAGENT)
        .ci(CiRule::success("Risk class and data protection", CiOn::Both))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
        .start();
    t.permission(BOT, "write");
    t.permission("karma-works", "admin");
    let pr = t
        .open_pr(BOT)
        .file(".github/workflows/ci.yml", "on: [pull_request]\n")
        .title("Tighten the CI workflow")
        .create();
    t.settle();
    t.pr(pr).comment("karma-works", "@mergifyio queue");
    t.settle();
    assert!(!t.status().is_some_and(|s| s.queued.contains(&pr)), "waits for KI-Review");
    t.pr(pr).push_as("schlange[bot]", ".github/workflows/evil.yml", "on: [push]\n");
    t.settle();
    t.pr(pr).check("KI-Review").succeed();
    for _ in 0..6 {
        t.advance("5m");
        t.settle();
        if let Some(d) = t.gh.open_queue_drafts().first() {
            let _ = d;
            t.queue_pr().check("ci-ok").succeed();
        }
    }
    let queued = t.status().is_some_and(|s| s.queued.contains(&pr));
    let merged = t.gh.pr(pr).unwrap().merged;
    eprintln!("comments: {:#?}", t.pr(pr).comments());
    assert!(!queued && !merged, "spoofed push kept the human's command: queued={queued} merged={merged}");
    t.shutdown();
}
