//! Dequeue and "checks ended" codes on the paths the critique of phase 1abc named
//! (`.critic/phase-1abc.md`, "Dequeue codes" and "Branch edits"), driven event by event.

use std::collections::BTreeMap;

use schlange_config::rules::Rules;
use schlange_core::engine::{DequeueReason, LandError};
use schlange_core::model::{CheckRun, Conclusion, PullRequest};
use schlange_core::reasons as code;
use schlange_core::{Decision, Event, State, step};

fn config(extra_queue: &str, top: &str) -> String {
    format!(
        "{top}queue_rules:\n  - name: default\n{extra_queue}    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
         pull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n"
    )
}

fn run(name: &str, sha: &str, ok: Option<Conclusion>, seq: u64) -> CheckRun {
    CheckRun {
        name: name.into(),
        app: None,
        head_sha: sha.into(),
        conclusion: ok,
        seq,
        ..CheckRun::default()
    }
}

fn pr(n: u64) -> PullRequest {
    let mut p = PullRequest::new(n, "a", "main", &format!("pr-{n}"), &format!("h{n}"));
    p.checks = vec![run("quick", &format!("h{n}"), Some(Conclusion::Success), n)];
    p
}

struct Driver {
    rules: Rules,
    state: State,
    /// (car, branch, draft) per built car, in build order.
    cars: Vec<(u64, String, u64)>,
}

impl Driver {
    fn new(cfg: &str) -> Self {
        Driver {
            rules: Rules::from_yaml(cfg).unwrap(),
            state: State::default(),
            cars: vec![],
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

    /// Answers every `BuildCar` in `d` (and in the decisions that follow) with a built car.
    fn build_all(&mut self, mut d: Vec<Decision>) {
        while let Some(Decision::BuildCar { car, branch, .. }) = d
            .iter()
            .find(|d| matches!(d, Decision::BuildCar { .. }))
            .cloned()
        {
            let draft = 1000 + self.cars.len() as u64;
            self.cars.push((car, branch.clone(), draft));
            d = self.feed(Event::CarBuilt {
                car,
                branch,
                head_sha: format!("c{car}"),
                tree: format!("t{car}"),
                draft,
            });
        }
    }

    /// Queues #1 and #2 on base b0, each in its own car (#2's car tested on top of #1).
    fn two_cars(&mut self) {
        let d = self.snapshot(vec![pr(1), pr(2)], &[("main", "b0")]);
        self.build_all(d);
        assert_eq!(self.cars.len(), 2, "two cars: {:?}", self.state.trains);
    }

    fn draft(&self, k: usize, checks: Vec<CheckRun>) -> PullRequest {
        let (car, branch, number) = &self.cars[k];
        let mut d = PullRequest::new(*number, "bot", "main", branch, &format!("c{car}"));
        d.draft = true;
        d.checks = checks;
        d
    }

    /// Makes car `k` green and answers the read before landing; returns its `Land`.
    fn land(&mut self, k: usize) -> Decision {
        let (car, ..) = self.cars[k];
        let draft = self.draft(
            k,
            vec![run(
                "full",
                &format!("c{car}"),
                Some(Conclusion::Success),
                50,
            )],
        );
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

/// The reason of the `Dequeue` or `ChecksEnded` of pull request `n` in `d`.
fn reason_of(d: &[Decision], n: u64) -> Option<DequeueReason> {
    d.iter().find_map(|d| match d {
        Decision::Dequeue { number, reason, .. } | Decision::ChecksEnded { number, reason, .. }
            if *number == n =>
        {
            Some(reason.clone())
        }
        _ => None,
    })
}

fn dequeued(d: &[Decision], n: u64) -> Option<DequeueReason> {
    d.iter().find_map(|d| match d {
        Decision::Dequeue { number, reason, .. } if *number == n => Some(reason.clone()),
        _ => None,
    })
}

#[test]
fn a_refused_landing_ends_the_checks_of_its_own_pull_requests_with_a_code() {
    let mut dr = Driver::new(&config("", ""));
    dr.two_cars();
    let Decision::Land { car, .. } = dr.land(0) else {
        unreachable!()
    };
    let d = dr.feed(Event::Landed {
        car,
        commits: vec![],
        error: Some(LandError::BaseMoved {
            actual: "b9".into(),
        }),
    });
    let r = reason_of(&d, 1).expect("#1, whose landing was refused, gets a code");
    assert_eq!(r.code, Some(code::MERGE_QUEUE_RESET), "{r:?}");
    assert!(dr.state.queued.contains_key(&1), "stays queued");
}

#[test]
fn cars_behind_a_refused_pull_request_get_pr_ahead_dequeued() {
    let mut dr = Driver::new(&config("", ""));
    dr.two_cars();
    let Decision::Land { car, .. } = dr.land(0) else {
        unreachable!()
    };
    let d = dr.feed(Event::Landed {
        car,
        commits: vec![],
        error: Some(LandError::Conflict { number: 1 }),
    });
    assert_eq!(
        dequeued(&d, 1).and_then(|r| r.code),
        Some(code::PR_UNEXPECTEDLY_FAILED_TO_MERGE)
    );
    assert_eq!(
        reason_of(&d, 2).and_then(|r| r.code),
        Some(code::PR_AHEAD_DEQUEUED),
        "{d:#?}"
    );
}

#[test]
fn a_foreign_push_ahead_is_not_reported_as_an_accepted_branch_edit() {
    let mut dr = Driver::new(&config("", ""));
    dr.two_cars();
    let branch = dr.cars[0].1.clone();
    let d = dr.snapshot(vec![], &[(&branch, "someone-else")]);
    assert_eq!(
        dequeued(&d, 1).and_then(|r| r.code),
        Some(code::DRAFT_PULL_REQUEST_CHANGED)
    );
    let r = reason_of(&d, 2).expect("#2 behind");
    assert_ne!(r.code, Some(code::BATCH_AHEAD_BRANCH_EDITED), "{r:?}");
    assert_eq!(r.code, Some(code::PR_AHEAD_DEQUEUED), "{r:?}");
}

#[test]
fn an_edited_batch_dequeues_every_pull_request_embarked_in_it() {
    // Docs (allow_queue_branch_edit unset): an edited queue branch dequeues all pull requests
    // embarked in it. #2's branch carries #1 too.
    let mut dr = Driver::new(&config("", ""));
    dr.two_cars();
    let branch = dr.cars[1].1.clone();
    let d = dr.snapshot(vec![], &[(&branch, "someone-else")]);
    for n in [1, 2] {
        assert_eq!(
            dequeued(&d, n).and_then(|r| r.code),
            Some(code::DRAFT_PULL_REQUEST_CHANGED),
            "#{n}: {d:#?}"
        );
    }
}

#[test]
fn an_accepted_branch_edit_reports_batch_ahead_branch_edited() {
    let mut dr = Driver::new(&config(
        "    allow_queue_branch_edit: true\n    merge_method: merge-batch\n",
        "",
    ));
    dr.two_cars();
    let branch = dr.cars[0].1.clone();
    let d = dr.snapshot(vec![], &[(&branch, "edited")]);
    assert!(dequeued(&d, 1).is_none(), "the edit is accepted: {d:#?}");
    let r = reason_of(&d, 2).expect("#2 behind is rebuilt");
    assert_eq!(r.code, Some(code::BATCH_AHEAD_BRANCH_EDITED));
    assert!(!r.assumed, "the code names this path");
}

#[test]
fn never_mode_conflict_with_the_moved_base_is_conflict_with_base_branch() {
    let mut dr = Driver::new(&config(
        "",
        "merge_queue:\n  reset_on_external_merge: never\n",
    ));
    dr.two_cars();
    // An external push to the base: the queue keeps its cars (never) and lands on the new tip.
    dr.snapshot(vec![], &[("main", "b-ext")]);
    let Decision::Land {
        car,
        expected_base_sha,
        ..
    } = dr.land(0)
    else {
        unreachable!()
    };
    assert_eq!(expected_base_sha, "b-ext");
    let d = dr.feed(Event::Landed {
        car,
        commits: vec![],
        error: Some(LandError::Conflict { number: 1 }),
    });
    assert_eq!(
        dequeued(&d, 1).and_then(|r| r.code),
        Some(code::CONFLICT_WITH_BASE_BRANCH),
        "{d:#?}"
    );
}

#[test]
fn a_draft_closed_by_someone_else_is_batch_pull_request_closed() {
    let mut dr = Driver::new(&config("", ""));
    dr.two_cars();
    let mut draft = dr.draft(0, vec![]);
    draft.closed = true;
    let d = dr.snapshot(vec![draft], &[]);
    let r = reason_of(&d, 1).expect("#1's batch pull request was closed");
    assert_eq!(r.code, Some(code::BATCH_PULL_REQUEST_CLOSED), "{d:#?}");
    assert!(
        reason_of(&d, 2).and_then(|r| r.code).is_some(),
        "#2 behind gets a code: {d:#?}"
    );
    assert!(
        d.iter().any(|d| matches!(d, Decision::CloseCar { .. })),
        "the car is torn down"
    );
}

#[test]
fn codes_the_name_states_are_not_marked_assumed() {
    // A retry and a reset after an external merge are what the codes say.
    let mut dr = Driver::new(&config("", ""));
    dr.two_cars();
    let d = dr.snapshot(vec![], &[("main", "b-ext")]);
    let r = reason_of(&d, 1).expect("reset");
    assert_eq!(r.code, Some(code::MERGE_QUEUE_RESET));
    assert!(!r.assumed, "{r:?}");
}
