//! MQ-32: two `serve` processes on one repository never both act.
//!
//! - Across hosts (separate data directories): the leader is paused right before it lands; the
//!   follower's clock runs ahead, so it sees the lease expired, takes the fence on GitHub
//!   (`refs/schlange/lease`, fast-forward only) and lands the pull request itself. The old
//!   leader resumes believing its lease is still valid by its own clock: its landing is refused
//!   before it is sent (the fence moved), nothing it does reaches GitHub, and it steps down.
//! - On one data directory: the SQLite lease admits one process; the other follows and takes
//!   over once the first one is gone and the lease expired.

use std::sync::Arc;
use std::time::{Duration, Instant};

use schlange_github::service::{PauseHook, Service, WorkerStatus};
use schlange_github::time::ManualClock;
use schlange_github_fake::tester::{Running, temp_dir};
use schlange_github_fake::{CiOn, CiRule, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const REPO: &str = "karma-works/arzt-agent";

fn arztagent_ci(t: Tester) -> Tester {
    t.ci(CiRule::success(
        "Risk class and data protection",
        CiOn::Both,
    ))
    .ci(CiRule::success("ci-ok", CiOn::PullRequest))
    .ci(CiRule::fails_on("ci-ok", CiOn::Queue, "BREAKS_FULL_SUITE"))
    .ci(CiRule::success("KI-Review", CiOn::PullRequest))
}

fn status(s: &Service) -> Option<WorkerStatus> {
    s.status().get(REPO).cloned()
}

fn wait(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(30);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn writes_since(t: &Running, from: usize) -> Vec<String> {
    t.gh.requests()[from..]
        .iter()
        .filter(|r| !r.starts_with("GET "))
        .filter(|r| !r.contains("/access_tokens "))
        .cloned()
        .collect()
}

#[test]
fn a_paused_leader_that_lost_the_lease_never_writes_and_nothing_lands_twice() {
    let hook = PauseHook::new();
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .lease("host-a", 60)
        .reconcile_every("30s")
        .pause_before_land(hook.clone())
        .start();
    let a_status = status(t.service.as_ref().unwrap()).unwrap();
    assert_eq!(a_status.role, "leader", "{:?}", a_status.lease);
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .title("a")
        .create();
    // A (webhooks go to A) queues it, waits out the batch time, builds the draft, sees it
    // green and stops right before landing.
    t.pr(pr).wait_queued();
    t.advance("5m");
    wait("A paused before its landing", || hook.reached());
    assert!(!t.gh.pr(pr).unwrap().merged, "A has not landed");
    // Host B starts now; its clock runs two minutes ahead of A's (a skew larger than the
    // lease), so by B's clock A's lease has expired while A, paused, still trusts it.
    let clock_b = Arc::new(ManualClock::new(
        schlange_github::time::Clock::now(t.clock.as_ref()) + 120,
    ));
    let b = t.second_service("host-b", None, clock_b.clone());
    // B leads now (it took the fence on GitHub) and lands the pull request itself.
    wait("B leads", || status(&b).is_some_and(|s| s.role == "leader"));
    for _ in 0..40 {
        if t.gh.pr(pr).unwrap().merged {
            break;
        }
        clock_b.advance(30);
        std::thread::sleep(Duration::from_millis(150));
    }
    assert!(
        t.gh.pr(pr).unwrap().merged,
        "B landed it; B's log:\n{}",
        status(&b)
            .map(|s| s.log_tail.join("\n"))
            .unwrap_or_default()
    );
    let squashes: Vec<_> = t.gh.squashes().into_iter().filter(|s| s.pr == pr).collect();
    assert_eq!(squashes.len(), 1);
    // B goes away; from now on every request comes from A.
    b.shutdown();
    let from = t.gh.requests().len();
    hook.release();
    wait("A stepped down", || {
        status(t.service.as_ref().unwrap()).is_some_and(|s| s.role == "follower")
    });
    std::thread::sleep(Duration::from_millis(200));
    let w = writes_since(&t, from);
    assert!(
        w.is_empty(),
        "the old leader wrote after losing the lease:\n{}",
        w.join("\n")
    );
    let a_status = status(t.service.as_ref().unwrap()).unwrap();
    assert!(
        a_status.writes_refused >= 1,
        "the landing was refused before it was sent: {a_status:?}"
    );
    assert!(
        a_status.log_tail.iter().any(|l| l.contains("fence moved")),
        "{}",
        a_status.log_tail.join("\n")
    );
    // Still exactly one landing of the pull request.
    let squashes: Vec<_> = t.gh.squashes().into_iter().filter(|s| s.pr == pr).collect();
    assert_eq!(squashes.len(), 1);
    let v = t.gh.invariant_violations(false);
    assert!(v.is_empty(), "{v:?}");
    t.shutdown();
}

#[test]
fn two_processes_on_one_data_directory_never_both_lead() {
    let mut t = arztagent_ci(Tester::new(ARZTAGENT))
        .lease("proc-1", 60)
        .persistent()
        .start();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let b = t.second_service("proc-2", Some(dir), t.clock.clone());
    wait("the second process follows", || {
        status(&b).is_some_and(|s| s.role == "follower")
    });
    assert_eq!(t.status().unwrap().role, "leader");
    let st = status(&b).unwrap();
    assert_eq!(
        st.lease.as_ref().and_then(|l| l.other.clone()).as_deref(),
        Some("proc-1"),
        "{st:?}"
    );
    // The leader goes away (its lease on GitHub runs out); the follower takes over.
    t.stop_service();
    t.advance("61s");
    wait("the second process leads", || {
        status(&b).is_some_and(|s| s.role == "leader")
    });
    // It acts: a pull request is queued by it.
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    for _ in 0..40 {
        if status(&b).is_some_and(|s| s.queued.contains(&pr)) {
            break;
        }
        b.reconcile_now();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(status(&b).unwrap().queued.contains(&pr));
    b.shutdown();
    let _ = temp_dir;
    t.shutdown();
}
