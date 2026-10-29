//! Identity read failures must retain a durable, visible hold before old facts can act.
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");

#[test]
fn failed_identity_read_holds_existing_queue_and_survives_restart() {
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .reconcile_every("1h")
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    t.pr(pr).wait_queued();
    t.settle();
    let before = t.status().unwrap().reconciliations;
    let head = t.gh.pr(pr).unwrap().head_sha;
    let request_count = t.gh.requests_with_query().len();
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Status(403),
        100,
    ));
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("failed read reconciliation", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    assert!(
        t.gh.requests_with_query()
            .iter()
            .skip(request_count)
            .any(|r| r.contains(&format!("/commits/{head}/check-suites")) && r.ends_with(" 403"))
    );
    let state = t.status().unwrap().state;
    assert!(
        state["check_identity_unavailable"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_u64() == Some(pr))),
        "{state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.stop_service();
    t.start_service();
    let state = t.status().unwrap().state;
    assert!(
        state["check_identity_unavailable"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_u64() == Some(pr))),
        "{state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.gh.clear_faults();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("fresh reconciliation after recovery", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert!(
        !state["check_identity_unavailable"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_u64() == Some(pr))),
        "{state}"
    );
    t.advance("5m");
    t.pr(pr).wait_merged();
    t.shutdown();
}

#[test]
fn failed_read_of_new_pull_request_installs_base_bound_hold() {
    let t = Tester::new(ARZTAGENT).reconcile_every("1h").start();
    t.gh.add_fault(FaultRule::new(
        "GET",
        "/check-suites",
        FaultKind::Status(403),
        100,
    ));
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/new.py", "a = 1\n")
        .create();
    t.settle();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("reconciliation of unknown pull", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert!(
        state["check_identity_targets"]["main"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_u64() == Some(pr))),
        "{state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn webhook_arriving_before_landing_prevents_stale_write() {
    use schlange_github::service::PauseHook;
    let hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .pause_before_land(hook.clone())
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/race.py", "a = 1\n")
        .create();
    t.pr(pr).wait_queued();
    t.advance("5m");
    t.wait_until("paused before landing", |_| hook.reached());
    t.gh.label(pr, "halt", true);
    let body = serde_json::json!({"repository": {"full_name": t.gh.config().full_name()},
        "ref": "refs/heads/main", "after": "abc", "deleted": false})
    .to_string();
    let signature = schlange_github::webhook::sign(&t.gh.config().webhook_secret, body.as_bytes());
    let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
        .header("x-hub-signature-256", &signature)
        .header("x-github-event", "push")
        .header("x-github-delivery", "before-stale-land")
        .header("content-type", "application/json")
        .send(&body)
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    hook.release();
    t.wait_dead();
    assert!(
        !t.gh.pr(pr).unwrap().merged,
        "a webhook arrived before the write"
    );
    assert!(
        !t.decisions()
            .iter()
            .any(|d| d.contains("\"type\":\"land\"")),
        "suppressed writes must not be logged as sent"
    );
    t.shutdown();
}

#[test]
fn startup_keeps_bootstrap_hold_until_a_validated_view_is_installed() {
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .fault(FaultRule::new(
            "GET",
            "/rules/branches/main",
            FaultKind::Status(403),
            100,
        ))
        .start();
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert_eq!(state["identity_recovery_prepared"], true, "{state}");
    let requests = t.gh.requests();
    assert!(
        requests
            .iter()
            .any(|r| r.contains("/rules/branches/main") && r.ends_with(" 403")),
        "{requests:?}"
    );
    assert!(
        !requests
            .iter()
            .any(|r| !r.starts_with("GET ") && !r.contains("/access_tokens ")),
        "{requests:?}"
    );
    t.shutdown();
}

#[test]
fn complete_fresh_batch_is_staged_but_bootstrap_stays_held() {
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/stage.py", "a = 1\n")
        .create();
    t.settle();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("validated reconciliation", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(
        state["pull_requests"].get(pr.to_string()).is_some(),
        "{state}"
    );
    assert!(
        state["check_identity_targets"]
            .as_object()
            .is_some_and(|m| m.is_empty()),
        "{state}"
    );
    assert!(
        state["identity_discovery_epoch"].as_u64().is_some(),
        "complete fenced discovery is required: {state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn late_lost_webhook_change_prevents_staged_batch_attestation() {
    use schlange_github::service::PauseHook;
    use schlange_github_fake::HookFaults;
    let hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .identity_before_attest(hook.clone())
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/race.py", "a = 1\n")
        .create();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("paused before identity attestation", |_| hook.reached());
    t.gh.label(pr, "halt", true);
    hook.release();
    t.wait_until("reconciliation after race", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(
        state.get("identity_discovery_epoch").is_none(),
        "late changed input must prevent attestation: {state}"
    );
    t.shutdown();
}

#[test]
fn staged_retarget_requires_fresh_atomic_cross_base_view() {
    use schlange_github_fake::HookFaults;
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/retarget.py", "a = 1\n")
        .create();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("initial staged pull", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    assert_eq!(
        t.status().unwrap().state["pull_requests"][pr.to_string()]["base"],
        "main"
    );
    t.gh.retarget_pr_for_test(pr, "other");
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("retargeted staged pull", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert_eq!(
        state["pull_requests"][pr.to_string()]["base"],
        "other",
        "{state}"
    );
    assert!(
        state["check_identity_targets"]
            .as_object()
            .is_some_and(|m| m.is_empty()),
        "{state}"
    );
    t.shutdown();
}

#[test]
fn retarget_read_denial_keeps_both_bases_held_until_fresh_retry() {
    use schlange_github_fake::HookFaults;
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/retarget-denied.py", "a = 1\n")
        .create();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("initial staged pull", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    t.gh.retarget_pr_for_test(pr, "other");
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{pr}"),
        FaultKind::Status(403),
        100,
    ));
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("denied retarget", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert_eq!(
        state["pull_requests"][pr.to_string()]["base"],
        "main",
        "{state}"
    );
    assert!(
        state["check_identity_targets"]["main"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{state}"
    );
    assert!(
        state["check_identity_targets"]["other"]
            .as_array()
            .is_some_and(|a| !a.is_empty()),
        "{state}"
    );
    assert!(state.get("identity_discovery_epoch").is_none(), "{state}");
    t.gh.clear_faults();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("fresh retarget retry", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(
        state["pull_requests"][pr.to_string()]["base"],
        "other",
        "{state}"
    );
    assert_eq!(state["identity_bootstrap_hold"], true);
    t.shutdown();
}

#[test]
fn crash_after_atomic_publication_recovers_under_bootstrap_hold() {
    use schlange_github::service::CrashPoint;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .crash_at(CrashPoint::AfterIdentityPublication)
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/crash.py", "a = 1\n")
        .create();
    t.wait_dead();
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.stop_service();
    t.serve_cfg.crash = None;
    t.start_service();
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(
        state["pull_requests"].get(pr.to_string()).is_some(),
        "{state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn crash_before_atomic_publication_does_not_publish_a_partial_view() {
    use schlange_github::service::CrashPoint;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .crash_at(CrashPoint::BeforeIdentityPublication)
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/pre-crash.py", "a = 1\n")
        .create();
    t.wait_dead();
    let crashed = t.status().unwrap().state;
    assert!(
        crashed["pull_requests"].get(pr.to_string()).is_none(),
        "{crashed}"
    );
    assert_eq!(crashed["identity_bootstrap_hold"], true);
    t.stop_service();
    t.serve_cfg.crash = None;
    t.start_service();
    let recovered = t.status().unwrap().state;
    assert_eq!(recovered["identity_bootstrap_hold"], true);
    assert!(
        recovered["pull_requests"].get(pr.to_string()).is_some(),
        "{recovered}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn empty_repository_releases_only_after_clean_validation() {
    let t = Tester::new(ARZTAGENT).persistent().start();
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], false, "{state}");
    assert!(
        state["pull_requests"]
            .as_object()
            .is_some_and(|p| p.is_empty())
    );
    assert!(state["queued"].as_object().is_some_and(|q| q.is_empty()));
    let writes: Vec<_> =
        t.gh.requests()
            .into_iter()
            .filter(|r| !r.starts_with("GET ") && !r.contains("/access_tokens "))
            .collect();
    assert!(
        writes.is_empty(),
        "empty validation must not write: {writes:?}"
    );
    t.shutdown();
}

#[test]
fn webhook_between_empty_validation_and_release_keeps_bootstrap_held() {
    use schlange_github::service::PauseHook;
    use schlange_github::webhook::sign;
    use std::sync::atomic::Ordering;
    let hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .identity_before_release(hook.clone())
        .start();
    let count = t
        .serve_cfg
        .identity_test_release_count
        .as_ref()
        .unwrap()
        .clone();
    assert_eq!(count.load(Ordering::SeqCst), 1);
    let body = serde_json::json!({"repository": {"full_name": t.gh.config().full_name()},
        "ref": "refs/heads/main", "after": "abc", "deleted": false})
    .to_string();
    let deliver = |id: &str| {
        let signature = sign(&t.gh.config().webhook_secret, body.as_bytes());
        let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
            .header("x-hub-signature-256", &signature)
            .header("x-github-event", "push")
            .header("x-github-delivery", id)
            .header("content-type", "application/json")
            .send(&body)
            .unwrap();
        assert_eq!(response.status().as_u16(), 202);
    };
    deliver("start-release-race");
    t.wait_until("paused before empty release", |_| hook.reached());
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/new.py", "a = 1\n")
        .create();
    deliver("intervening-change");
    hook.release();
    t.wait_until("new pull staged under bootstrap", |r| {
        r.status().is_some_and(|s| {
            s.state["identity_bootstrap_hold"] == true
                && s.state["pull_requests"].get(pr.to_string()).is_some()
        })
    });
    assert_eq!(
        count.load(Ordering::SeqCst),
        1,
        "a stale empty view must not release"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn lost_remote_lease_before_attestation_keeps_bootstrap_held() {
    use schlange_github::service::PauseHook;
    use schlange_github::time::{Clock, ManualClock};
    use std::sync::Arc;
    let hook = PauseHook::new();
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .identity_before_attest(hook.clone())
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/lease.py", "a = 1\n")
        .create();
    t.wait_until("paused before attestation", |_| hook.reached());
    let before = t.status().unwrap().reconciliations;
    let clock = Arc::new(ManualClock::new(t.clock.now() + 120));
    let other = t.second_service("host-b", None, clock);
    t.wait_until("other host holds lease", |_| {
        other.status().values().any(|s| s.role == "leader")
    });
    hook.release();
    t.wait_until("original host finishes the attempt", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(
        state.get("identity_discovery_epoch").is_none(),
        "lost lease must prevent attestation: {state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    other.shutdown();
    t.shutdown();
}

#[test]
fn empty_repository_without_journal_does_not_assume_clean_history() {
    let t = Tester::new(ARZTAGENT).start();
    let state = t.status().unwrap().state;
    assert_eq!(
        state["identity_bootstrap_hold"], true,
        "absence of a journal cannot prove no in-doubt work: {state}"
    );
    t.shutdown();
}

#[test]
fn control_after_empty_release_is_rejected_without_failstop() {
    let t = Tester::new(ARZTAGENT).persistent().start();
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], false);
    let (status, body) = t.admin(
        "PUT",
        "merge-queue/pause",
        Some(r#"{"reason":"maintenance"}"#),
    );
    assert_eq!(status, 422, "{body}");
    assert!(body.contains("identity recovery"), "{body}");
    assert!(t.service.as_ref().unwrap().dead_workers().is_empty());
    t.shutdown();
}

#[test]
fn clean_new_pull_can_enter_queue_after_complete_command_scan() {
    use schlange_github_fake::HookFaults;
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/clean.py", "a = 1\n")
        .create();
    let before = t.status().unwrap().reconciliations;
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("clean pull reconciliation", |r| {
        r.status().is_some_and(|s| s.reconciliations > before)
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], false, "{state}");
    assert!(state["queued"].get(pr.to_string()).is_some(), "{state}");
    assert!(
        t.gh.requests()
            .iter()
            .any(|r| r.contains(&format!("/issues/{pr}/comments"))),
        "command scan must read the pull's comments"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn empty_persistent_no_lease_does_not_assume_clean_lineage() {
    let t = Tester::new(ARZTAGENT).persistent().start();
    let state = t.status().unwrap().state;
    assert_eq!(
        state["identity_bootstrap_hold"], true,
        "no-lease startup cannot prove a clean prior writer: {state}"
    );
    t.shutdown();
}

#[test]
fn unknown_lease_lineage_does_not_overwrite_local_recovery_snapshot() {
    use schlange_core::State;
    use schlange_core::journal::Journal;
    use schlange_github::store::Store;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let store = Store::open(&dir).unwrap();
    let mut journal = store.journal(&t.gh.config().full_name());
    let mut old = State::default();
    old.host.bot = "preserve-previous-snapshot".into();
    journal.checkpoint(&old).unwrap();
    t.start_service();
    let recovered = journal.recover().unwrap().unwrap();
    assert_eq!(
        recovered.state.host.bot, "preserve-previous-snapshot",
        "untrusted lineage cannot overwrite prior state"
    );
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    t.shutdown();
}

#[test]
fn no_lease_startup_preserves_an_existing_recovery_snapshot() {
    use schlange_core::State;
    use schlange_core::journal::Journal;
    use schlange_github::store::Store;
    let mut t = Tester::new(ARZTAGENT).persistent().start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let store = Store::open(&dir).unwrap();
    let mut journal = store.journal(&t.gh.config().full_name());
    let mut old = State::default();
    old.host.bot = "previous-no-lease-snapshot".into();
    journal.checkpoint(&old).unwrap();
    t.start_service();
    let recovered = journal.recover().unwrap().unwrap();
    assert_eq!(recovered.state.host.bot, "previous-no-lease-snapshot");
    assert!(
        !recovered.state.identity_bootstrap_hold,
        "no-lease startup must not rewrite the old checkpoint"
    );
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    t.shutdown();
}

#[test]
fn trusted_takeover_corrupt_journal_stops_without_overwrite_or_effects() {
    use schlange_github::service::Service;
    use schlange_github::store::Store;
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
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
        assert!(Instant::now() < deadline, "trusted takeover must fail-stop");
        std::thread::sleep(Duration::from_millis(5));
    }
    let why = service.dead_workers().remove(&repo).unwrap();
    assert!(why.contains("journal recovery failed"), "{why}");
    let state: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(state, "{broken");
    let requests = t.gh.requests_with_query();
    for request in requests.iter().skip(count) {
        assert!(
            !(request.starts_with("POST ")
                || request.starts_with("PATCH ")
                || request.starts_with("PUT ")
                || request.starts_with("DELETE "))
                || request.contains("/git/refs/schlange/lease")
                || request.contains("/git/commits")
                || request.contains("/git/refs"),
            "unexpected effect: {request}"
        );
    }
    service.shutdown();
    t.shutdown();
}

#[test]
fn identity_permission_failure_on_open_pull_keeps_service_held() {
    use schlange_github::time::Clock;
    let t = Tester::new(ARZTAGENT).reconcile_every("1h").start();
    t.gh.set_required_checks(&["ci-ok"]);
    t.gh.set_classic_required_checks(Some(&[]));
    t.gh.add_fault(FaultRule::new(
        "GET",
        "/check-suites",
        FaultKind::Status(403),
        100,
    ));
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "identity read denied",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    let before = t.gh.requests_with_query().len();
    t.advance("1s");
    let expected_time = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until(
        "check suite permission denial and completed reconciliation",
        |r| {
            r.gh.requests_with_query()
                .iter()
                .skip(before)
                .any(|request| {
                    request.contains(&format!("/commits/{head}/check-suites"))
                        && request.ends_with(" 403")
                })
                && r.status()
                    .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= expected_time))
        },
    );
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(!t.gh.pr(pr).unwrap().merged);
    let repo = t.gh.config().full_name();
    let create_pull = format!("POST /repos/{repo}/pulls ");
    assert!(
        !t.gh
            .requests_with_query()
            .iter()
            .skip(before)
            .any(|r| r.starts_with(&create_pull)),
        "no queue draft creation request"
    );
    let decisions = t.service.as_ref().unwrap().decision_log(&repo);
    assert!(
        !decisions
            .iter()
            .any(|d| d.contains("land car") || d.contains("action of `")),
        "no Land or Act while held: {decisions:?}"
    );
    assert!(
        t.gh.open_queue_drafts().is_empty(),
        "no queue draft may be created from incomplete checks"
    );
    t.gh.clear_faults();
    t.gh.report_check_with_identity(pr, "ci-ok", "success", "ci.yml", "pull_request");
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("successful fresh check identity staging", |r| {
        r.status().is_some_and(|s| {
            s.state["pull_requests"][pr.to_string()]["checks"]
                .as_array()
                .is_some_and(|v| {
                    v.iter().any(|c| {
                        c["name"] == "ci-ok"
                            && c["app_id"] == 15368
                            && c["head_sha"] == head
                            && c["conclusion"] == "success"
                    })
                })
        })
    });
    assert_eq!(
        t.status().unwrap().state["identity_bootstrap_hold"],
        true,
        "successful read does not prove recovery release"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

#[test]
fn app_bound_requirement_is_staged_with_the_open_pull_under_global_hold() {
    use serde_json::json;
    let t = Tester::new(ARZTAGENT).reconcile_every("1h").start();
    t.gh.set_classic_required_checks(Some(&[]));
    t.gh.add_fault(FaultRule::new("GET", "/rules/branches/main", FaultKind::Json(json!([{
        "type": "required_status_checks", "ruleset_id": 77,
        "parameters": {"required_status_checks": [{"context": "ci-ok", "integration_id": 15368}]}
    }])), 1000));
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "bound check",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.report_check_with_identity(pr, "ci-ok", "success", "ci.yml", "pull_request");
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("app-bound requirement and PR staged", |r| {
        r.status().is_some_and(|s| {
            s.state["bound_required_checks"]["main"]
                .as_array()
                .is_some_and(|v| {
                    v.iter()
                        .any(|c| c["context"] == "ci-ok" && c["app_id"] == 15368)
                })
                && s.state["pull_requests"][pr.to_string()]["checks"]
                    .as_array()
                    .is_some_and(|checks| {
                        checks.iter().any(|check| {
                            check["name"] == "ci-ok"
                                && check["app_id"] == 15368
                                && check["head_sha"] == head
                                && check["conclusion"] == "success"
                        })
                    })
        })
    });
    let state = t.status().unwrap().state;
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(
        state["pull_requests"][pr.to_string()]["required_check_names"]
            .as_array()
            .is_some_and(|names| names.iter().any(|n| n == "@github-actions/ci-ok")),
        "{state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(t.gh.open_queue_drafts().is_empty());
    t.shutdown();
}

#[test]
fn staged_app_bound_requirement_does_not_bind_another_app_check() {
    use serde_json::json;
    let t = Tester::new(ARZTAGENT).reconcile_every("1h").start();
    t.gh.set_classic_required_checks(Some(&[]));
    t.gh.add_fault(FaultRule::new(
        "GET",
        "/rules/branches/main",
        FaultKind::Json(json!([{
            "type": "required_status_checks", "ruleset_id": 77,
            "parameters": {"required_status_checks": [{"context": "ci-ok", "integration_id": 42}]}
        }])),
        1000,
    ));
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "wrong app",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.report_check_with_identity(pr, "ci-ok", "success", "ci.yml", "pull_request");
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("wrong-app check and bound requirement staged", |r| {
        r.status().is_some_and(|s| {
            s.state["bound_required_checks"]["main"]
                .as_array()
                .is_some_and(|v| {
                    v.iter()
                        .any(|c| c["context"] == "ci-ok" && c["app_id"] == 42)
                })
                && s.state["pull_requests"][pr.to_string()]["checks"]
                    .as_array()
                    .is_some_and(|v| {
                        v.iter().any(|c| {
                            c["name"] == "ci-ok"
                                && c["app_id"] == 15368
                                && c["head_sha"] == head
                                && c["conclusion"] == "success"
                        })
                    })
        })
    });
    let state = t.status().unwrap().state;
    assert!(
        state["pull_requests"][pr.to_string()]["required_check_names"]
            .as_array()
            .is_none_or(|v| !v.iter().any(|n| n == "@github-actions/ci-ok")),
        "{state}"
    );
    assert_eq!(state["identity_bootstrap_hold"], true, "{state}");
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(t.gh.open_queue_drafts().is_empty());
    t.shutdown();
}

#[test]
fn no_lease_restart_preserves_queued_and_in_doubt_command_without_dispatch() {
    use schlange_core::State;
    use schlange_core::actions::{CommandReply, Op, PendingAction};
    use schlange_core::engine::{Decision, EntryStatus, QueueEntry};
    use schlange_core::journal::Journal;
    use schlange_core::model::PullRequest;
    use schlange_github::store::Store;
    use schlange_github::time::Clock;

    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "queued before crash",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    let op = Op::Update {
        number: pr,
        head_sha: head.clone(),
    };
    let mut state = State::default();
    state.pull_requests.insert(
        pr,
        PullRequest::new(pr, "arztagent-bot", "main", "pr-1", &head),
    );
    state.queued.insert(
        pr,
        QueueEntry {
            queue: "default".into(),
            rule: "queue".into(),
            since: 1,
            head_sha: head.clone(),
            base: "main".into(),
            seq: 1,
            waiting_since: 1,
            status: EntryStatus::Waiting,
            external_reset: false,
            by: Some("alice".into()),
            priority: 2000,
            priority_rule: None,
            interrupt: false,
        },
    );
    state.actions.pending.insert(
        9,
        PendingAction {
            rule: "command `update` by @alice".into(),
            op: op.clone(),
            level: None,
            reply: Some(CommandReply {
                comment: 77,
                sender: "alice".into(),
                command: "update".into(),
            }),
            report: None,
            sent: 1,
            attempts: 1,
            edge: None,
        },
    );
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    let mut journal = store.journal(&repo);
    journal.checkpoint(&state).unwrap();
    let decision = Decision::Act {
        id: 9,
        rule: "command `update` by @alice".into(),
        op,
    };
    journal.intent(1, &decision).unwrap();
    let before = t.gh.requests_with_query().len();
    t.start_service();
    t.advance("10m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("post-restart reconciliation completed", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    t.settle();
    let status = t.status().unwrap().state;
    assert_eq!(status["identity_bootstrap_hold"], true, "{status}");
    assert_eq!(
        status["queued"][pr.to_string()]["head_sha"],
        head,
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["reply"]["sender"], "alice",
        "{status}"
    );
    let recovered = journal.recover().unwrap().unwrap();
    assert_eq!(recovered.in_doubt, vec![(1, decision)]);
    assert_eq!(recovered.state.queued[&pr].head_sha, head);
    assert!(t.gh.open_queue_drafts().is_empty());
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(
        !t.gh
            .requests_with_query()
            .iter()
            .skip(before)
            .any(|r| r.starts_with("PUT ")
                || r.starts_with("PATCH ")
                || r.starts_with("DELETE ")
                || (r.starts_with("POST /repos/") && !r.contains("/access_tokens"))),
        "unknown lineage must not send forge writes"
    );
    t.shutdown();
}

#[test]
fn no_lease_restart_preserves_in_doubt_landing_and_command() {
    use schlange_core::State;
    use schlange_core::actions::{CommandReply, Op, PendingAction};
    use schlange_core::engine::{CarPr, Decision, EntryStatus, QueueEntry};
    use schlange_core::journal::Journal;
    use schlange_core::model::PullRequest;
    use schlange_core::queue::{Car, CarStatus, Train};
    use schlange_github::store::Store;
    use schlange_github::time::Clock;

    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "queued before crash",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    let op = Op::Update {
        number: pr,
        head_sha: head.clone(),
    };
    let mut state = State::default();
    state.pull_requests.insert(
        pr,
        PullRequest::new(pr, "arztagent-bot", "main", "pr-1", &head),
    );
    state.queued.insert(
        pr,
        QueueEntry {
            queue: "default".into(),
            rule: "queue".into(),
            since: 1,
            head_sha: head.clone(),
            base: "main".into(),
            seq: 1,
            waiting_since: 1,
            status: EntryStatus::InCar { car: 12 },
            external_reset: false,
            by: Some("alice".into()),
            priority: 2000,
            priority_rule: None,
            interrupt: false,
        },
    );
    let base_sha = t.gh.branch_sha("main").unwrap();
    let tested_tree = t.gh.tree_id_of(&head);
    let car_pr = CarPr {
        number: pr,
        head_sha: head.clone(),
    };
    let mut train = Train::new("main");
    train.base_sha = Some(base_sha.clone());
    train.cars.push(Car {
        id: 12,
        queue: "default".into(),
        branch: None,
        draft: Some(pr),
        in_place: true,
        base_sha: base_sha.clone(),
        prs: vec![car_pr.clone()],
        own: vec![pr],
        head_sha: Some(head.clone()),
        tree: Some(tested_tree.clone()),
        started: 1,
        status: CarStatus::Landing { since: 1 },
        test: None,
        attempt: 0,
        measured: false,
        drifted: false,
        replaces: None,
        deps: vec![],
        stacked_on: None,
        scopes: vec![],
        barrier: false,
    });
    state.trains.insert("main".into(), train);
    state.actions.pending.insert(
        9,
        PendingAction {
            rule: "command `update` by @alice".into(),
            op: op.clone(),
            level: None,
            reply: Some(CommandReply {
                comment: 77,
                sender: "alice".into(),
                command: "update".into(),
            }),
            report: None,
            sent: 1,
            attempts: 1,
            edge: None,
        },
    );
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    let mut journal = store.journal(&repo);
    journal.checkpoint(&state).unwrap();
    let decision = Decision::Act {
        id: 9,
        rule: "command `update` by @alice".into(),
        op,
    };
    journal.intent(1, &decision).unwrap();
    let landing = Decision::Land {
        car: 12,
        queue: "default".into(),
        base_branch: "main".into(),
        expected_base_sha: base_sha,
        prs: vec![car_pr],
        expected_tree: tested_tree,
        messages: vec![],
        method: Default::default(),
        head: Some(head.clone()),
        draft: None,
    };
    journal.intent(2, &landing).unwrap();
    let before = t.gh.requests_with_query().len();
    t.start_service();
    t.advance("10m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("post-restart reconciliation completed", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    t.settle();
    let status = t.status().unwrap().state;
    assert_eq!(status["identity_bootstrap_hold"], true, "{status}");
    assert_eq!(status["recovery_in_place"]["12"]["number"], pr, "{status}");
    assert_eq!(
        status["recovery_in_place"]["12"]["head_sha"], head,
        "{status}"
    );
    assert_eq!(
        status["queued"][pr.to_string()]["head_sha"],
        head,
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["reply"]["sender"], "alice",
        "{status}"
    );
    let recovered = journal.recover().unwrap().unwrap();
    assert_eq!(recovered.in_doubt, vec![(1, decision), (2, landing)]);
    assert_eq!(recovered.state.trains["main"].cars[0].id, 12);
    assert_eq!(
        status["trains"]["main"]["cars"][0]["status"]["status"], "landing",
        "{status}"
    );
    assert_eq!(recovered.state.queued[&pr].head_sha, head);
    assert!(t.gh.open_queue_drafts().is_empty());
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(
        !t.gh
            .requests_with_query()
            .iter()
            .skip(before)
            .any(|r| r.starts_with("PUT ")
                || r.starts_with("PATCH ")
                || r.starts_with("DELETE ")
                || (r.starts_with("POST /repos/") && !r.contains("/access_tokens"))),
        "unknown lineage must not send forge writes"
    );
    t.shutdown();
}

#[test]
fn no_lease_hold_does_not_execute_or_ack_a_real_command_after_head_changes() {
    use schlange_github::time::Clock;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .start();
    t.gh.set_collaborator("alice", "write");
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "command pending",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    t.settle();
    let old_head = t.gh.pr(pr).unwrap().head_sha;
    let comment = t.gh.comment(pr, "alice", "@mergifyio update");
    t.gh.push_pr(pr, &[("backend/a.py".into(), "a = 2\n".into())]);
    let head = t.gh.pr(pr).unwrap().head_sha;
    assert_ne!(head, old_head, "the command preceded a new head");
    let before = t.gh.requests_with_query().len();
    t.advance("1m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("post-command reconciliation", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    t.settle();
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    assert_eq!(t.gh.pr(pr).unwrap().head_sha, head);
    assert!(
        !t.gh.reactions().iter().any(|(id, _, _)| *id == comment),
        "held command must not be acknowledged"
    );
    t.stop_service();
    t.start_service();
    t.advance("1m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("post-restart command reconciliation", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    t.settle();
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    assert_eq!(t.gh.pr(pr).unwrap().head_sha, head);
    assert!(!t.gh.reactions().iter().any(|(id, _, _)| *id == comment));
    assert!(t.gh.comments_on(pr).iter().any(|c| c.id == comment));
    assert!(
        !t.gh
            .requests_with_query()
            .iter()
            .skip(before)
            .any(|r| r.starts_with("PUT ")
                || r.starts_with("PATCH ")
                || r.starts_with("DELETE ")
                || (r.starts_with("POST /repos/") && !r.contains("/access_tokens"))),
        "unverified command must not produce forge writes"
    );
    t.shutdown();
}

#[test]
fn trusted_leader_stops_before_checkpoint_when_renewal_breaks_lineage() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::store::Store;
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    // Test-only seed: the earlier acquisition established a real fake fence, while this
    // marker stands in for a previously established cooperating lineage.
    assert!(store.meta(&format!("fence:{repo}")).unwrap().is_some());
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let mut journal = store.journal(&repo);
    let old = State {
        counter: 77,
        ..State::default()
    };
    journal.checkpoint(&old).unwrap();
    let in_doubt = schlange_core::Decision::Refresh {
        token: 33,
        numbers: vec![1],
        branches: vec!["main".into()],
    };
    journal.intent(1, &in_doubt).unwrap();
    t.start_service();
    assert!(
        t.status().unwrap().recovered.is_some(),
        "trusted takeover must actually recover the journal"
    );
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let before_state: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    store
        .set_meta(&format!("fence:{repo}"), "broken-parent")
        .unwrap();
    let before_requests = t.gh.requests_with_query().len();
    t.advance("31s");
    t.service.as_ref().unwrap().reconcile_now();
    let deadline = Instant::now() + Duration::from_secs(5);
    while t
        .service
        .as_ref()
        .unwrap()
        .dead_workers()
        .get(&repo)
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "leader must fail-stop when trusted lineage breaks"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let why = t
        .service
        .as_ref()
        .unwrap()
        .dead_workers()
        .remove(&repo)
        .unwrap();
    assert!(why.contains("lineage"), "{why}");
    let after_state: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(
        after_state, before_state,
        "broken lineage must not checkpoint"
    );
    assert_eq!(
        journal.recover().unwrap().unwrap().in_doubt,
        vec![(1, in_doubt)]
    );
    for request in t.gh.requests_with_query().iter().skip(before_requests) {
        let write = request.starts_with("POST ")
            || request.starts_with("PUT ")
            || request.starts_with("PATCH ")
            || request.starts_with("DELETE ");
        let fence_commit = request.starts_with(&format!("POST /repos/{repo}/git/commits "));
        let fence_ref =
            request.starts_with(&format!("PATCH /repos/{repo}/git/refs/schlange/lease "));
        let token = request.contains("/access_tokens");
        assert!(
            !write || fence_commit || fence_ref || token,
            "unexpected non-lease effect: {request}"
        );
    }
    t.shutdown();
}

#[test]
fn worker_build_gate_rejects_signed_ingress_after_durable_claim() {
    worker_build_gate_case(false, true, false, false, false, false, false);
}

#[test]
fn worker_build_gate_rejects_ingress_after_created_ref() {
    worker_build_gate_case(true, true, false, false, false, false, false);
}

#[test]
fn worker_build_gate_sends_merge_without_ingress_change() {
    worker_build_gate_case(true, false, false, false, false, false, false);
}

#[test]
fn worker_build_gate_commits_verified_result() {
    worker_build_gate_case(true, false, true, false, false, false, false);
}

#[test]
fn worker_build_gate_rejects_ingress_before_completion() {
    worker_build_gate_case(false, true, true, false, false, false, false);
}

#[test]
fn worker_build_gate_keeps_claim_when_completion_fails() {
    worker_build_gate_case(false, false, true, true, false, false, false);
}

#[test]
fn worker_build_gate_rejects_fence_move_before_completion() {
    worker_build_gate_case(false, false, true, false, true, false, false);
}

#[test]
fn worker_build_gate_crash_before_completion_preserves_claim() {
    worker_build_gate_case(false, false, true, false, false, true, false);
}

#[test]
fn worker_build_gate_crash_after_completion_does_not_replay() {
    worker_build_gate_case(false, false, true, false, false, false, true);
}

fn worker_build_gate_case(
    after_create: bool,
    inject_ingress: bool,
    confirm_all: bool,
    fail_commit: bool,
    move_fence: bool,
    crash_before: bool,
    crash_after: bool,
) {
    use schlange_core::model::{CheckRun, Conclusion, PullRequest};
    use schlange_core::{Decision, Event, State, journal::Journal};
    use schlange_github::service::PauseHook;
    use schlange_github::store::Store;
    use schlange_github::time::Clock;
    use schlange_github_fake::HookFaults;
    use std::time::{Duration, Instant};
    let config = "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n";
    let mut t = Tester::new(config)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..HookFaults::default()
        })
        .start();
    t.stop_service();
    let pr = t.gh.open_pr(
        "alice",
        "build gate",
        &[("a".into(), "one".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    let base = t.gh.branch_sha("main").unwrap();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    if crash_before {
        t.serve_cfg.crash = Some(schlange_github::service::CrashPoint::BeforeBuildCommit);
    } else if crash_after {
        t.serve_cfg.crash = Some(schlange_github::service::CrashPoint::AfterBuildCommit);
    }
    t.start_service();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let fence = store.meta(&format!("fence:{repo}")).unwrap().unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water, fence) VALUES (?1, 'session-a', 'host-a', 7, ?2)", rusqlite::params![repo, fence]).unwrap();
    let mut journal = store.journal(&repo).fenced("host-a");
    let revision = store.inbox_revision(&repo).unwrap();
    let mut source = PullRequest::new(pr, "alice", "main", "pr-1", &head);
    source.checks = vec![CheckRun {
        name: "ci".into(),
        head_sha: head,
        conclusion: Some(Conclusion::Success),
        seq: 1,
        ..Default::default()
    }];
    let rules = schlange_config::rules::Rules::from_yaml(config).unwrap();
    let (state, decisions) = schlange_core::step(
        &State::default(),
        &rules,
        &Event::Snapshot {
            pull_requests: vec![source],
            branches: [("main".into(), Some(base))].into(),
            token: None,
        },
        t.clock.now(),
    );
    let assigned = journal
        .commit_session_step("session-a", revision, &state, &decisions, || t.clock.now())
        .unwrap();
    let (seq, build) = assigned
        .iter()
        .find(|(_, d)| matches!(d, Decision::BuildCar { .. }))
        .unwrap();
    let checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    let intent: String = db
        .query_row(
            "SELECT decision FROM intents WHERE repo = ?1 AND seq = ?2",
            rusqlite::params![repo, *seq as i64],
            |r| r.get(0),
        )
        .unwrap();
    let hook = PauseHook::new();
    t.gh.set_webhook_target(None);
    let before = t.gh.requests_with_query().len();
    t.service
        .as_ref()
        .unwrap()
        .test_start_session_build(
            "session-a",
            revision + 1,
            *seq,
            build.clone(),
            hook.clone(),
            after_create,
            confirm_all,
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(Instant::now() < deadline, "worker did not claim build");
        std::thread::yield_now();
    }
    let Decision::BuildCar {
        branch, base_sha, ..
    } = build
    else {
        unreachable!()
    };
    if after_create {
        assert_eq!(t.gh.branch_sha(branch).as_deref(), Some(base_sha.as_str()));
    } else if confirm_all {
        assert!(
            t.gh.branch_sha(branch).is_some(),
            "full build must exist before completion barrier"
        );
    } else {
        assert!(t.gh.branch_sha(branch).is_none());
    }
    let claim: (String, String, i64, i64, String, String) = db.query_row("SELECT holder, session, revision, seq, fence, decision FROM session_build_claims WHERE repo = ?1", [&repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))).unwrap();
    assert_eq!(
        claim,
        (
            "host-a".into(),
            "session-a".into(),
            (revision + 1) as i64,
            *seq as i64,
            fence.clone(),
            serde_json::to_string(build).unwrap()
        )
    );
    if fail_commit {
        db.execute_batch("CREATE TRIGGER fail_build_completion BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(ABORT, 'injected completion failure'); END;").unwrap();
    }
    if move_fence {
        t.gh.test_move_fence_to_default_tip();
    }
    let body = if inject_ingress {
        let new_head = t.gh.push_base(&[("race.txt".into(), "changed".into())]);
        let body = serde_json::json!({"repository":{"full_name":repo},"ref":"refs/heads/main","after":new_head,"deleted":false}).to_string();
        let sig = schlange_github::webhook::sign(&t.gh.config().webhook_secret, body.as_bytes());
        let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
            .header("x-hub-signature-256", &sig)
            .header("x-github-event", "push")
            .header("x-github-delivery", "build-race")
            .header("content-type", "application/json")
            .send(&body)
            .unwrap();
        assert_eq!(response.status().as_u16(), 202);
        Some(body)
    } else {
        None
    };
    hook.release();
    t.wait_dead();
    let reason = &t.service.as_ref().unwrap().dead_workers()[&repo];
    assert!(
        reason.contains(if crash_after {
            "injected crash after build commit"
        } else if crash_before {
            "injected crash before build commit"
        } else if move_fence {
            "fence moved"
        } else if fail_commit {
            "injected completion failure"
        } else if confirm_all && !inject_ingress {
            "test build committed"
        } else if inject_ingress {
            "build ingress changed"
        } else {
            "build attempt unresolved"
        }),
        "{reason}"
    );
    let writes: Vec<_> = t.gh.requests_with_query()[before..]
        .iter()
        .filter(|r| {
            let mut parts = r.split_whitespace();
            let method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
            let token = format!(
                "/app/installations/{}/access_tokens",
                t.gh.config().installation_id
            );
            let commits = format!("/repos/{repo}/git/commits");
            let fence_path = format!("/repos/{repo}/git/refs/schlange/lease");
            matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
                && !(method == "POST" && (path == token || path == commits))
                && !(method == "PATCH" && path == fence_path)
        })
        .cloned()
        .collect();
    if after_create || confirm_all {
        assert_eq!(
            writes.len(),
            if confirm_all {
                3
            } else if inject_ingress {
                1
            } else {
                2
            },
            "{writes:?}"
        );
        assert_eq!(
            writes[0].split_whitespace().take(2).collect::<Vec<_>>(),
            ["POST", &format!("/repos/{repo}/git/refs")]
        );
        if !inject_ingress || confirm_all {
            assert_eq!(
                writes[1].split_whitespace().take(2).collect::<Vec<_>>(),
                ["POST", &format!("/repos/{repo}/merges")]
            );
        }
        if confirm_all {
            assert_eq!(
                writes[2].split_whitespace().take(2).collect::<Vec<_>>(),
                ["POST", &format!("/repos/{repo}/pulls")]
            );
        }
    } else {
        assert!(writes.is_empty(), "{writes:?}");
    }
    if confirm_all && !inject_ingress && !fail_commit && !move_fence && !crash_before {
        let count: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM session_build_claims WHERE repo = ?1",
                [&repo],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "verified completion must remove its claim");
        let Decision::BuildCar { car, branch, .. } = build else {
            unreachable!()
        };
        let prefix = store
            .load_car(&repo, *car)
            .unwrap()
            .expect("prefix must be durable with completion");
        assert_eq!(prefix.len(), 1);
        assert_eq!(prefix[0].0, pr);
        let recovered = journal.recover().unwrap().unwrap();
        assert!(!recovered.in_doubt.iter().any(|(n, _)| n == seq));
        let saved_car = recovered
            .state
            .trains
            .values()
            .flat_map(|train| &train.cars)
            .find(|saved| saved.id == *car)
            .expect("completed car in checkpoint");
        assert_eq!(saved_car.branch.as_deref(), Some(branch.as_str()));
        assert_eq!(saved_car.tree.as_deref(), Some(prefix[0].1.as_str()));
        let draft = t.gh.pr(saved_car.draft.expect("draft recorded")).unwrap();
        assert!(draft.draft && draft.open);
        assert_eq!(draft.head_ref, *branch);
        assert_eq!(saved_car.head_sha.as_deref(), Some(draft.head_sha.as_str()));
        t.stop_service();
        if crash_after {
            t.serve_cfg.crash = None;
            let restart_at = t.gh.requests_with_query().len();
            let successor =
                schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone())
                    .unwrap();
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let statuses = successor.status();
                assert!(
                    successor.dead_workers().is_empty(),
                    "successor must recover held: {:?}",
                    successor.dead_workers()
                );
                if statuses.get(&repo).is_some_and(|s| s.reconciliations > 0) {
                    break;
                }
                assert!(
                    Instant::now() < deadline,
                    "successor did not reach a recovery boundary"
                );
                std::thread::yield_now();
            }
            assert_eq!(
                successor.status()[&repo].state["identity_bootstrap_hold"],
                true
            );
            assert_eq!(store.load_car(&repo, *car).unwrap(), Some(prefix.clone()));
            assert_eq!(t.gh.open_queue_drafts().len(), 1);
            successor.shutdown();
            let requests = t.gh.requests_with_query();
            assert!(
                !requests[restart_at..].iter().any(|r| r.starts_with("POST ")
                    && (r.contains("/merges") || r.contains("/pulls") || r.contains("/git/refs"))),
                "successor must not replay build writes"
            );
        }
        return;
    }
    assert!(journal.recover().is_err(), "claimed build remains unknown");
    if fail_commit {
        db.execute_batch("DROP TRIGGER fail_build_completion;")
            .unwrap();
    }
    let count: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM session_build_claims WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    if let Some(body) = body {
        let persisted_body: Vec<u8> = db
            .query_row(
                "SELECT body FROM inbox WHERE guid = 'build-race'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(persisted_body, body.as_bytes());
    }
    let after_checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    let after_intent: String = db
        .query_row(
            "SELECT decision FROM intents WHERE repo = ?1 AND seq = ?2",
            rusqlite::params![repo, *seq as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_checkpoint, checkpoint);
    assert_eq!(after_intent, intent);
    t.stop_service();
    if move_fence {
        return;
    }
    let restart_requests = t.gh.requests_with_query().len();
    t.serve_cfg.crash = None;
    let successor =
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !successor.dead_workers().contains_key(&repo) {
        assert!(Instant::now() < deadline, "successor did not stop");
        std::thread::yield_now();
    }
    assert!(successor.dead_workers()[&repo].contains("started build requires explicit recovery"));
    let after_restart: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_restart, checkpoint);
    let after_restart_intent: String = db
        .query_row(
            "SELECT decision FROM intents WHERE repo = ?1 AND seq = ?2",
            rusqlite::params![repo, *seq as i64],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_restart_intent, intent);
    successor.shutdown();
    let unexpected: Vec<_> = t.gh.requests_with_query()[restart_requests..]
        .iter()
        .filter(|r| {
            let mut parts = r.split_whitespace();
            let method = parts.next().unwrap_or("");
            let path = parts.next().unwrap_or("").split('?').next().unwrap_or("");
            let token = format!(
                "/app/installations/{}/access_tokens",
                t.gh.config().installation_id
            );
            let commits = format!("/repos/{repo}/git/commits");
            let fence_path = format!("/repos/{repo}/git/refs/schlange/lease");
            matches!(method, "POST" | "PUT" | "PATCH" | "DELETE")
                && !(method == "POST" && (path == token || path == commits))
                && !(method == "PATCH" && path == fence_path)
        })
        .cloned()
        .collect();
    assert!(unexpected.is_empty(), "{unexpected:?}");
}

#[test]
fn worker_refresh_persists_build_intent_when_mixed_with_cosmetics() {
    use schlange_core::{Decision, State, journal::Journal};
    use schlange_github::service::PauseHook;
    use schlange_github::store::Store;
    use schlange_github::time::Clock;
    use schlange_github_fake::HookFaults;
    use std::time::{Duration, Instant};
    let config = "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\n    merge_conditions:\n      - check-success = full\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n";
    let mut t = Tester::new(config)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..HookFaults::default()
        })
        .start();
    t.stop_service();
    let pr =
        t.gh.open_pr("alice", "green", &[("a".into(), "one".into())], false, &[]);
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.report_status(pr, "ci", "success");
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    t.start_service();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let mut journal = store.journal(&repo).fenced("host-a");
    let revision = store.inbox_revision(&repo).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'session-a', 'host-a', 7)", [&repo]).unwrap();
    let request = Decision::Refresh {
        token: 9,
        numbers: vec![pr],
        branches: vec!["main".into()],
    };
    let assigned = journal
        .commit_session_step(
            "session-a",
            revision,
            &State::default(),
            &[request.clone()],
            || t.clock.now(),
        )
        .unwrap();
    let hook = PauseHook::new();
    let request_start = t.gh.requests_with_query().len();
    t.service
        .as_ref()
        .unwrap()
        .test_start_session_refresh(
            "session-a",
            revision + 1,
            assigned[0].0,
            request,
            hook.clone(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    hook.release();
    t.wait_dead();
    assert!(
        t.service.as_ref().unwrap().dead_workers()[&repo]
            .contains("unsupported session successor effect")
    );
    let recovered = journal.held_redecide_snapshot().unwrap().unwrap();
    assert!(
        recovered
            .in_doubt
            .iter()
            .any(|(_, d)| matches!(d, Decision::BuildCar { prs, own, .. } if own == &[pr] && prs.len() == 1 && prs[0].number == pr && prs[0].head_sha == head)),
        "BuildCar must be durable even when cosmetics were emitted"
    );
    assert!(recovered.state.queued.contains_key(&pr));
    assert_eq!(recovered.state.queued[&pr].head_sha, head);
    assert!(
        recovered.state.pull_requests[&pr]
            .checks
            .iter()
            .any(|check| check.name == "ci"
                && check.status
                && check.head_sha == head
                && check.conclusion == Some(schlange_core::model::Conclusion::Success))
    );
    assert!(
        t.decisions()
            .iter()
            .any(|d| d.contains("\"type\":\"cosmetic\""))
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    for request in &t.gh.requests_with_query()[request_start..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path == format!("/repos/{repo}/git/commits")
                        || path == format!("/repos/{repo}/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH"
                        && path == format!("/repos/{repo}/git/refs/schlange/lease")),
                "unexpected write: {request}"
            );
        }
    }
    let before_checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    let before_intents = recovered.in_doubt.clone();
    t.stop_service();
    let before_restart = t.gh.requests_with_query().len();
    let successor =
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let statuses = successor.status();
        assert!(
            successor.dead_workers().is_empty(),
            "{:?}",
            successor.dead_workers()
        );
        if statuses.get(&repo).is_some_and(|s| s.reconciliations > 0) {
            break;
        }
        assert!(Instant::now() < deadline, "successor did not reconcile");
        std::thread::yield_now();
    }
    assert_eq!(
        successor.status()[&repo].state["identity_bootstrap_hold"],
        true
    );
    let after_checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_checkpoint, before_checkpoint);
    assert_eq!(journal.recover().unwrap().unwrap().in_doubt, before_intents);
    successor.shutdown();
    assert!(
        !t.gh.requests_with_query()[before_restart..]
            .iter()
            .any(|r| r.starts_with("POST ")
                && (r.contains("/merges") || r.contains("/pulls") || r.contains("/git/refs"))),
        "in-doubt build must not be replayed"
    );
    t.shutdown();
}

#[test]
fn worker_refresh_commits_fresh_reply_then_durably_reenters_hold() {
    use schlange_core::{Decision, State, journal::Journal};
    use schlange_github::service::PauseHook;
    use schlange_github::store::Store;
    use schlange_github::time::Clock;
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    t.start_service();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let mut journal = store.journal(&repo).fenced("host-a");
    let revision = store.inbox_revision(&repo).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'session-a', 'host-a', 7)", [&repo]).unwrap();
    let request = Decision::Refresh {
        token: 9,
        numbers: vec![],
        branches: vec!["main".into()],
    };
    let assigned = journal
        .commit_session_step(
            "session-a",
            revision,
            &State::default(),
            &[request.clone()],
            || t.clock.now(),
        )
        .unwrap();
    let expected = t.gh.branch_sha("main").unwrap();
    let hook = PauseHook::new();
    // This feature-gated harness seeds an already-released session; production release is not implemented.
    let requests = t.gh.requests_with_query().len();
    t.service
        .as_ref()
        .unwrap()
        .test_start_session_refresh(
            "session-a",
            revision + 1,
            assigned[0].0,
            request,
            hook.clone(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    hook.release();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !journal
        .held_redecide_snapshot()
        .unwrap()
        .unwrap()
        .in_doubt
        .is_empty()
    {
        assert!(Instant::now() < deadline, "fresh reply must commit");
        std::thread::yield_now();
    }
    let state = journal.held_redecide_snapshot().unwrap().unwrap().state;
    assert_eq!(state.branches["main"], expected);
    assert!(!state.identity_bootstrap_hold);
    assert_eq!(store.inbox_revision(&repo).unwrap(), revision + 2);
    t.service.as_ref().unwrap().reconcile_now();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !journal
        .held_redecide_snapshot()
        .unwrap()
        .unwrap()
        .state
        .identity_bootstrap_hold
    {
        assert!(
            Instant::now() < deadline,
            "rebootstrap hold must be durable"
        );
        std::thread::yield_now();
    }
    assert!(t.service.as_ref().unwrap().dead_workers().is_empty());
    assert_eq!(store.inbox_revision(&repo).unwrap(), revision + 3);
    let high: i64 = db
        .query_row(
            "SELECT high_water FROM status_sequence WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(high, 8);
    let observed = t.gh.requests_with_query();
    assert!(
        observed[requests..]
            .iter()
            .filter(|r| r.starts_with(&format!("GET /repos/{repo}/git/ref/heads/main")))
            .count()
            >= 2,
        "both refresh passes must read the branch"
    );
    for request in &observed[requests..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path == format!("/repos/{repo}/git/commits")
                        || path == format!("/repos/{repo}/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH"
                        && path == format!("/repos/{repo}/git/refs/schlange/lease")),
                "unexpected write: {request}"
            );
        }
    }
    t.shutdown();
}

#[test]
fn worker_refresh_rejects_signed_push_after_read_before_reply_commit() {
    use schlange_core::{Decision, State, journal::Journal};
    use schlange_github::service::PauseHook;
    use schlange_github::store::Store;
    use schlange_github::time::Clock;
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    t.start_service();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let mut journal = store.journal(&repo).fenced("host-a");
    let revision = store.inbox_revision(&repo).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'session-a', 'host-a', 7)", [&repo]).unwrap();
    let request = Decision::Refresh {
        token: 9,
        numbers: vec![],
        branches: vec!["main".into()],
    };
    let assigned = journal
        .commit_session_step(
            "session-a",
            revision,
            &State::default(),
            &[request.clone()],
            || t.clock.now(),
        )
        .unwrap();
    let before = db
        .query_row::<String, _, _>(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    let hook = PauseHook::new();
    let request_start = t.gh.requests_with_query().len();
    t.service
        .as_ref()
        .unwrap()
        .test_start_session_refresh(
            "session-a",
            revision + 1,
            assigned[0].0,
            request,
            hook.clone(),
        )
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(
            Instant::now() < deadline,
            "worker did not reach refresh read barrier"
        );
        std::thread::yield_now();
    }
    t.gh.set_webhook_target(None);
    let new_head = t.gh.push_base(&[("worker-race.txt".into(), "new".into())]);
    let body = serde_json::json!({"repository":{"full_name":repo},"ref":"refs/heads/main","after":new_head,"deleted":false}).to_string();
    let sig = schlange_github::webhook::sign(&t.gh.config().webhook_secret, body.as_bytes());
    let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
        .header("x-hub-signature-256", &sig)
        .header("x-github-event", "push")
        .header("x-github-delivery", "refresh-race-push")
        .header("content-type", "application/json")
        .send(&body)
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    let before_control = t.service.as_ref().unwrap().inflight();
    let admin_url = format!(
        "http://{}/repos/{repo}/merge-queue/pause",
        t.service.as_ref().unwrap().addr
    );
    let control = std::thread::spawn(move || {
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(10)))
                .build(),
        );
        agent
            .put(&admin_url)
            .header("authorization", "Bearer test-admin-token")
            .header("content-type", "application/json")
            .send(r#"{"reason":"queued after read"}"#)
            .unwrap()
            .status()
            .as_u16()
    });
    let deadline = Instant::now() + Duration::from_secs(10);
    while t.service.as_ref().unwrap().inflight() <= before_control {
        assert!(Instant::now() < deadline, "control was not queued");
        std::thread::yield_now();
    }
    hook.release();
    t.wait_dead();
    assert_eq!(
        control.join().unwrap(),
        503,
        "a stopped worker must not acknowledge the queued control"
    );
    assert!(
        t.service.as_ref().unwrap().dead_workers()[&repo]
            .contains("refresh ingress changed after read")
    );
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        before
    );
    assert_eq!(
        journal.held_redecide_snapshot().unwrap().unwrap().in_doubt,
        assigned
    );
    let pending = store.pending_inbox_all_epochs(&repo).unwrap();
    let delivered = pending
        .iter()
        .find(|p| p.guid == "refresh-race-push")
        .expect("signed input retained");
    assert_eq!(delivered.body, body.as_bytes());
    t.stop_service();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    t.start_service();
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    assert_eq!(
        journal.held_redecide_snapshot().unwrap().unwrap().in_doubt,
        assigned
    );
    assert!(
        store
            .pending_inbox_all_epochs(&repo)
            .unwrap()
            .contains(delivered)
    );
    for request in &t.gh.requests_with_query()[request_start..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path == format!("/repos/{repo}/git/commits")
                        || path == format!("/repos/{repo}/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH"
                        && path == format!("/repos/{repo}/git/refs/schlange/lease")),
                "unexpected write: {request}"
            );
        }
    }
    t.shutdown();
}

#[test]
fn clean_empty_status_redecides_and_releases_after_durable_ack() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::store::{InboxDelivery, Store};
    use std::time::{Duration, Instant};
    let config = "queue_rules:\n  - name: default\npull_request_rules: []\n";
    let mut t = Tester::new(config)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 0)", [&repo]).unwrap();
    let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "clean-status".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
    store.accept_inbox(&item, 10).unwrap();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    t.start_service();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let processed: i64 = db
            .query_row(
                "SELECT processed FROM inbox WHERE guid = 'clean-status'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        if processed == 1
            && !store.journal(&repo).has_inbox_redecide().unwrap()
            && t.status()
                .is_some_and(|s| s.state["identity_bootstrap_hold"] == false)
        {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "clean state should complete fenced re-decision and release"
        );
        std::thread::yield_now();
    }
    assert!(t.service.as_ref().unwrap().dead_workers().is_empty());
    t.shutdown();
}

#[test]
fn status_ack_crash_boundaries_keep_redecision_durable() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::service::{CrashPoint, Service};
    use schlange_github::store::{InboxDelivery, Store};
    use schlange_github::time::Clock;
    for (crash, expected_processed) in [
        (CrashPoint::BeforeHeldStatusAck, 0_i64),
        (CrashPoint::AfterHeldStatusAck, 1_i64),
    ] {
        let mut t = Tester::new(ARZTAGENT)
            .persistent()
            .lease("host-a", 60)
            .reconcile_every("1h")
            .start();
        t.stop_service();
        let dir = t.serve_cfg.data_dir.clone().unwrap();
        let repo = t.gh.config().full_name();
        let store = Store::open(&dir).unwrap();
        store
            .journal(&repo)
            .checkpoint(&State {
                counter: 77,
                ..State::default()
            })
            .unwrap();
        store
            .set_meta(&format!("fence-trust:{repo}"), "verified")
            .unwrap();
        let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
        let original_checkpoint: String = db
            .query_row(
                "SELECT state FROM checkpoints WHERE repo = ?1",
                [&repo],
                |r| r.get(0),
            )
            .unwrap();
        db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
        let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "crash-status".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
        store.accept_inbox(&item, 10).unwrap();
        t.serve_cfg.crash = Some(crash);
        t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
        t.advance("61s");
        let requests = t.gh.requests_with_query().len();
        let service = Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
        t.gh.set_webhook_target(Some(service.webhook_url()));
        t.service = Some(service);
        t.wait_dead();
        let expected_reason = if expected_processed == 0 {
            "injected crash before held status acknowledgment"
        } else {
            "injected crash after held status acknowledgment"
        };
        assert!(t.service.as_ref().unwrap().dead_workers()[&repo].contains(expected_reason));
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT processed FROM inbox WHERE guid = 'crash-status'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            expected_processed
        );
        assert_eq!(
            store.journal(&repo).has_inbox_redecide().unwrap(),
            expected_processed == 1
        );
        assert_eq!(
            db.query_row::<Vec<u8>, _, _>(
                "SELECT body FROM inbox WHERE guid = 'crash-status'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            item.body
        );
        let (session, holder, high, confirmed): (String, String, i64, i64) = db.query_row("SELECT session, holder, high_water, confirmed FROM status_sequence WHERE repo = ?1", [&repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))).unwrap();
        assert_ne!(session, "old-session");
        assert_eq!((holder.as_str(), high, confirmed), ("host-b", 7, 1));
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT COUNT(*) FROM intents WHERE repo = ?1",
                [&repo],
                |r| r.get(0)
            )
            .unwrap(),
            0
        );
        if expected_processed == 0 {
            assert_eq!(
                db.query_row::<String, _, _>(
                    "SELECT state FROM checkpoints WHERE repo = ?1",
                    [&repo],
                    |r| r.get(0)
                )
                .unwrap(),
                original_checkpoint
            );
        }
        t.stop_service();
        t.serve_cfg.crash = None;
        t.serve_cfg.lease.as_mut().unwrap().holder = "host-c".into();
        t.advance("61s");
        t.start_service();
        t.advance("1s");
        let target = t.clock.now();
        t.service.as_ref().unwrap().reconcile_now();
        t.wait_until("successor held after status crash", |r| {
            r.status().is_some_and(|s| {
                s.state["identity_bootstrap_hold"] == true
                    && s.last_reconcile.is_some_and(|at| at >= target)
            })
        });
        assert!(store.journal(&repo).has_inbox_redecide().unwrap());
        assert_eq!(
            db.query_row::<i64, _, _>(
                "SELECT processed FROM inbox WHERE guid = 'crash-status'",
                [],
                |r| r.get(0)
            )
            .unwrap(),
            1
        );
        let state: String = db
            .query_row(
                "SELECT state FROM checkpoints WHERE repo = ?1",
                [&repo],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&state).unwrap()["counter"],
            77
        );
        for request in &t.gh.requests_with_query()[requests..] {
            let mut parts = request.split_whitespace();
            let method = parts.next().unwrap();
            let path = parts.next().unwrap().split('?').next().unwrap();
            if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
                assert!(
                    (method == "POST"
                        && (path == format!("/repos/{repo}/git/commits")
                            || path == format!("/repos/{repo}/git/refs")
                            || path.ends_with("/access_tokens")))
                        || (method == "PATCH"
                            && path == format!("/repos/{repo}/git/refs/schlange/lease")),
                    "unexpected write: {request}"
                );
            }
        }
        t.shutdown();
    }
}

#[test]
fn status_ack_does_not_commit_after_remote_fence_moves() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::service::{PauseHook, Service};
    use schlange_github::store::{InboxDelivery, Store};
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "fenced-status".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
    store.accept_inbox(&item, 10).unwrap();
    let hook = PauseHook::new();
    t.serve_cfg.identity_test_before_status_ack = Some(hook.clone());
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    let service = Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    t.gh.set_webhook_target(Some(service.webhook_url()));
    t.service = Some(service);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    t.gh.test_move_fence_to_default_tip();
    hook.release();
    t.wait_until("fence loss observed", |r| {
        r.status().is_some_and(|s| s.lease.is_some_and(|l| !l.held))
    });
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT processed FROM inbox WHERE guid = 'fenced-status'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert!(!store.journal(&repo).has_inbox_redecide().unwrap());
    assert_eq!(store.pending_inbox_all_epochs(&repo).unwrap(), vec![item]);
    t.shutdown();
}

#[test]
fn status_hint_stays_pending_when_fresh_check_read_is_denied() {
    status_hint_pending_after_check_fault(403);
}

#[test]
fn status_hint_stays_pending_when_fresh_check_read_is_rate_limited() {
    status_hint_pending_after_check_fault(429);
}

fn status_hint_pending_after_check_fault(code: u16) {
    use schlange_core::{State, journal::Journal, model::PullRequest};
    use schlange_github::store::{InboxDelivery, Store};
    use schlange_github::time::Clock;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let pr = t.gh.open_pr(
        "alice",
        "status target",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    let mut initial = State::default();
    initial
        .pull_requests
        .insert(pr, PullRequest::new(pr, "alice", "main", "pr-1", &head));
    store.journal(&repo).checkpoint(&initial).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "denied-status".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"{head}","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
    store.accept_inbox(&item, 10).unwrap();
    let request_start = t.gh.requests_with_query().len();
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Status(code),
        100,
    ));
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    t.start_service();
    t.advance("1s");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("denied read completed", |r| {
        r.status().is_some_and(|s| {
            if code == 429 {
                s.log_tail.iter().any(|line| {
                    line.contains(&format!("/commits/{head}/check-suites"))
                        && line.contains("rate limited")
                })
            } else {
                s.last_reconcile.is_some_and(|at| at >= target)
            }
        })
    });
    assert!(
        t.gh.requests_with_query()[request_start..]
            .iter()
            .any(|r| r.contains(&format!("/commits/{head}/check-suites"))
                && r.ends_with(&format!(" {code}")))
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT processed FROM inbox WHERE guid = 'denied-status'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0
    );
    assert!(
        store
            .pending_inbox_all_epochs(&repo)
            .unwrap()
            .iter()
            .any(|p| p == &item)
    );
    assert!(!store.journal(&repo).has_inbox_redecide().unwrap());
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    for request in &t.gh.requests_with_query()[request_start..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path.ends_with("/git/commits")
                        || path.ends_with("/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH" && path.ends_with("/git/refs/schlange/lease")),
                "unexpected write: {request}"
            );
        }
    }
    if code == 403 {
        use std::time::{Duration, Instant};
        t.gh.clear_faults();
        t.advance("1s");
        t.service.as_ref().unwrap().reconcile_now();
        let deadline = Instant::now() + Duration::from_secs(10);
        while db
            .query_row::<i64, _, _>(
                "SELECT processed FROM inbox WHERE guid = 'denied-status'",
                [],
                |r| r.get(0),
            )
            .unwrap()
            == 0
        {
            assert!(
                Instant::now() < deadline,
                "the same status must be acknowledged after a successful fresh read"
            );
            std::thread::yield_now();
        }
        assert!(store.journal(&repo).has_inbox_redecide().unwrap());
        assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    }
    t.shutdown();
}

#[test]
fn later_signed_status_wakes_held_reconciliation_after_inbox_drains() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::store::{InboxDelivery, Store};
    use schlange_github::time::Clock;
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store.journal(&repo).checkpoint(&State::default()).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let body = |id: u64, state: &str| {
        format!(
            r#"{{"repository":{{"full_name":"{repo}"}},"id":{id},"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"{state}","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#
        )
    };
    store
        .accept_inbox(
            &InboxDelivery {
                repo: repo.clone(),
                epoch: "old".into(),
                guid: "first-status".into(),
                event: "status".into(),
                body: body(42, "success").into_bytes(),
                received_at: 1,
            },
            10,
        )
        .unwrap();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    t.start_service();
    let deadline = Instant::now() + Duration::from_secs(10);
    while store.has_pending_inbox(&repo).unwrap() {
        assert!(Instant::now() < deadline);
        std::thread::yield_now();
    }
    let later = body(43, "failure");
    let signature = schlange_github::webhook::sign(&t.gh.config().webhook_secret, later.as_bytes());
    let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
        .header("x-hub-signature-256", &signature)
        .header("x-github-event", "status")
        .header("x-github-delivery", "later-status")
        .header("content-type", "application/json")
        .send(&later)
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    let deadline = Instant::now() + Duration::from_secs(10);
    while db
        .query_row::<i64, _, _>(
            "SELECT processed FROM inbox WHERE guid = 'later-status'",
            [],
            |r| r.get(0),
        )
        .unwrap()
        == 0
    {
        assert!(
            Instant::now() < deadline,
            "later accepted status must cause another fresh held reconciliation"
        );
        std::thread::yield_now();
    }
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    assert!(
        db.query_row::<i64, _, _>(
            "SELECT revision FROM inbox_redecide WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap()
            > 0
    );
    let edited = format!(
        r#"{{"action":"edited","repository":{{"full_name":"{repo}"}},"issue":{{"number":5,"pull_request":{{}}}},"comment":{{"id":77,"user":{{"login":"bot"}},"body":"- [x] queue","updated_at":"2026-01-01T00:00:00Z"}},"sender":{{"login":"alice"}}}}"#
    );
    for (event, guid, payload) in [
        ("issue_comment", "older-checkbox", edited),
        ("status", "blocked-status", body(44, "success")),
    ] {
        let sig = schlange_github::webhook::sign(&t.gh.config().webhook_secret, payload.as_bytes());
        let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
            .header("x-hub-signature-256", &sig)
            .header("x-github-event", event)
            .header("x-github-delivery", guid)
            .header("content-type", "application/json")
            .send(&payload)
            .unwrap();
        assert_eq!(response.status().as_u16(), 202);
    }
    t.advance("1s");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("unsupported oldest remains held", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    let pending = store.pending_inbox_all_epochs(&repo).unwrap();
    assert_eq!(
        pending.iter().map(|p| p.guid.as_str()).collect::<Vec<_>>(),
        vec!["older-checkbox", "blocked-status"]
    );
    t.stop_service();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-c".into();
    t.advance("61s");
    t.start_service();
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    assert_eq!(store.pending_inbox_all_epochs(&repo).unwrap(), pending);
    assert!(
        db.query_row::<i64, _, _>(
            "SELECT revision FROM inbox_redecide WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap()
            > 0
    );
    t.shutdown();
}

#[test]
fn trusted_status_hint_is_acknowledged_only_after_held_full_reconcile() {
    use schlange_core::model::PullRequest;
    use schlange_core::{State, journal::Journal};
    use schlange_github::service::PauseHook;
    use schlange_github::store::{InboxDelivery, Store};
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let pr = t.gh.open_pr(
        "alice",
        "status target",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.report_status(pr, "ci", "success");
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    let mut initial = State {
        counter: 77,
        ..State::default()
    };
    initial
        .pull_requests
        .insert(pr, PullRequest::new(pr, "alice", "main", "pr-1", &head));
    store.journal(&repo).checkpoint(&initial).unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "status-guid".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"{head}","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
    assert!(
        item.replay_hint().is_some(),
        "status must be accepted as hint, head={head}"
    );
    store.accept_inbox(&item, 10).unwrap();
    assert!(
        store
            .journal(&repo)
            .pending_status_snapshot()
            .unwrap()
            .is_some(),
        "fixture must have clean recoverable status"
    );
    let mut second = item.clone();
    second.guid = "status-guid-second".into();
    store.accept_inbox(&second, 10).unwrap();
    let requests = t.gh.requests_with_query().len();
    let hook = PauseHook::new();
    t.serve_cfg.identity_test_before_status_ack = Some(hook.clone());
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    let service =
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    t.gh.set_webhook_target(Some(service.webhook_url()));
    t.service = Some(service);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(Instant::now() < deadline, "status ack hook not reached");
        std::thread::yield_now();
    }
    let comment = format!(
        r#"{{"action":"edited","repository":{{"full_name":"{repo}"}},"issue":{{"number":{pr},"pull_request":{{}}}},"comment":{{"id":77,"user":{{"login":"bot"}},"body":"- [x] queue","updated_at":"2026-01-01T00:00:00Z"}},"sender":{{"login":"alice"}}}}"#
    );
    let sig = schlange_github::webhook::sign(&t.gh.config().webhook_secret, comment.as_bytes());
    let response = ureq::post(&t.service.as_ref().unwrap().webhook_url())
        .header("x-hub-signature-256", &sig)
        .header("x-github-event", "issue_comment")
        .header("x-github-delivery", "during-status-ack")
        .header("content-type", "application/json")
        .send(&comment)
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Status(403),
        100,
    ));
    hook.release();
    use schlange_github::time::Clock;
    t.advance("1s");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("post-ingress denied read", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    assert!(
        t.status()
            .unwrap()
            .log_tail
            .iter()
            .any(|line| line.contains("held status ingress mismatch"))
    );
    assert!(
        t.gh.requests_with_query()[requests..]
            .iter()
            .any(|r| r.contains(&format!("/commits/{head}/check-suites")) && r.ends_with(" 403"))
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT processed FROM inbox WHERE guid = 'status-guid'",
            [],
            |r| r.get(0)
        )
        .unwrap(),
        0,
        "new ingress invalidates pending ack"
    );
    assert!(!store.journal(&repo).has_inbox_redecide().unwrap());
    let checkbox = store
        .pending_inbox_all_epochs(&repo)
        .unwrap()
        .into_iter()
        .find(|p| p.guid == "during-status-ack")
        .expect("edited checkbox retained");
    assert_eq!(checkbox.body, comment.as_bytes());
    t.gh.clear_faults();
    t.advance("1s");
    t.service.as_ref().unwrap().reconcile_now();
    let deadline = Instant::now() + Duration::from_secs(10);
    while db
        .query_row::<i64, _, _>(
            "SELECT processed FROM inbox WHERE guid = 'status-guid'",
            [],
            |r| r.get(0),
        )
        .unwrap()
        == 0
    {
        assert!(
            Instant::now() < deadline,
            "status hint must be durably acknowledged after reconciliation: {:?}",
            t.status()
        );
        std::thread::yield_now();
    }
    let deadline = Instant::now() + Duration::from_secs(10);
    while db
        .query_row::<i64, _, _>(
            "SELECT processed FROM inbox WHERE guid = 'status-guid-second'",
            [],
            |r| r.get(0),
        )
        .unwrap()
        == 0
    {
        assert!(
            Instant::now() < deadline,
            "second queued status must not need restart"
        );
        std::thread::yield_now();
    }
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    let state: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    let state: serde_json::Value = serde_json::from_str(&state).unwrap();
    assert_eq!(state["counter"], 77);
    assert_eq!(state["identity_bootstrap_hold"], true);
    assert_eq!(state["pull_requests"][pr.to_string()]["head_sha"], head);
    assert!(
        state["pull_requests"][pr.to_string()]["checks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|check| check["name"] == "ci"
                && check["status"] == true
                && check["head_sha"] == head
                && check["conclusion"] == "success"),
        "fresh status must be present"
    );
    assert!(
        t.gh.requests_with_query()[requests..]
            .iter()
            .any(|r| r.starts_with(&format!("GET /repos/{repo}/pulls/{pr} "))),
        "target PR must be read before acknowledgment"
    );
    assert!(
        t.gh.requests_with_query()[requests..]
            .iter()
            .any(|r| r.starts_with(&format!("GET /repos/{repo}/pulls?"))),
        "fresh discovery must occur before acknowledgment"
    );
    let marker: i64 = db
        .query_row(
            "SELECT revision FROM inbox_redecide WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    let persisted: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    t.stop_service();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-c".into();
    t.advance("61s");
    let service =
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    t.gh.set_webhook_target(Some(service.webhook_url()));
    t.service = Some(service);
    t.wait_until("restart held", |r| {
        r.status()
            .is_some_and(|s| s.state["identity_bootstrap_hold"] == true && s.reconciliations > 0)
    });
    assert!(t.service.as_ref().unwrap().dead_workers().is_empty());
    assert!(
        store
            .pending_inbox_all_epochs(&repo)
            .unwrap()
            .contains(&checkbox)
    );
    assert_eq!(
        db.query_row::<i64, _, _>(
            "SELECT revision FROM inbox_redecide WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        marker
    );
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        persisted
    );
    for request in &t.gh.requests_with_query()[requests..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path.ends_with("/git/commits")
                        || path.ends_with("/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH" && path.ends_with("/git/refs/schlange/lease")),
                "unexpected write: {request}"
            );
        }
    }
    t.shutdown();
}

#[test]
fn crash_after_session_rotation_keeps_successor_held() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::service::{CrashPoint, Service};
    use schlange_github::store::{InboxDelivery, Store};
    use schlange_github::time::Clock;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    store
        .journal(&repo)
        .checkpoint(&State {
            counter: 77,
            ..State::default()
        })
        .unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "status-guid".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
    store.accept_inbox(&item, 10).unwrap();
    let checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    t.serve_cfg.crash = Some(CrashPoint::AfterStatusSessionRotation);
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    let service = Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    t.gh.set_webhook_target(Some(service.webhook_url()));
    t.service = Some(service);
    t.wait_dead();
    assert!(
        t.service.as_ref().unwrap().dead_workers()[&repo]
            .contains("injected crash after status session rotation")
    );
    let (session, confirmed): (String, i64) = db
        .query_row(
            "SELECT session, confirmed FROM status_sequence WHERE repo = ?1",
            [&repo],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_ne!(session, "old-session");
    assert_eq!(confirmed, 0);
    t.stop_service();
    t.serve_cfg.crash = None;
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-c".into();
    t.advance("61s");
    let service = Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    t.gh.set_webhook_target(Some(service.webhook_url()));
    t.service = Some(service);
    t.advance("10m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("successor held after unconfirmed session", |r| {
        r.status().is_some_and(|s| {
            s.state["identity_bootstrap_hold"] == true
                && s.last_reconcile.is_some_and(|at| at >= target)
                && s.lease.is_some_and(|l| l.held)
        })
    });
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT session FROM status_sequence WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        session
    );
    assert_eq!(store.pending_inbox_all_epochs(&repo).unwrap(), vec![item]);
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        checkpoint
    );
    assert_ne!(
        store
            .meta(&format!("fence-trust:{repo}"))
            .unwrap()
            .as_deref(),
        Some("verified")
    );
    t.shutdown();
}

#[test]
fn fence_move_after_session_rotation_preserves_inbox_and_blocks_successor() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::service::PauseHook;
    use schlange_github::store::{InboxDelivery, Store};
    use schlange_github::time::Clock;
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    let mut journal = store.journal(&repo);
    journal
        .checkpoint(&State {
            counter: 77,
            ..State::default()
        })
        .unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let item = InboxDelivery { repo: repo.clone(), epoch: "old".into(), guid: "status-guid".into(), event: "status".into(), body: format!(r#"{{"repository":{{"full_name":"{repo}"}},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}}"#).into_bytes(), received_at: 1 };
    store.accept_inbox(&item, 10).unwrap();
    let hook = PauseHook::new();
    t.serve_cfg.identity_test_after_session_rotate = Some(hook.clone());
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    let request_start = t.gh.requests_with_query().len();
    let service =
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap();
    t.gh.set_webhook_target(Some(service.webhook_url()));
    t.service = Some(service);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !hook.reached() {
        assert!(Instant::now() < deadline, "rotation hook not reached");
        std::thread::yield_now();
    }
    let (rotated, holder, high): (String, String, i64) = db
        .query_row(
            "SELECT session, holder, high_water FROM status_sequence WHERE repo = ?1",
            [&repo],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_ne!(rotated, "old-session");
    assert_eq!((holder.as_str(), high), ("host-b", 7));
    let checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    t.gh.test_move_fence_to_default_tip();
    hook.release();
    t.wait_dead();
    assert!(
        t.service.as_ref().unwrap().dead_workers()[&repo]
            .contains("lease fence changed after session rotation")
    );
    assert_eq!(
        store.pending_inbox_all_epochs(&repo).unwrap(),
        vec![item.clone()]
    );
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        checkpoint
    );
    t.stop_service();
    t.serve_cfg.identity_test_after_session_rotate = None;
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-c".into();
    t.advance("61s");
    t.start_service();
    t.advance("10m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("successor held", |r| {
        r.status().is_some_and(|s| {
            s.state["identity_bootstrap_hold"] == true
                && s.last_reconcile.is_some_and(|at| at >= target)
                && s.lease.is_some_and(|l| l.held)
        })
    });
    assert_eq!(
        store
            .meta(&format!("fence-trust:{repo}"))
            .unwrap()
            .as_deref(),
        Some("untrusted")
    );
    let after_session: String = db
        .query_row(
            "SELECT session FROM status_sequence WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_session, rotated);
    assert_eq!(store.pending_inbox_all_epochs(&repo).unwrap(), vec![item]);
    assert_eq!(
        db.query_row::<String, _, _>(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0)
        )
        .unwrap(),
        checkpoint
    );
    for request in &t.gh.requests_with_query()[request_start..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().unwrap();
        let path = parts.next().unwrap().split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path.ends_with("/git/commits")
                        || path.ends_with("/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH" && path.ends_with("/git/refs/schlange/lease")),
                "unexpected write: {request}"
            );
        }
    }
    t.shutdown();
}

#[test]
fn trusted_successor_rotates_session_without_consuming_checkbox() {
    use schlange_core::{State, journal::Journal};
    use schlange_github::store::{InboxDelivery, Store};
    use std::time::{Duration, Instant};
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    assert!(store.meta(&format!("fence:{repo}")).unwrap().is_some());
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let mut journal = store.journal(&repo);
    journal
        .checkpoint(&State {
            counter: 77,
            ..State::default()
        })
        .unwrap();
    let checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'old-session', 'host-a', 7)", [&repo]).unwrap();
    let body = format!(r#"{{"action":"edited","repository":{{"full_name":"{repo}"}},"issue":{{"number":5,"pull_request":{{}}}},"comment":{{"id":77,"user":{{"login":"bot"}},"body":"- [x] queue"}},"sender":{{"login":"alice"}}}}"#).into_bytes();
    let item = InboxDelivery {
        repo: repo.clone(),
        epoch: "old".into(),
        guid: "checkbox-guid".into(),
        event: "issue_comment".into(),
        body,
        received_at: 1,
    };
    store.accept_inbox(&item, 10).unwrap();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    let request_start = t.gh.requests_with_query().len();
    t.start_service();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let (session, holder, high): (String, String, i64) = db
            .query_row(
                "SELECT session, holder, high_water FROM status_sequence WHERE repo = ?1",
                [&repo],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        if holder == "host-b" {
            assert_ne!(session, "old-session");
            assert_eq!(high, 7);
            break;
        }
        assert!(
            Instant::now() < deadline,
            "trusted successor must issue a new session"
        );
        std::thread::yield_now();
    }
    assert_eq!(store.pending_inbox_all_epochs(&repo).unwrap(), vec![item]);
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    let after: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after, checkpoint);
    for request in &t.gh.requests_with_query()[request_start..] {
        let mut parts = request.split_whitespace();
        let method = parts.next().expect("method");
        let path = parts.next().expect("path").split('?').next().unwrap();
        if matches!(method, "POST" | "PATCH" | "DELETE" | "PUT") {
            assert!(
                (method == "POST"
                    && (path.ends_with("/git/commits")
                        || path.ends_with("/git/refs")
                        || path.ends_with("/access_tokens")))
                    || (method == "PATCH" && path.ends_with("/git/refs/schlange/lease")),
                "unexpected write: {method} {path}"
            );
        }
    }
    t.shutdown();
}

#[test]
fn seeded_cooperating_successor_recovers_nonempty_journal_and_stays_held() {
    use schlange_core::State;
    use schlange_core::actions::{CommandReply, Op, PendingAction};
    use schlange_core::engine::{Decision, EntryStatus, QueueEntry};
    use schlange_core::journal::Journal;
    use schlange_core::model::PullRequest;
    use schlange_github::store::Store;
    use schlange_github::time::Clock;

    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "queued before crash",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    let op = Op::Update {
        number: pr,
        head_sha: head.clone(),
    };
    let mut state = State::default();
    state.pull_requests.insert(
        pr,
        PullRequest::new(pr, "arztagent-bot", "main", "pr-1", &head),
    );
    state.queued.insert(
        pr,
        QueueEntry {
            queue: "default".into(),
            rule: "queue".into(),
            since: 1,
            head_sha: head.clone(),
            base: "main".into(),
            seq: 1,
            waiting_since: 1,
            status: EntryStatus::Waiting,
            external_reset: false,
            by: Some("alice".into()),
            priority: 2000,
            priority_rule: None,
            interrupt: false,
        },
    );
    state.actions.pending.insert(
        9,
        PendingAction {
            rule: "command `update` by @alice".into(),
            op: op.clone(),
            level: None,
            reply: Some(CommandReply {
                comment: 77,
                sender: "alice".into(),
                command: "update".into(),
            }),
            report: None,
            sent: 1,
            attempts: 1,
            edge: None,
        },
    );
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = Store::open(&dir).unwrap();
    let mut journal = store.journal(&repo);
    journal.checkpoint(&state).unwrap();
    let decision = Decision::Act {
        id: 9,
        rule: "command `update` by @alice".into(),
        op,
    };
    journal.intent(1, &decision).unwrap();
    assert!(store.meta(&format!("fence:{repo}")).unwrap().is_some());
    // Test-only trust seed represents an already established cooperating lineage.
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    t.serve_cfg.lease.as_mut().unwrap().holder = "host-b".into();
    t.advance("61s");
    let before = t.gh.requests_with_query().len();
    t.start_service();
    assert!(
        t.status().unwrap().recovered.is_some(),
        "successor must recover the trusted journal"
    );
    t.advance("10m");
    let target = t.clock.now();
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("post-restart reconciliation completed", |r| {
        r.status()
            .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= target))
    });
    t.settle();
    let status = t.status().unwrap().state;
    assert_eq!(status["identity_bootstrap_hold"], true, "{status}");
    assert_eq!(
        status["queued"][pr.to_string()]["head_sha"],
        head,
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["reply"]["sender"], "alice",
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["op"]["op"], "update",
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["op"]["head_sha"], head,
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["reply"]["comment"], 77,
        "{status}"
    );
    assert_eq!(
        status["actions"]["pending"]["9"]["reply"]["command"], "update",
        "{status}"
    );
    let recovered = journal.recover().unwrap().unwrap();
    assert_eq!(recovered.in_doubt, vec![(1, decision)]);
    assert_eq!(recovered.state.queued[&pr].head_sha, head);
    assert!(t.gh.open_queue_drafts().is_empty());
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(
        !t.gh.requests_with_query().iter().skip(before).any(|r| {
            let write = r.starts_with("PUT ")
                || r.starts_with("PATCH ")
                || r.starts_with("DELETE ")
                || r.starts_with("POST ");
            let fence_commit = r.starts_with(&format!("POST /repos/{repo}/git/commits "));
            let fence_ref = r.starts_with(&format!("PATCH /repos/{repo}/git/refs/schlange/lease "));
            write && !fence_commit && !fence_ref && !r.contains("/access_tokens")
        }),
        "held successor must not send non-lease writes"
    );
    t.shutdown();
}

#[test]
fn fresh_fenced_adoption_can_queue_unambiguous_new_pr_without_manual_approval() {
    const QUEUE_ONLY: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\npull_request_rules:\n  - name: bots\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let t = Tester::new(QUEUE_ONLY)
        .persistent()
        .lease("host-a", 60)
        .reconcile_every("1h")
        .start();
    assert!(
        t.gh.prs().is_empty(),
        "fake repo starts without pull requests"
    );
    t.advance("2s");
    let pr = t.gh.open_pr(
        "bot",
        "new work",
        &[("new.txt".into(), "new\n".into())],
        false,
        &[],
    );
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.report_check_with_identity(pr, "quick", "success", "quick.yml", "pull_request");
    t.service.as_ref().unwrap().reconcile_now();
    t.wait_until("fresh PR and exact green check staged", |r| {
        r.status().is_some_and(|s| {
            s.state["pull_requests"][pr.to_string()]["checks"]
                .as_array()
                .is_some_and(|v| {
                    v.iter().any(|c| {
                        c["name"] == "quick"
                            && c["head_sha"] == head
                            && c["conclusion"] == "success"
                    })
                })
        })
    });
    t.settle();
    let state = t.status().unwrap().state;
    assert!(
        state["queued"].get(pr.to_string()).is_some(),
        "a new, unambiguous PR should queue without operator approval: {state}"
    );
    t.shutdown();
}

#[test]
fn persistent_unprocessed_checkbox_survives_service_restart_under_hold() {
    use rusqlite::OptionalExtension;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .start();
    t.gh.set_collaborator("alice", "write");
    let pr = t.gh.open_pr(
        "arztagent-bot",
        "durable checkbox",
        &[("backend/a.py".into(), "a = 1\n".into())],
        false,
        &[],
    );
    let comment =
        t.gh.comment(pr, "schlange[bot]", "<!-- schlange:status -->\n- [ ] queue");
    t.gh.edit_comment(comment, "alice", "<!-- schlange:status -->\n- [x] queue")
        .unwrap();
    let dir = t.serve_cfg.data_dir.clone().unwrap();
    let repo = t.gh.config().full_name();
    let store = schlange_github::store::Store::open(&dir).unwrap();
    t.wait_until("edited checkbox durably received", |_| store.pending_inbox(&repo, "initial").unwrap().iter().any(|item| {
        schlange_github::webhook::parse(&item.event, &item.body).is_ok_and(|parsed| parsed.hints.iter().any(|h| matches!(h, schlange_github::webhook::Hint::Comment(c) if c.id == comment && c.edited && c.sender == "alice" && c.author == "schlange[bot]")))
    }));
    t.settle();
    let original_head = t.gh.pr(pr).unwrap().head_sha;
    let before_pending = store.pending_inbox(&repo, "initial").unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    let before_checkpoint: Option<String> = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let before_intents: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM intents WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    t.stop_service();
    let before_requests = t.gh.requests_with_query().len();
    t.start_service();
    t.settle();
    assert_eq!(t.status().unwrap().state["identity_bootstrap_hold"], true);
    assert!(store.pending_inbox(&repo, "initial").unwrap().iter().any(|item| {
        schlange_github::webhook::parse(&item.event, &item.body).is_ok_and(|parsed| parsed.hints.iter().any(|h| matches!(h, schlange_github::webhook::Hint::Comment(c) if c.id == comment && c.edited && c.sender == "alice")))
    }), "restart must retain editor-bearing payload until safe processing");
    assert_eq!(
        store.pending_inbox(&repo, "initial").unwrap(),
        before_pending,
        "raw event/version/ordering must survive restart"
    );
    let after_checkpoint: Option<String> = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .optional()
        .unwrap();
    let after_intents: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM intents WHERE repo = ?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(after_checkpoint, before_checkpoint);
    assert_eq!(after_intents, before_intents);
    assert_eq!(t.gh.pr(pr).unwrap().head_sha, original_head);
    assert!(
        !t.gh
            .requests_with_query()
            .iter()
            .skip(before_requests)
            .any(|r| r.starts_with("PUT ")
                || r.starts_with("PATCH ")
                || r.starts_with("DELETE ")
                || (r.starts_with("POST /repos/") && !r.contains("/access_tokens"))),
        "held startup must not send forge writes"
    );
    assert!(!t.gh.reactions().iter().any(|(id, _, _)| *id == comment));
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}
