//! The state machine: `(state, event) → (state, decisions)`, without I/O.
//!
//! Events are what the forge reports: webhook-like changes, truth reads (`Snapshot`), and the
//! replies to the engine's own decisions (`CarBuilt`, `CarConflict`, `Landed`). Decisions are
//! what the forge adapter carries out. After every event the engine re-evaluates every pull
//! request and every queue (`process`), so handling the same event twice changes nothing
//! (MQ-12, MQ-29).
//!
//! Every decision that changes the base is bound to head SHAs and to the expected base SHA:
//! landing is a compare-and-swap, and the forge refuses it when anything moved.
//!
//! Replies are not trusted to arrive: any of them can be lost, duplicated, delayed or replaced
//! by an error although the side effect happened (`Event::Failed`). The queue retries or
//! resolves every outstanding request from a fresh read (see `queue`). The state is
//! serialisable; a host journals it before every side effect (`journal`) and after a crash
//! restarts from the journal with `Event::Restarted` and a full read.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use schlange_config::rules::Rules;

use crate::eval::{self, Context};
use crate::model::{CheckRun, PullRequest, Time};
use crate::queue::{self, Train};
pub use crate::reasons::DequeueReason;
use crate::reasons::{self as code, blocks_requeue};

/// A pull request as tested in a car: number and the head SHA it was tested with.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CarPr {
    pub number: u64,
    pub head_sha: String,
}

/// One squash commit the forge created while landing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LandedCommit {
    pub number: u64,
    pub sha: String,
    pub tree: String,
}

/// Why the forge refused (part of) a landing.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LandError {
    /// Compare-and-swap failed: the base is no longer the expected commit.
    BaseMoved { actual: String },
    /// The pull request's head is not the tested head.
    HeadChanged { number: u64 },
    /// The squash merge did not apply.
    Conflict { number: u64 },
}

/// What the forge reports.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    PullRequestOpened {
        pull_request: PullRequest,
    },
    PullRequestSynchronized {
        number: u64,
        head_sha: String,
        files: Vec<String>,
    },
    ReadyForReview {
        number: u64,
    },
    ConvertedToDraft {
        number: u64,
    },
    Labeled {
        number: u64,
        label: String,
    },
    Unlabeled {
        number: u64,
        label: String,
    },
    CheckRun {
        number: u64,
        run: CheckRun,
    },
    Closed {
        number: u64,
    },
    /// A truth read from the forge: these pull requests and branches as they are now.
    /// `branches` maps a branch to its head SHA, `None` when it does not exist. `token` ties
    /// the read to a `Refresh` decision.
    Snapshot {
        pull_requests: Vec<PullRequest>,
        branches: BTreeMap<String, Option<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token: Option<u64>,
    },
    /// Reply to `BuildCar`: the queue branch and its draft pull request exist.
    CarBuilt {
        car: u64,
        branch: String,
        head_sha: String,
        tree: String,
        draft: u64,
    },
    /// Reply to `BuildCar`: pull request `number` does not merge; nothing was created.
    CarConflict {
        car: u64,
        number: u64,
        /// It does not merge onto the base alone.
        with_base: bool,
        /// Pull requests ahead it conflicts with (when it merges onto the base alone).
        with: Vec<u64>,
    },
    /// Reply to `Land`: the commits created, in order, and the error that stopped it.
    Landed {
        car: u64,
        commits: Vec<LandedCommit>,
        error: Option<LandError>,
    },
    /// Virtual time reached a wake-up the engine asked for.
    Tick,
    /// The forge answered a decision with an error. The side effect may still have happened
    /// (a timeout or 5xx after a successful write, MQ-35).
    Failed {
        decision: Decision,
        error: String,
    },
    /// Reply to `CloseCar`: the draft is closed, the branch deleted and the draft's CI stopped.
    CarClosed {
        car: u64,
    },
    /// The engine restarted from its journal; outstanding requests are in doubt.
    Restarted,
}

/// What the engine decided. The forge adapter turns decisions into side effects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    Enqueue {
        number: u64,
        queue: String,
        rule: String,
    },
    Dequeue {
        number: u64,
        queue: String,
        reason: DequeueReason,
    },
    /// The pull request's checks ended (its car was discarded); it stays queued.
    ChecksEnded {
        number: u64,
        queue: String,
        reason: DequeueReason,
    },
    /// A condition could not be evaluated; it counts as false.
    EvaluationError { number: u64, message: String },
    /// Create `branch` at `base_sha`, merge the pull request heads in order, open a draft
    /// pull request on it. Reply: `CarBuilt` or `CarConflict`.
    BuildCar {
        car: u64,
        queue: String,
        base_branch: String,
        branch: String,
        base_sha: String,
        prs: Vec<CarPr>,
        /// The pull requests this car checks; the others are ahead of it.
        own: Vec<u64>,
    },
    /// Check the pull request in place: no queue branch, no draft pull request.
    CheckInPlace {
        car: u64,
        number: u64,
        head_sha: String,
    },
    /// Close the draft pull request (and any open draft on `branch`), delete the queue branch
    /// and stop the draft's CI. Reply: `CarClosed`. Idempotent.
    CloseCar {
        car: u64,
        branch: Option<String>,
        draft: Option<u64>,
    },
    /// Read these pull requests and branches now. Reply: `Snapshot` with `token`.
    Refresh {
        token: u64,
        numbers: Vec<u64>,
        branches: Vec<String>,
    },
    /// Squash-merge the pull requests in order onto `base_branch`, each as a compare-and-swap
    /// from the expected base, each only at the given head. Reply: `Landed`.
    Land {
        car: u64,
        queue: String,
        base_branch: String,
        expected_base_sha: String,
        prs: Vec<CarPr>,
        expected_tree: String,
    },
    /// The pull request landed on the base.
    Merged {
        number: u64,
        queue: String,
        sha: String,
    },
    /// The queue of a base branch stops: nothing lands until a human looks (MQ-02).
    Halt { base_branch: String, reason: String },
    /// Deliver `Tick` at this virtual time.
    WakeAt { at: Time },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum EntryStatus {
    /// Waiting for a car.
    Waiting,
    /// Checked in a car.
    InCar { car: u64 },
    /// Part of a red batch that is being split.
    Bisecting,
    /// Conflicts with pull requests ahead; keeps its place until they leave or merge.
    Held { on: Vec<u64> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueueEntry {
    pub queue: String,
    pub rule: String,
    pub since: Time,
    pub head_sha: String,
    pub base: String,
    /// Queue order within the base branch.
    pub seq: u64,
    /// Start of the batch wait (`batch_max_wait_time`).
    pub waiting_since: Time,
    pub status: EntryStatus,
}

/// How a queued pull request ended up, the last time it left the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum Outcome {
    Merged { sha: String },
    Dequeued { reason: DequeueReason },
}

/// A car being closed: it keeps its slot until the forge confirms (`CarClosed`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Closing {
    /// The base branch whose slot it holds; `None` when unknown (a late reply).
    pub base: Option<String>,
    pub branch: Option<String>,
    pub draft: Option<u64>,
    /// When the close was (last) sent.
    pub since: Time,
}

/// One successful first run of a queue's checks, for `checks_timeout: auto`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sample {
    pub at: Time,
    pub secs: Time,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct State {
    pub pull_requests: BTreeMap<u64, PullRequest>,
    pub queued: BTreeMap<u64, QueueEntry>,
    /// Branch heads as last read from the forge.
    pub branches: BTreeMap<String, String>,
    /// One queue per base branch.
    pub trains: BTreeMap<String, Train>,
    pub outcomes: BTreeMap<u64, Outcome>,
    /// Pull requests the `queue` action does not queue again at this head (failure codes).
    pub blocked_heads: BTreeMap<u64, String>,
    pub wakeups: BTreeSet<Time>,
    pub reported_errors: BTreeSet<(u64, String)>,
    pub counter: u64,
    /// Time of the event being handled.
    #[serde(default)]
    pub now: Time,
    /// Cars being closed, by car id.
    #[serde(default)]
    pub closing: BTreeMap<u64, Closing>,
    /// Runtimes per `queue@base` for `checks_timeout: auto`.
    #[serde(default)]
    pub history: BTreeMap<String, Vec<Sample>>,
    /// Pull requests whose landing had an unknown outcome (lost or lying reply, restart): if
    /// one shows up merged later, the queue merged it (MQ-35).
    #[serde(default)]
    pub unconfirmed_land: BTreeSet<u64>,
    /// Pull requests whose own attributes failed the merge conditions, with the attributes they
    /// had then: not queued again until they change.
    #[serde(default)]
    pub blocked_attrs: BTreeMap<u64, String>,
}

impl State {
    pub fn next_id(&mut self) -> u64 {
        self.counter += 1;
        self.counter
    }
}

/// Applies one event and re-evaluates every pull request and queue.
pub fn step(state: &State, rules: &Rules, event: &Event, now: Time) -> (State, Vec<Decision>) {
    let mut next = state.clone();
    next.now = now;
    let mut out = Vec::new();
    next.wakeups.retain(|t| *t > now);
    let token = apply(&mut next, rules, event, &mut out);
    process(&mut next, rules, now, token, &mut out);
    (next, out)
}

fn apply(state: &mut State, rules: &Rules, event: &Event, out: &mut Vec<Decision>) -> Option<u64> {
    fn pr(state: &mut State, n: u64) -> Option<&mut PullRequest> {
        state.pull_requests.get_mut(&n)
    }
    match event {
        Event::PullRequestOpened { pull_request } => {
            state
                .pull_requests
                .insert(pull_request.number, pull_request.clone());
        }
        Event::PullRequestSynchronized {
            number,
            head_sha,
            files,
        } => {
            if let Some(p) = pr(state, *number) {
                p.head_sha = head_sha.clone();
                p.files = files.clone();
            }
        }
        Event::ReadyForReview { number } => {
            if let Some(p) = pr(state, *number) {
                p.draft = false;
            }
        }
        Event::ConvertedToDraft { number } => {
            if let Some(p) = pr(state, *number) {
                p.draft = true;
            }
        }
        Event::Labeled { number, label } => {
            if let Some(p) = pr(state, *number) {
                p.labels.insert(label.clone());
            }
        }
        Event::Unlabeled { number, label } => {
            if let Some(p) = pr(state, *number) {
                p.labels.remove(label);
            }
        }
        Event::CheckRun { number, run } => {
            if let Some(p) = pr(state, *number) {
                match p.checks.iter_mut().find(|c| c.seq == run.seq) {
                    Some(existing) => *existing = run.clone(),
                    None => p.checks.push(run.clone()),
                }
            }
        }
        Event::Closed { number } => {
            if let Some(p) = pr(state, *number) {
                p.closed = true;
            }
        }
        Event::Snapshot {
            pull_requests,
            branches,
            token,
        } => {
            for p in pull_requests {
                // Merged is final: a read that says otherwise lags behind (MQ-33).
                if !p.merged && state.pull_requests.get(&p.number).is_some_and(|o| o.merged) {
                    continue;
                }
                state.pull_requests.insert(p.number, p.clone());
            }
            for (b, sha) in branches {
                match sha {
                    Some(s) => state.branches.insert(b.clone(), s.clone()),
                    None => state.branches.remove(b),
                };
            }
            return *token;
        }
        Event::CarBuilt {
            car,
            branch,
            head_sha,
            tree,
            draft,
        } => queue::car_built(state, *car, branch, head_sha, tree, *draft, out),
        Event::CarConflict {
            car,
            number,
            with_base,
            with,
        } => queue::car_conflict(state, rules, *car, *number, *with_base, with, out),
        Event::Landed {
            car,
            commits,
            error,
        } => queue::landed(state, rules, *car, commits, error.as_ref(), out),
        Event::Tick => {}
        Event::Failed { decision, .. } => queue::side_effect_failed(state, decision, out),
        Event::CarClosed { car } => queue::car_closed(state, *car),
        Event::Restarted => queue::expire_all(state),
    }
    None
}

/// Whether a pull request is one of the queue's own draft pull requests.
pub fn is_queue_pull_request(rules: &Rules, pr: &PullRequest) -> bool {
    rules
        .queue_rules
        .iter()
        .any(|q| pr.head_ref.starts_with(&q.queue_branch_prefix))
}

fn process(
    state: &mut State,
    rules: &Rules,
    now: Time,
    token: Option<u64>,
    out: &mut Vec<Decision>,
) {
    let numbers: Vec<u64> = state.pull_requests.keys().copied().collect();
    for number in numbers {
        let pr = state.pull_requests[&number].clone();
        if is_queue_pull_request(rules, &pr) || queue::is_car_draft(state, number) {
            continue;
        }
        evaluate_pull_request(state, rules, &pr, now, out);
    }
    queue::process_trains(state, rules, now, token, out);
}

/// Removes a pull request from the queue with a reason and discards what depended on it.
pub(crate) fn dequeue(
    state: &mut State,
    rules: &Rules,
    number: u64,
    reason: DequeueReason,
    out: &mut Vec<Decision>,
) {
    let Some(entry) = state.queued.remove(&number) else {
        return;
    };
    if blocks_requeue(reason.code)
        && let Some(pr) = state.pull_requests.get(&number)
    {
        state.blocked_heads.insert(number, pr.head_sha.clone());
    }
    queue::discard_containing(state, rules, &entry.base, number, out);
    state.outcomes.insert(
        number,
        Outcome::Dequeued {
            reason: reason.clone(),
        },
    );
    out.push(Decision::Dequeue {
        number,
        queue: entry.queue,
        reason,
    });
}

fn evaluate_pull_request(
    state: &mut State,
    rules: &Rules,
    pr: &PullRequest,
    now: Time,
    out: &mut Vec<Decision>,
) {
    let ctx = Context::default();
    let mut errors = Vec::new();
    let number = pr.number;

    if let Some(entry) = state.queued.get(&number).cloned() {
        let rule = rules.queue_rules.iter().find(|q| q.name == entry.queue);
        if pr.merged && queue::landing_in_flight(state, number) {
            // The queue's own landing, seen before its reply: the reply (or the read that
            // resolves it) decides.
            return;
        }
        if pr.merged && state.unconfirmed_land.contains(&number) {
            // The queue's own landing, whose reply was lost or lied (MQ-35).
            let sha = pr.merge_commit_sha.clone().unwrap_or_default();
            queue::discard_containing(state, rules, &entry.base, number, out);
            queue::record_merge(state, number, &sha, out);
            return;
        }
        let reason = if pr.merged {
            Some(DequeueReason::new(
                code::PR_MANUALLY_MERGED,
                "the pull request was merged outside the queue",
            ))
        } else if pr.closed {
            Some(DequeueReason::assumed(
                code::PR_DEQUEUED,
                "the pull request was closed",
            ))
        } else if pr.base != entry.base {
            Some(DequeueReason::new(
                code::BASE_BRANCH_CHANGED,
                format!(
                    "the base branch changed from `{}` to `{}`",
                    entry.base, pr.base
                ),
            ))
        } else if pr.head_sha != entry.head_sha {
            Some(DequeueReason::new(
                code::PULL_REQUEST_UPDATED,
                format!(
                    "the head moved from {} to {} after it was queued",
                    short(&entry.head_sha),
                    short(&pr.head_sha)
                ),
            ))
        } else {
            match rule {
                None => Some(DequeueReason::new(
                    code::QUEUE_RULE_MISSING,
                    format!("queue `{}` no longer exists", entry.queue),
                )),
                Some(q) => {
                    let (ok, results) = eval::eval_list(&q.queue_conditions, pr, &ctx, &mut errors);
                    (!ok).then(|| {
                        DequeueReason::assumed(
                            code::PR_DEQUEUED,
                            format!(
                                "queue_conditions no longer hold (condition {} is false)",
                                results.iter().position(|r| !r).map_or(0, |i| i + 1)
                            ),
                        )
                    })
                }
            }
        };
        if let Some(reason) = reason {
            dequeue(state, rules, number, reason, out);
        }
    } else if !pr.closed && !pr.merged {
        if state.blocked_heads.get(&number) == Some(&pr.head_sha) {
            return;
        }
        state.blocked_heads.remove(&number);
        if state.blocked_attrs.get(&number) == Some(&fingerprint(pr)) {
            return;
        }
        state.blocked_attrs.remove(&number);
        'rules: for rule in &rules.pull_request_rules {
            let Some(target) = &rule.queue else { continue };
            let (matches, _) = eval::eval_list(&rule.conditions, pr, &ctx, &mut errors);
            if !matches {
                continue;
            }
            for q in &rules.queue_rules {
                if target.as_ref().is_some_and(|t| *t != q.name) {
                    continue;
                }
                let (ok, _) = eval::eval_list(&q.queue_conditions, pr, &ctx, &mut errors);
                if ok {
                    let seq = state.next_id();
                    state.queued.insert(
                        number,
                        QueueEntry {
                            queue: q.name.clone(),
                            rule: rule.name.clone(),
                            since: now,
                            head_sha: pr.head_sha.clone(),
                            base: pr.base.clone(),
                            seq,
                            waiting_since: now,
                            status: EntryStatus::Waiting,
                        },
                    );
                    state
                        .trains
                        .entry(pr.base.clone())
                        .or_insert_with(|| Train::new(&pr.base));
                    out.push(Decision::Enqueue {
                        number,
                        queue: q.name.clone(),
                        rule: rule.name.clone(),
                    });
                    break 'rules;
                }
            }
        }
    }
    for e in errors {
        let message = e.to_string();
        if state.reported_errors.insert((number, message.clone())) {
            out.push(Decision::EvaluationError { number, message });
        }
    }
}

/// The attributes of a pull request that its own merge conditions can read, as one string.
pub(crate) fn fingerprint(pr: &PullRequest) -> String {
    format!(
        "{}|{}|{}|{}|{:?}|{:?}|{:?}",
        pr.head_sha, pr.base, pr.title, pr.draft, pr.labels, pr.files, pr.dependabot_update_types
    )
}

/// First seven characters of a SHA, for messages.
pub fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conclusion;

    const CONFIG: &str = "queue_rules:\n  - name: default\n    queue_conditions:\n      - base = main\n      - -draft\n      - -label = halt\n      - check-success = ci\n\
                          pull_request_rules:\n  - name: auto\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

    fn pr(n: u64, author: &str) -> PullRequest {
        PullRequest::new(n, author, "main", &format!("b{n}"), &format!("s{n}"))
    }

    fn green(n: u64, seq: u64) -> Event {
        Event::CheckRun {
            number: n,
            run: CheckRun {
                name: "ci".into(),
                app: None,
                head_sha: format!("s{n}"),
                conclusion: Some(Conclusion::Success),
                seq,
            },
        }
    }

    fn only_queue_decisions(d: Vec<Decision>) -> Vec<Decision> {
        d.into_iter()
            .filter(|d| matches!(d, Decision::Enqueue { .. } | Decision::Dequeue { .. }))
            .collect()
    }

    #[test]
    fn enqueue_then_dequeue_on_halt() {
        let rules = Rules::from_yaml(CONFIG).unwrap();
        let s = State::default();
        let (s, d) = step(
            &s,
            &rules,
            &Event::PullRequestOpened {
                pull_request: pr(1, "bot"),
            },
            0,
        );
        assert!(d.is_empty(), "no check yet");
        let (s, d) = step(&s, &rules, &green(1, 1), 10);
        assert_eq!(
            only_queue_decisions(d),
            vec![Decision::Enqueue {
                number: 1,
                queue: "default".into(),
                rule: "auto".into()
            }]
        );
        // Same event again: idempotent, no second entry (MQ-12).
        let (s, d) = step(&s, &rules, &green(1, 1), 11);
        assert!(only_queue_decisions(d).is_empty());
        let (s, d) = step(
            &s,
            &rules,
            &Event::Labeled {
                number: 1,
                label: "halt".into(),
            },
            20,
        );
        assert!(matches!(
            &only_queue_decisions(d)[..],
            [Decision::Dequeue { number: 1, .. }]
        ));
        assert!(s.queued.is_empty());
    }

    #[test]
    fn other_author_is_not_queued() {
        let rules = Rules::from_yaml(CONFIG).unwrap();
        let (s, _) = step(
            &State::default(),
            &rules,
            &Event::PullRequestOpened {
                pull_request: pr(2, "human"),
            },
            0,
        );
        let (s, d) = step(&s, &rules, &green(2, 1), 5);
        assert!(d.is_empty());
        assert!(s.queued.is_empty());
    }
}
