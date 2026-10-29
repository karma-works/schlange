//! Check-identity reads fail closed and a partially built car is resolved after a restart.
use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");

fn start() -> Tester {
    Tester::new(ARZTAGENT)
        .persistent()
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .reconcile_every("1h")
}

fn green(t: &Running, n: u64) {
    t.pr(n).check("ci-ok").succeed();
    t.pr(n).check("Risk class and data protection").succeed();
    t.pr(n).check("KI-Review").succeed();
}

fn identity_held(t: &Running) -> bool {
    t.status().is_some_and(|s| {
        s.state["check_identity_unavailable"]
            .as_array()
            .is_some_and(|a| a.iter().any(|v| v.as_str() == Some("main")))
    })
}

#[test]
fn check_identity_read_failure_holds_the_queue_across_restarts_and_recovers() {
    let mut t = start().start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    green(&t, pr);
    t.pr(pr).wait_queued();
    t.settle();
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.stop_service();
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Status(403),
        100,
    ));
    t.start_service();
    t.wait_until("identity hold after restart", identity_held);
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(
        t.gh.open_queue_drafts().is_empty(),
        "nothing is built while held"
    );

    // A second restart while the read still fails keeps the durable hold and never acts.
    t.stop_service();
    t.start_service();
    t.wait_until("hold survives restart", identity_held);
    assert!(!t.gh.pr(pr).unwrap().merged);

    // The fault clears: the next reconciliation validates identity, releases the hold and the
    // already queued pull request proceeds.
    t.gh.clear_faults();
    let before = t.status().unwrap().reconciliations;
    t.advance("1h");
    t.wait_until("identity hold cleared", |r| {
        r.status()
            .is_some_and(|s| s.reconciliations > before && !identity_held(r))
    });
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn partial_build_intent_is_resolved_across_restart_and_the_queue_resumes() {
    let mut t = start().start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    green(&t, pr);
    t.pr(pr).wait_queued();
    t.settle();

    // The branch is created, but opening the draft fails. The worker keeps the BuildCar
    // pending; stopping it now leaves the branch and the journal intent behind.
    t.gh.add_fault(FaultRule::new(
        "POST",
        "/pulls",
        FaultKind::Status(500),
        100,
    ));
    t.advance("5m");
    t.wait_until("a partial queue branch", |r| {
        r.gh.branches()
            .iter()
            .any(|b| b.starts_with("mergify/merge-queue/"))
            && r.gh.open_queue_drafts().is_empty()
    });
    let partial: Vec<String> =
        t.gh.branches()
            .into_iter()
            .filter(|b| b.starts_with("mergify/merge-queue/"))
            .collect();
    assert_eq!(partial.len(), 1, "{partial:?}");
    t.stop_service();

    // On restart the in-doubt build is abandoned (its branch deleted) and the pull request is
    // rebuilt; with the fault gone the queue lands it.
    t.gh.clear_faults();
    t.start_service();
    t.advance("5m");
    t.wait_until("the partial branch was cleaned up", |r| {
        r.gh.branch_sha(&partial[0]).is_none()
    });
    t.queue_pr().check("ci-ok").succeed();
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn in_doubt_build_intent_survives_an_identity_hold_then_resolves_and_resumes() {
    use schlange_core::engine::Decision;
    use schlange_core::journal::Journal;
    use schlange_core::queue::CarStatus;
    use schlange_github::store::Store;

    let mut t = start().start();
    let pr = t
        .open_pr("arztagent-bot")
        .file("backend/a.py", "a = 1\n")
        .create();
    green(&t, pr);
    t.pr(pr).wait_queued();
    t.settle();

    // Crash during `BuildCar`: the branch is created, opening the draft fails. The last
    // checkpoint still carries the `Building` car, so a restart must expire it from the state.
    t.gh.add_fault(FaultRule::new(
        "POST",
        "/pulls",
        FaultKind::Status(500),
        100,
    ));
    t.advance("5m");
    t.wait_until("a partial queue branch", |r| {
        r.gh.branches()
            .iter()
            .any(|b| b.starts_with("mergify/merge-queue/"))
            && r.gh.open_queue_drafts().is_empty()
    });
    let branch =
        t.gh.branches()
            .into_iter()
            .find(|b| b.starts_with("mergify/merge-queue/"))
            .expect("partial branch");
    t.stop_service();
    t.gh.clear_faults();

    // The worker left the branch and the intent in doubt. Re-create the intent at a fresh
    // sequence number, as a crash between the checkpoint and the answer would.
    let repo = t.gh.config().full_name();
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let (car, base, base_sha, prs, own) = {
        let journal = store.journal(&repo);
        let recovered = journal.recover().unwrap().expect("journal");
        let (base, train) = recovered
            .state
            .trains
            .iter()
            .find(|(_, train)| {
                train
                    .cars
                    .iter()
                    .any(|c| matches!(c.status, CarStatus::Building { .. }))
            })
            .expect("a building car in the checkpoint");
        let car = train
            .cars
            .iter()
            .find(|c| matches!(c.status, CarStatus::Building { .. }))
            .expect("building car");
        (
            car.id,
            base.clone(),
            car.base_sha.clone(),
            car.prs.clone(),
            car.own.clone(),
        )
    };
    let seq = 1_000;
    {
        let mut journal = store.journal(&repo);
        journal
            .intent(
                seq,
                &Decision::BuildCar {
                    car,
                    queue: "default".into(),
                    base_branch: base,
                    branch: branch.clone(),
                    base_sha,
                    prs,
                    own,
                    stack_on: None,
                },
            )
            .unwrap();
    }

    // The identity read fails first: the unresolved build intent and its orphan branch must
    // stay untouched while the queue is held.
    let head = t.gh.pr(pr).unwrap().head_sha;
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Status(403),
        100,
    ));
    t.start_service();
    t.wait_until(
        "identity hold before resolving the build intent",
        identity_held,
    );
    assert!(t.gh.branch_sha(&branch).is_some());
    assert!(t.gh.open_queue_drafts().is_empty(), "no build while held");
    let held = store.journal(&repo).recover().unwrap().unwrap();
    assert!(
        held.in_doubt.iter().any(|(s, d)| {
            *s == seq && matches!(d, Decision::BuildCar { car: c, .. } if *c == car)
        }),
        "{:?}",
        held.in_doubt
    );

    // Recovery: the hold clears, Restarted expires the build, and the intent is resolved only
    // after the state (and close/sweep) owns the branch.
    t.gh.clear_faults();
    t.advance("1h");
    t.wait_until("hold cleared and the orphan branch swept", |r| {
        !identity_held(r) && r.gh.branch_sha(&branch).is_none()
    });
    let recovered = store.journal(&repo).recover().unwrap().unwrap();
    assert!(
        recovered.in_doubt.is_empty(),
        "the recovered BuildCar intent must be resolved: {:?}",
        recovered.in_doubt
    );

    t.advance("5m");
    t.settle();
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}
