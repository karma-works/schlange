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

use schlange_config::rules::{MergeMethod, Rules};

pub use crate::control::PendingCommand;
pub use crate::cosmetics::Cosmetic;
use crate::eval::{self, Context, QueueAttrs};
pub use crate::message::CommitMessage;
use crate::model::{CheckRun, Comment, PullRequest, Time};
use crate::protections::Hold;
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
    /// Further commits this landing put on the base's first-parent history for the pull
    /// request, oldest first, before `sha`: the replayed commits of a `rebase`, the pull
    /// request's own commits of an in-place `fast-forward`. Their trees were not tested on
    /// their own (only `sha`'s).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub extra: Vec<String>,
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
    /// The forge refused the merge for good (missing permission, protection, validation):
    /// retrying the same head cannot succeed. `message` is the forge's reason with a hint.
    Refused { number: u64, message: String },
}

/// What the forge reports.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    PullRequestOpened {
        pull_request: Box<PullRequest>,
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
    /// Reply to `BuildCar`: the forge refuses pull request `number` for good (for example a
    /// workflow change without the permission to push it); nothing was created for the car.
    CarRefused {
        car: u64,
        number: u64,
        message: String,
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
    /// Comments on pull requests, new or edited: commands and ticked checkboxes. A comment the
    /// engine has handled before is ignored, so a reconciliation may hand over all of them.
    Comments {
        comments: Vec<Comment>,
    },
    /// The configuration on the default branch changed: every pull request is evaluated again
    /// under the new rules (MQ-19).
    ConfigurationChanged,
    /// Reply to `Act`: what the forge did with a rule action's request.
    ActionDone {
        id: u64,
        result: crate::actions::ActionResult,
    },
    /// An operator changed the queue's controls (pause, freeze) through the API or the CLI.
    Control {
        control: Control,
    },
    /// The checks the base branch's protection or rulesets require, as the forge read them
    /// (`branch_protection_injection_mode`).
    RequiredChecks {
        base: String,
        checks: Vec<String>,
    },
}

/// An operator's change of the queue controls.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    /// Pause the whole merge queue with a reason (docs: merge-queue/pause).
    Pause {
        reason: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        by: Option<String>,
    },
    Unpause,
    /// Create or replace a scheduled freeze.
    Freeze {
        freeze: Freeze,
    },
    /// Delete a freeze.
    Unfreeze {
        id: u64,
    },
}

/// The queue is paused: nothing is checked or merged; pull requests can still join.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Pause {
    pub reason: String,
    /// Engine time of the pause.
    pub at: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
}

/// A scheduled freeze (docs: merge-protections/freeze): from `start` until `end` (engine time;
/// `None`: until deleted), matching pull requests are not merged. Their checks still run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Freeze {
    pub id: u64,
    pub reason: String,
    pub start: Time,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end: Option<Time>,
    /// Conditions a pull request must match to be frozen (empty: every pull request).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub matching: Vec<String>,
    /// Conditions that let a pull request through when it matches all of them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude: Vec<String>,
}

impl Freeze {
    pub fn active(&self, now: Time) -> bool {
        now >= self.start && self.end.is_none_or(|e| now < e)
    }

    fn conds(list: &[String]) -> Vec<schlange_config::conditions::Condition> {
        let v = serde_json::Value::Array(
            list.iter()
                .map(|s| serde_json::Value::String(s.clone()))
                .collect(),
        );
        schlange_config::conditions::parse_list(&v, "", &mut Vec::new())
    }

    /// Whether this freeze holds the pull request now (active, matching, not excepted).
    pub fn holds(&self, pr: &PullRequest, ctx: &Context, now: Time) -> bool {
        if !self.active(now) {
            return false;
        }
        let m = Self::conds(&self.matching);
        if m.len() != self.matching.len() {
            return true; // an unreadable condition never lets anything through
        }
        if !eval::eval_list(&m, pr, ctx, &mut Vec::new()).0 {
            return false;
        }
        if self.exclude.is_empty() {
            return true;
        }
        let x = Self::conds(&self.exclude);
        !(x.len() == self.exclude.len() && eval::eval_list(&x, pr, ctx, &mut Vec::new()).0)
    }
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
        /// Build on this commit (the head of the car ahead) and merge only `own` into it:
        /// `fast-forward` and `merge-batch` land the batch branch itself, so each car's branch
        /// must contain the branch ahead. `prs` still lists every pull request in order (for
        /// conflict reports).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stack_on: Option<String>,
    },
    /// Check the pull request in place: no queue branch, no draft pull request.
    CheckInPlace {
        car: u64,
        number: u64,
        head_sha: String,
        /// Direct merge: the pull request's own CI already meets the merge conditions; it
        /// lands without a queue run (docs: merge-queue/direct-merge).
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        direct: bool,
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
        /// Squash commit messages the queue decided (`commit_message_format`); a pull request
        /// without an entry gets the forge's own message.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        messages: Vec<CommitMessage>,
        /// How to land (`merge_method`): `squash`, `merge` and `rebase` land each pull request at
        /// its head; `fast-forward` moves the base onto `head`; `merge-batch` merges `head`
        /// (the batch branch, draft `draft`) with one merge commit.
        #[serde(default)]
        method: MergeMethod,
        /// The car's tested head (the queue branch, or the pull request in place).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        draft: Option<u64>,
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
    /// Labels, comments, reactions and reported checks. Never read back as a condition and
    /// sent with the lowest priority, so it never delays landing.
    Cosmetic { cosmetic: Cosmetic },
    /// A rule action's request (`pull_request_rules` actions and the commands that run them).
    /// A side effect: journaled before it is sent; reply `ActionDone`.
    Act {
        id: u64,
        rule: String,
        op: crate::actions::Op,
    },
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
    /// The base moved outside the queue; clear after the next successful car build.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external_reset: bool,
    /// Who queued it with a command or checkbox; `None` when a rule queued it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub by: Option<String>,
    /// From `priority_rules` (the first rule that matches; `medium`, 2000, otherwise).
    #[serde(default = "default_priority")]
    pub priority: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority_rule: Option<String>,
    /// The matching priority rule allows interrupting running checks of lower priority.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub interrupt: bool,
}

fn default_priority() -> u32 {
    schlange_config::queue_extras::DEFAULT_PRIORITY
}

/// What the host tells the engine about where it runs.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Host {
    /// Unix time of the engine's time 0 (0 when the clock is Unix time, as on GitHub).
    #[serde(default)]
    pub epoch: Time,
    /// The queue app's login (`<slug>[bot]`), for the `merged-by` trailer.
    #[serde(default)]
    pub bot: String,
    /// `owner/name` of the repository.
    #[serde(default)]
    pub repository: String,
}

impl Default for Host {
    fn default() -> Self {
        Host {
            epoch: 0,
            bot: "schlange[bot]".into(),
            repository: String::new(),
        }
    }
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
    #[serde(default)]
    pub host: Host,
    /// Comments handled, by id and edit version.
    #[serde(default)]
    pub seen_comments: BTreeSet<(u64, String)>,
    /// `queue` commands waiting for the queue conditions.
    #[serde(default)]
    pub pending_commands: BTreeMap<u64, PendingCommand>,
    /// Dequeued (or a pending command cancelled) with `dequeue`: no rule queues it again until
    /// a `queue` command.
    #[serde(default)]
    pub manual_dequeued: BTreeSet<u64>,
    /// Why each open pull request that is not queued is held (protections, a waiting command,
    /// a manual dequeue), for `pr explain`.
    #[serde(default)]
    pub holds: BTreeMap<u64, Vec<Hold>>,
    /// The code of the last time a pull request's queue checks ended (`queue-dequeue-reason`).
    #[serde(default)]
    pub checks_ended: BTreeMap<u64, String>,
    /// What the queue last showed on pull requests (labels, comments, checks).
    #[serde(default)]
    pub cosmetics: crate::cosmetics::Applied,
    /// What the rule actions remember (see `actions`).
    #[serde(default)]
    pub actions: crate::actions::ActionsState,
    /// The merge queue is paused (docs: merge-queue/pause).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pause: Option<Pause>,
    /// Scheduled freezes by id.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub freezes: BTreeMap<u64, Freeze>,
    /// Checks the protection of each base branch requires (as last read).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub required_checks: BTreeMap<String, Vec<String>>,
    /// Why a queued pull request may not land right now (a freeze, a merge protection, the
    /// pause), for status output; recomputed on every step.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub landing_holds: BTreeMap<u64, String>,
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
    for d in &out {
        if let Decision::Dequeue { number, reason, .. }
        | Decision::ChecksEnded { number, reason, .. } = d
            && let Some(c) = reason.code
        {
            next.checks_ended.insert(*number, c.to_string());
        }
    }
    crate::cosmetics::update(&mut next, rules, &mut out);
    (next, out)
}

/// The evaluation context of a pull request: the queue attributes and the clock.
pub fn context(state: &State, number: u64) -> Context {
    let epoch = state.host.epoch as i64;
    let queue = state.queued.get(&number).map(|e| {
        let position = state
            .queued
            .values()
            .filter(|o| o.base == e.base && o.queue == e.queue && o.seq < e.seq)
            .count() as i64;
        let started = match e.status {
            EntryStatus::InCar { car } => state
                .trains
                .get(&e.base)
                .and_then(|t| t.cars.iter().find(|c| c.id == car))
                .map(|c| epoch + c.started as i64),
            _ => None,
        };
        QueueAttrs {
            name: e.queue.clone(),
            position,
            queued_at: epoch + e.since as i64,
            merge_started_at: started,
        }
    });
    Context {
        sender: None,
        sender_permission: None,
        now: epoch + state.now as i64,
        repository: state.host.repository.clone(),
        queue,
        dequeue_reason: state.checks_ended.get(&number).cloned(),
    }
}

fn apply(state: &mut State, rules: &Rules, event: &Event, out: &mut Vec<Decision>) -> Option<u64> {
    fn pr(state: &mut State, n: u64) -> Option<&mut PullRequest> {
        state.pull_requests.get_mut(&n)
    }
    match event {
        Event::PullRequestOpened { pull_request } => {
            state
                .pull_requests
                .insert(pull_request.number, (**pull_request).clone());
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
                match p
                    .checks
                    .iter_mut()
                    .find(|c| c.seq == run.seq && c.status == run.status)
                {
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
        } => queue::car_conflict(state, rules, *car, *number, *with_base, with, None, out),
        Event::CarRefused {
            car,
            number,
            message,
        } => queue::car_conflict(state, rules, *car, *number, true, &[], Some(message), out),
        Event::Landed {
            car,
            commits,
            error,
        } => queue::landed(state, rules, *car, commits, error.as_ref(), out),
        Event::Tick => {}
        Event::Failed {
            decision: Decision::Act { id, .. },
            error,
        } => crate::actions::failed(state, *id, error, out),
        Event::Failed { decision, .. } => queue::side_effect_failed(state, decision, out),
        Event::CarClosed { car } => queue::car_closed(state, *car),
        Event::Restarted => {
            queue::expire_all(state, out);
            crate::actions::restarted(state, out);
        }
        Event::ActionDone { id, result } => crate::actions::done(state, *id, result, out),
        Event::Comments { comments } => crate::control::comments(state, rules, comments, out),
        Event::Control { control } => crate::gates::apply(state, control, out),
        Event::RequiredChecks { base, checks } => {
            let mut c = checks.clone();
            c.sort();
            c.dedup();
            if c.is_empty() {
                state.required_checks.remove(base);
            } else {
                state.required_checks.insert(base.clone(), c);
            }
        }
        Event::ConfigurationChanged => {
            // Attributes and evaluation errors were judged under the old rules.
            state.blocked_attrs.clear();
            state.reported_errors.clear();
            state.actions.frozen_labels.clear();
            state.actions.label_flips.clear();
        }
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
    crate::gates::refresh_priorities(state, rules);
    queue::process_trains(state, rules, now, token, out);
    crate::actions::run(state, rules, token, out);
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

#[allow(clippy::too_many_arguments)]
fn enqueue(
    state: &mut State,
    rules: &Rules,
    number: u64,
    base: &str,
    head_sha: &str,
    queue: &str,
    rule: &str,
    by: Option<String>,
    now: Time,
    out: &mut Vec<Decision>,
) {
    let seq = state.next_id();
    state.queued.insert(
        number,
        QueueEntry {
            queue: queue.to_string(),
            rule: rule.to_string(),
            since: now,
            head_sha: head_sha.to_string(),
            base: base.to_string(),
            seq,
            waiting_since: now,
            status: EntryStatus::Waiting,
            external_reset: false,
            by,
            priority: default_priority(),
            priority_rule: None,
            interrupt: false,
        },
    );
    crate::gates::assign_priority(state, rules, number);
    state
        .trains
        .entry(base.to_string())
        .or_insert_with(|| Train::new(base));
    out.push(Decision::Enqueue {
        number,
        queue: queue.to_string(),
        rule: rule.to_string(),
    });
}

/// A pending `queue` command: cancelled by a foreign push or a close, entered once the queue
/// conditions hold and nothing holds the pull request.
fn pending_command(
    state: &mut State,
    rules: &Rules,
    pr: &PullRequest,
    holds: &mut Vec<Hold>,
    now: Time,
    out: &mut Vec<Decision>,
) {
    let number = pr.number;
    let Some(p) = state.pending_commands.get(&number).cloned() else {
        return;
    };
    let say = |out: &mut Vec<Decision>, body: String| {
        crate::cosmetics::emit(
            out,
            Cosmetic::Reply {
                number,
                to: p.comment,
                body,
            },
        );
    };
    if pr.closed || pr.merged {
        state.pending_commands.remove(&number);
        say(
            out,
            format!(
                "The queue command by @{} is cancelled: the pull request was closed.",
                p.by
            ),
        );
        return;
    }
    // A new head cancels the command, whoever pushed it: Schlange never pushes to pull
    // requests, and no commit metadata can prove who did (N2).
    if pr.head_sha != p.head_sha {
        state.pending_commands.remove(&number);
        let pusher = pr
            .head_pusher
            .as_ref()
            .map(|u| format!("@{u}"))
            .unwrap_or_else(|| "someone".into());
        say(
            out,
            format!(
                "The queue command by @{} is cancelled: {pusher} pushed new commits ({}) after \
                 it. The command vouched for the commits it saw; review the new ones and comment \
                 `@mergifyio queue` again.",
                p.by,
                short(&pr.head_sha)
            ),
        );
        return;
    }
    if let Some(t) = &p.queue
        && !rules.queue_rules.iter().any(|q| &q.name == t)
    {
        state.pending_commands.remove(&number);
        say(
            out,
            format!(
                "The queue command by @{} is cancelled: queue `{t}` no longer exists.",
                p.by
            ),
        );
        return;
    }
    if !holds.is_empty() {
        holds.push(Hold::wait(
            "queue-command",
            format!(
                "a queue command by @{} waits until the holds above clear",
                p.by
            ),
        ));
        return;
    }
    let ctx = context(state, number);
    let mut errors = Vec::new();
    let target = rules.queue_rules.iter().find(|q| {
        p.queue.as_ref().is_none_or(|t| *t == q.name)
            && eval::eval_list(
                &crate::gates::queue_conditions(state, q, &pr.base),
                pr,
                &ctx,
                &mut errors,
            )
            .0
    });
    match target {
        Some(q) => {
            let name = q.name.clone();
            state.pending_commands.remove(&number);
            enqueue(
                state,
                rules,
                number,
                &pr.base,
                &pr.head_sha,
                &name,
                &format!("queue command by @{}", p.by),
                Some(p.by.clone()),
                now,
                out,
            );
        }
        None => holds.push(Hold::wait(
            "queue-command",
            format!(
                "a queue command by @{} waits for the queue conditions of {}",
                p.by,
                p.queue
                    .as_ref()
                    .map(|q| format!("`{q}`"))
                    .unwrap_or_else(|| "any queue".into())
            ),
        )),
    }
    report_errors(state, number, errors, out);
}

fn report_errors(
    state: &mut State,
    number: u64,
    errors: Vec<eval::EvalError>,
    out: &mut Vec<Decision>,
) {
    for e in errors {
        let message = e.to_string();
        if state.reported_errors.insert((number, message.clone())) {
            out.push(Decision::EvaluationError { number, message });
        }
    }
}

fn evaluate_pull_request(
    state: &mut State,
    rules: &Rules,
    pr: &PullRequest,
    now: Time,
    out: &mut Vec<Decision>,
) {
    let ctx = context(state, pr.number);
    let mut errors = Vec::new();
    let number = pr.number;
    let open = !pr.closed && !pr.merged;
    let (mut holds, wake) = if open {
        crate::protections::protections(state, pr, now)
    } else {
        (vec![], None)
    };
    if let Some(at) = wake {
        queue::wake(state, at, out);
    }

    if let Some(entry) = state.queued.get(&number).cloned() {
        state.holds.remove(&number);
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
                    let conds = crate::gates::queue_conditions(state, q, &entry.base);
                    let (ok, results) = eval::eval_list(&conds, pr, &ctx, &mut errors);
                    if !ok {
                        // Sticky (observed on Mergify, sandbox s05 and s07): when the cause goes
                        // away (the label is removed, the draft is ready again) the pull request
                        // is not queued again at this head; a new head or a `queue` command
                        // lifts it (`dequeued-waits-for-new-head`).
                        state.blocked_heads.insert(number, pr.head_sha.clone());
                        Some(DequeueReason::assumed(
                            code::PR_DEQUEUED,
                            format!(
                                "queue_conditions no longer hold (condition {} is false)",
                                results.iter().position(|r| !r).map_or(0, |i| i + 1)
                            ),
                        ))
                    } else {
                        holds.first().map(|h| {
                            DequeueReason::assumed(
                                code::PR_DEQUEUED,
                                format!("a merge protection holds it ({}): {}", h.kind, h.text),
                            )
                        })
                    }
                }
            }
        };
        if let Some(reason) = reason {
            dequeue(state, rules, number, reason, out);
        }
    } else if open {
        if state.pending_commands.contains_key(&number) {
            pending_command(state, rules, pr, &mut holds, now, out);
            store_holds(state, number, holds);
            report_errors(state, number, errors, out);
            return;
        }
        if state.manual_dequeued.contains(&number) {
            holds.push(Hold::wait(
                "manually-dequeued",
                "dequeued with a command; it rejoins only with a new `queue` command",
            ));
            store_holds(state, number, holds);
            return;
        }
        if state.blocked_heads.get(&number) == Some(&pr.head_sha) {
            // Rule 10: the wait is a hold with a reason, never a silent skip.
            let why = match state.outcomes.get(&number) {
                Some(Outcome::Dequeued { reason }) => format!(
                    "{}{}",
                    reason.code.map(|c| format!("{c}: ")).unwrap_or_default(),
                    reason.text
                ),
                _ => "it left the queue at this head".into(),
            };
            holds.push(Hold::wait(
                "dequeued-waits-for-new-head",
                format!(
                    "dequeued ({why}); it rejoins with a new head (a push) or a `queue` command"
                ),
            ));
            store_holds(state, number, holds);
            return;
        }
        store_holds(state, number, holds.clone());
        if !holds.is_empty() {
            return;
        }
        state.blocked_heads.remove(&number);
        if state.blocked_attrs.get(&number) == Some(&fingerprint(rules, pr)) {
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
                let conds = crate::gates::queue_conditions(state, q, &pr.base);
                let (ok, _) = eval::eval_list(&conds, pr, &ctx, &mut errors);
                if ok {
                    enqueue(
                        state,
                        rules,
                        number,
                        &pr.base,
                        &pr.head_sha,
                        &q.name,
                        &rule.name,
                        None,
                        now,
                        out,
                    );
                    break 'rules;
                }
            }
        }
    } else {
        state.holds.remove(&number);
    }
    report_errors(state, number, errors, out);
}

fn store_holds(state: &mut State, number: u64, holds: Vec<Hold>) {
    if holds.is_empty() {
        state.holds.remove(&number);
    } else {
        state.holds.insert(number, holds);
    }
}

/// The attributes of a pull request that its own merge conditions can read, as one string.
/// The queue's own labels are left out: the queue changing them must not count as the pull
/// request changing.
pub(crate) fn fingerprint(rules: &Rules, pr: &PullRequest) -> String {
    let own = crate::cosmetics::queue_labels(rules);
    let labels: Vec<&String> = pr.labels.iter().filter(|l| !own.contains(*l)).collect();
    format!(
        "{}|{}|{}|{}|{:?}|{:?}|{:?}|{}",
        pr.head_sha,
        pr.base,
        pr.title,
        pr.draft,
        labels,
        pr.files,
        pr.dependabot_update_types,
        pr.body
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
                ..Default::default()
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
                pull_request: Box::new(pr(1, "bot")),
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
                pull_request: Box::new(pr(2, "human")),
            },
            0,
        );
        let (s, d) = step(&s, &rules, &green(2, 1), 5);
        assert!(only_queue_decisions(d.clone()).is_empty());
        assert!(s.queued.is_empty());
        // Ready for the queue, but no rule queues it: the checkbox comment, once.
        assert!(matches!(
            &d[..],
            [Decision::Cosmetic {
                cosmetic: Cosmetic::ControlsComment { number: 2, .. }
            }]
        ));
        let (_, d) = step(&s, &rules, &green(2, 1), 6);
        assert!(d.is_empty(), "{d:?}");
    }
}
