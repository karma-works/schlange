//! The queue engine driven event by event, without a forge: landing protocol, tree check,
//! refusals and foreign pushes.

use std::collections::BTreeMap;

use schlange_config::rules::Rules;
use schlange_core::engine::{LandError, LandedCommit};
use schlange_core::model::{CheckRun, Conclusion, PullRequest};
use schlange_core::queue::branch_name;
use schlange_core::reasons as code;
use schlange_core::{Decision, Event, State, step};

const CONFIG: &str = "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n";

fn run(name: &str, sha: &str, ok: Option<Conclusion>, seq: u64) -> CheckRun {
    CheckRun {
        name: name.into(),
        app: None,
        head_sha: sha.into(),
        conclusion: ok,
        seq,
        ..Default::default()
    }
}

struct Driver {
    rules: Rules,
    state: State,
}

impl Driver {
    fn new() -> Self {
        Driver {
            rules: Rules::from_yaml(CONFIG).unwrap(),
            state: State::default(),
        }
    }

    fn feed(&mut self, e: Event) -> Vec<Decision> {
        let (s, d) = step(&self.state, &self.rules, &e, 0);
        self.state = s;
        d
    }

    fn snapshot(&mut self, prs: Vec<PullRequest>, branches: &[(&str, &str)]) -> Vec<Decision> {
        self.feed(Event::Snapshot {
            pull_requests: prs,
            branches: branches
                .iter()
                .map(|(b, s)| (b.to_string(), Some(s.to_string())))
                .collect::<BTreeMap<_, _>>(),
            token: None,
        })
    }

    /// Queues #1 at head h1 on base b0 and builds its car; returns the car id.
    fn queued_car(&mut self) -> u64 {
        let mut pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
        pr.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
        let d = self.snapshot(vec![pr], &[("main", "b0")]);
        let Some(Decision::BuildCar { car, branch, .. }) = d
            .iter()
            .find(|d| matches!(d, Decision::BuildCar { .. }))
            .cloned()
        else {
            panic!("no car: {d:?}")
        };
        self.feed(Event::CarBuilt {
            car,
            branch,
            head_sha: "c1".into(),
            tree: "t1".into(),
            draft: 1000,
        });
        car
    }

    fn draft(&self, checks: Vec<CheckRun>) -> PullRequest {
        let mut d = PullRequest::new(1000, "bot", "main", "mergify/merge-queue/x", "c1");
        d.draft = true;
        d.checks = checks;
        d
    }

    /// Makes the car's draft green and answers the refresh; returns the Land decision.
    fn green_and_verify(&mut self) -> Decision {
        let draft = self.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
        let d = self.snapshot(vec![draft], &[]);
        let Some(Decision::Refresh { token, .. }) = d
            .iter()
            .find(|d| matches!(d, Decision::Refresh { .. }))
            .cloned()
        else {
            panic!("no refresh: {d:?}")
        };
        let d = self.feed(Event::Snapshot {
            pull_requests: vec![],
            branches: BTreeMap::new(),
            token: Some(token),
        });
        d.into_iter()
            .find(|d| matches!(d, Decision::Land { .. }))
            .expect("land")
    }
}

#[test]
fn failed_identity_read_holds_the_car_across_ticks_and_restart() {
    let mut dr = Driver::new();
    dr.queued_car();
    let decisions = dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    assert!(dr.state.queued.contains_key(&1));
    assert!(dr.state.holds.get(&1).is_some_and(|holds| {
        holds
            .iter()
            .any(|hold| hold.kind == "check-identity-unavailable")
    }));
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
    dr.state = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    let decisions = dr.feed(Event::Restarted);
    assert!(!decisions.iter().any(|d| matches!(
        d,
        Decision::CloseCar { .. }
            | Decision::Land { .. }
            | Decision::Refresh { .. }
            | Decision::Act { .. }
    )));
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    let decisions = dr.snapshot(vec![draft], &[]);
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
    let decisions = dr.feed(Event::Tick);
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
}

#[test]
fn restart_does_not_announce_in_place_check_while_identity_is_unavailable() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.state.trains.get_mut("main").unwrap().cars[0].in_place = true;
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let decisions = dr.feed(Event::Restarted);
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::CheckInPlace { .. }))
    );
    assert!(dr.state.identity_restart_pending);
    let restored: State = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    assert!(restored.identity_restart_pending);
}

#[test]
fn held_base_rejects_unverified_stale_snapshot_after_new_head() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let stale = dr.state.pull_requests[&1].clone();
    dr.feed(Event::PullRequestSynchronized {
        number: 1,
        head_sha: "new-head".into(),
        files: vec![],
    });
    dr.snapshot(vec![stale], &[]);
    assert_eq!(dr.state.pull_requests[&1].head_sha, "new-head");
    assert!(dr.state.check_identity_unavailable.contains("main"));
}

#[test]
fn identity_release_requires_complete_matching_generation_and_atomic_requirements() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let generation = dr.state.check_identity_epochs["main"];
    let pr = dr.state.pull_requests[&1].clone();
    let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    draft.base_tip = Some("b0".into());
    let batch = |generation, pull_requests: Vec<PullRequest>| Event::CheckIdentityBatch {
        base: "main".into(),
        generation,
        pull_requests,
        base_tip: "b0".into(),
        checks: vec!["full".into()],
        bound_checks: vec![],
    };
    dr.feed(batch(generation, vec![pr.clone()]));
    assert!(dr.state.check_identity_unavailable.contains("main"));
    assert!(!dr.state.required_checks.contains_key("main"));
    dr.feed(batch(generation + 1, vec![pr.clone(), draft.clone()]));
    assert!(dr.state.check_identity_unavailable.contains("main"));
    let decisions = dr.feed(batch(generation, vec![pr, draft]));
    assert!(
        decisions.is_empty(),
        "publication must checkpoint before any decisions: {decisions:?}"
    );
    assert!(!dr.state.check_identity_unavailable.contains("main"));
    assert_eq!(dr.state.required_checks["main"], vec!["full"]);
}

#[test]
fn identity_batch_recomputes_required_actions_classification_from_app_binding() {
    use schlange_core::model::RequiredCheck;
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let generation = dr.state.check_identity_epochs["main"];
    let mut pr = dr.state.pull_requests[&1].clone();
    pr.required_check_names.insert("stale".into());
    let mut check = run("ci", "c1", Some(Conclusion::Success), 2);
    check.app = Some("github-actions".into());
    check.app_id = Some(42);
    let draft = dr.draft(vec![check]);
    dr.feed(Event::CheckIdentityBatch {
        base: "main".into(),
        generation,
        pull_requests: vec![pr, draft],
        base_tip: "b0".into(),
        checks: vec![],
        bound_checks: vec![RequiredCheck {
            context: "ci".into(),
            app_id: 42,
        }],
    });
    assert!(
        !dr.state.pull_requests[&1]
            .required_check_names
            .contains("stale")
    );
    assert!(
        dr.state.pull_requests[&1000]
            .required_check_names
            .contains("@github-actions/ci")
    );
}

#[test]
fn identity_batch_started_before_new_head_cannot_release_hold() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let generation = dr.state.check_identity_epochs["main"];
    let pr = dr.state.pull_requests[&1].clone();
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    dr.feed(Event::PullRequestSynchronized {
        number: 1,
        head_sha: "new-head".into(),
        files: vec![],
    });
    dr.feed(Event::CheckIdentityBatch {
        base: "main".into(),
        generation,
        pull_requests: vec![pr, draft],
        base_tip: "b0".into(),
        checks: vec![],
        bound_checks: vec![],
    });
    assert!(dr.state.check_identity_unavailable.contains("main"));
    assert_eq!(dr.state.pull_requests[&1].head_sha, "new-head");
}

#[test]
fn held_read_does_not_discard_expired_timer_obligations() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.state.wakeups.insert(0);
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    assert!(dr.state.wakeups.contains(&0));
    dr.feed(Event::Tick);
    assert!(dr.state.wakeups.contains(&0));
}

#[test]
fn held_identity_does_not_mark_cosmetics_as_emitted() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.state.cosmetics.status.clear();
    let decisions = dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::Cosmetic { .. }))
    );
    assert!(dr.state.cosmetics.status.is_empty());
}

#[test]
fn identity_hold_preserves_existing_hold_reasons() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.state.holds.insert(
        1,
        vec![schlange_core::protections::Hold::wait(
            "existing",
            "existing reason",
        )],
    );
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let holds = &dr.state.holds[&1];
    assert!(holds.iter().any(|h| h.kind == "existing"));
    assert!(holds.iter().any(|h| h.kind == "check-identity-unavailable"));
}

#[test]
fn held_comment_is_preserved_without_dequeuing_or_acknowledging() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let comment = schlange_core::model::Comment {
        id: 44,
        number: 1,
        author: "alice".into(),
        sender: "alice".into(),
        body: "@mergifyio dequeue".into(),
        edited: false,
        version: "v1".into(),
        sender_permission: Some("write".into()),
        by_queue: false,
        head_sha: Some("h1".into()),
    };
    let decisions = dr.feed(Event::Comments {
        comments: vec![comment.clone()],
    });
    assert!(decisions.is_empty());
    assert!(dr.state.queued.contains_key(&1));
    assert!(!dr.state.seen_comments.contains(&(44, "v1".into())));
    let restored: State = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    assert_eq!(restored.identity_deferred_comments, vec![comment]);
}

#[test]
fn identity_batch_cannot_replace_confirmed_merge_sha() {
    let mut dr = Driver::new();
    dr.queued_car();
    let pr = dr.state.pull_requests.get_mut(&1).unwrap();
    pr.merged = true;
    pr.merge_commit_sha = Some("confirmed".into());
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let generation = dr.state.check_identity_epochs["main"];
    let mut changed = dr.state.pull_requests[&1].clone();
    changed.merge_commit_sha = Some("different".into());
    let draft = dr.draft(vec![]);
    dr.feed(Event::CheckIdentityBatch {
        base: "main".into(),
        generation,
        pull_requests: vec![changed, draft],
        base_tip: "b0".into(),
        checks: vec![],
        bound_checks: vec![],
    });
    assert!(dr.state.check_identity_unavailable.contains("main"));
    assert_eq!(
        dr.state.pull_requests[&1].merge_commit_sha.as_deref(),
        Some("confirmed")
    );
}

#[test]
fn landing_reply_received_while_held_does_not_emit_actions() {
    let mut dr = Driver::new();
    let car = dr.queued_car();
    dr.green_and_verify();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let decisions = dr.feed(Event::Landed {
        car,
        commits: vec![LandedCommit {
            number: 1,
            sha: "m1".into(),
            tree: "t1".into(),
            extra: vec![],
        }],
        error: None,
    });
    assert!(!decisions.iter().any(|d| matches!(
        d,
        Decision::Merged { .. }
            | Decision::CloseCar { .. }
            | Decision::BuildCar { .. }
            | Decision::Act { .. }
    )));
    assert!(
        dr.state.queued.contains_key(&1),
        "the reply needs safe durable recovery"
    );
    let restored: State = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    assert_eq!(restored.identity_deferred_landed.len(), 1);
    assert_eq!(restored.identity_deferred_landed[0].commits[0].sha, "m1");
}

#[test]
fn build_reply_received_while_held_is_retained_without_advancing_car() {
    let mut dr = Driver::new();
    let mut pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
    pr.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
    let decisions = dr.snapshot(vec![pr], &[("main", "b0")]);
    let (car, branch) = decisions
        .iter()
        .find_map(|d| match d {
            Decision::BuildCar { car, branch, .. } => Some((*car, branch.clone())),
            _ => None,
        })
        .unwrap();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let before = serde_json::to_value(&dr.state.trains["main"].cars[0]).unwrap();
    let decisions = dr.feed(Event::CarBuilt {
        car,
        branch,
        head_sha: "c1".into(),
        tree: "t1".into(),
        draft: 1000,
    });
    assert_eq!(
        serde_json::to_value(&dr.state.trains["main"].cars[0]).unwrap(),
        before
    );
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::Refresh { .. } | Decision::Land { .. }))
    );
    let restored: State = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    assert_eq!(restored.identity_deferred_built.len(), 1);
}

#[test]
fn unknown_car_landing_reply_is_not_discarded_during_hold() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    dr.feed(Event::Landed {
        car: 999,
        commits: vec![],
        error: None,
    });
    assert!(
        dr.state
            .identity_deferred_landed
            .iter()
            .any(|reply| reply.car == 999)
    );
}

#[test]
fn failed_build_reply_is_preserved_while_base_is_held() {
    let mut dr = Driver::new();
    let mut pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
    pr.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
    let decisions = dr.snapshot(vec![pr], &[("main", "b0")]);
    let build = decisions
        .into_iter()
        .find(|d| matches!(d, Decision::BuildCar { .. }))
        .unwrap();
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    let before = serde_json::to_value(&dr.state.trains["main"].cars[0]).unwrap();
    let decisions = dr.feed(Event::Failed {
        decision: build.clone(),
        error: "uncertain".into(),
    });
    assert_eq!(
        serde_json::to_value(&dr.state.trains["main"].cars[0]).unwrap(),
        before
    );
    assert!(decisions.is_empty());
    let restored: State = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    assert_eq!(restored.identity_deferred_failed[0].decision, build);
}

#[test]
fn clean_waiting_queue_recovers_after_restart_and_keeps_batch_timer() {
    use schlange_config::rules::BatchSize;
    let mut dr = Driver::new();
    dr.rules.queue_rules[0].batch_size = BatchSize::Range { min: 5, max: 5 };
    dr.rules.queue_rules[0].batch_max_wait_time = 300;
    let mut pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
    pr.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
    dr.snapshot(vec![pr.clone()], &[("main", "b0")]);
    assert!(
        dr.state
            .trains
            .get("main")
            .is_none_or(|t| t.cars.is_empty())
    );
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    dr.feed(Event::Restarted);
    let generation = dr.state.check_identity_epochs["main"];
    let decisions = dr.feed(Event::CheckIdentityBatch {
        base: "main".into(),
        generation,
        pull_requests: vec![pr],
        base_tip: "b0".into(),
        checks: vec![],
        bound_checks: vec![],
    });
    assert!(!dr.state.identity_restart_pending);
    assert!(!dr.state.check_identity_unavailable.contains("main"));
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::BuildCar { .. }))
    );
    let (state, decisions) = step(&dr.state, &dr.rules, &Event::Tick, 300);
    assert!(!state.check_identity_unavailable.contains("main"));
    assert!(
        decisions
            .iter()
            .any(|d| matches!(d, Decision::BuildCar { .. }))
    );
}

#[test]
fn waiting_restart_with_historical_action_state_stays_held() {
    let mut dr = Driver::new();
    dr.state.actions.fired.insert("legacy/action".into());
    dr.feed(Event::CheckIdentityUnavailable {
        bases: vec!["main".into()],
    });
    dr.feed(Event::Restarted);
    let generation = dr.state.check_identity_epochs["main"];
    dr.feed(Event::CheckIdentityBatch {
        base: "main".into(),
        generation,
        pull_requests: vec![],
        base_tip: "b0".into(),
        checks: vec![],
        bound_checks: vec![],
    });
    assert!(dr.state.check_identity_unavailable.contains("main"));
    assert!(dr.state.identity_restart_pending);
}

#[test]
fn landed_tree_differs_from_tested_tree_halts_the_queue() {
    let mut dr = Driver::new();
    let car = dr.queued_car();
    let land = dr.green_and_verify();
    assert!(
        matches!(&land, Decision::Land { expected_base_sha, expected_tree, .. }
            if expected_base_sha == "b0" && expected_tree == "t1")
    );
    let d = dr.feed(Event::Landed {
        car,
        commits: vec![LandedCommit {
            number: 1,
            sha: "m1".into(),
            tree: "OTHER".into(),
            extra: vec![],
        }],
        error: None,
    });
    assert!(
        d.iter().any(|d| matches!(d, Decision::Halt { .. })),
        "{d:?}"
    );
    assert!(dr.state.trains["main"].halted.is_some());
    // A halted queue builds nothing more.
    let mut pr2 = PullRequest::new(2, "a", "main", "pr-2", "h2");
    pr2.checks = vec![run("quick", "h2", Some(Conclusion::Success), 3)];
    let d = dr.snapshot(vec![pr2], &[("main", "m1")]);
    assert!(!d.iter().any(|d| matches!(d, Decision::BuildCar { .. })));
}

#[test]
fn landing_refused_because_the_base_moved_resets_the_queue() {
    let mut dr = Driver::new();
    let car = dr.queued_car();
    dr.green_and_verify();
    let d = dr.feed(Event::Landed {
        car,
        commits: vec![],
        error: Some(LandError::BaseMoved {
            actual: "b9".into(),
        }),
    });
    assert!(
        d.iter()
            .any(|d| matches!(d, Decision::BuildCar { base_sha, .. } if base_sha == "b9")),
        "{d:?}"
    );
    assert!(dr.state.queued.contains_key(&1), "stays queued");
}

#[test]
fn external_reset_context_survives_restart_and_stale_car_replies() {
    let mut dr = Driver::new();
    let old_car = dr.queued_car();
    dr.snapshot(vec![], &[("main", "b9")]);
    assert!(dr.state.queued[&1].external_reset);
    // A checkpoint/restart and replies from the discarded car cannot clear the new context.
    dr.state = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    dr.feed(Event::Restarted);
    dr.feed(Event::CarBuilt {
        car: old_car,
        branch: "old".into(),
        head_sha: "old".into(),
        tree: "old".into(),
        draft: 999,
    });
    dr.feed(Event::CarConflict {
        car: old_car,
        number: 1,
        with_base: true,
        with: vec![],
    });
    assert!(
        dr.state.queued[&1].external_reset,
        "stale replies must not clear reset context"
    );
    let current = dr.state.trains["main"]
        .cars
        .iter()
        .find(|c| matches!(c.status, schlange_core::queue::CarStatus::Building { .. }))
        .expect("replacement car")
        .id;
    dr.feed(Event::CarConflict {
        car: current,
        number: 1,
        with_base: true,
        with: vec![],
    });
    let schlange_core::engine::Outcome::Dequeued { reason } = &dr.state.outcomes[&1] else {
        panic!("expected dequeue")
    };
    assert_eq!(reason.code, Some(code::PR_DEQUEUED));
    assert!(reason.text.contains("does not merge onto the base branch"));
    assert_eq!(
        dr.state.blocked_heads.get(&1).map(String::as_str),
        Some("h1")
    );
}

#[test]
fn landing_refused_because_the_head_moved_dequeues() {
    let mut dr = Driver::new();
    let car = dr.queued_car();
    dr.green_and_verify();
    let d = dr.feed(Event::Landed {
        car,
        commits: vec![],
        error: Some(LandError::HeadChanged { number: 1 }),
    });
    assert!(
        d.iter()
            .any(|d| matches!(d, Decision::Dequeue { reason, .. }
            if reason.code == Some(code::PULL_REQUEST_UPDATED)))
    );
}

#[test]
fn a_stale_green_is_rechecked_before_landing() {
    // The fresh read shows the merge check red again (a rerun): no landing (I1).
    let mut dr = Driver::new();
    dr.queued_car();
    let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    let d = dr.snapshot(vec![draft.clone()], &[]);
    let Some(Decision::Refresh { token, .. }) = d
        .iter()
        .find(|d| matches!(d, Decision::Refresh { .. }))
        .cloned()
    else {
        panic!("no refresh")
    };
    draft
        .checks
        .push(run("full", "c1", Some(Conclusion::Failure), 3));
    let d = dr.feed(Event::Snapshot {
        pull_requests: vec![draft],
        branches: BTreeMap::new(),
        token: Some(token),
    });
    assert!(
        !d.iter().any(|d| matches!(d, Decision::Land { .. })),
        "{d:?}"
    );
}

#[test]
fn a_label_seen_only_in_the_fresh_read_prevents_landing() {
    // The webhook for the label was lost; the read before landing sees it (MQ-11, I1).
    let mut dr = Driver::new();
    let cfg = "queue_rules:\n  - name: default\n    queue_conditions:\n      - -label = halt\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n";
    dr.rules = Rules::from_yaml(cfg).unwrap();
    dr.queued_car();
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    let d = dr.snapshot(vec![draft], &[]);
    let Some(Decision::Refresh { token, .. }) = d
        .iter()
        .find(|d| matches!(d, Decision::Refresh { .. }))
        .cloned()
    else {
        panic!("no refresh")
    };
    let mut pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
    pr.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
    pr.labels.insert("halt".into());
    let d = dr.feed(Event::Snapshot {
        pull_requests: vec![pr],
        branches: BTreeMap::new(),
        token: Some(token),
    });
    assert!(!d.iter().any(|d| matches!(d, Decision::Land { .. })));
    assert!(
        d.iter()
            .any(|d| matches!(d, Decision::Dequeue { number: 1, .. }))
    );
}

#[test]
fn a_foreign_push_to_the_queue_branch_dequeues() {
    let mut dr = Driver::new();
    dr.queued_car();
    let branch = dr.state.trains["main"].cars[0].branch.clone().unwrap();
    let d = dr.snapshot(vec![], &[(&branch, "someone-else")]);
    assert!(
        d.iter()
            .any(|d| matches!(d, Decision::Dequeue { reason, .. }
            if reason.code == Some(code::DRAFT_PULL_REQUEST_CHANGED)))
    );
    assert!(d.iter().any(|d| matches!(d, Decision::CloseCar { .. })));
}

#[test]
fn the_same_snapshot_twice_changes_nothing() {
    // Idempotence (MQ-12, MQ-29): a duplicated delivery is harmless.
    let mut dr = Driver::new();
    dr.queued_car();
    let draft = dr.draft(vec![run("full", "c1", None, 2)]);
    dr.snapshot(vec![draft.clone()], &[]);
    let before = format!("{:?}", dr.state.trains);
    let d = dr.snapshot(vec![draft], &[]);
    assert!(d.is_empty(), "{d:?}");
    assert_eq!(before, format!("{:?}", dr.state.trains));
}

#[test]
fn a_retargeted_pull_request_leaves_its_queue() {
    // MQ-10: the base branch of a queued pull request changes.
    let mut dr = Driver::new();
    dr.queued_car();
    let mut pr = PullRequest::new(1, "a", "release", "pr-1", "h1");
    pr.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
    let d = dr.snapshot(vec![pr], &[]);
    assert!(
        d.iter()
            .any(|d| matches!(d, Decision::Dequeue { reason, .. }
            if reason.code == Some(code::BASE_BRANCH_CHANGED)))
    );
    assert!(d.iter().any(|d| matches!(d, Decision::CloseCar { .. })));
}

#[test]
fn branch_names_follow_the_prefix_and_ten_hex() {
    let b = branch_name("mergify/merge-queue/", "main", 7);
    let rest = b.strip_prefix("mergify/merge-queue/").unwrap();
    assert_eq!(rest.len(), 10);
    assert!(
        rest.chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
    );
    assert_eq!(b, branch_name("mergify/merge-queue/", "main", 7));
}

/// Every check passed and the check conditions are still false (a count): the reason names
/// what was seen, never an empty "failed:" list.
#[test]
fn a_false_check_condition_without_a_failed_check_names_what_it_saw() {
    let mut dr = Driver::new();
    dr.rules = Rules::from_yaml(
        "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - \"#check-success >= 2\"\n\
         pull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n",
    )
    .unwrap();
    dr.queued_car();
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    let d = dr.snapshot(vec![draft], &[]);
    let Some(Decision::Refresh { token, .. }) = d
        .iter()
        .find(|d| matches!(d, Decision::Refresh { .. }))
        .cloned()
    else {
        panic!("no confirming read: {d:?}")
    };
    let d = dr.feed(Event::Snapshot {
        pull_requests: vec![],
        branches: BTreeMap::new(),
        token: Some(token),
    });
    let text = d
        .iter()
        .find_map(|d| match d {
            Decision::Dequeue { reason, .. } => Some(reason.text.clone()),
            _ => None,
        })
        .expect("dequeued");
    assert!(!text.trim_end().ends_with("failed:"), "{text}");
    assert!(text.contains("full=success"), "{text}");
}

#[test]
fn bound_requirements_change_through_a_typed_forge_event() {
    use schlange_core::model::RequiredCheck;
    let mut dr = Driver::new();
    let requirement = RequiredCheck {
        context: "ci".into(),
        app_id: 42,
    };
    dr.feed(Event::RequiredChecksTyped {
        base: "main".into(),
        checks: vec![],
        bound_checks: vec![requirement.clone()],
    });
    assert_eq!(dr.state.bound_required_checks["main"], vec![requirement]);
    dr.feed(Event::RequiredChecksTyped {
        base: "main".into(),
        checks: vec![],
        bound_checks: vec![],
    });
    assert!(!dr.state.bound_required_checks.contains_key("main"));
}
