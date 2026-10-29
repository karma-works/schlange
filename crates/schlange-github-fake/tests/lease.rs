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
    let deadline = Instant::now() + Duration::from_secs(120);
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn trusted_takeover_with_corrupt_checkpoint_stops_without_overwriting_it() {
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute(
        "INSERT OR REPLACE INTO checkpoints (repo, state, written) VALUES (?1, ?2, 1)",
        rusqlite::params![repo, "{broken"],
    )
    .unwrap();
    let count = t.gh.requests_with_query().len();
    let service = Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    while !service.dead_workers().contains_key(&repo) {
        assert!(
            Instant::now() < deadline,
            "corrupt journal must stop the worker"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let why = service.dead_workers().remove(&repo).unwrap();
    assert!(
        why.contains("cannot recover journal after taking the lease"),
        "{why}"
    );
    let state: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "{broken");
    service.shutdown();
    let allowed = [
        format!("POST /repos/{repo}/git/commits"),
        format!("PATCH /repos/{repo}/git/refs/schlange/lease"),
    ];
    assert!(!allowed.contains(&format!("PATCH /repos/{repo}/git/refs/heads/main")));
    for request in t.gh.requests_with_query().iter().skip(count) {
        let (route, _) = request.rsplit_once(' ').expect("request has a status");
        assert!(
            !(route.starts_with("POST ")
                || route.starts_with("PATCH ")
                || route.starts_with("PUT ")
                || route.starts_with("DELETE "))
                || allowed.iter().any(|item| item == route),
            "unexpected effect: {request}"
        );
    }
    t.shutdown();
}

#[test]
fn health_reports_blocked_worker_and_recovers_after_it_resumes() {
    use std::io::{Read, Write};
    use std::sync::atomic::Ordering;

    let hook = PauseHook::new();
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .pause_before_land(hook.clone())
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    t.pr(pr).wait_queued();
    t.advance("5m");
    wait("worker paused before landing", || hook.reached());
    let health = || {
        let mut conn = std::net::TcpStream::connect(t.service.as_ref().unwrap().addr).unwrap();
        conn.write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .unwrap();
        let mut response = String::new();
        conn.read_to_string(&mut response).unwrap();
        response
    };
    assert!(health().starts_with("HTTP/1.1 200"));
    t.serve_cfg
        .heartbeat_test_offset
        .as_ref()
        .unwrap()
        .store(1801, Ordering::SeqCst);
    let response = health();
    assert!(response.starts_with("HTTP/1.1 503"), "{response}");
    assert!(response.contains("heartbeat stale"));
    hook.release();
    wait("heartbeat after worker resumes", || {
        health().starts_with("HTTP/1.1 200")
    });
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn watchdog_reconciles_when_green_check_webhook_is_lost() {
    use schlange_github_fake::{FaultKind, FaultRule, HookFaults};
    use std::sync::atomic::Ordering;
    let hook = PauseHook::new();
    hook.release();
    let observer_hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .pause_before_land(hook)
        .watchdog_pause(observer_hook.clone())
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .reconcile_every("1h")
        .start();
    let cycles = t.serve_cfg.watchdog_test_cycles.as_ref().unwrap();
    wait("initial observer sample finished", || {
        cycles.load(Ordering::SeqCst) >= 1
    });
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    for check in ["ci-ok", "Risk class and data protection", "KI-Review"] {
        t.pr(pr).check(check).succeed();
    }
    t.service.as_ref().unwrap().reconcile_now();
    t.pr(pr).wait_queued();
    t.advance("5m");
    let draft = t.queue_pr();
    t.settle();
    draft.check("ci-ok").succeed();
    assert!(!t.gh.pr(pr).unwrap().merged);
    let offset = t.serve_cfg.heartbeat_test_offset.as_ref().unwrap();
    let observations = t.serve_cfg.watchdog_test_observations.as_ref().unwrap();
    let samples = t.serve_cfg.watchdog_test_samples.as_ref().unwrap();
    let before = samples.load(Ordering::SeqCst);
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{}", draft.number),
        FaultKind::Status(404),
        100,
    ));
    offset.store(61, Ordering::SeqCst);
    wait("failed observer snapshot", || {
        samples.load(Ordering::SeqCst) > before
    });
    t.gh.clear_faults();
    assert_eq!(
        observations.load(Ordering::SeqCst),
        0,
        "a failed read cannot be actionable"
    );
    let before = samples.load(Ordering::SeqCst);
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{}", draft.number),
        FaultKind::RateLimit { retry_after: 1 },
        1,
    ));
    offset.store(122, Ordering::SeqCst);
    wait("rate-limited observer snapshot", || {
        samples.load(Ordering::SeqCst) > before
    });
    assert_eq!(
        observations.load(Ordering::SeqCst),
        0,
        "a rate-limited read cannot be actionable"
    );
    assert!(
        t.client.stats().rate_limited > 0,
        "observer actually received HTTP 429"
    );
    let pull_path = format!("GET /repos/karma-works/arzt-agent/pulls/{pr} ");
    let before_pull_reads =
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&pull_path))
            .count();
    let before = samples.load(Ordering::SeqCst);
    offset.store(183, Ordering::SeqCst);
    wait("observer paused after fresh read", || {
        observer_hook.reached()
    });
    assert!(
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&pull_path))
            .count()
            > before_pull_reads,
        "429 must discard even successful earlier memoized reads"
    );
    let (code, _) = t.admin(
        "PUT",
        "merge-queue/pause",
        Some(r#"{"reason":"maintenance"}"#),
    );
    assert_eq!(code, 200);
    observer_hook.release();
    wait("stale observer sample discarded", || {
        samples.load(Ordering::SeqCst) > before
    });
    assert_eq!(
        observations.load(Ordering::SeqCst),
        0,
        "changed state invalidates the observation"
    );
    let (code, _) = t.admin("DELETE", "merge-queue/pause", None);
    assert_eq!(code, 200);
    t.wait_until("replacement batch after unpause", |t| {
        t.gh.open_queue_drafts()
            .iter()
            .any(|p| p.number != draft.number)
    });
    let replacement =
        t.gh.open_queue_drafts()
            .into_iter()
            .find(|p| p.number != draft.number)
            .unwrap()
            .number;
    t.settle();
    t.pr(replacement).check("ci-ok").succeed();
    offset.store(244, Ordering::SeqCst);
    wait("first actionable watchdog observation", || {
        observations.load(Ordering::SeqCst) >= 1
    });
    assert!(
        !t.gh.pr(pr).unwrap().merged,
        "one observation must not trigger recovery"
    );
    offset.store(305, Ordering::SeqCst);
    wait("second actionable watchdog observation", || {
        observations.load(Ordering::SeqCst) >= 2
    });
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn watchdog_resumes_real_reads_when_background_budget_is_exhausted() {
    use schlange_github::client::BudgetConfig;
    use schlange_github_fake::HookFaults;
    use std::sync::atomic::Ordering;
    let hook = PauseHook::new();
    hook.release();
    let reconcile_hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .pause_before_land(hook)
        .watchdog_reconcile_pause(reconcile_hook.clone())
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .reconcile_every("1h")
        .start();
    let cycles = t.serve_cfg.watchdog_test_cycles.as_ref().unwrap();
    wait("initial observer cycle", || {
        cycles.load(Ordering::SeqCst) >= 1
    });
    let mut first = t.open_pr("arztagent-bot");
    for n in 0..120 {
        first = first.file(&format!("backend/a{n}.py"), "a\n");
    }
    let a = first.create();
    let b = t
        .open_pr("arztagent-bot")
        .file("backend/b.py", "b\n")
        .create();
    for n in [a, b] {
        t.gh.comment(n, "schlange[bot]", "<!-- schlange:status -->\nexisting");
        for check in ["ci-ok", "Risk class and data protection", "KI-Review"] {
            t.pr(n).check(check).succeed();
        }
    }
    t.service.as_ref().unwrap().reconcile_now();
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.advance("5m");
    let draft = t.queue_pr();
    t.settle();
    draft.check("ci-ok").succeed();
    t.wait_until("cosmetic queue drained", |t| {
        t.status().is_some_and(|s| s.cosmetics_waiting == 0)
    });
    let constrained = BudgetConfig {
        capacity: 17.0,
        refill_per_sec: 0.0001,
        cosmetic_reserve: 12.0,
    };
    let file_prefix = format!("GET /repos/karma-works/arzt-agent/pulls/{a}/files ");
    let file_reads_before =
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&file_prefix))
            .count();
    let page_path = format!("GET /repos/karma-works/arzt-agent/pulls/{a}/files?");
    let page2_before =
        t.gh.requests_with_query()
            .iter()
            .filter(|r| r.starts_with(&page_path) && r.contains("page=2"))
            .count();
    let before_deferred = t.client.stats().deferred;
    let before_not_modified = t.client.stats().not_modified;
    let samples = t.serve_cfg.watchdog_test_samples.as_ref().unwrap();
    let observations = t.serve_cfg.watchdog_test_observations.as_ref().unwrap();
    let partial = t.serve_cfg.watchdog_test_partial_prs.as_ref().unwrap();
    let offset = t.serve_cfg.heartbeat_test_offset.as_ref().unwrap();
    let mut first_completed_count = None;
    let mut verified_resume = false;
    for i in 1..=18 {
        let before = samples.load(Ordering::SeqCst);
        t.client.test_set_budget(constrained.clone());
        offset.store(i * 61, Ordering::SeqCst);
        wait("budget-limited observer cycle", || {
            samples.load(Ordering::SeqCst) > before || reconcile_hook.reached()
        });
        let reads =
            t.gh.requests()
                .iter()
                .filter(|r| r.starts_with(&format!("GET /repos/karma-works/arzt-agent/pulls/{a} ")))
                .count();
        if let Some(previous) = first_completed_count {
            if !verified_resume {
                assert_eq!(
                    reads, previous,
                    "a completed PR is not fetched again when the observer resumes"
                );
                verified_resume = true;
            }
        } else if partial.load(Ordering::SeqCst) >= 1 {
            first_completed_count = Some(reads);
        }
        if observations.load(Ordering::SeqCst) >= 2 {
            break;
        }
    }
    assert!(
        t.client.stats().deferred > before_deferred,
        "observer must hit the real local budget"
    );
    assert!(
        t.client.stats().not_modified > before_not_modified,
        "observer resolves ETag 304 responses while resuming"
    );
    assert!(
        verified_resume,
        "observer must defer after a completed PR and resume its remaining reads"
    );
    assert!(
        observations.load(Ordering::SeqCst) >= 2,
        "two complete observations must eventually finish"
    );
    assert!(
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&file_prefix))
            .count()
            >= file_reads_before + 4,
        "each completed observation must follow the paginated files list"
    );
    assert!(
        t.gh.requests_with_query()
            .iter()
            .filter(|r| r.starts_with(&page_path) && r.contains("page=2"))
            .count()
            >= page2_before + 2,
        "both independent observations must request the second file page"
    );
    assert!(
        reconcile_hook.reached(),
        "reconciliation must wait for the budget barrier"
    );
    t.client.test_set_budget(BudgetConfig {
        capacity: 10_000.0,
        refill_per_sec: 10_000.0,
        cosmetic_reserve: 10.0,
    });
    reconcile_hook.release();
    t.pr(a).wait_merged();
    t.pr(b).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn changed_head_during_resumed_observation_never_lands_from_cached_data() {
    use schlange_github::client::BudgetConfig;
    use schlange_github_fake::HookFaults;
    use std::sync::atomic::Ordering;
    let hook = PauseHook::new();
    hook.release();
    let t = Tester::new(ARZTAGENT)
        .pause_before_land(hook)
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .reconcile_every("1h")
        .start();
    let cycles = t.serve_cfg.watchdog_test_cycles.as_ref().unwrap();
    wait("initial observer cycle", || {
        cycles.load(Ordering::SeqCst) >= 1
    });
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "old\n")
        .create();
    t.gh.comment(pr, "schlange[bot]", "<!-- schlange:status -->\nexisting");
    for check in ["ci-ok", "Risk class and data protection", "KI-Review"] {
        t.pr(pr).check(check).succeed();
    }
    t.service.as_ref().unwrap().reconcile_now();
    t.pr(pr).wait_queued();
    t.advance("5m");
    let draft = t.queue_pr();
    t.settle();
    draft.check("ci-ok").succeed();
    t.wait_until("cosmetic queue drained", |t| {
        t.status().is_some_and(|s| s.cosmetics_waiting == 0)
    });
    let constrained = BudgetConfig {
        capacity: 17.0,
        refill_per_sec: 0.0001,
        cosmetic_reserve: 12.0,
    };
    let samples = t.serve_cfg.watchdog_test_samples.as_ref().unwrap();
    let observations = t.serve_cfg.watchdog_test_observations.as_ref().unwrap();
    let completed = t.serve_cfg.watchdog_test_completed.as_ref().unwrap();
    let offset = t.serve_cfg.heartbeat_test_offset.as_ref().unwrap();
    let reconciliations_before = t.status().unwrap().reconciliations;
    let path = format!("GET /repos/karma-works/arzt-agent/pulls/{pr} ");
    let before_reads =
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&path))
            .count();
    t.client.test_set_budget(constrained.clone());
    let before = samples.load(Ordering::SeqCst);
    offset.store(61, Ordering::SeqCst);
    wait("first partial observer sample", || {
        samples.load(Ordering::SeqCst) > before
    });
    let old_reads =
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&path))
            .count();
    assert!(
        old_reads > before_reads,
        "the old PR representation was read into the partial memo"
    );
    let old_head = t.gh.pr(pr).unwrap().head_sha;
    t.pr(pr).push("backend/a.py", "new head\n"); // its webhook is lost
    let new_head = t.gh.pr(pr).unwrap().head_sha;
    assert_ne!(old_head, new_head);
    let mut first_completed_at = None;
    let mut second_completed_at = None;
    for i in 2..=16 {
        let before = samples.load(Ordering::SeqCst);
        t.client.test_set_budget(constrained.clone());
        offset.store(i * 61, Ordering::SeqCst);
        wait("next observer sample", || {
            samples.load(Ordering::SeqCst) > before
        });
        if completed.load(Ordering::SeqCst) >= 1 && first_completed_at.is_none() {
            first_completed_at = Some(i * 61);
        }
        if completed.load(Ordering::SeqCst) >= 2 {
            second_completed_at = Some(i * 61);
            break;
        }
    }
    let first = first_completed_at.expect("first observation completed");
    let second = second_completed_at.expect("changed-head observation completed");
    assert!(
        second - first <= 660,
        "second observation must finish before confirmation expiry"
    );
    assert!(
        t.gh.requests()
            .iter()
            .filter(|r| r.starts_with(&path))
            .count()
            > old_reads,
        "a later independent observation re-reads the changed PR"
    );
    assert_eq!(
        observations.load(Ordering::SeqCst),
        1,
        "a stale first observation cannot be confirmed by the changed head"
    );
    assert_eq!(
        t.status().unwrap().reconciliations,
        reconciliations_before,
        "observer must not request reconciliation on changed head"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.client.test_set_budget(BudgetConfig {
        capacity: 10_000.0,
        refill_per_sec: 10_000.0,
        cosmetic_reserve: 10.0,
    });
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("new head cancels the old queue entry", |t| {
        t.dequeue_code(pr).is_some()
    });
    assert_eq!(
        t.status().unwrap().state["pull_requests"][pr.to_string()]["head_sha"],
        new_head
    );
    assert!(
        !t.gh.pr(pr).unwrap().merged,
        "fresh reconciliation must not land cached data"
    );
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn shutdown_joins_an_inflight_watchdog_observation() {
    use schlange_github_fake::HookFaults;
    use std::sync::atomic::Ordering;
    let land_hook = PauseHook::new();
    land_hook.release();
    let observer_hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .pause_before_land(land_hook)
        .watchdog_pause(observer_hook.clone())
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .reconcile_every("1h")
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    for check in ["ci-ok", "Risk class and data protection", "KI-Review"] {
        t.pr(pr).check(check).succeed();
    }
    t.service.as_ref().unwrap().reconcile_now();
    t.pr(pr).wait_queued();
    t.advance("5m");
    let draft = t.queue_pr();
    t.settle();
    draft.check("ci-ok").succeed();
    t.serve_cfg
        .heartbeat_test_offset
        .as_ref()
        .unwrap()
        .store(61, Ordering::SeqCst);
    wait("observer read is in flight", || observer_hook.reached());
    let shutdown_requested = t.service.as_ref().unwrap().shutdown_probe();
    let (started_tx, started_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        started_tx.send(()).unwrap();
        t.shutdown();
        done_tx.send(()).unwrap();
    });
    started_rx.recv_timeout(Duration::from_secs(20)).unwrap();
    wait("shutdown flag set", shutdown_requested);
    assert!(
        done_rx.try_recv().is_err(),
        "shutdown waits for the in-flight observation"
    );
    observer_hook.release();
    done_rx
        .recv_timeout(Duration::from_secs(20))
        .expect("observer must stop after the read returns");
}

#[test]
fn watchdog_does_not_observe_a_lease_follower() {
    use schlange_github::time::Clock;
    use std::sync::atomic::Ordering;
    let hook = PauseHook::new();
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .lease("leader", 60)
        .pause_before_land(hook)
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a\n")
        .create();
    t.pr(pr).wait_queued();
    let cycles = t.serve_cfg.watchdog_test_cycles.as_ref().unwrap();
    let samples = t.serve_cfg.watchdog_test_samples.as_ref().unwrap();
    let offset = t.serve_cfg.heartbeat_test_offset.as_ref().unwrap();
    wait("initial watchdog cycle", || {
        cycles.load(Ordering::SeqCst) >= 1
    });
    offset.store(61, Ordering::SeqCst);
    wait("leader watchdog sampled active queue", || {
        samples.load(Ordering::SeqCst) >= 1
    });
    let mut cfg = t.serve_cfg.clone();
    cfg.client = schlange_github::client::Client::new(t.client.config().clone());
    cfg.lease.as_mut().unwrap().holder = "new-leader".into();
    cfg.pause_before_land = None;
    cfg.watchdog_test_cycles = None;
    cfg.watchdog_test_samples = None;
    cfg.watchdog_test_observations = None;
    cfg.heartbeat_test_offset = None;
    let ahead = Arc::new(ManualClock::new(t.clock.now() + 120));
    let new_leader = Service::start(cfg, ahead).unwrap();
    wait("new leader takes fence", || {
        status(&new_leader).is_some_and(|s| s.role == "leader")
    });
    t.advance("120s");
    wait("former leader steps down", || {
        t.status().is_some_and(|s| s.role == "follower")
    });
    let before = cycles.load(Ordering::SeqCst);
    let samples_before = samples.load(Ordering::SeqCst);
    offset.store(122, Ordering::SeqCst);
    wait("follower watchdog cycle", || {
        cycles.load(Ordering::SeqCst) > before
    });
    assert_eq!(
        samples.load(Ordering::SeqCst),
        samples_before,
        "a demoted leader must stop sampling its former queue"
    );
    new_leader.shutdown();
    t.shutdown();
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
    // Liveness, not safety: B gets as much wall-clock time as a loaded machine needs (the
    // virtual clock moves on in 30-second steps meanwhile); the safety assertions below do
    // not depend on how long this took.
    let deadline = Instant::now() + Duration::from_secs(180);
    while !t.gh.pr(pr).unwrap().merged && Instant::now() < deadline {
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
    // A steps down through whichever check sees the takeover first: the moved fence on
    // GitHub, or the lease another process now holds. Either way it sent nothing (above).
    assert!(
        a_status.log_tail.iter().any(|l| l.contains("fence moved")
            || l.contains("stepped down: another process holds the lease")),
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
    let deadline = Instant::now() + Duration::from_secs(120);
    while !status(&b).is_some_and(|s| s.queued.contains(&pr)) && Instant::now() < deadline {
        b.reconcile_now();
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(status(&b).unwrap().queued.contains(&pr));
    b.shutdown();
    let _ = temp_dir;
    t.shutdown();
}
