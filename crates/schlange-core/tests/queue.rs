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
