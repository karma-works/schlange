//! Landing on GitHub under faults (critique of phase 1c, G1–G4): `schlange serve` against the
//! fake GitHub. Every test checks the fake's I0, I1, I3 and "nothing lands twice" at the end.

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, Tester, Then, guard_labels};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const BOT: &str = "arztagent-bot";
/// Every pull request on `main` with a green `ci` is queued; `ci` on the draft lands it.
const SIMPLE: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    batch_size: 2\n    batch_max_wait_time: 5 minutes\n    queue_conditions:\n      - base = main\n      - check-success = ci\n    merge_conditions:\n      - check-success = ci\n\
                      pull_request_rules:\n  - name: all\n    conditions:\n      - base = main\n    actions:\n      queue:\n";

fn arztagent() -> Tester {
    Tester::new(ARZTAGENT)
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::PullRequest))
        .ci(CiRule::fails_on("ci-ok", CiOn::Queue, "BREAKS_FULL_SUITE"))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        // Arztagent's merge conditions at landing time: never with `halt`.
        .merge_guard(guard_labels(&["halt"]))
}

fn green_pr(t: &Running, path: &str, content: &str) -> u64 {
    t.open_pr(BOT).file(path, content).title(path).create()
}

/// G1 (critic's reproduction): the merge call fails with 502 four times; after the first
/// failure someone labels the pull request `halt`. The queue dequeues it; no deferred retry
/// may merge it later.
#[test]
fn a_halt_label_during_failing_merge_calls_never_lands() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(
        FaultRule::new(
            "PUT",
            &format!("/pulls/{a}/merge"),
            FaultKind::Status(502),
            4,
        )
        .then(Then::Label(a, "halt".into())),
    );
    t.advance("5m");
    t.wait_until("dequeued", |t| t.dequeue_code(a).is_some());
    // Long enough for every deferred retry of the old adapter (and more).
    for _ in 0..10 {
        t.advance("2m");
        t.settle();
    }
    assert!(!t.gh.pr(a).unwrap().merged, "a halted pull request landed");
    assert!(t.gh.squashes().is_empty());
    t.assert_invariants();
    t.shutdown();
}

/// G2: GitHub rate-limits the merge call; while the client waits, someone pushes to the base.
/// The merge call must not be sent again blindly: it would land on the foreign commit with a
/// tree nobody tested. The queue sees the push, rebuilds and lands on the new base.
#[test]
fn a_base_push_during_a_rate_limit_wait_does_not_land_on_the_foreign_commit() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(
        FaultRule::new("PUT", "/merge", FaultKind::RateLimit { retry_after: 1 }, 1)
            .then(Then::PushBase(vec![("docs/x.md".into(), "x\n".into())])),
    );
    t.advance("5m");
    // The car is rebuilt on the new base and waits for its batch time again.
    for _ in 0..6 {
        if t.gh.pr(a).unwrap().merged {
            break;
        }
        t.advance("5m");
        t.settle();
    }
    t.pr(a).wait_merged();
    let q = t.gh.squashes();
    assert_eq!(q.len(), 1);
    assert!(q[0].tested_green, "landed a tree nobody tested: {q:?}");
    let history = t.gh.base_history();
    assert_eq!(history.len(), 2, "the landing and the foreign push");
    assert_eq!(t.gh.parents_of(&history[0]), vec![history[1].clone()]);
    let s = t.status().unwrap();
    assert!(s.halted.is_empty(), "{:?}", s.halted);
    t.assert_invariants();
    t.shutdown();
}

/// MQ-02: the merge call lands on a base that moved in the one-request window and then lies
/// with a 502. The adapter must still compare the landed tree with the tested one and halt;
/// treating the unexpected parent as `HeadChanged` would silently continue on an untested tree.
#[test]
fn a_lying_502_after_a_base_push_halts_on_the_untested_tree() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(
        FaultRule::new("PUT", &format!("/pulls/{a}/merge"), FaultKind::Lie(502), 1)
            .then(Then::PushBase(vec![("docs/x.md".into(), "x\n".into())])),
    );
    t.advance("5m");
    t.wait_until("the queue halts on the untested landing", |t| {
        t.status().is_some_and(|s| !s.halted.is_empty())
    });
    let s = t.status().unwrap();
    assert!(s.halted.contains_key("main"), "{:?}", s.halted);
    assert!(t.gh.pr(a).unwrap().merged);
    // The landing itself cannot be prevented (GitHub executes the merge before the lie), so
    // the queue's I0 checker must see exactly that landing; the point is the halt.
    let violations = t.gh.invariant_violations(false);
    assert!(
        violations.iter().all(|v| v.contains("I0")),
        "unexpected violations: {violations:?}"
    );
    t.shutdown();
}

/// G3: a transient 405 ("Base branch was modified") and a transient 409 are retried after a
/// fresh read, and the pull requests land.
#[test]
fn transient_405_and_409_on_the_merge_are_retried_after_a_read() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.gh.add_fault(FaultRule::new(
        "PUT",
        &format!("/pulls/{a}/merge"),
        FaultKind::StatusMessage(
            405,
            "Base branch was modified. Review and try the merge again.".into(),
        ),
        1,
    ));
    t.gh.add_fault(FaultRule::new(
        "PUT",
        &format!("/pulls/{b}/merge"),
        FaultKind::StatusMessage(
            409,
            "Head branch was modified. Review and try the merge again.".into(),
        ),
        1,
    ));
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.advance("5m");
    t.pr(a).wait_merged();
    t.pr(b).wait_merged();
    assert_eq!(t.gh.squashes().len(), 2);
    t.assert_invariants();
    t.shutdown();
}

/// G4: a permanent refusal (422) dequeues the pull request with a reason instead of retrying
/// forever; the queue goes on with the next one.
#[test]
fn a_permanent_422_on_the_merge_dequeues_with_a_reason() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.gh.add_fault(FaultRule::new(
        "PUT",
        &format!("/pulls/{a}/merge"),
        FaultKind::StatusMessage(
            422,
            "Merge commits are not allowed on this repository.".into(),
        ),
        1000,
    ));
    t.pr(a).wait_queued();
    t.advance("5m");
    t.wait_until("dequeued", |t| t.dequeue_code(a).is_some());
    assert_eq!(
        t.dequeue_code(a).as_deref(),
        Some("PR_UNEXPECTEDLY_FAILED_TO_MERGE")
    );
    t.gh.clear_faults();
    // The queue is not stuck: the next pull request lands.
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.pr(b).wait_queued();
    t.advance("5m");
    t.pr(b).wait_merged();
    assert!(!t.gh.pr(a).unwrap().merged);
    t.assert_invariants();
    t.shutdown();
}

/// G4: without `workflows: write`, GitHub refuses every commit that changes a workflow file
/// (403). The pull request leaves the queue with a reason that names the permission; it does
/// not stall the queue.
#[test]
fn a_workflow_change_without_the_permission_dequeues_and_the_queue_goes_on() {
    // Arztagent never queues `.github/` changes automatically; a plain configuration does.
    let t = Tester::new(SIMPLE)
        .ci(CiRule::success("ci", CiOn::Both))
        .start();
    let w = t
        .open_pr("someone")
        .file(".github/workflows/ci.yml", "on: push\n")
        .title("ci")
        .create();
    let b = t.open_pr("someone").file("b.txt", "b\n").create();
    t.pr(w).wait_queued();
    t.pr(b).wait_queued();
    t.advance("5m");
    t.wait_until("the workflow change left the queue", |t| {
        t.dequeue_code(w).is_some()
    });
    assert_eq!(
        t.dequeue_code(w).as_deref(),
        Some("PR_UNEXPECTEDLY_FAILED_TO_MERGE")
    );
    let s = t.status().unwrap();
    let text = s.state["outcomes"][w.to_string()]["reason"]["text"]
        .as_str()
        .unwrap_or("")
        .to_string();
    assert!(text.contains("workflows"), "{text}");
    for _ in 0..4 {
        if t.gh.pr(b).unwrap().merged {
            break;
        }
        t.advance("5m");
        t.settle();
    }
    t.pr(b).wait_merged();
    assert!(!t.gh.pr(w).unwrap().merged);
    t.assert_invariants();
    t.shutdown();
}

/// `mergeable: null` is "unknown", not "no conflict": a pull request that conflicts with the
/// base is dequeued for that reason, never held behind the pull request ahead of it.
#[test]
fn unknown_mergeability_is_not_taken_for_no_conflict() {
    let t = arztagent().base_file("backend/c.py", "c = 0\n").start();
    t.gh.set_mergeable_lag(3);
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let c = green_pr(&t, "backend/c.py", "c = 1\n");
    t.pr(a).wait_queued();
    t.pr(c).wait_queued();
    // The base changes the same line: #c no longer merges onto the base.
    t.gh.push_base(&[("backend/c.py".into(), "c = 2\n".into())]);
    t.settle();
    t.advance("5m");
    for _ in 0..4 {
        if t.gh.pr(a).unwrap().merged {
            break;
        }
        t.advance("5m");
        t.settle();
    }
    t.pr(a).wait_merged();
    t.wait_until("#c left the queue", |t| t.dequeue_code(c).is_some());
    assert_eq!(t.dequeue_code(c).as_deref(), Some("PR_DEQUEUED"));
    let s = t.status().unwrap();
    assert!(
        s.state["outcomes"][c.to_string()]["reason"]["text"]
            .as_str()
            .unwrap_or("")
            .contains("does not merge onto the base branch")
    );
    assert!(
        s.state["holds"][c.to_string()]
            .to_string()
            .contains("dequeued-waits-for-new-head")
    );
    t.advance("10m");
    t.settle();
    assert!(
        !t.status().unwrap().queued.contains(&c),
        "same head remains held"
    );
    t.assert_invariants();
    t.shutdown();
}

/// More than 30 commit statuses: the required one is on the second page.
#[test]
fn combined_statuses_are_read_across_pages() {
    let config = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - base = main\n      - check-success = required\n    merge_conditions:\n      - check-success = full\n\
                  pull_request_rules:\n  - name: all\n    conditions:\n      - base = main\n    actions:\n      queue:\n";
    let t = Tester::new(config)
        .ci(CiRule::success("full", CiOn::Queue))
        .start();
    let a = t.open_pr("someone").file("a.txt", "a\n").create();
    for i in 0..34 {
        t.gh.report_status(a, &format!("lint-{i:02}"), "success");
    }
    t.gh.report_status(a, "required", "success");
    t.pr(a).wait_queued();
    t.pr(a).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

/// Read-after-write lag: after the queue's own writes, reads return the old state for a while.
/// Nothing lands twice, nothing is left behind.
#[test]
fn reads_that_lag_behind_writes_change_nothing() {
    let t = arztagent().start();
    t.gh.set_read_lag(2);
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.advance("5m");
    t.pr(a).wait_merged();
    t.pr(b).wait_merged();
    let c = green_pr(&t, "backend/c.py", "c = 1\n");
    t.pr(c).wait_queued();
    t.advance("5m");
    t.pr(c).wait_merged();
    assert_eq!(t.gh.squashes().len(), 3);
    t.assert_invariants();
    t.shutdown();
}
