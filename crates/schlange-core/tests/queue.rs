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
fn incomplete_check_identity_holds_a_queued_car_until_fresh_snapshot() {
    let mut dr = Driver::new();
    dr.queued_car();
    let d = dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1, 1000],
    });
    assert!(
        dr.state.queued.contains_key(&1),
        "temporary read failure must not dequeue"
    );
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
    let mut green = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    green.base_tip = Some("b0".into());
    let d = dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "c1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_request: Box::new(green),
    });
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
    assert!(
        dr.state
            .holds
            .get(&1)
            .is_some_and(|h| h.iter().any(|h| h.kind == "check-identity-unavailable"))
    );
    let persisted = serde_json::to_vec(&dr.state).unwrap();
    dr.state = serde_json::from_slice(&persisted).unwrap();
    let d = dr.feed(Event::Tick);
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
    let mut fresh = dr.state.pull_requests[&1].clone();
    fresh.base_tip = Some("b0".into());
    let d = dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "h1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_request: Box::new(fresh),
    });
    assert!(d.iter().any(|d| matches!(d, Decision::Refresh { .. })));
}

#[test]
fn stale_old_head_snapshot_cannot_clear_identity_hold() {
    let mut dr = Driver::new();
    let mut newer = PullRequest::new(1, "a", "main", "pr-1", "new-head");
    newer.checks = vec![run("quick", "new-head", Some(Conclusion::Success), 1)];
    dr.snapshot(vec![newer], &[("main", "base")]);
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1],
    });
    let mut stale = PullRequest::new(1, "a", "main", "pr-1", "old-head");
    stale.checks = vec![run("quick", "old-head", Some(Conclusion::Success), 2)];
    let d = dr.snapshot(vec![stale], &[]);
    assert!(dr.state.check_identity_unavailable.contains(&1));
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
}

#[test]
fn old_generation_cannot_clear_newer_identity_failure() {
    let mut dr = Driver::new();
    let mut p = PullRequest::new(1, "a", "main", "pr-1", "h1");
    p.base_tip = Some("b0".into());
    p.checks = vec![run("quick", "h1", Some(Conclusion::Success), 1)];
    dr.snapshot(vec![p.clone()], &[("main", "b0")]);
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 7,
        numbers: vec![1],
    });
    let d = dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 6,
        expected_head: "h1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_request: Box::new(p),
    });
    assert!(dr.state.check_identity_unavailable.contains(&1));
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
}

#[test]
fn complete_identity_batch_replaces_and_clears_previous_bound_checks() {
    use schlange_core::model::RequiredCheck;
    for next in [
        vec![],
        vec![RequiredCheck {
            context: "ci".into(),
            app_id: 43,
        }],
    ] {
        let mut dr = Driver::new();
        dr.queued_car();
        dr.state.bound_required_checks.insert(
            "main".into(),
            vec![RequiredCheck {
                context: "ci".into(),
                app_id: 42,
            }],
        );
        dr.feed(Event::CheckIdentityUnavailable {
            base: "main".into(),
            generation: 4,
            numbers: vec![1, 1000],
        });
        let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
        draft.base_tip = Some("b0".into());
        dr.feed(Event::CheckIdentityVerified {
            base: "main".into(),
            epoch: 1,
            generation: 5,
            expected_head: "c1".into(),
            expected_base: "b0".into(),
            required_checks: vec![],
            bound_required_checks: next.clone(),
            pull_request: Box::new(draft),
        });
        assert_eq!(dr.state.bound_required_checks["main"][0].app_id, 42);
        let mut original = dr.state.pull_requests[&1].clone();
        original.base_tip = Some("b0".into());
        dr.feed(Event::CheckIdentityVerified {
            base: "main".into(),
            epoch: 1,
            generation: 5,
            expected_head: "h1".into(),
            expected_base: "b0".into(),
            required_checks: vec![],
            bound_required_checks: next.clone(),
            pull_request: Box::new(original),
        });
        assert_eq!(dr.state.bound_required_checks["main"], next);
    }
}

#[test]
fn differing_bound_checks_cannot_release_identity_batch() {
    use schlange_core::model::RequiredCheck;
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1, 1000],
    });
    let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    draft.base_tip = Some("b0".into());
    dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "c1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![RequiredCheck {
            context: "ci".into(),
            app_id: 42,
        }],
        pull_request: Box::new(draft),
    });
    let mut original = dr.state.pull_requests[&1].clone();
    original.base_tip = Some("b0".into());
    dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "h1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![RequiredCheck {
            context: "ci".into(),
            app_id: 43,
        }],
        pull_request: Box::new(original),
    });
    assert!(dr.state.check_identity_targets.contains_key("main"));
}

#[test]
fn differing_required_checks_cannot_release_identity_batch() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1, 1000],
    });
    let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    draft.base_tip = Some("b0".into());
    dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "c1".into(),
        expected_base: "b0".into(),
        required_checks: vec!["ci".into()],
        bound_required_checks: vec![],
        pull_request: Box::new(draft),
    });
    let mut original = dr.state.pull_requests[&1].clone();
    original.base_tip = Some("b0".into());
    let d = dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "h1".into(),
        expected_base: "b0".into(),
        required_checks: vec!["other".into()],
        bound_required_checks: vec![],
        pull_request: Box::new(original),
    });
    assert!(dr.state.check_identity_targets.contains_key("main"));
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
}

#[test]
fn restart_invalidates_partially_staged_identity_reads() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1, 1000],
    });
    let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    draft.base_tip = Some("b0".into());
    dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "c1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_request: Box::new(draft),
    });
    dr.state = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    dr.feed(Event::Restarted);
    let mut original = dr.state.pull_requests[&1].clone();
    original.base_tip = Some("b0".into());
    let d = dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch: 1,
        generation: 5,
        expected_head: "h1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_request: Box::new(original),
    });
    assert!(dr.state.check_identity_targets.contains_key("main"));
    assert!(
        !d.iter()
            .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
    );
}

#[test]
fn staged_identity_batch_is_invalidated_by_pr_and_base_events() {
    for event in [
        Event::PullRequestSynchronized {
            number: 1,
            head_sha: "h2".into(),
            files: vec![],
        },
        Event::CheckRun {
            number: 1,
            run: run("quick", "h1", Some(Conclusion::Failure), 9),
        },
        Event::Closed { number: 1 },
        Event::Labeled {
            number: 1,
            label: "halt".into(),
        },
        Event::RequiredChecks {
            base: "main".into(),
            checks: vec!["new".into()],
        },
        Event::Snapshot {
            pull_requests: vec![],
            branches: [("main".into(), Some("b1".into()))].into(),
            token: None,
        },
    ] {
        let mut dr = Driver::new();
        dr.queued_car();
        dr.feed(Event::CheckIdentityUnavailable {
            base: "main".into(),
            generation: 4,
            numbers: vec![1, 1000],
        });
        let mut draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
        draft.base_tip = Some("b0".into());
        dr.feed(Event::CheckIdentityVerified {
            base: "main".into(),
            epoch: 1,
            generation: 5,
            expected_head: "c1".into(),
            expected_base: "b0".into(),
            required_checks: vec![],
            bound_required_checks: vec![],
            pull_request: Box::new(draft),
        });
        assert!(dr.state.check_identity_staged.contains_key("main"));
        dr.feed(event);
        assert!(!dr.state.check_identity_staged.contains_key("main"));
        let mut original = dr.state.pull_requests[&1].clone();
        original.base_tip = Some("b0".into());
        let d = dr.feed(Event::CheckIdentityVerified {
            base: "main".into(),
            epoch: 1,
            generation: 5,
            expected_head: original.head_sha.clone(),
            expected_base: "b0".into(),
            required_checks: vec![],
            bound_required_checks: vec![],
            pull_request: Box::new(original),
        });
        assert!(dr.state.check_identity_targets.contains_key("main"));
        assert!(
            !d.iter()
                .any(|d| matches!(d, Decision::Land { .. } | Decision::Refresh { .. }))
        );
    }
}

#[test]
fn identity_hold_defers_cleanup_and_retries_until_recovered() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.state
        .branches
        .insert("mergify/merge-queue/0123456789".into(), "orphan".into());
    dr.state.closing.insert(
        99,
        schlange_core::engine::Closing {
            base: Some("main".into()),
            branch: Some("mergify/merge-queue/9876543210".into()),
            draft: None,
            since: 0,
        },
    );
    let (state, decisions) = step(
        &dr.state,
        &dr.rules,
        &Event::CheckIdentityUnavailable {
            base: "main".into(),
            generation: 4,
            numbers: vec![1],
        },
        10000,
    );
    dr.state = state;
    assert!(
        !decisions
            .iter()
            .any(|d| matches!(d, Decision::CloseCar { .. }))
    );
    let mut fresh = dr.state.pull_requests[&1].clone();
    fresh.base_tip = Some("b0".into());
    let (state, decisions) = step(
        &dr.state,
        &dr.rules,
        &Event::CheckIdentityVerified {
            base: "main".into(),
            epoch: 1,
            generation: 5,
            expected_head: "h1".into(),
            expected_base: "b0".into(),
            required_checks: vec![],
            bound_required_checks: vec![],
            pull_request: Box::new(fresh),
        },
        10001,
    );
    dr.state = state;
    assert!(
        decisions
            .iter()
            .any(|d| matches!(d, Decision::CloseCar { .. }))
    );
}

#[test]
fn restart_does_not_announce_in_place_check_while_identity_is_unavailable() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.state.trains.get_mut("main").unwrap().cars[0].in_place = true;
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1],
    });
    let d = dr.feed(Event::Restarted);
    assert!(!d.iter().any(|d| matches!(d, Decision::CheckInPlace { .. })));
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

#[test]
fn identity_holds_all_bases_before_any_train_can_advance() {
    let mut dr = Driver::new();
    dr.queued_car();
    let mut other = PullRequest::new(2, "a", "aaa", "pr-2", "h2");
    other.checks = vec![run("quick", "h2", Some(Conclusion::Success), 1)];
    let built = dr.snapshot(vec![other], &[("aaa", "b2")]);
    let Some(Decision::BuildCar { car, branch, .. }) = built
        .into_iter()
        .find(|d| matches!(d, Decision::BuildCar { .. }))
    else {
        panic!("second train was not built")
    };
    dr.feed(Event::CarBuilt {
        car,
        branch,
        head_sha: "c2".into(),
        tree: "t2".into(),
        draft: 2000,
    });
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    dr.state.pull_requests.insert(1000, draft);
    let mut draft2 = PullRequest::new(2000, "bot", "aaa", "mergify/merge-queue/y", "c2");
    draft2.draft = true;
    dr.state.pull_requests.insert(2000, draft2);
    let bases = BTreeMap::from([
        ("main".into(), (4, vec![1, 1000])),
        ("aaa".into(), (4, vec![2, 2000])),
    ]);
    let d = dr.feed(Event::CheckIdentitiesUnavailable { bases });
    assert!(dr.state.check_identity_targets.contains_key("main"));
    assert!(dr.state.check_identity_targets.contains_key("aaa"));
    assert!(
        !d.iter().any(|d| matches!(
            d,
            Decision::Refresh { .. } | Decision::Land { .. } | Decision::BuildCar { .. }
        )),
        "{d:?}"
    );
}

#[test]
fn bootstrap_hold_blocks_restart_and_tick_until_all_identity_targets_are_verified() {
    let mut dr = Driver::new();
    dr.queued_car();
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    dr.state.pull_requests.insert(1000, draft);
    let d = dr.feed(Event::IdentityBootstrapHeld);
    assert!(d.is_empty(), "{d:?}");
    let state = serde_json::to_vec(&dr.state).unwrap();
    dr.state = serde_json::from_slice(&state).unwrap();
    for event in [Event::Restarted, Event::Tick] {
        let d = dr.feed(event);
        assert!(d.is_empty(), "{d:?}");
    }
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1, 1000],
    });
    let d = dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 9 });
    assert!(
        dr.state.identity_bootstrap_hold,
        "incomplete identity reads cannot release bootstrap"
    );
    assert!(d.is_empty(), "{d:?}");
}

#[test]
fn bootstrap_release_must_not_run_decisions_before_deferred_events() {
    let mut dr = Driver::new();
    dr.queued_car();
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    dr.state.pull_requests.insert(1000, draft);
    dr.feed(Event::IdentityBootstrapHeld);
    dr.state.trains.clear();
    // Isolate release sequencing after the fixture's old timer was resolved.
    dr.state.wakeups.clear();
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 9 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 9 });
    let d = dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 9 });
    assert!(!dr.state.identity_bootstrap_hold);
    assert!(
        d.is_empty(),
        "release must not run a train before deferred pause/restart events: {d:?}"
    );
}

#[test]
fn verified_empty_base_can_clear_only_its_empty_identity_hold() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![],
    });
    let epoch = dr.state.check_identity_epochs["main"];
    dr.feed(Event::CheckIdentityEmptyVerified {
        base: "main".into(),
        epoch,
        generation: 5,
        base_tip: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
    });
    assert!(!dr.state.check_identity_targets.contains_key("main"));
    assert_eq!(dr.state.branches["main"], "b0");
    assert!(dr.state.identity_bootstrap_hold);
}

#[test]
fn verified_identity_cannot_reverse_an_already_confirmed_merge() {
    let mut dr = Driver::new();
    let mut merged = PullRequest::new(1, "a", "main", "pr-1", "h1");
    merged.closed = true;
    merged.merged = true;
    merged.merge_commit_sha = Some("merge-sha".into());
    dr.state.pull_requests.insert(1, merged.clone());
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1],
    });
    let epoch = dr.state.check_identity_epochs["main"];
    let mut stale = merged;
    stale.merged = false;
    stale.merge_commit_sha = None;
    stale.base_tip = Some("b0".into());
    dr.feed(Event::CheckIdentityVerified {
        base: "main".into(),
        epoch,
        generation: 5,
        expected_head: "h1".into(),
        expected_base: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_request: Box::new(stale),
    });
    assert!(dr.state.pull_requests[&1].merged);
    assert_eq!(
        dr.state.pull_requests[&1].merge_commit_sha.as_deref(),
        Some("merge-sha")
    );
    assert!(dr.state.check_identity_targets.contains_key("main"));
}

#[test]
fn bootstrap_cannot_release_just_because_no_targets_were_discovered() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 9 });
    assert!(
        dr.state.identity_bootstrap_hold,
        "an empty target map is not proof of complete discovery"
    );
}

#[test]
fn discovery_attestation_requires_the_same_epoch_and_is_reset_by_rebootstrap() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 8 });
    assert!(dr.state.identity_bootstrap_hold);
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 7 });
    assert!(
        dr.state.identity_bootstrap_hold,
        "restart must invalidate an old attestation"
    );
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 8 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 8 });
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 8 });
    assert!(!dr.state.identity_bootstrap_hold);
}

#[test]
fn repository_identity_publication_moves_retarget_atomically_or_not_at_all() {
    use schlange_core::engine::VerifiedIdentityBase;
    let mut dr = Driver::new();
    let old = PullRequest::new(1, "a", "main", "pr-1", "h1");
    dr.state.pull_requests.insert(1, old);
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::CheckIdentitiesUnavailable {
        bases: BTreeMap::from([
            ("main".into(), (4, vec![1])),
            ("other".into(), (4, vec![1])),
        ]),
    });
    let main_epoch = dr.state.check_identity_epochs["main"];
    let other_epoch = dr.state.check_identity_epochs["other"];
    let mut new = PullRequest::new(1, "a", "other", "pr-1", "h1");
    new.base_tip = Some("b1".into());
    let main = VerifiedIdentityBase {
        epoch: main_epoch,
        generation: 5,
        base_tip: "b0".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_requests: vec![],
    };
    let other = VerifiedIdentityBase {
        epoch: other_epoch,
        generation: 5,
        base_tip: "b1".into(),
        required_checks: vec![],
        bound_required_checks: vec![],
        pull_requests: vec![new],
    };
    let d = dr.feed(Event::IdentityRepositoryVerified {
        bases: BTreeMap::from([("main".into(), main.clone())]),
    });
    assert!(d.is_empty());
    assert_eq!(dr.state.pull_requests[&1].base, "main");
    assert_eq!(dr.state.check_identity_targets.len(), 2);
    let d = dr.feed(Event::IdentityRepositoryVerified {
        bases: BTreeMap::from([("main".into(), main), ("other".into(), other)]),
    });
    assert!(d.is_empty());
    assert_eq!(dr.state.pull_requests[&1].base, "other");
    assert!(dr.state.check_identity_targets.is_empty());
    assert!(dr.state.identity_bootstrap_hold);
}

#[test]
fn repository_identity_rejects_any_bad_base_without_partial_publication() {
    use schlange_core::engine::VerifiedIdentityBase;
    for fault in 0..4 {
        let mut dr = Driver::new();
        let first = PullRequest::new(1, "a", "aaa", "pr-1", "h1");
        let mut old_second = PullRequest::new(2, "b", "zzz", "pr-2", "h2");
        if fault == 3 {
            old_second.closed = true;
            old_second.merged = true;
            old_second.merge_commit_sha = Some("known".into());
        }
        dr.state.pull_requests.insert(1, first.clone());
        dr.state.pull_requests.insert(2, old_second.clone());
        dr.feed(Event::IdentityBootstrapHeld);
        dr.feed(Event::CheckIdentitiesUnavailable {
            bases: BTreeMap::from([("aaa".into(), (4, vec![1])), ("zzz".into(), (4, vec![2]))]),
        });
        let mut p1 = first;
        p1.title = "new first".into();
        p1.base_tip = Some("b1".into());
        let mut p2 = old_second;
        p2.title = "new second".into();
        p2.base_tip = Some("b2".into());
        let a = VerifiedIdentityBase {
            epoch: dr.state.check_identity_epochs["aaa"],
            generation: 5,
            base_tip: "b1".into(),
            required_checks: vec![],
            bound_required_checks: vec![],
            pull_requests: vec![p1],
        };
        let mut b = VerifiedIdentityBase {
            epoch: dr.state.check_identity_epochs["zzz"],
            generation: 5,
            base_tip: "b2".into(),
            required_checks: vec![],
            bound_required_checks: vec![],
            pull_requests: vec![p2],
        };
        match fault {
            0 => b.epoch += 1,
            1 => b.pull_requests[0].base_tip = Some("wrong".into()),
            2 => b.pull_requests[0].number = 1,
            3 => {
                b.pull_requests[0].merged = false;
                b.pull_requests[0].merge_commit_sha = None;
            }
            _ => unreachable!(),
        }
        dr.feed(Event::IdentityRepositoryVerified {
            bases: BTreeMap::from([("aaa".into(), a), ("zzz".into(), b)]),
        });
        assert_eq!(dr.state.pull_requests[&1].title, "", "fault {fault}");
        assert_eq!(dr.state.pull_requests[&2].title, "", "fault {fault}");
        assert_eq!(dr.state.check_identity_targets.len(), 2, "fault {fault}");
        assert!(dr.state.identity_bootstrap_hold);
    }
}

#[test]
fn bootstrap_drops_stale_attestation_when_new_safety_input_arrives() {
    use schlange_core::engine::Control;
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    dr.feed(Event::Control {
        control: Control::Pause {
            reason: "maintenance".into(),
            by: None,
        },
    });
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 7 });
    assert!(
        dr.state.identity_bootstrap_hold,
        "a safety input must invalidate the prior attestation"
    );
}

#[test]
fn recovery_preparation_persists_in_doubt_actions_and_in_place_announcements_without_sending() {
    use schlange_core::actions::{Op, PendingAction};
    let mut dr = Driver::new();
    let car = dr.queued_car();
    dr.state.trains.get_mut("main").unwrap().cars[0].in_place = true;
    dr.state.actions.pending.insert(
        99,
        PendingAction {
            rule: "dispatch".into(),
            op: Op::Dispatch {
                number: 1,
                workflow: "ci.yml".into(),
                reference: None,
                inputs: BTreeMap::new(),
            },
            level: None,
            reply: None,
            report: None,
            sent: 42,
            attempts: 1,
            edge: None,
        },
    );
    dr.feed(Event::IdentityBootstrapHeld);
    let decisions = dr.feed(Event::IdentityRecoveryPrepared);
    assert!(decisions.is_empty(), "{decisions:?}");
    assert!(dr.state.recovery_pending_actions.contains(&99));
    assert_eq!(dr.state.recovery_in_place[&car].number, 1000);
    assert_eq!(dr.state.recovery_in_place[&car].head_sha, "c1");
    dr.state.trains.get_mut("main").unwrap().cars[0].head_sha = Some("changed".into());
    dr.feed(Event::IdentityRecoveryPrepared);
    assert_eq!(
        dr.state.recovery_in_place[&car].head_sha, "c1",
        "repeat must preserve the original identity"
    );
    assert_eq!(dr.state.actions.pending[&99].sent, 42);
    let encoded = serde_json::to_vec(&dr.state).unwrap();
    dr.state = serde_json::from_slice(&encoded).unwrap();
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 7 });
    assert!(
        dr.state.identity_bootstrap_hold,
        "unresolved recovery work cannot be released"
    );
}

#[test]
fn recovery_clean_keeps_unconfirmed_landing_held() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.state.unconfirmed_land.insert(9);
    dr.state = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 9 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 9 });
    assert!(!dr.state.identity_recovery_clean);
    assert!(dr.state.unconfirmed_land.contains(&9));
}

#[test]
fn recovery_clean_preserves_armed_and_outstanding_actions() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.state.actions.armed.insert("1/rule/backport".into());
    dr.state
        .actions
        .asked
        .insert("1/rule/check".into(), "outstanding".into());
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 9 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 9 });
    assert!(!dr.state.identity_recovery_clean);
    assert!(dr.state.actions.armed.contains("1/rule/backport"));
    assert!(dr.state.actions.asked.contains_key("1/rule/check"));
}

#[test]
fn recovery_clean_keeps_unresolved_head_bound_command_held() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.state.pending_commands.insert(
        1,
        schlange_core::control::PendingCommand {
            queue: None,
            by: "alice".into(),
            comment: Some(7),
            head_sha: "h1".into(),
            since: 0,
        },
    );
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 9 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 9 });
    assert!(
        !dr.state.identity_recovery_clean,
        "unresolved command requires head-bound recovery"
    );
    assert_eq!(dr.state.pending_commands[&1].head_sha, "h1");
}

#[test]
fn recovery_clean_does_not_discard_unresolved_timer() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.state.wakeups.insert(42);
    dr.state = serde_json::from_slice(&serde_json::to_vec(&dr.state).unwrap()).unwrap();
    dr.feed(Event::IdentityRecoveryPrepared);
    assert!(
        dr.state.wakeups.contains(&42),
        "preparation must retain pre-existing timer"
    );
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 9 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 9 });
    assert!(
        !dr.state.identity_recovery_clean,
        "timer needs re-decision before release"
    );
    assert!(
        dr.state.wakeups.contains(&42),
        "unresolved timer must remain durable"
    );
    let (later, _) = step(
        &dr.state,
        &dr.rules,
        &Event::IdentityRecoveryCleanFinalized { ingress_epoch: 9 },
        50,
    );
    assert!(
        !later.identity_recovery_clean,
        "expired timer also needs re-decision"
    );
    assert!(later.wakeups.contains(&42));
}

#[test]
fn bootstrap_requires_recovery_preparation_even_when_discovery_is_verified() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 9 });
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 9 });
    assert!(
        dr.state.identity_bootstrap_hold,
        "recovery must be prepared in this bootstrap session"
    );
}

#[test]
fn recovery_discards_only_superseded_in_place_announcement() {
    let mut dr = Driver::new();
    let car = dr.queued_car();
    dr.state.trains.get_mut("main").unwrap().cars[0].in_place = true;
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    dr.feed(Event::IdentityRecoveryInPlaceObsolete {
        car,
        ingress_epoch: 7,
    });
    assert!(
        dr.state.recovery_in_place.contains_key(&car),
        "matching car still needs announcement"
    );
    dr.state.trains.get_mut("main").unwrap().cars[0].head_sha = Some("new-head".into());
    dr.feed(Event::IdentityRecoveryInPlaceObsolete {
        car,
        ingress_epoch: 7,
    });
    assert!(!dr.state.recovery_in_place.contains_key(&car));
    assert!(dr.state.identity_bootstrap_hold);
}

#[test]
fn repository_publication_binds_pr_check_selection_to_the_batch_requirements() {
    use schlange_core::engine::VerifiedIdentityBase;
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1],
    });
    let mut p = PullRequest::new(1, "a", "main", "pr-1", "h1");
    p.base_tip = Some("b0".into());
    let batch = VerifiedIdentityBase {
        epoch: dr.state.check_identity_epochs["main"],
        generation: 5,
        base_tip: "b0".into(),
        required_checks: vec!["ci".into()],
        bound_required_checks: vec![],
        pull_requests: vec![p],
    };
    dr.feed(Event::IdentityRepositoryVerified {
        bases: BTreeMap::from([("main".into(), batch)]),
    });
    assert!(
        dr.state.pull_requests[&1]
            .required_check_names
            .contains("ci")
    );
}

#[test]
fn repository_publication_derives_required_workflows_and_rejects_wrong_app() {
    use schlange_config::conditions::Condition;
    use schlange_core::engine::VerifiedIdentityBase;
    use schlange_core::model::RequiredCheck;
    for (required_id, missing_workflow) in [(42, false), (43, false), (42, true)] {
        let mut dr = Driver::new();
        dr.feed(Event::IdentityBootstrapHeld);
        dr.feed(Event::CheckIdentityUnavailable {
            base: "main".into(),
            generation: 4,
            numbers: vec![1],
        });
        let mut p = PullRequest::new(1, "a", "main", "pr-1", "h1");
        p.base_tip = Some("b0".into());
        p.required_check_names.insert("bogus".into());
        p.checks = vec![
            CheckRun {
                name: "ci".into(),
                app: Some("github-actions".into()),
                app_id: Some(42),
                head_sha: "h1".into(),
                conclusion: Some(Conclusion::Failure),
                seq: 1,
                suite_id: Some(1),
                suite_created_at: Some("2026-01-01T00:00:00Z".into()),
                workflow: Some("a.yml".into()),
                event: Some("pull_request".into()),
                ..Default::default()
            },
            CheckRun {
                name: "ci".into(),
                app: Some("github-actions".into()),
                app_id: Some(42),
                head_sha: "h1".into(),
                conclusion: Some(Conclusion::Success),
                seq: 2,
                suite_id: Some(2),
                suite_created_at: Some("2026-01-01T00:00:01Z".into()),
                workflow: (!missing_workflow).then(|| "b.yml".into()),
                event: Some("pull_request".into()),
                ..Default::default()
            },
        ];
        let batch = VerifiedIdentityBase {
            epoch: dr.state.check_identity_epochs["main"],
            generation: 5,
            base_tip: "b0".into(),
            required_checks: vec![],
            bound_required_checks: vec![RequiredCheck {
                context: "ci".into(),
                app_id: required_id,
            }],
            pull_requests: vec![p],
        };
        dr.feed(Event::IdentityRepositoryVerified {
            bases: BTreeMap::from([("main".into(), batch)]),
        });
        let read = &dr.state.pull_requests[&1];
        assert!(!read.required_check_names.contains("bogus"));
        let cond = Condition::RequiredCheck {
            context: "ci".into(),
            app_id: required_id,
        };
        assert!(
            !schlange_core::eval::eval(&cond, read, &Default::default(), &mut vec![]),
            "app={required_id}, missing={missing_workflow}"
        );
        if required_id == 42 {
            assert!(read.required_check_names.contains("@github-actions/ci"));
        }
        if missing_workflow {
            assert!(read.checks_incomplete());
        }
    }
}

#[test]
fn bootstrap_applies_deferred_configuration_reset_without_decisions() {
    let mut dr = Driver::new();
    dr.queued_car();
    let draft = dr.draft(vec![run("full", "c1", Some(Conclusion::Success), 2)]);
    dr.state.pull_requests.insert(1000, draft);
    dr.state
        .reported_errors
        .insert((1, "old-rule-error".into()));
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    let d = dr.feed(Event::IdentityRecoveryConfigApplied { ingress_epoch: 7 });
    assert!(d.is_empty(), "{d:?}");
    assert!(dr.state.reported_errors.is_empty());
    assert!(dr.state.identity_bootstrap_hold);
}

#[test]
fn clean_bootstrap_recovery_requires_no_inflight_cars_or_actions() {
    let mut dr = Driver::new();
    dr.queued_car();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 7 });
    assert!(
        !dr.state.identity_recovery_clean,
        "an in-flight car needs explicit recovery"
    );
    dr.feed(Event::IdentityBootstrapReleased { ingress_epoch: 7 });
    assert!(dr.state.identity_bootstrap_hold);
}

#[test]
fn new_identity_hold_invalidates_prior_clean_recovery() {
    let mut dr = Driver::new();
    dr.feed(Event::IdentityBootstrapHeld);
    dr.feed(Event::IdentityRecoveryPrepared);
    dr.feed(Event::IdentityDiscoveryVerified { ingress_epoch: 7 });
    dr.feed(Event::IdentityRecoveryCleanFinalized { ingress_epoch: 7 });
    assert!(dr.state.identity_recovery_clean);
    dr.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![],
    });
    assert!(
        !dr.state.identity_recovery_clean,
        "new holds require recovery revalidation"
    );
}
