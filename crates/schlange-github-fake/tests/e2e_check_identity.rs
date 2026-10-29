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
    // Establish the preexisting queue synthetically; bootstrap publication is tested separately.
    t.service
        .as_ref()
        .unwrap()
        .test_release_empty_identity_hold()
        .unwrap();
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
            .is_some_and(|a| a.iter().any(|n| n.as_str() == Some("main"))),
        "{state}"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.stop_service();
    t.start_service();
    let state = t.status().unwrap().state;
    assert!(
        state["check_identity_unavailable"]
            .as_array()
            .is_some_and(|a| a.iter().any(|n| n.as_str() == Some("main"))),
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
            .is_some_and(|a| a.iter().any(|n| n.as_str() == Some("main"))),
        "{state}"
    );
    t.advance("5m");
    t.pr(pr).wait_merged();
    t.shutdown();
}

#[test]
fn guarded_worker_retains_landing_intent_when_ingress_changes_before_send() {
    use schlange_core::journal::Journal;
    use schlange_github::{service::PauseHook, store::Store};
    let hook = PauseHook::new();
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .pause_before_land(hook.clone())
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .start();
    t.service
        .as_ref()
        .unwrap()
        .test_release_empty_identity_hold()
        .unwrap();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    t.pr(pr).wait_queued();
    t.settle();
    t.advance("5m");
    t.wait_until("worker paused before landing", |_| hook.reached());
    let service = t.service.as_ref().unwrap();
    let repo = t.gh.config().full_name();
    t.wait_until("prior webhook deliveries drained", |r| r.gh.hooks_idle());
    t.gh.set_webhook_target(None);
    let epoch = service.test_guard_next_landing(&repo).unwrap();
    let body = serde_json::json!({"repository":{"full_name":repo},"ref":"refs/heads/main","after":"abcd","deleted":false}).to_string();
    let response = ureq::post(&service.webhook_url())
        .header("x-github-event", "push")
        .header("x-github-delivery", "guard-test-push")
        .header(
            "x-hub-signature-256",
            &schlange_github::webhook::sign(&t.gh.config().webhook_secret, body.as_bytes()),
        )
        .send(body.as_bytes())
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    assert_eq!(service.test_ingress_epoch(&repo), epoch + 1);
    let before = t.gh.requests_with_query().len();
    hook.release();
    t.wait_until("worker stops after ingress change", |r| {
        !r.service.as_ref().unwrap().dead_workers().is_empty()
    });
    assert_eq!(
        service.dead_workers()[&repo],
        "identity fence changed before decision execution"
    );
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(
        !t.gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("PUT ") && r.contains("/merge"))
    );
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let recovery = store.journal(&repo).recover().unwrap().unwrap();
    assert!(recovery.in_doubt.iter().any(
        |(_, d)| matches!(d, schlange_core::Decision::Land { prs, .. }
        if prs.iter().any(|p| p.number == pr && p.head_sha == t.gh.pr(pr).unwrap().head_sha))
    ));
    let before_state = serde_json::to_vec(&recovery.state).unwrap();
    t.stop_service();
    t.service = Some(
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap(),
    );
    t.wait_until("restart refuses unresolved journal", |r| {
        !r.service.as_ref().unwrap().dead_workers().is_empty()
    });
    let restarted = store.journal(&repo).recover().unwrap().unwrap();
    assert_eq!(serde_json::to_vec(&restarted.state).unwrap(), before_state);
    assert_eq!(restarted.in_doubt, recovery.in_doubt);
    t.shutdown();
}

#[test]
fn restart_with_in_doubt_intent_does_not_overwrite_checkpoint() {
    use schlange_core::journal::Journal;
    use schlange_github::store::Store;
    let mut t = Tester::new(ARZTAGENT).persistent().start();
    t.stop_service();
    let repo = t.gh.config().full_name();
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let mut journal = store.journal(&repo);
    let mut state = schlange_core::State::default();
    state.pull_requests.insert(
        1,
        schlange_core::model::PullRequest::new(1, "alice", "main", "p", "head"),
    );
    state.holds.insert(
        1,
        vec![schlange_core::protections::Hold::wait(
            "manual",
            "manual hold",
        )],
    );
    journal.checkpoint(&state).unwrap();
    let decision = schlange_core::Decision::Refresh {
        token: 99,
        numbers: vec![1],
        branches: vec!["main".into()],
    };
    journal.intent(99, &decision).unwrap();
    let before = journal.recover().unwrap().unwrap();
    let requests = t.gh.requests_with_query().len();
    t.service = Some(
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap(),
    );
    t.wait_until("restart rejects in-doubt journal", |r| {
        !r.service.as_ref().unwrap().dead_workers().is_empty()
    });
    assert!(
        t.service.as_ref().unwrap().dead_workers()[&repo].contains("unresolved journal intents")
    );
    let after = journal.recover().unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&before.state).unwrap(),
        serde_json::to_vec(&after.state).unwrap()
    );
    assert_eq!(before.in_doubt, after.in_doubt);
    assert!(
        !t.gh.requests_with_query()[requests..]
            .iter()
            .any(|r| !r.starts_with("GET ") && !r.contains("/access_tokens"))
    );
    t.shutdown();
}

#[test]
fn restart_persists_identity_hold_alongside_existing_reason() {
    use schlange_core::journal::Journal;
    use schlange_github::store::Store;
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .reconcile_every("1h")
        .start();
    t.stop_service();
    let pr =
        t.gh.open_pr("alice", "held", &[("a".into(), "new".into())], false, &[]);
    let head = t.gh.pr(pr).unwrap().head_sha;
    let repo = t.gh.config().full_name();
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let mut journal = store.journal(&repo);
    let mut state = schlange_core::State::default();
    state.pull_requests.insert(
        pr,
        schlange_core::model::PullRequest::new(pr, "alice", "main", "pr-1", &head),
    );
    state.holds.insert(
        pr,
        vec![schlange_core::protections::Hold::wait(
            "manual",
            "manual hold",
        )],
    );
    journal.checkpoint(&state).unwrap();
    t.start_service();
    t.wait_until("persisted identity hold", |_| {
        journal
            .recover()
            .ok()
            .flatten()
            .is_some_and(|r| r.state.check_identity_unavailable.contains("main"))
    });
    let persisted = journal.recover().unwrap().unwrap().state;
    assert!(persisted.holds[&pr].iter().any(|h| h.kind == "manual"));
    assert!(
        persisted.holds[&pr]
            .iter()
            .any(|h| h.kind == "check-identity-unavailable")
    );
    t.shutdown();
}

#[test]
fn first_leased_start_can_hold_an_empty_journal() {
    use schlange_github::lease::LeaseConfig;
    let mut t = Tester::new(ARZTAGENT).persistent().start();
    t.stop_service();
    let fresh = t
        .serve_cfg
        .data_dir
        .as_ref()
        .unwrap()
        .with_extension("fresh-lease");
    t.serve_cfg.data_dir = Some(fresh);
    t.serve_cfg.lease = Some(LeaseConfig {
        holder: "first-owner".into(),
        ttl: 60,
    });
    t.service = Some(
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap(),
    );
    t.wait_until("lease acquired or worker stopped", |r| {
        let service = r.service.as_ref().unwrap();
        !service.dead_workers().is_empty() || r.status().is_some_and(|s| s.role == "leader")
    });
    assert!(t.service.as_ref().unwrap().dead_workers().is_empty());
    t.wait_until("identity hold persisted", |r| {
        r.status().is_some_and(|s| {
            s.state["check_identity_unavailable"]
                .as_array()
                .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("main")))
        })
    });
    t.shutdown();
}

#[test]
fn leader_stops_before_checkpoint_when_lineage_write_fails_on_renewal() {
    let t = Tester::new(ARZTAGENT)
        .persistent()
        .lease("leader", 60)
        .reconcile_every("1h")
        .start();
    let repo = t.gh.config().full_name();
    let db = rusqlite::Connection::open(t.serve_cfg.data_dir.as_ref().unwrap().join("schlange.db"))
        .unwrap();
    let checkpoint: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo=?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    db.execute_batch("CREATE TRIGGER fail_fence BEFORE INSERT ON meta BEGIN SELECT RAISE(FAIL, 'injected'); END;").unwrap();
    t.advance("31s");
    t.wait_until("leader stops on lost journal lineage", |r| {
        !r.service.as_ref().unwrap().dead_workers().is_empty()
    });
    assert!(t.service.as_ref().unwrap().dead_workers()[&repo].contains("journal lineage"));
    let after: String = db
        .query_row(
            "SELECT state FROM checkpoints WHERE repo=?1",
            [&repo],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(checkpoint, after);
    t.shutdown();
}

#[test]
fn partial_guarded_build_stays_in_doubt_across_restart_without_replay() {
    use schlange_core::journal::Journal;
    use schlange_github::{service::PauseHook, store::Store};
    let hook = PauseHook::new();
    let mut t = Tester::new(ARZTAGENT)
        .persistent()
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .start();
    t.service
        .as_ref()
        .unwrap()
        .test_release_empty_identity_hold()
        .unwrap();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    t.pr(pr).wait_queued();
    t.settle();
    let expected_head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.set_webhook_target(None);
    let service = t.service.as_ref().unwrap();
    let repo = t.gh.config().full_name();
    let epoch = service.test_guard_next_build(&repo, hook.clone()).unwrap();
    let before = t.gh.requests_with_query().len();
    t.advance("5m");
    t.wait_until("build paused before merge", |_| hook.reached());
    let body = serde_json::json!({"repository":{"full_name":repo},"ref":"refs/heads/main","after":"abcd","deleted":false}).to_string();
    let response = ureq::post(&service.webhook_url())
        .header("x-github-event", "push")
        .header("x-github-delivery", "guard-build-push")
        .header(
            "x-hub-signature-256",
            &schlange_github::webhook::sign(&t.gh.config().webhook_secret, body.as_bytes()),
        )
        .send(body.as_bytes())
        .unwrap();
    assert_eq!(response.status().as_u16(), 202);
    assert_eq!(service.test_ingress_epoch(&repo), epoch + 1);
    hook.release();
    t.wait_until("partial build stops", |r| {
        !r.service.as_ref().unwrap().dead_workers().is_empty()
    });
    assert_eq!(
        service.dead_workers()[&repo],
        "identity fence changed during decision execution"
    );
    let requests = &t.gh.requests_with_query()[before..];
    assert!(
        requests
            .iter()
            .any(|r| r.starts_with("POST ") && r.contains("/git/refs"))
    );
    assert!(
        !requests
            .iter()
            .any(|r| r.starts_with("POST ") && r.contains("/merges"))
    );
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let recovery = store.journal(&repo).recover().unwrap().unwrap();
    let branch = recovery
        .in_doubt
        .iter()
        .find_map(|(_, d)| match d {
            schlange_core::Decision::BuildCar {
                branch,
                prs,
                base_sha,
                ..
            } if prs
                .iter()
                .any(|p| p.number == pr && p.head_sha == expected_head) =>
            {
                Some((branch.clone(), base_sha.clone()))
            }
            _ => None,
        })
        .expect("BuildCar remains in doubt");
    assert_eq!(
        t.gh.branch_sha(&branch.0).as_deref(),
        Some(branch.1.as_str())
    );
    assert!(t.gh.open_queue_drafts().is_empty());
    t.stop_service();
    let after_stop = t.gh.requests_with_query().len();
    t.service = Some(
        schlange_github::service::Service::start(t.serve_cfg.clone(), t.clock.clone()).unwrap(),
    );
    t.wait_until("restart preserves partial build", |r| {
        !r.service.as_ref().unwrap().dead_workers().is_empty()
    });
    assert!(
        t.service.as_ref().unwrap().dead_workers()[&repo].contains("unresolved journal intents")
    );
    let restarted = store.journal(&repo).recover().unwrap().unwrap();
    assert_eq!(
        serde_json::to_vec(&recovery.state).unwrap(),
        serde_json::to_vec(&restarted.state).unwrap()
    );
    assert_eq!(recovery.in_doubt, restarted.in_doubt);
    assert!(
        !t.gh.requests_with_query()[after_stop..]
            .iter()
            .any(|r| !r.starts_with("GET ")
                && !r.starts_with("HEAD ")
                && !r.contains("/access_tokens"))
    );
    t.shutdown();
}
