//! The queue of one base branch ("train"): serial mode with cumulative speculation.
//!
//! Car k tests the base plus the batches of cars 1..k. At most `max_parallel_checks` cars
//! exist at a time (I4), and a car that is being closed keeps its slot until the forge confirms
//! that its draft is closed and its CI stopped. Only the front car decides: it lands when
//! green, or its failure is handled when red; a red car behind a running one waits, because the
//! cause may lie ahead. When a car is discarded, every car behind it is discarded too (MQ-23).
//! A red batch of several pull requests is split as described in `bisect`.
//!
//! Every request to the forge can be lost, answered twice, answered late or answered with an
//! error although it took effect (MQ-35). So no state waits forever for a reply: a car being
//! built, verified, confirmed or landed, a car being closed, and the read of the base branch
//! are all retried or resolved from a fresh read after [`REPLY_TIMEOUT`]. A landing whose
//! outcome is unknown is resolved from the pull requests' `merged` state and the base tip; a
//! pull request that later shows up merged after an uncertain landing counts as landed by the
//! queue, and the compare-and-swap on the base means nothing lands twice.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use schlange_config::conditions::Condition;
use schlange_config::rules::{BatchSize, ChecksTimeout, QueueRule, Rules};

use crate::bisect;
use crate::engine::{
    CarPr, Closing, Decision, DequeueReason, EntryStatus, LandError, LandedCommit, Outcome, Sample,
    State, dequeue, fingerprint, short,
};
use crate::eval;
use crate::model::{CheckRun, Conclusion, PullRequest, Time};
use crate::reasons::{self as code, static_code};

/// How long the engine waits for a reply (or a fresh read) before it retries or resolves the
/// request from a read. Virtual or real seconds.
pub const REPLY_TIMEOUT: Time = 120;
/// `checks_timeout: auto`: qualifying runs needed before a timeout applies (docs: about twenty
/// in seven days).
pub const AUTO_MIN_RUNS: usize = 20;
/// `checks_timeout: auto`: the measurement window.
pub const AUTO_WINDOW: Time = 7 * 86400;
/// `checks_timeout: auto`: safety margin on the 95th percentile. The docs name a margin but not
/// its size; 1.5 is our choice.
pub const AUTO_MARGIN: f64 = 1.5;
/// `checks_timeout: auto`: half-life of a run's weight ("recent runs weighted more heavily";
/// the weighting is our choice).
pub const AUTO_HALF_LIFE: Time = 86400;
/// No checks timeout is shorter than this (docs: at least 60 seconds).
pub const MIN_TIMEOUT: Time = 60;
/// A verification read that answers later than this after it was sent is too old to land on
/// (the answer reflects the forge when the request was served): the engine reads again.
pub const MAX_READ_AGE: Time = 10;
/// A wait that is due at once (after a restart).
pub const EXPIRED: Time = Time::MAX;
const BASE_HISTORY: usize = 64;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CarStatus {
    /// `BuildCar` sent at `since`, no reply yet.
    Building {
        since: Time,
    },
    Running,
    Green,
    Failed {
        #[serde(deserialize_with = "static_code::deserialize")]
        code: crate::reasons::Code,
        text: String,
    },
    /// A fresh read was requested before landing.
    Verifying {
        token: u64,
        since: Time,
    },
    /// The checks looked red or timed out; a fresh read was requested before acting on it
    /// (a lost webhook must not dequeue anything, MQ-33).
    Confirming {
        token: u64,
        since: Time,
    },
    /// `Land` sent at `since`, no reply yet.
    Landing {
        since: Time,
    },
    /// The landing's outcome is unknown (lost reply, error, restart); a read was requested.
    LandCheck {
        token: u64,
        since: Time,
    },
    /// The checks failed and are retried (`max_checks_retries`): the car is rebuilt on a fresh
    /// draft as soon as a slot is free.
    Retry,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Car {
    pub id: u64,
    pub queue: String,
    pub branch: Option<String>,
    pub draft: Option<u64>,
    pub in_place: bool,
    pub base_sha: String,
    /// Every pull request in the car, those ahead first.
    pub prs: Vec<CarPr>,
    /// The pull requests this car checks.
    pub own: Vec<u64>,
    pub head_sha: Option<String>,
    pub tree: Option<String>,
    pub started: Time,
    pub status: CarStatus,
    /// Index of the split test when the car belongs to a bisection.
    pub test: Option<usize>,
    /// 0 for the first run, k for the k-th retry.
    #[serde(default)]
    pub attempt: u32,
    /// Its runtime went into the `checks_timeout: auto` history.
    #[serde(default)]
    pub measured: bool,
    /// The base moved outside the queue under it and the queue did not reset
    /// (`reset_on_external_merge: never`): what lands is the tested change on a newer base, not
    /// exactly the tested tree. (An accepted branch edit is not drift: with `squash` the
    /// edit would be dropped, and the tree check halts the queue.)
    #[serde(default)]
    pub drifted: bool,
    /// A retry (`max_checks_retries`): the id of the attempt it replaces. Until the forge
    /// confirms that attempt's close, the retry is not built, and the two share one slot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replaces: Option<u64>,
    /// The cars this car was built on top of (`parallel` mode: the last car ahead in each of
    /// its scopes; `isolated`: none). In `serial` mode every car depends on all cars ahead and
    /// the list is not used.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub deps: Vec<u64>,
    /// `fast-forward`, `merge-batch`: the head of the car ahead that this car's branch was
    /// built on (only its own pull requests were merged on top).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stacked_on: Option<String>,
    /// The scopes of the car's own pull requests (`scopes`), and whether one is a barrier.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scopes: Vec<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub barrier: bool,
}

/// `merge_queue.mode`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    #[default]
    Serial,
    Parallel,
    Isolated,
}

pub fn mode(rules: &Rules) -> Mode {
    match rules.merge_queue.mode.as_str() {
        "parallel" => Mode::Parallel,
        "isolated" => Mode::Isolated,
        _ => Mode::Serial,
    }
}

/// Whether car `i` waits for cars it was built on (they have not landed yet).
pub fn has_live_deps(train: &Train, i: usize) -> bool {
    match train.mode {
        Mode::Serial => i > 0,
        Mode::Isolated => false,
        Mode::Parallel => train.cars[i]
            .deps
            .iter()
            .any(|d| train.cars.iter().any(|c| c.id == *d)),
    }
}

/// The scopes of a pull request and whether it is a barrier.
pub fn pr_scopes(rules: &Rules, pr: Option<&PullRequest>) -> (Vec<String>, bool) {
    match (&rules.scopes, pr) {
        (Some(s), Some(p)) => s.of(&p.files),
        _ => (vec![], false),
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Train {
    pub base: String,
    pub base_sha: Option<String>,
    pub cars: Vec<Car>,
    pub halted: Option<String>,
    pub bisection: Option<bisect::Bisection>,
    /// An outstanding read of the base branch: token and time.
    #[serde(default)]
    pub base_request: Option<(u64, Time)>,
    /// Earlier base tips, newest last: a read that shows one of them is stale (MQ-33).
    #[serde(default)]
    pub history: Vec<String>,
    /// `merge_queue.mode`, set from the rules on every step.
    #[serde(skip)]
    pub mode: Mode,
}

impl Train {
    pub fn new(base: &str) -> Self {
        Train {
            base: base.to_string(),
            ..Default::default()
        }
    }

    /// Branches of the queue's cars that exist (or are being created) on the forge.
    pub fn branches(&self) -> Vec<String> {
        self.cars.iter().filter_map(|c| c.branch.clone()).collect()
    }

    fn set_base(&mut self, sha: String) {
        if let Some(old) = self.base_sha.replace(sha.clone())
            && old != sha
        {
            self.history.retain(|h| *h != old && *h != sha);
            self.history.push(old);
            if self.history.len() > BASE_HISTORY {
                self.history.remove(0);
            }
        }
    }
}

/// `reset_on_external_merge: never`, unless a queue lands its batch branch itself
/// (`fast-forward`, `merge-batch`): a base that moved outside the queue can never be
/// fast-forwarded onto a tested branch, and a stacked branch was built on the car ahead, not on
/// the base, so there is no tested change to carry over. Such a queue resets (our choice; the
/// docs do not say).
fn never_reset(rules: &Rules) -> bool {
    rules.merge_queue.reset_on_external_merge == "never"
        && !rules.queue_rules.iter().any(|q| q.method().lands_branch())
}

pub(crate) fn rule<'a>(rules: &'a Rules, name: &str) -> Option<&'a QueueRule> {
    rules.queue_rules.iter().find(|q| q.name == name)
}

pub(crate) fn wake(state: &mut State, at: Time, out: &mut Vec<Decision>) {
    if at != EXPIRED && state.wakeups.insert(at) {
        out.push(Decision::WakeAt { at });
    }
}

fn due(state: &State, since: Time) -> bool {
    since == EXPIRED || state.now >= since.saturating_add(REPLY_TIMEOUT)
}

pub fn is_car_draft(state: &State, number: u64) -> bool {
    state.trains.values().any(|t| {
        t.cars
            .iter()
            .any(|c| !c.in_place && c.draft == Some(number))
    }) || state.closing.values().any(|c| c.draft == Some(number))
}

/// Deterministic queue branch name: the prefix and ten hex characters.
pub fn branch_name(prefix: &str, base: &str, car: u64) -> String {
    let digest = Sha256::digest(format!("schlange/{base}/{car}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{}", &hex[..10])
}

/// The strict name pattern of a queue branch: a prefix and exactly ten lowercase hex
/// characters (docs: queue branch cleanup). Only such branches are ever cleaned up (MQ-36).
pub fn is_queue_branch(name: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|p| {
        name.strip_prefix(p.as_str()).is_some_and(|rest| {
            rest.len() == 10 && rest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
        })
    })
}

/// Cars being closed that still hold a slot of `base`. The close of an attempt that a pending
/// retry replaces is the retry's slot (the retry is built only once that close is confirmed), so
/// it is counted once, as the retry car.
pub fn closing_count(state: &State, base: &str) -> usize {
    let replaced: Vec<u64> = state
        .trains
        .get(base)
        .map(|t| {
            t.cars
                .iter()
                .filter(|c| c.status == CarStatus::Retry)
                .filter_map(|c| c.replaces)
                .collect()
        })
        .unwrap_or_default();
    state
        .closing
        .iter()
        .filter(|(id, c)| c.base.as_deref() == Some(base) && !replaced.contains(id))
        .count()
}

/// Whether a pending retry still waits for the close of the attempt it replaces.
pub fn retry_waits_for_close(state: &State, car: &Car) -> bool {
    car.replaces
        .is_some_and(|old| state.closing.contains_key(&old))
}

/// Closes a car's draft and deletes its branch. The slot stays taken until the forge
/// confirms (`CarClosed`) or the close is retried.
pub(crate) fn close(state: &mut State, base: Option<&str>, car: &Car, out: &mut Vec<Decision>) {
    // In place there is nothing to close; a retry not built yet has nothing on the forge (the
    // attempt it replaces is being closed under its own id).
    if car.in_place || car.status == CarStatus::Retry {
        return;
    }
    state.closing.insert(
        car.id,
        Closing {
            base: base.map(str::to_string),
            branch: car.branch.clone(),
            draft: car.draft,
            since: state.now,
        },
    );
    out.push(Decision::CloseCar {
        car: car.id,
        branch: car.branch.clone(),
        draft: car.draft,
    });
}

/// Puts a queued pull request back to waiting, keeping its place, and says why its checks ended.
pub(crate) fn requeue(
    state: &mut State,
    number: u64,
    reason: Option<DequeueReason>,
    out: &mut Vec<Decision>,
) {
    if let Some(entry) = state.queued.get_mut(&number) {
        entry.status = EntryStatus::Waiting;
        if let Some(reason) = reason {
            out.push(Decision::ChecksEnded {
                number,
                queue: entry.queue.clone(),
                reason,
            });
        }
    }
}

/// Removes the cars from index `from` on and puts their pull requests back to waiting.
/// The first removed car's own pull requests get `first`, the ones behind get `behind`.
pub(crate) fn drop_cars_from(
    state: &mut State,
    base: &str,
    from: usize,
    first: Option<DequeueReason>,
    behind: Option<DequeueReason>,
    skip: &[u64],
    out: &mut Vec<Decision>,
) {
    let Some(train) = state.trains.get(base) else {
        return;
    };
    if from >= train.cars.len() {
        return;
    }
    let mut idx = vec![from];
    idx.extend(dependents_of(state, base, from));
    drop_cars(state, base, &idx, first, behind, skip, out);
}

/// The dependents of car `i` of `base` under the configured mode (stored with the train).
pub(crate) fn dependents_of(state: &State, base: &str, i: usize) -> Vec<usize> {
    dependents(&state.trains[base], i)
}

/// Indices of the cars that depend on car `i`, directly or through others, in order. Serial:
/// every car behind it; isolated: none; parallel: the cars whose dependency chain reaches it.
pub fn dependents(train: &Train, i: usize) -> Vec<usize> {
    match train.mode {
        Mode::Serial => (i + 1..train.cars.len()).collect(),
        Mode::Isolated => vec![],
        Mode::Parallel => {
            let mut ids = vec![train.cars[i].id];
            let mut out = Vec::new();
            for (k, c) in train.cars.iter().enumerate().skip(i + 1) {
                if c.deps.iter().any(|d| ids.contains(d)) {
                    ids.push(c.id);
                    out.push(k);
                }
            }
            out
        }
    }
}

/// A car replaced by its retry or rebuild under a new id: the cars built on it (parallel mode:
/// `deps`) are built on the same slot, so they keep waiting for it. Without this they would see
/// no live dependency and land before it (and, for a method that lands the batch branch, with
/// its content).
fn rename_dep(train: &mut Train, old: u64, new: u64) {
    for c in &mut train.cars {
        for d in &mut c.deps {
            if *d == old {
                *d = new;
            }
        }
    }
}

/// Removes the cars at `idx` (the first gets `first`, the others `behind`) and puts their own
/// pull requests back to waiting.
pub(crate) fn drop_cars(
    state: &mut State,
    base: &str,
    idx: &[usize],
    first: Option<DequeueReason>,
    behind: Option<DequeueReason>,
    skip: &[u64],
    out: &mut Vec<Decision>,
) {
    let Some(train) = state.trains.get_mut(base) else {
        return;
    };
    let mut sorted: Vec<usize> = idx
        .iter()
        .copied()
        .filter(|i| *i < train.cars.len())
        .collect();
    sorted.sort();
    sorted.dedup();
    let lead = idx.first().copied();
    let mut removed: Vec<(bool, Car)> = Vec::new();
    for &i in sorted.iter().rev() {
        removed.push((Some(i) == lead, train.cars.remove(i)));
    }
    removed.reverse();
    for (is_first, car) in removed.iter() {
        let i = if *is_first { 0 } else { 1 };
        close(state, Some(base), car, out);
        let reason = if i == 0 { &first } else { &behind };
        for &n in &car.own {
            if !skip.contains(&n) {
                requeue(state, n, reason.clone(), out);
            }
        }
    }
}

/// Discards every car (and a bisection) that contains pull request `number`.
pub(crate) fn discard_containing(
    state: &mut State,
    _rules: &Rules,
    base: &str,
    number: u64,
    out: &mut Vec<Decision>,
) {
    let Some(train) = state.trains.get(base) else {
        return;
    };
    if train
        .bisection
        .as_ref()
        .is_some_and(|b| b.batch.contains(&number))
    {
        bisect::abort(
            state,
            base,
            Some(DequeueReason::new(
                code::BATCH_REBUILT,
                format!("#{number} left the batch that was being split"),
            )),
            &[number],
            out,
        );
        return;
    }
    let Some(idx) = train
        .cars
        .iter()
        .position(|c| c.prs.iter().any(|p| p.number == number))
    else {
        return;
    };
    let car = &train.cars[idx];
    // The batch is rebuilt without the pull request when others stay in it; the cars behind
    // were tested on top of the old batch. When the pull request is not one of the first
    // removed car's own (it was ahead of it: in an earlier car, or landing), that car lost a
    // pull request ahead.
    let own = car.own.contains(&number);
    let rebuilt = own && car.own.len() > 1;
    let ahead = DequeueReason::new(
        code::PR_AHEAD_DEQUEUED,
        format!("#{number} ahead in the queue left it"),
    );
    let behind = if rebuilt {
        DequeueReason::new(
            code::BATCH_AHEAD_REBUILT,
            format!("the batch ahead was rebuilt without #{number}"),
        )
    } else {
        ahead.clone()
    };
    let first = if own {
        DequeueReason::new(code::BATCH_REBUILT, format!("#{number} left this batch"))
    } else {
        ahead
    };
    drop_cars_from(state, base, idx, Some(first), Some(behind), &[number], out);
}

/// Discards every car and a bisection; every pull request waits again.
pub(crate) fn reset(state: &mut State, base: &str, reason: DequeueReason, out: &mut Vec<Decision>) {
    bisect::abort(state, base, Some(reason.clone()), &[], out);
    // Every car, also those that do not depend on the first (parallel and isolated mode).
    let all: Vec<usize> = (0..state.trains.get(base).map_or(0, |t| t.cars.len())).collect();
    drop_cars(
        state,
        base,
        &all,
        Some(reason.clone()),
        Some(reason),
        &[],
        out,
    );
}

fn find_car(state: &State, car: u64) -> Option<(String, usize)> {
    state.trains.iter().find_map(|(b, t)| {
        t.cars
            .iter()
            .position(|c| c.id == car)
            .map(|i| (b.clone(), i))
    })
}

pub(crate) fn car_built(
    state: &mut State,
    car: u64,
    branch: &str,
    head_sha: &str,
    tree: &str,
    draft: u64,
    out: &mut Vec<Decision>,
) {
    let now = state.now;
    let Some((base, i)) = find_car(state, car) else {
        // The car was discarded before the reply arrived (or the reply is a late duplicate):
        // remove what the forge created.
        let entry = state.closing.entry(car).or_insert(Closing {
            base: None,
            branch: None,
            draft: None,
            since: now,
        });
        entry.branch = Some(branch.to_string());
        entry.draft = Some(draft);
        entry.since = now;
        out.push(Decision::CloseCar {
            car,
            branch: Some(branch.to_string()),
            draft: Some(draft),
        });
        return;
    };
    let c = &mut state.trains.get_mut(&base).expect("found").cars[i];
    if !matches!(c.status, CarStatus::Building { .. }) {
        return; // a duplicated reply
    }
    c.branch = Some(branch.to_string());
    c.head_sha = Some(head_sha.to_string());
    c.tree = Some(tree.to_string());
    c.draft = Some(draft);
    c.started = now;
    c.status = CarStatus::Running;
    for number in &c.own {
        if let Some(entry) = state.queued.get_mut(number) {
            entry.external_reset = false;
        }
    }
    // The queue's own push to its branch is known now (not a foreign push).
    state
        .branches
        .insert(branch.to_string(), head_sha.to_string());
}

/// The forge confirmed that a car's draft is closed, its branch deleted and its CI stopped.
pub(crate) fn car_closed(state: &mut State, car: u64) {
    if let Some(c) = state.closing.remove(&car) {
        if let Some(b) = c.branch {
            state.branches.remove(&b);
        }
        if let Some(d) = c.draft
            && let Some(p) = state.pull_requests.get_mut(&d)
        {
            p.closed = true;
        }
    }
}

/// A car could not be built: `number` conflicts (with the base or with pull requests ahead),
/// or the forge refuses it for good (`refused`, the forge's reason).
#[allow(clippy::too_many_arguments)]
pub(crate) fn car_conflict(
    state: &mut State,
    rules: &Rules,
    car: u64,
    number: u64,
    with_base: bool,
    with: &[u64],
    refused: Option<&str>,
    out: &mut Vec<Decision>,
) {
    let Some((base, i)) = find_car(state, car) else {
        return;
    };
    let train = state.trains.get_mut(&base).expect("found");
    if !matches!(train.cars[i].status, CarStatus::Building { .. }) {
        return;
    }
    let removed: Vec<Car> = train.cars.drain(i..).collect();
    let in_bisection = removed.iter().any(|c| c.test.is_some());
    for c in &removed {
        // Nothing was created for the conflicting car itself; the cars behind it exist.
        if c.id != car {
            close(state, Some(&base), c, out);
        }
        for &n in &c.own {
            if n != number && !in_bisection {
                requeue(state, n, None, out);
            }
        }
    }
    if in_bisection {
        bisect::abort(state, &base, None, &[number], out);
    }
    if let Some(message) = refused {
        dequeue(
            state,
            rules,
            number,
            DequeueReason::new(code::PR_UNEXPECTEDLY_FAILED_TO_MERGE, message),
            out,
        );
    } else if with_base {
        let external_reset = state.queued.get(&number).is_some_and(|e| e.external_reset);
        if external_reset && let Some(pr) = state.pull_requests.get(&number) {
            state.blocked_heads.insert(number, pr.head_sha.clone());
        }
        dequeue(
            state,
            rules,
            number,
            DequeueReason::new(
                if external_reset {
                    code::PR_DEQUEUED
                } else {
                    code::CONFLICT_WITH_BASE_BRANCH
                },
                "the pull request does not merge onto the base branch",
            ),
            out,
        );
    } else {
        // The forge confirmed this pull request still merges onto the moved base by itself.
        // A later conflict after a queued predecessor lands has its own specific reason.
        if let Some(entry) = state.queued.get_mut(&number) {
            entry.external_reset = false;
        }
        discard_containing(state, rules, &base, number, out);
        if let Some(entry) = state.queued.get_mut(&number) {
            entry.status = EntryStatus::Held { on: with.to_vec() };
            out.push(Decision::ChecksEnded {
                number,
                queue: entry.queue.clone(),
                reason: DequeueReason::new(
                    code::CONFLICT_WITH_PULL_AHEAD,
                    format!(
                        "conflicts with {} ahead in the queue; held until that leaves or merges",
                        with.iter()
                            .map(|n| format!("#{n}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                ),
            });
        }
    }
}

fn landing_in_flight_car(car: &Car) -> bool {
    matches!(
        car.status,
        CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
    )
}

/// Whether a `Land` for the pull request is in flight (or its outcome is being read).
pub(crate) fn landing_in_flight(state: &State, number: u64) -> bool {
    state.trains.values().any(|t| {
        t.cars.iter().any(|c| {
            c.own.contains(&number)
                && matches!(
                    c.status,
                    CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
                )
        })
    })
}

/// A car whose pull request is about to change (a rule action sent a request that alters what
/// the conditions read) and that waits for the read before landing goes back to `Green`: the
/// answer to that read may predate the change, so it is read again after it.
pub(crate) fn invalidate_verification(state: &mut State, number: u64) {
    for t in state.trains.values_mut() {
        for c in &mut t.cars {
            if c.own.contains(&number) && matches!(c.status, CarStatus::Verifying { .. }) {
                c.status = CarStatus::Green;
            }
        }
    }
}

/// A queued pull request merged by the queue: it leaves with outcome `merged`.
pub(crate) fn record_merge(state: &mut State, number: u64, sha: &str, out: &mut Vec<Decision>) {
    state.unconfirmed_land.remove(&number);
    if let Some(pr) = state.pull_requests.get_mut(&number) {
        pr.merged = true;
        pr.closed = true;
        if pr.merge_commit_sha.is_none() && !sha.is_empty() {
            pr.merge_commit_sha = Some(sha.to_string());
        }
    }
    if let Some(entry) = state.queued.remove(&number) {
        state.outcomes.insert(
            number,
            Outcome::Merged {
                sha: sha.to_string(),
            },
        );
        out.push(Decision::Merged {
            number,
            queue: entry.queue,
            sha: sha.to_string(),
        });
    }
}

pub(crate) fn landed(
    state: &mut State,
    rules: &Rules,
    car: u64,
    commits: &[LandedCommit],
    error: Option<&LandError>,
    out: &mut Vec<Decision>,
) {
    let Some((base, i)) = find_car(state, car) else {
        // A late reply for a landing the engine already resolved otherwise: record what it
        // says landed.
        for c in commits {
            if state.queued.contains_key(&c.number) {
                let base = state.queued[&c.number].base.clone();
                discard_containing(state, rules, &base, c.number, out);
                record_merge(state, c.number, &c.sha, out);
            }
        }
        return;
    };
    let never = never_reset(rules);
    if never
        && commits.is_empty()
        && let Some(LandError::BaseMoved { actual }) = error
    {
        // A refusal for a landing that is no longer outstanding (a duplicated or late reply of
        // an earlier attempt: the car already moved to the new tip and may be landing again)
        // changes nothing; acting on it would forget the landing in flight.
        let car = &state.trains[&base].cars[i];
        let outstanding = matches!(
            car.status,
            CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
        );
        if !outstanding || state.trains[&base].base_sha.as_deref() == Some(actual.as_str()) {
            return;
        }
        // `reset_on_external_merge: never`: the queue stays as it is; land again on the new
        // tip (still a compare-and-swap).
        let actual = actual.clone();
        let train = state.trains.get_mut(&base).expect("found");
        train.history.retain(|h| *h != actual);
        train.set_base(actual.clone());
        for c in &mut train.cars {
            c.drifted = true;
        }
        train.cars[i].status = CarStatus::Green;
        state.branches.insert(base.clone(), actual);
        return;
    }
    let independent: Vec<u64> = {
        let t = &state.trains[&base];
        let deps = dependents(t, i);
        t.cars
            .iter()
            .enumerate()
            .filter(|(k, _)| *k != i && !deps.contains(k))
            .map(|(_, c)| c.id)
            .collect()
    };
    let train = state.trains.get_mut(&base).expect("found");
    let c = train.cars.remove(i);
    if !commits.is_empty() {
        // Parallel and isolated mode: a car that does not depend on the one that landed was
        // tested on a base without it; it lands as its tested change on the new base.
        for o in train
            .cars
            .iter_mut()
            .filter(|o| independent.contains(&o.id))
        {
            o.drifted = true;
        }
    }
    for commit in commits {
        if !commit.sha.is_empty() {
            train.set_base(commit.sha.clone());
        }
    }
    if let Some(sha) = commits
        .last()
        .map(|c| c.sha.clone())
        .filter(|s| !s.is_empty())
    {
        state.branches.insert(base.clone(), sha);
    }
    close(state, Some(&base), &c, out);
    for commit in commits {
        record_merge(state, commit.number, &commit.sha, out);
    }
    let landed: Vec<u64> = commits.iter().map(|c| c.number).collect();
    let rest: Vec<u64> = c
        .own
        .iter()
        .copied()
        .filter(|n| !landed.contains(n))
        .collect();
    let was_test = c.test.is_some();
    match error {
        None => {
            let landed_tree = commits
                .last()
                .map(|c| c.tree.clone())
                .filter(|t| !t.is_empty());
            if !c.drifted && landed_tree.is_some() && landed_tree != c.tree {
                // MQ-02: what landed is not what was tested. Stop everything.
                let reason = format!(
                    "landed tree {} differs from tested tree {} of car {}",
                    short(landed_tree.as_deref().unwrap_or("")),
                    short(c.tree.as_deref().unwrap_or("")),
                    c.id
                );
                halt(state, &base, reason, out);
                return;
            }
            if was_test {
                bisect::after_landing(state, rules, &base, out);
            }
        }
        Some(err) => {
            let reason = match err {
                LandError::BaseMoved { actual } => {
                    if let Some(t) = state.trains.get_mut(&base) {
                        // The compare-and-swap is authoritative, also over the stale-read guard.
                        t.history.retain(|h| h != actual);
                        t.set_base(actual.clone());
                    }
                    state.branches.insert(base.clone(), actual.clone());
                    DequeueReason::assumed(
                        code::MERGE_QUEUE_RESET,
                        format!("the base branch moved to {} while landing", short(actual)),
                    )
                }
                LandError::HeadChanged { number } => {
                    // After an uncertain landing the pull request may be closed because the
                    // queue merged it already; the next read tells.
                    if !state.unconfirmed_land.contains(number) {
                        dequeue(
                            state,
                            rules,
                            *number,
                            DequeueReason::new(
                                code::PULL_REQUEST_UPDATED,
                                "the head changed between the test and the merge",
                            ),
                            out,
                        );
                    }
                    DequeueReason::new(code::BATCH_REBUILT, format!("#{number} left this batch"))
                }
                LandError::Conflict { number } => {
                    // A car tested on exactly this base cannot conflict with it; after an
                    // external commit that `reset_on_external_merge: never` did not retest, the
                    // conflict is one with the base branch.
                    let reason = if c.drifted {
                        DequeueReason::new(
                            code::CONFLICT_WITH_BASE_BRANCH,
                            "the pull request no longer merges onto the base branch, which moved \
                             outside the queue (reset_on_external_merge: never)",
                        )
                    } else {
                        DequeueReason::new(
                            code::PR_UNEXPECTEDLY_FAILED_TO_MERGE,
                            "the squash merge did not apply",
                        )
                    };
                    dequeue(state, rules, *number, reason, out);
                    DequeueReason::new(code::BATCH_REBUILT, format!("#{number} left this batch"))
                }
                LandError::Refused { number, message } => {
                    dequeue(
                        state,
                        rules,
                        *number,
                        DequeueReason::new(code::PR_UNEXPECTEDLY_FAILED_TO_MERGE, message.clone()),
                        out,
                    );
                    DequeueReason::new(code::BATCH_REBUILT, format!("#{number} left this batch"))
                }
            };
            // The car's own pull requests that did not land wait again, with the reason; the
            // cars behind were discarded with them (`PR_AHEAD_DEQUEUED` when a pull request
            // ahead of them left).
            for n in rest {
                requeue(state, n, Some(reason.clone()), out);
            }
            reset(state, &base, reason, out);
        }
    }
}

fn halt(state: &mut State, base: &str, reason: String, out: &mut Vec<Decision>) {
    reset(
        state,
        base,
        DequeueReason::assumed(code::MERGE_QUEUE_RESET, "the queue halted"),
        out,
    );
    if let Some(t) = state.trains.get_mut(base) {
        t.halted = Some(reason.clone());
    }
    out.push(Decision::Halt {
        base_branch: base.to_string(),
        reason,
    });
}

/// Verdict over a running car.
pub(crate) enum Verdict {
    Green,
    Pending,
    Failed(&'static str, String),
    /// The checks are fine, but these pull requests' own attributes fail the merge conditions
    /// (a label, the title …). Not a CI failure: they leave with their own reason.
    Attributes(Vec<(u64, String)>),
}

/// Whether a condition depends on check runs (the queue draft's CI).
fn uses_checks(c: &Condition) -> bool {
    c.atoms().iter().any(|a| a.attribute.starts_with("check"))
}

/// The pull request as the merge conditions see it: its own attributes, the checks of the
/// car's head (the draft pull request, or the pull request itself in place).
pub(crate) fn car_view(state: &State, car: &Car, number: u64) -> Option<PullRequest> {
    let mut pr = state.pull_requests.get(&number)?.clone();
    let head = car.head_sha.clone()?;
    let draft = car.draft.and_then(|d| state.pull_requests.get(&d));
    pr.checks = draft.map(|d| d.checks.clone()).unwrap_or_default();
    pr.head_sha = head;
    Some(pr)
}

fn history_key(queue: &str, base: &str) -> String {
    format!("{queue}@{base}")
}

/// `checks_timeout: auto`: the weighted 95th percentile of the queue's recent successful first
/// runs on this base, times [`AUTO_MARGIN`], at least [`MIN_TIMEOUT`]; `None` until
/// [`AUTO_MIN_RUNS`] runs in [`AUTO_WINDOW`] exist (docs: merge-queue/lifecycle, checks timeout).
pub fn auto_timeout(state: &State, queue: &str, base: &str) -> Option<Time> {
    let samples = state.history.get(&history_key(queue, base))?;
    let now = state.now;
    let mut w: Vec<(Time, f64)> = samples
        .iter()
        .filter(|s| now.saturating_sub(s.at) <= AUTO_WINDOW)
        .map(|s| {
            let age = now.saturating_sub(s.at) as f64 / AUTO_HALF_LIFE as f64;
            (s.secs, 0.5f64.powf(age))
        })
        .collect();
    if w.len() < AUTO_MIN_RUNS {
        return None;
    }
    w.sort_by_key(|x| x.0);
    let total: f64 = w.iter().map(|x| x.1).sum();
    let mut acc = 0.0;
    let mut p95 = w.last().map(|x| x.0).unwrap_or(0);
    for (secs, weight) in &w {
        acc += weight;
        if acc >= 0.95 * total {
            p95 = *secs;
            break;
        }
    }
    Some(((p95 as f64 * AUTO_MARGIN).ceil() as Time).max(MIN_TIMEOUT))
}

/// The checks timeout of a car's queue on `base`, and whether it came from `auto`.
fn timeout_of(state: &State, rule: &QueueRule, base: &str) -> Option<(Time, bool)> {
    match rule.checks_timeout {
        ChecksTimeout::Fixed(t) => Some((t, false)),
        ChecksTimeout::Disabled => None,
        ChecksTimeout::Auto => auto_timeout(state, &rule.name, base).map(|t| (t, true)),
    }
}

/// Records a car's runtime for `checks_timeout: auto`. Only first attempts of normal cars count
/// (not retries, not bisection tests, not in-place checks whose CI started before the car).
fn record_run(state: &mut State, car: &Car, base: &str) {
    if car.test.is_some() || car.attempt > 0 || car.in_place || car.measured || car.drifted {
        return;
    }
    let now = state.now;
    let samples = state
        .history
        .entry(history_key(&car.queue, base))
        .or_default();
    samples.push(Sample {
        at: now,
        secs: now.saturating_sub(car.started),
    });
    samples.retain(|s| now.saturating_sub(s.at) <= AUTO_WINDOW);
}

pub(crate) fn verdict(state: &State, rules: &Rules, car: &Car, base: &str, now: Time) -> Verdict {
    let Some(rule) = rule(rules, &car.queue) else {
        return Verdict::Pending;
    };
    if car.head_sha.is_none() || car.tree.is_none() {
        return Verdict::Pending;
    }
    let conds = crate::gates::merge_conditions(state, rule, base);
    let check_conds: Vec<Condition> = conds.iter().filter(|c| uses_checks(c)).cloned().collect();
    let mut checks_ok = true;
    let mut attr_fail = Vec::new();
    let mut views = Vec::new();
    for &n in &car.own {
        let Some(view) = car_view(state, car, n) else {
            return Verdict::Pending;
        };
        let ctx = crate::engine::context(state, n);
        for (k, c) in conds.iter().enumerate() {
            if !uses_checks(c) && !eval::eval(c, &view, &ctx, &mut Vec::new()) {
                attr_fail.push((
                    n,
                    format!(
                        "the pull request itself no longer meets merge condition {}",
                        k + 1
                    ),
                ));
                break;
            }
        }
        let (ok, _) = eval::eval_list(&check_conds, &view, &ctx, &mut Vec::new());
        checks_ok &= ok;
        views.push(view);
    }
    if checks_ok {
        return if attr_fail.is_empty() {
            Verdict::Green
        } else {
            Verdict::Attributes(attr_fail)
        };
    }
    let runs: Vec<&CheckRun> = views
        .first()
        .map(|v| v.current_checks())
        .unwrap_or_default();
    let referenced = eval::referenced_checks(&check_conds);
    let relevant: Vec<&CheckRun> = if referenced.is_empty() {
        runs.clone()
    } else {
        runs.iter()
            .copied()
            .filter(|r| referenced.iter().any(|n| eval::check_matches(n, r)))
            .collect()
    };
    let missing: Vec<&str> = referenced
        .iter()
        .filter(|n| !runs.iter().any(|r| eval::check_matches(n, r)))
        .map(String::as_str)
        .collect();
    let failing = |c: Option<Conclusion>| {
        matches!(
            c,
            Some(
                Conclusion::Failure
                    | Conclusion::TimedOut
                    | Conclusion::ActionRequired
                    | Conclusion::Cancelled
            )
        )
    };
    let red: Vec<&str> = relevant
        .iter()
        .filter(|r| failing(r.conclusion))
        .map(|r| r.name.as_str())
        .collect();
    // Every check the conditions name has reported and they are still false (for example a
    // `neutral` where `success` is required).
    let decided = !relevant.is_empty()
        && missing.is_empty()
        && relevant.iter().all(|r| r.conclusion.is_some());
    let at = car.draft.map(|d| format!(" on #{d}")).unwrap_or_default();
    if !red.is_empty() || decided {
        let hard = relevant.iter().any(|r| {
            matches!(
                r.conclusion,
                Some(Conclusion::Failure | Conclusion::TimedOut | Conclusion::ActionRequired)
            )
        });
        let cancelled = relevant
            .iter()
            .any(|r| r.conclusion == Some(Conclusion::Cancelled));
        let mut names: Vec<String> = if red.is_empty() {
            relevant
                .iter()
                .filter(|r| r.conclusion != Some(Conclusion::Success))
                .map(|r| r.name.clone())
                .collect()
        } else {
            red.iter().map(|s| s.to_string()).collect()
        };
        if names.is_empty() {
            // Every check reported success and the check conditions are still false (a count,
            // a regex, a negation): name what was seen, never an empty list.
            names = relevant
                .iter()
                .map(|r| {
                    format!(
                        "{}={}",
                        r.name,
                        r.conclusion
                            .map(|c| format!("{c:?}").to_lowercase())
                            .unwrap_or_default()
                    )
                })
                .collect();
            names.insert(
                0,
                "no check failed, the check conditions are false with".into(),
            );
        }
        return if !hard && cancelled {
            Verdict::Failed(
                code::CHECKS_INTERRUPTED,
                format!(
                    "checks cancelled before a verdict{at}: {}",
                    names.join(", ")
                ),
            )
        } else {
            Verdict::Failed(
                code::CHECKS_FAILED,
                format!(
                    "merge conditions are false{at}; failed: {}",
                    names.join(", ")
                ),
            )
        };
    }
    if let Some((t, auto)) = timeout_of(state, rule, base)
        && now >= car.started + t
    {
        let pending: Vec<&str> = runs
            .iter()
            .filter(|r| r.conclusion.is_none())
            .map(|r| r.name.as_str())
            .collect();
        let how = if auto {
            format!(
                "Schlange gave the checks {t}s, a limit it estimates from how long CI took on \
                 this queue lately, and they were not green by then (a duration in \
                 `checks_timeout` sets the limit, null removes it)"
            )
        } else {
            format!("checks did not pass within {t}s")
        };
        return Verdict::Failed(
            code::CHECKS_TIMEOUT,
            format!(
                "{how}; still running: [{}]; never reported: [{}]",
                pending.join(", "),
                missing.join(", ")
            ),
        );
    }
    Verdict::Pending
}

/// The reads a car needs before acting.
fn refresh(state: &mut State, car: &Car, branches: Vec<String>, out: &mut Vec<Decision>) -> u64 {
    let token = state.next_id();
    let mut numbers = car.own.clone();
    numbers.extend(car.draft.filter(|_| !car.in_place));
    out.push(Decision::Refresh {
        token,
        numbers,
        branches,
    });
    wake(state, state.now + REPLY_TIMEOUT, out);
    token
}

/// Takes pull requests out of the queue whose own attributes fail the merge conditions; the
/// `queue` action does not queue them again until they change.
fn dequeue_for_attributes(
    state: &mut State,
    rules: &Rules,
    list: Vec<(u64, String)>,
    out: &mut Vec<Decision>,
) {
    for (n, text) in list {
        if let Some(pr) = state.pull_requests.get(&n) {
            let fp = fingerprint(rules, pr);
            state.blocked_attrs.insert(n, fp);
        }
        dequeue(
            state,
            rules,
            n,
            DequeueReason::assumed(code::PR_DEQUEUED, text),
            out,
        );
    }
}

/// Re-evaluates running cars. A red or timed-out verdict is only believed after a fresh read.
/// Returns whether the train changed shape.
fn evaluate_cars(
    state: &mut State,
    rules: &Rules,
    base: &str,
    now: Time,
    token: Option<u64>,
    out: &mut Vec<Decision>,
) -> bool {
    // An accepted branch edit: the tree of the edited head, once the draft read shows it.
    for i in 0..state.trains[base].cars.len() {
        let car = &state.trains[base].cars[i];
        if car.tree.is_none()
            && let (Some(d), Some(h)) = (car.draft, car.head_sha.clone())
            && let Some(pr) = state.pull_requests.get(&d)
            && pr.head_sha == h
        {
            let tree = pr.head_tree.clone();
            state.trains.get_mut(base).expect("train").cars[i].tree = tree;
        }
    }
    let cars = state.trains[base].cars.clone();
    for (i, car) in cars.iter().enumerate() {
        let confirmed = match car.status {
            CarStatus::Running => false,
            CarStatus::Confirming { token: t, .. } if token == Some(t) => true,
            _ => continue,
        };
        let status = match verdict(state, rules, car, base, now) {
            Verdict::Green => {
                record_run(state, car, base);
                state.trains.get_mut(base).expect("train").cars[i].measured = true;
                CarStatus::Green
            }
            Verdict::Failed(code, text) if confirmed => CarStatus::Failed { code, text },
            Verdict::Attributes(list) if confirmed => {
                dequeue_for_attributes(state, rules, list, out);
                return true;
            }
            Verdict::Failed(..) | Verdict::Attributes(_) => {
                let branches = car.branch.iter().cloned().collect();
                let token = refresh(state, car, branches, out);
                CarStatus::Confirming { token, since: now }
            }
            Verdict::Pending if confirmed => CarStatus::Running,
            Verdict::Pending => {
                if let Some((t, _)) =
                    rule(rules, &car.queue).and_then(|r| timeout_of(state, r, base))
                {
                    wake(state, car.started + t, out);
                }
                continue;
            }
        };
        state.trains.get_mut(base).expect("train").cars[i].status = status;
    }
    false
}

/// Removes a car whose build is in doubt (error, lost reply, restart) and cleans up whatever
/// the forge may have created; its pull requests get a new car.
fn abandon_car(state: &mut State, base: &str, id: u64, out: &mut Vec<Decision>) {
    let Some(i) = state.trains[base].cars.iter().position(|c| c.id == id) else {
        return;
    };
    let car = state.trains[base].cars[i].clone();
    if car.test.is_some() || car.attempt > 0 {
        // A bisection test or a retry: the same attempt is built again on a fresh branch once
        // the forge confirmed the close, so the retry count does not start over (a check that
        // always times out would otherwise be retried forever, I2).
        close(state, Some(base), &car, out);
        let id = state.next_id();
        let prefix = branch_prefix(car.branch.as_deref());
        let rebuilt = Car {
            id,
            branch: Some(branch_name(&prefix, base, id)),
            draft: None,
            head_sha: None,
            tree: None,
            started: state.now,
            status: CarStatus::Retry,
            measured: false,
            replaces: Some(car.id),
            ..car.clone()
        };
        let train = state.trains.get_mut(base).expect("train");
        train.cars[i] = rebuilt;
        rename_dep(train, car.id, id);
        if let Some(b) = train.bisection.as_mut()
            && b.land == Some(car.id)
        {
            b.land = Some(id);
        }
        for &n in &car.own {
            if let Some(e) = state.queued.get_mut(&n)
                && e.status == (EntryStatus::InCar { car: car.id })
            {
                e.status = EntryStatus::InCar { car: id };
            }
        }
    } else {
        drop_cars_from(state, base, i, None, None, &[], out);
    }
}

/// The prefix of a queue branch name (prefix plus ten hex characters); the default prefix
/// for a car without a branch.
fn branch_prefix(branch: Option<&str>) -> String {
    branch
        .and_then(|b| b.len().checked_sub(10).and_then(|k| b.get(..k)))
        .map_or_else(|| "mergify/merge-queue/".into(), str::to_string)
}

/// The landing's outcome is unknown: read the pull requests and the base.
fn land_unknown(state: &mut State, base: &str, i: usize, out: &mut Vec<Decision>) {
    let car = state.trains[base].cars[i].clone();
    for &n in &car.own {
        state.unconfirmed_land.insert(n);
    }
    let token = refresh(state, &car, vec![base.to_string()], out);
    state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::LandCheck {
        token,
        since: state.now,
    };
}

/// Retries or resolves every request of `base` that waited too long for its reply.
fn timeouts(state: &mut State, base: &str, out: &mut Vec<Decision>) {
    let cars = state.trains[base].cars.clone();
    for car in cars {
        let Some(i) = state.trains[base].cars.iter().position(|c| c.id == car.id) else {
            continue;
        };
        let since = match car.status {
            CarStatus::Building { since }
            | CarStatus::Verifying { since, .. }
            | CarStatus::Confirming { since, .. }
            | CarStatus::Landing { since }
            | CarStatus::LandCheck { since, .. } => since,
            _ => continue,
        };
        if !due(state, since) {
            wake(state, since + REPLY_TIMEOUT, out);
            continue;
        }
        let now = state.now;
        match car.status {
            CarStatus::Building { .. } => abandon_car(state, base, car.id, out),
            CarStatus::Landing { .. } => land_unknown(state, base, i, out),
            CarStatus::LandCheck { .. } => land_unknown(state, base, i, out),
            CarStatus::Verifying { .. } => {
                let mut branches = vec![base.to_string()];
                branches.extend(car.branch.clone());
                let token = refresh(state, &car, branches, out);
                state.trains.get_mut(base).expect("train").cars[i].status =
                    CarStatus::Verifying { token, since: now };
            }
            CarStatus::Confirming { .. } => {
                let branches = car.branch.iter().cloned().collect();
                let token = refresh(state, &car, branches, out);
                state.trains.get_mut(base).expect("train").cars[i].status =
                    CarStatus::Confirming { token, since: now };
            }
            _ => {}
        }
    }
}

/// Retries closes the forge did not confirm.
fn closing_timeouts(state: &mut State, out: &mut Vec<Decision>) {
    let ids: Vec<u64> = state.closing.keys().copied().collect();
    for id in ids {
        let c = state.closing[&id].clone();
        if !due(state, c.since) {
            wake(state, c.since + REPLY_TIMEOUT, out);
            continue;
        }
        state.closing.get_mut(&id).expect("closing").since = state.now;
        wake(state, state.now + REPLY_TIMEOUT, out);
        out.push(Decision::CloseCar {
            car: id,
            branch: c.branch,
            draft: c.draft,
        });
    }
}

/// Deletes queue branches (strict name pattern) and closes queue drafts that no car uses:
/// leftovers of lost replies, errors or a crash (docs: queue branch cleanup; MQ-36).
fn collect_garbage(state: &mut State, rules: &Rules, out: &mut Vec<Decision>) {
    let prefixes: Vec<String> = rules
        .queue_rules
        .iter()
        .map(|q| q.queue_branch_prefix.clone())
        .collect();
    let mut known: Vec<String> = state.trains.values().flat_map(|t| t.branches()).collect();
    known.extend(state.closing.values().filter_map(|c| c.branch.clone()));
    let known_drafts: Vec<u64> = state
        .trains
        .values()
        .flat_map(|t| t.cars.iter().filter_map(|c| c.draft))
        .chain(state.closing.values().filter_map(|c| c.draft))
        .collect();
    let mut leftovers: Vec<String> = state
        .branches
        .keys()
        .filter(|b| is_queue_branch(b, &prefixes) && !known.contains(b))
        .cloned()
        .collect();
    for p in state.pull_requests.values() {
        if !p.closed
            && is_queue_branch(&p.head_ref, &prefixes)
            && !known.contains(&p.head_ref)
            && !known_drafts.contains(&p.number)
            && !leftovers.contains(&p.head_ref)
        {
            leftovers.push(p.head_ref.clone());
        }
    }
    for branch in leftovers {
        let draft = state
            .pull_requests
            .values()
            .find(|p| p.head_ref == branch && !p.closed)
            .map(|p| p.number);
        let id = state.next_id();
        // No slot: a leftover is either a phantom of a lagging read (already gone) or was
        // forgotten; its CI is counted on the forge, not as one of the engine's cars.
        state.closing.insert(
            id,
            Closing {
                base: None,
                branch: Some(branch.clone()),
                draft,
                since: state.now,
            },
        );
        out.push(Decision::CloseCar {
            car: id,
            branch: Some(branch),
            draft,
        });
    }
}

/// The checks of the front car failed and `max_checks_retries` allows another attempt: the car
/// is rebuilt on a fresh draft (docs: automatic CI retries). The cars behind keep running: the
/// retried batch is the same pull requests at the same heads.
fn retry(
    state: &mut State,
    rules: &Rules,
    base: &str,
    i: usize,
    text: &str,
    out: &mut Vec<Decision>,
) {
    let old = state.trains[base].cars[i].clone();
    let max = rule(rules, &old.queue).map_or(0, |r| r.max_checks_retries);
    let prefix = rule(rules, &old.queue)
        .map(|r| r.queue_branch_prefix.clone())
        .unwrap_or_else(|| "mergify/merge-queue/".into());
    close(state, Some(base), &old, out);
    let id = state.next_id();
    let car = Car {
        id,
        branch: Some(branch_name(&prefix, base, id)),
        draft: None,
        in_place: false,
        head_sha: None,
        tree: None,
        started: state.now,
        status: CarStatus::Retry,
        attempt: old.attempt + 1,
        measured: false,
        replaces: Some(old.id),
        ..old.clone()
    };
    let train = state.trains.get_mut(base).expect("train");
    train.cars[i] = car;
    rename_dep(train, old.id, id);
    if let Some(b) = train.bisection.as_mut()
        && b.land == Some(old.id)
    {
        b.land = Some(id);
    }
    if rule(rules, &old.queue).is_some_and(|r| r.method().lands_branch()) {
        // The cars behind were built on the old attempt's branch, which will never land.
        let behind = dependents_of(state, base, i);
        let reason = DequeueReason::new(
            code::BATCH_AHEAD_REBUILT,
            "the batch ahead is checked again on a new branch",
        );
        drop_cars(
            state,
            base,
            &behind,
            Some(reason.clone()),
            Some(reason),
            &[],
            out,
        );
    }
    for &n in &old.own {
        if let Some(e) = state.queued.get_mut(&n) {
            if e.status == (EntryStatus::InCar { car: old.id }) {
                e.status = EntryStatus::InCar { car: id };
            }
            out.push(Decision::ChecksEnded {
                number: n,
                queue: e.queue.clone(),
                reason: DequeueReason::new(
                    code::CHECKS_RETRIED,
                    format!("retry {} of {max}: {text}", old.attempt + 1),
                ),
            });
        }
    }
}

/// A red split test of a bisection is retried like any car while `max_checks_retries` allows
/// (docs: batches, a split's `previous_check_retries`). Returns whether a test was retried.
fn retry_split_tests(
    state: &mut State,
    rules: &Rules,
    base: &str,
    out: &mut Vec<Decision>,
) -> bool {
    let mut changed = false;
    loop {
        let train = &state.trains[base];
        let failed = train.cars.iter().position(|c| {
            c.test.is_some()
                && matches!(c.status, CarStatus::Failed { .. })
                && rule(rules, &c.queue).is_some_and(|r| c.attempt < r.max_checks_retries)
        });
        let Some(i) = failed else { break };
        let CarStatus::Failed { text, .. } = train.cars[i].status.clone() else {
            unreachable!("matched above")
        };
        retry(state, rules, base, i, &text, out);
        changed = true;
    }
    changed
}

/// Whether a landing of `base` is in flight (being verified, sent, or resolved).
fn landing_busy(train: &Train, except: u64) -> bool {
    train.cars.iter().any(|c| {
        c.id != except
            && matches!(
                c.status,
                CarStatus::Verifying { .. }
                    | CarStatus::Landing { .. }
                    | CarStatus::LandCheck { .. }
            )
    })
}

/// Why car `car` may not land now (the pause, a freeze, a merge protection), recorded per pull
/// request for status output; wakes the engine when the hold can end by itself.
fn car_hold(
    state: &mut State,
    rules: &Rules,
    car: &Car,
    now: Time,
    out: &mut Vec<Decision>,
) -> bool {
    let mut held = false;
    for &n in &car.own {
        let Some(pr) = state.pull_requests.get(&n).cloned() else {
            continue;
        };
        if let Some((text, wake_at)) = crate::gates::landing_hold(state, rules, &pr, now) {
            held = true;
            state.landing_holds.insert(n, text);
            if let Some(at) = wake_at {
                wake(state, at, out);
            }
        }
    }
    held
}

/// The car that decides next: serial mode, the front car; otherwise the first car that waits
/// for no car it was built on and has something to decide. Only one landing is in flight per
/// base. During a bisection only its landing test lands.
fn deciding_car(
    state: &mut State,
    rules: &Rules,
    base: &str,
    now: Time,
    out: &mut Vec<Decision>,
) -> Option<usize> {
    let train = state.trains[base].clone();
    for (i, c) in train.cars.iter().enumerate() {
        if has_live_deps(&train, i) {
            if train.mode == Mode::Serial {
                return None;
            }
            continue;
        }
        // A landing in flight is resolved whatever a bisection plans: its reply (or the read
        // after it) is the only thing that tells whether the base moved.
        if let Some(b) = &train.bisection
            && b.land != Some(c.id)
            && !landing_in_flight_car(c)
        {
            if train.mode == Mode::Serial {
                return None;
            }
            continue;
        }
        let decides = match c.status {
            CarStatus::Green => !landing_busy(&train, c.id) && !car_hold(state, rules, c, now, out),
            CarStatus::Verifying { .. } | CarStatus::LandCheck { .. } => true,
            CarStatus::Failed { .. } => c.test.is_none(),
            _ => false,
        };
        if decides || train.mode == Mode::Serial {
            return Some(i);
        }
    }
    None
}

/// Handles the deciding car; returns whether the train changed.
fn front(
    state: &mut State,
    rules: &Rules,
    base: &str,
    now: Time,
    token: Option<u64>,
    out: &mut Vec<Decision>,
) -> bool {
    let Some(i) = deciding_car(state, rules, base, now, out) else {
        return false;
    };
    let train = &state.trains[base];
    let car = train.cars[i].clone();
    if let Some(b) = &train.bisection
        && b.land != Some(car.id)
        && !landing_in_flight_car(&car)
    {
        return false;
    }
    let paused = state.pause.is_some();
    match &car.status {
        CarStatus::Green if paused || landing_busy(train, car.id) => false,
        CarStatus::Green if car_hold(state, rules, &car, now, out) => false,
        CarStatus::Verifying { .. } if paused => {
            state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::Green;
            false
        }
        CarStatus::Green => {
            let mut branches = vec![base.to_string()];
            branches.extend(car.branch.clone());
            let t = refresh(state, &car, branches, out);
            state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::Verifying {
                token: t,
                since: now,
            };
            false
        }
        CarStatus::Verifying { token: t, since }
            if token == Some(*t) && now > since + MAX_READ_AGE =>
        {
            // A late answer: land only on a recent read (I1).
            let mut branches = vec![base.to_string()];
            branches.extend(car.branch.clone());
            let t = refresh(state, &car, branches, out);
            state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::Verifying {
                token: t,
                since: now,
            };
            false
        }
        CarStatus::Verifying { token: t, .. } if token == Some(*t) => {
            let v = verdict(state, rules, &car, base, now);
            let expected = state.trains[base].base_sha.clone();
            let held = matches!(v, Verdict::Green) && car_hold(state, rules, &car, now, out);
            match (v, expected, car.tree.clone()) {
                (Verdict::Green, _, _) if held => {
                    state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::Green;
                    false
                }
                (Verdict::Green, Some(expected), Some(tree)) => {
                    state.trains.get_mut(base).expect("train").cars[i].status =
                        CarStatus::Landing { since: now };
                    wake(state, now + REPLY_TIMEOUT, out);
                    let format =
                        rule(rules, &car.queue).and_then(|r| r.commit_message_format.as_ref());
                    let messages = car
                        .own
                        .iter()
                        .filter_map(|n| {
                            let pr = state.pull_requests.get(n)?;
                            let by = state
                                .queued
                                .get(n)
                                .and_then(|e| e.by.clone())
                                .unwrap_or_else(|| state.host.bot.clone());
                            let m = crate::message::compose(format, pr, &by);
                            (!m.is_inherited()).then_some(m)
                        })
                        .collect();
                    out.push(Decision::Land {
                        car: car.id,
                        queue: car.queue.clone(),
                        base_branch: base.to_string(),
                        expected_base_sha: expected,
                        prs: car
                            .prs
                            .iter()
                            .filter(|p| car.own.contains(&p.number))
                            .cloned()
                            .collect(),
                        expected_tree: tree,
                        messages,
                        method: rule(rules, &car.queue)
                            .map(|r| r.method())
                            .unwrap_or_default(),
                        head: car.head_sha.clone(),
                        draft: car.draft.filter(|_| !car.in_place),
                    });
                    false
                }
                (Verdict::Attributes(list), ..) => {
                    // The fresh read is the confirmation.
                    dequeue_for_attributes(state, rules, list, out);
                    true
                }
                _ => {
                    state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::Running;
                    true
                }
            }
        }
        CarStatus::LandCheck { token: t, .. } if token == Some(*t) => {
            let order: Vec<u64> = car
                .prs
                .iter()
                .filter(|p| car.own.contains(&p.number))
                .map(|p| p.number)
                .collect();
            let mut commits = Vec::new();
            for &n in &order {
                match state.pull_requests.get(&n) {
                    Some(pr) if pr.merged => commits.push(LandedCommit {
                        number: n,
                        sha: pr.merge_commit_sha.clone().unwrap_or_default(),
                        tree: String::new(),
                        extra: vec![],
                    }),
                    _ => break,
                }
            }
            let tip = state.branches.get(base).cloned();
            let expected = state.trains[base].base_sha.clone();
            if commits.len() == order.len() {
                landed(state, rules, car.id, &commits, None, out);
            } else if !commits.is_empty() || (tip.is_some() && tip != expected) {
                let actual = tip.unwrap_or_default();
                landed(
                    state,
                    rules,
                    car.id,
                    &commits,
                    Some(&LandError::BaseMoved { actual }),
                    out,
                );
            } else {
                // Nothing landed (as far as this read shows): verify and land again. The
                // compare-and-swap on the base and the forge's refusal to merge a closed pull
                // request keep this from landing twice.
                state.trains.get_mut(base).expect("train").cars[i].status = CarStatus::Green;
            }
            true
        }
        CarStatus::Failed { .. }
            if car.test.is_none() && skip_may_stand_in(state, rules, base, i) =>
        {
            // `skip_intermediate_results`: a car behind that carries this one may still pass
            // and stand in for it; its failure is handled once no such car is left running.
            false
        }
        CarStatus::Failed { code, text } if car.test.is_none() => {
            let (code, text) = (*code, text.clone());
            if rule(rules, &car.queue).is_some_and(|r| car.attempt < r.max_checks_retries) {
                retry(state, rules, base, i, &text, out);
                return true;
            }
            let mut behind = dependents_of(state, base, i);
            if state.trains[base].mode != Mode::Serial && car.own.len() > 1 {
                // The failed batch is split next; a bisection runs alone in the train (our
                // simplification outside serial mode), so the other cars are rebuilt after it.
                // A car that is landing stays: its reply or the read after it decides, and
                // dropping it would lose track of a landing in flight.
                let train = &state.trains[base];
                behind = (0..train.cars.len())
                    .filter(|k| {
                        *k != i
                            && !matches!(
                                train.cars[*k].status,
                                CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
                            )
                    })
                    .collect();
            }
            drop_cars(
                state,
                base,
                &behind,
                Some(DequeueReason::new(
                    code::BATCH_AHEAD_FAILED,
                    format!("the batch ahead failed: {text}"),
                )),
                Some(DequeueReason::new(
                    code::BATCH_AHEAD_FAILED,
                    format!("a batch ahead failed: {text}"),
                )),
                &[],
                out,
            );
            let train = state.trains.get_mut(base).expect("train");
            let Some(i) = train.cars.iter().position(|c| c.id == car.id) else {
                return true;
            };
            let failed = train.cars.remove(i);
            close(state, Some(base), &failed, out);
            bisect::batch_failed(state, rules, base, &failed, code, &text, out);
            true
        }
        _ => false,
    }
}

/// Releases held pull requests whose blockers left the queue.
fn release_held(state: &mut State, base: &str) {
    let queued: Vec<u64> = state.queued.keys().copied().collect();
    for entry in state.queued.values_mut() {
        if entry.base != base {
            continue;
        }
        if let EntryStatus::Held { on } = &entry.status
            && !on.iter().any(|n| queued.contains(n))
        {
            entry.status = EntryStatus::Waiting;
        }
    }
}

fn base_and_foreign_pushes(state: &mut State, rules: &Rules, base: &str, out: &mut Vec<Decision>) {
    let known = state.branches.get(base).cloned();
    let now = state.now;
    let never = never_reset(rules);
    let train = &state.trains[base];
    // While a car lands, a new tip is most likely the landing itself, seen before its reply:
    // the landing's outcome (its reply, or the read that resolves it) decides; a push from
    // outside shows up there as a refused compare-and-swap.
    let landing = train.cars.iter().any(|c| {
        matches!(
            c.status,
            CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
        )
    });
    match (train.base_sha.clone(), known) {
        (Some(old), Some(sha)) if old != sha && landing => {}
        (_, None) => {
            let ask = train
                .base_request
                .is_none_or(|(_, since)| due(state, since));
            if ask {
                let token = state.next_id();
                state.trains.get_mut(base).expect("train").base_request = Some((token, now));
                wake(state, now + REPLY_TIMEOUT, out);
                out.push(Decision::Refresh {
                    token,
                    numbers: vec![],
                    branches: vec![base.to_string()],
                });
            }
        }
        (None, Some(sha)) => {
            let train = state.trains.get_mut(base).expect("train");
            train.base_request = None;
            train.set_base(sha);
        }
        (Some(old), Some(sha)) if old != sha => {
            let train = state.trains.get_mut(base).expect("train");
            train.base_request = None;
            if train.history.contains(&sha) {
                // A lagging read shows an older tip (read after write, MQ-33): ignore it.
                state.branches.insert(base.to_string(), old);
            } else if never {
                // `reset_on_external_merge: never`: the queue stays as it is and lands on the
                // new tip.
                train.set_base(sha);
                for c in &mut train.cars {
                    c.drifted = true;
                }
            } else {
                train.set_base(sha.clone());
                for entry in state
                    .queued
                    .values_mut()
                    .filter(|e| e.base == base && !matches!(e.status, EntryStatus::Held { .. }))
                {
                    entry.external_reset = true;
                }
                reset(
                    state,
                    base,
                    DequeueReason::new(
                        code::MERGE_QUEUE_RESET,
                        format!(
                            "the base branch moved outside the queue ({} → {})",
                            short(&old),
                            short(&sha)
                        ),
                    ),
                    out,
                );
            }
        }
        _ => {
            state.trains.get_mut(base).expect("train").base_request = None;
        }
    }
    // Foreign pushes to queue branches (MQ-08): the tested state no longer holds.
    loop {
        let train = &state.trains[base];
        let foreign = train.cars.iter().position(|c| {
            !matches!(c.status, CarStatus::Building { .. })
                && match (&c.branch, &c.head_sha) {
                    (Some(b), Some(h)) => state.branches.get(b).is_some_and(|s| s != h),
                    _ => false,
                }
        });
        let Some(i) = foreign else { break };
        let car = train.cars[i].clone();
        let branch = car.branch.clone().unwrap_or_default();
        // `allow_queue_branch_edit` needs a method that lands the batch branch itself
        // (`merge-batch`, `fast-forward`; docs: queue rules). With `squash` each pull request
        // lands at its own head, so the edit would never land and what lands would not be
        // what was tested: the edit is then a failure like without the option.
        let accept = car.test.is_none()
            && rule(rules, &car.queue).is_some_and(|r| {
                r.allow_queue_branch_edit
                    && matches!(
                        r.merge_method.as_deref(),
                        Some("merge-batch" | "fast-forward")
                    )
            });
        if accept {
            // `allow_queue_branch_edit: true`: the edited batch keeps going on the pushed head;
            // the batches behind it are torn down and recomposed (docs: editing the batch
            // branch). The tree is taken from the draft once a read shows the new head.
            let head = state.branches[&branch].clone();
            let tree = car
                .draft
                .and_then(|d| state.pull_requests.get(&d))
                .filter(|d| d.head_sha == head)
                .and_then(|d| d.head_tree.clone());
            let c = &mut state.trains.get_mut(base).expect("train").cars[i];
            c.head_sha = Some(head);
            c.tree = tree;
            c.started = now;
            c.status = CarStatus::Running;
            c.measured = true;
            let reason = DequeueReason::new(
                code::BATCH_AHEAD_BRANCH_EDITED,
                format!("`{branch}` ahead was edited; the batch is rebuilt"),
            );
            drop_cars_from(
                state,
                base,
                i + 1,
                Some(reason.clone()),
                Some(reason),
                &[],
                out,
            );
            continue;
        }
        // Without `allow_queue_branch_edit` the edit is a failure of the batch: every pull
        // request embarked in the branch leaves (docs: queue rules, allow_queue_branch_edit),
        // also those ahead that the branch carries; the cars behind lost pull requests ahead.
        drop_cars_from(
            state,
            base,
            i,
            None,
            Some(DequeueReason::new(
                code::PR_AHEAD_DEQUEUED,
                format!("someone else pushed to `{branch}` ahead; its pull requests left"),
            )),
            &[],
            out,
        );
        if car.test.is_some() {
            bisect::abort(state, base, None, &[], out);
        }
        let embarked: Vec<u64> = car.prs.iter().map(|p| p.number).collect();
        for n in embarked {
            dequeue(
                state,
                rules,
                n,
                DequeueReason::new(
                    code::DRAFT_PULL_REQUEST_CHANGED,
                    format!("someone else pushed to the queue branch `{branch}`"),
                ),
                out,
            );
        }
    }
}

/// Someone closed a car's draft pull request (`BATCH_PULL_REQUEST_CLOSED`): its checks cannot
/// finish. The car and the cars behind it are torn down; the pull requests stay queued and get
/// a new car (that they stay queued is our choice: nothing about them failed).
fn closed_drafts(state: &mut State, base: &str, out: &mut Vec<Decision>) {
    loop {
        let train = &state.trains[base];
        let closed = train.cars.iter().position(|c| {
            !c.in_place
                && !matches!(c.status, CarStatus::Building { .. })
                && c.draft
                    .and_then(|d| state.pull_requests.get(&d))
                    .is_some_and(|d| d.closed && !d.merged)
        });
        let Some(i) = closed else { break };
        let car = train.cars[i].clone();
        let at = car.draft.map(|d| format!("#{d}")).unwrap_or_default();
        let reason = DequeueReason::new(
            code::BATCH_PULL_REQUEST_CLOSED,
            format!("the batch pull request {at} was closed outside the queue"),
        );
        if car.test.is_some() {
            bisect::abort(state, base, Some(reason), &[], out);
            continue;
        }
        drop_cars_from(
            state,
            base,
            i,
            Some(reason),
            Some(DequeueReason::assumed(
                code::BATCH_AHEAD_REBUILT,
                format!("the batch pull request {at} ahead was closed; the batch is rebuilt"),
            )),
            &[],
            out,
        );
    }
}

/// After a restart from the journal: every outstanding request is due at once, timers are
/// gone. The fresh read that follows resolves the rest.
/// Record restart work without emitting an announcement or resending an action.
/// Each recorded car still needs fresh validation before any announcement.
pub(crate) fn prepare_identity_recovery(state: &mut State) {
    for train in state.trains.values_mut() {
        if let Some((_, since)) = &mut train.base_request {
            *since = EXPIRED;
        }
        for car in &mut train.cars {
            if car.in_place
                && let (Some(number), Some(head_sha)) = (car.draft, car.head_sha.clone())
            {
                state
                    .recovery_in_place
                    .entry(car.id)
                    .or_insert(crate::engine::RecoveryInPlace { number, head_sha });
            }
            match &mut car.status {
                CarStatus::Building { since }
                | CarStatus::Verifying { since, .. }
                | CarStatus::Confirming { since, .. }
                | CarStatus::Landing { since }
                | CarStatus::LandCheck { since, .. } => *since = EXPIRED,
                _ => {}
            }
        }
    }
    for close in state.closing.values_mut() {
        close.since = EXPIRED;
    }
}

pub(crate) fn expire_all(state: &mut State, out: &mut Vec<Decision>) {
    state.wakeups.clear();
    for (base, t) in &state.trains {
        if state.check_identity_targets.contains_key(base) {
            continue;
        }
        // An in-place check has no effect on the forge besides being announced; announcing it
        // again is harmless, and the crash may have come before the first announcement.
        for c in t.cars.iter().filter(|c| c.in_place) {
            if let (Some(number), Some(head_sha)) = (c.draft, c.head_sha.clone()) {
                out.push(Decision::CheckInPlace {
                    car: c.id,
                    number,
                    head_sha,
                    direct: false,
                });
            }
        }
    }
    for t in state.trains.values_mut() {
        if let Some((_, since)) = &mut t.base_request {
            *since = EXPIRED;
        }
        for c in &mut t.cars {
            match &mut c.status {
                CarStatus::Building { since }
                | CarStatus::Verifying { since, .. }
                | CarStatus::Confirming { since, .. }
                | CarStatus::Landing { since }
                | CarStatus::LandCheck { since, .. } => *since = EXPIRED,
                _ => {}
            }
        }
    }
    for c in state.closing.values_mut() {
        c.since = EXPIRED;
    }
}

/// The forge answered a side effect with an error. The effect may or may not have happened
/// (a "lying" 5xx, MQ-35), so the engine never assumes either.
pub(crate) fn side_effect_failed(state: &mut State, decision: &Decision, out: &mut Vec<Decision>) {
    match decision {
        Decision::BuildCar { car, .. } => {
            if let Some((base, i)) = find_car(state, *car)
                && matches!(
                    state.trains[&base].cars[i].status,
                    CarStatus::Building { .. }
                )
            {
                abandon_car(state, &base, *car, out);
            }
        }
        Decision::CheckInPlace { car, .. } => {
            if let Some((base, _)) = find_car(state, *car) {
                abandon_car(state, &base, *car, out);
            }
        }
        Decision::Land { car, .. } => {
            if let Some((base, i)) = find_car(state, *car)
                && matches!(
                    state.trains[&base].cars[i].status,
                    CarStatus::Landing { .. }
                )
            {
                land_unknown(state, &base, i, out);
            }
        }
        // A close or a read is retried after the reply timeout.
        _ => {}
    }
}

pub fn process_trains(
    state: &mut State,
    rules: &Rules,
    now: Time,
    token: Option<u64>,
    out: &mut Vec<Decision>,
) {
    // Until the protected inputs are known, neither orphan discovery nor delayed close
    // retries may act on a potentially stale queue view. Both resume after recovery.
    if state.check_identity_targets.is_empty() {
        collect_garbage(state, rules, out);
        closing_timeouts(state, out);
    }
    state.landing_holds.clear();
    let bases: Vec<String> = state.trains.keys().cloned().collect();
    for base in bases {
        state.trains.get_mut(&base).expect("train").mode = mode(rules);
        if state.check_identity_targets.contains_key(&base)
            || state.check_identity_unavailable.iter().any(|n| {
                state.pull_requests.get(n).is_some_and(|p| p.base == base)
                    || state.trains[&base]
                        .cars
                        .iter()
                        .any(|c| c.draft == Some(*n) || c.own.contains(n))
            })
        {
            continue;
        }
        base_and_foreign_pushes(state, rules, &base, out);
        closed_drafts(state, &base, out);
        if state.trains[&base].halted.is_some() {
            continue;
        }
        release_held(state, &base);
        timeouts(state, &base, out);
        if state.pause.is_some() {
            pause_train(state, &base, out);
        }
        for _ in 0..64 {
            let mut changed = evaluate_cars(state, rules, &base, now, token, out);
            changed |= retry_split_tests(state, rules, &base, out);
            changed |= bisect::resolve(state, rules, &base, out);
            changed |= skip_intermediate(state, rules, &base, out);
            changed |= front(state, rules, &base, now, token, out);
            if !changed {
                break;
            }
        }
        preempt(state, rules, &base, out);
        fill(state, rules, &base, now, out);
    }
}

/// The merge queue is paused: running queue checks are cancelled (docs: merge-queue/pause). A
/// car whose landing is in flight finishes first; a check in place cannot be cancelled and
/// keeps running (it does not land while the pause lasts).
fn pause_train(state: &mut State, base: &str, out: &mut Vec<Decision>) {
    let train = &state.trains[base];
    if train.cars.iter().any(|c| {
        matches!(
            c.status,
            CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
        )
    }) {
        return;
    }
    let reason = DequeueReason::new(
        code::PR_CHECKS_STOPPED_BECAUSE_MERGE_QUEUE_PAUSE,
        format!(
            "the merge queue is paused: {}",
            state
                .pause
                .as_ref()
                .map(|p| p.reason.as_str())
                .unwrap_or("")
        ),
    );
    if train.bisection.is_some() {
        bisect::abort(state, base, Some(reason.clone()), &[], out);
    }
    let idx: Vec<usize> = state.trains[base]
        .cars
        .iter()
        .enumerate()
        .filter(|(_, c)| !c.in_place)
        .map(|(i, _)| i)
        .collect();
    if !idx.is_empty() {
        drop_cars(
            state,
            base,
            &idx,
            Some(reason.clone()),
            Some(reason),
            &[],
            out,
        );
    }
}

/// `allow_checks_interruption` (docs: priority): when the next pull request to embark has a
/// higher priority than the pull requests of a running car and its priority rule allows it,
/// that car (and the cars built on it) is discarded so the urgent one is tested first. The
/// discarded pull requests stay queued with `PR_WITH_HIGHER_PRIORITY_QUEUED`. Without the
/// option the urgent pull request goes right after the cars that run. A landing in flight,
/// a bisection and checks in place are never interrupted.
fn preempt(state: &mut State, rules: &Rules, base: &str, out: &mut Vec<Decision>) {
    let train = &state.trains[base];
    if state.pause.is_some() || train.bisection.is_some() {
        return;
    }
    let order = waiting_order(state, rules, base);
    let Some(&first) = order.first() else {
        return;
    };
    let e = &state.queued[&first];
    if !e.interrupt {
        return;
    }
    let p = e.priority;
    let lower = |c: &Car| {
        c.own
            .iter()
            .all(|n| state.queued.get(n).is_some_and(|o| o.priority < p))
    };
    let Some(i) = train.cars.iter().position(|c| {
        !c.in_place
            && c.test.is_none()
            && lower(c)
            && matches!(
                c.status,
                CarStatus::Running
                    | CarStatus::Confirming { .. }
                    | CarStatus::Green
                    | CarStatus::Retry
            )
    }) else {
        return;
    };
    let busy = |c: &Car| {
        matches!(
            c.status,
            CarStatus::Building { .. }
                | CarStatus::Verifying { .. }
                | CarStatus::Landing { .. }
                | CarStatus::LandCheck { .. }
        )
    };
    let mut idx = vec![i];
    idx.extend(dependents(train, i));
    if idx.iter().any(|k| busy(&train.cars[*k])) {
        return;
    }
    let reason = DequeueReason::new(
        code::PR_WITH_HIGHER_PRIORITY_QUEUED,
        format!("#{first} with a higher priority interrupted these checks"),
    );
    drop_cars(
        state,
        base,
        &idx,
        Some(reason.clone()),
        Some(reason),
        &[],
        out,
    );
}

/// Whether a car behind car `i` (serial mode, same queue, not landing the branch itself) is still
/// running and could stand in for it with `skip_intermediate_results`.
fn skip_may_stand_in(state: &State, rules: &Rules, base: &str, i: usize) -> bool {
    let train = &state.trains[base];
    let car = &train.cars[i];
    rules.merge_queue.skip_intermediate_results
        && train.mode == Mode::Serial
        && train.bisection.is_none()
        && !car.in_place
        && rule(rules, &car.queue).is_some_and(|r| !r.method().lands_branch())
        && train.cars[i + 1..]
            .iter()
            .take_while(|c| c.queue == car.queue && c.test.is_none() && !c.in_place)
            .any(|c| {
                matches!(
                    c.status,
                    CarStatus::Building { .. }
                        | CarStatus::Running
                        | CarStatus::Confirming { .. }
                        | CarStatus::Green
                )
            })
}

/// `skip_intermediate_results` (docs: batches, "Skip intermediate results"): when a car
/// further back is green, the cars ahead of it that it was built on merge with it without
/// waiting for their own checks, even a red one. Built for serial mode with `squash`, `merge`
/// and `rebase`: the absorbed cars' drafts are closed and their pull requests land with the
/// green car (its branch carries them; the commits are the tested prefixes of that branch).
fn skip_intermediate(
    state: &mut State,
    rules: &Rules,
    base: &str,
    out: &mut Vec<Decision>,
) -> bool {
    if !rules.merge_queue.skip_intermediate_results {
        return false;
    }
    let train = &state.trains[base];
    if train.mode != Mode::Serial || train.bisection.is_some() || train.cars.len() < 2 {
        return false;
    }
    let front = &train.cars[0];
    if !matches!(
        front.status,
        CarStatus::Running
            | CarStatus::Confirming { .. }
            | CarStatus::Failed { .. }
            | CarStatus::Retry
    ) || front.test.is_some()
        || front.in_place
    {
        return false;
    }
    let queue = front.queue.clone();
    if rule(rules, &queue).is_none_or(|r| r.method().lands_branch()) {
        return false;
    }
    let mut j = None;
    for (k, c) in train.cars.iter().enumerate().skip(1) {
        if c.queue != queue || c.test.is_some() || c.in_place {
            break;
        }
        if c.status == CarStatus::Green {
            j = Some(k);
            break;
        }
        if matches!(
            c.status,
            CarStatus::Verifying { .. } | CarStatus::Landing { .. } | CarStatus::LandCheck { .. }
        ) {
            break;
        }
    }
    let Some(j) = j else {
        return false;
    };
    let absorbed: Vec<Car> = train.cars[..j].to_vec();
    let mut own: Vec<u64> = absorbed.iter().flat_map(|c| c.own.clone()).collect();
    own.extend(train.cars[j].own.clone());
    let green_id = train.cars[j].id;
    let t = state.trains.get_mut(base).expect("train");
    t.cars.drain(..j);
    t.cars[0].own = own;
    t.cars[0].deps.clear();
    for c in &absorbed {
        close(state, Some(base), c, out);
        for &n in &c.own {
            if let Some(e) = state.queued.get_mut(&n) {
                e.status = EntryStatus::InCar { car: green_id };
                out.push(Decision::ChecksEnded {
                    number: n,
                    queue: e.queue.clone(),
                    reason: DequeueReason::new(
                        code::INTERMEDIATE_RESULTS_SKIPPED,
                        format!(
                            "car {green_id} behind it is green and carries it; it lands with \
                             that car"
                        ),
                    ),
                });
            }
        }
    }
    true
}

/// The waiting pull requests of `base` in queue order: higher priority first, then the order
/// of the queue rules in the configuration, then arrival (our reading of the docs: priority
/// is "always the first criterion"; the order of queue rules among equal priorities is ours).
pub fn waiting_order(state: &State, rules: &Rules, base: &str) -> Vec<u64> {
    let rank = |q: &str| {
        rules
            .queue_rules
            .iter()
            .position(|r| r.name == q)
            .unwrap_or(usize::MAX)
    };
    let mut w: Vec<(std::cmp::Reverse<u32>, usize, u64, u64)> = state
        .queued
        .iter()
        .filter(|(_, e)| e.base == base && e.status == EntryStatus::Waiting)
        .map(|(n, e)| (std::cmp::Reverse(e.priority), rank(&e.queue), e.seq, *n))
        .collect();
    w.sort();
    w.into_iter().map(|(_, _, _, n)| n).collect()
}

/// Whether Mergify's rule for checking in place holds for this queue rule.
pub fn in_place_allowed(rules: &Rules, rule: &QueueRule) -> bool {
    rules.merge_queue.max_parallel_checks == 1
        && rules.merge_queue.mode == "serial"
        && rule.batch_size.max() == 1
        && rule.max_checks_retries == 0
        && !rule.two_step()
}

/// Batch size: `min` while the queue is light, growing toward `max` when the backlog is deeper
/// than the free slots can drain at the smaller size (docs: dynamic batch sizing).
pub fn batch_size(size: BatchSize, backlog: u32, free_slots: u32) -> u32 {
    match size {
        BatchSize::Fixed(n) => n,
        BatchSize::Range { min, max } => backlog.div_ceil(free_slots.max(1)).clamp(min, max),
    }
}

/// The directories a pull request changes (`""` for the repository root).
fn changed_dirs(pr: Option<&PullRequest>) -> std::collections::BTreeSet<String> {
    pr.map(|p| {
        p.files
            .iter()
            .map(|f| f.rsplit_once('/').map_or("", |(d, _)| d).to_string())
            .collect()
    })
    .unwrap_or_default()
}

/// Jaccard share of two sets: common items out of all items either side names (0 when both
/// are empty).
fn share<T: Ord>(a: &std::collections::BTreeSet<T>, b: &std::collections::BTreeSet<T>) -> f64 {
    let union = a.union(b).count();
    if union == 0 {
        0.0
    } else {
        a.intersection(b).count() as f64 / union as f64
    }
}

/// Picks the next batch of at most `size` from the waiting pull requests (queue order). The
/// batch is seeded with the first one; the free places go one at a time to the best candidates
/// among the first `window` (docs: batches, "How Pull Requests Are Grouped Into a Batch"):
/// higher priority first (similarity never pulls a lower priority ahead), then the share of
/// scopes in common with the batch, then the share of changed directories, then queue order.
/// The window (batch size plus `max_parallel_checks`) and the measures are our reading of the
/// docs, which name neither exactly. The batch keeps queue order.
pub fn group_by_similarity(state: &State, order: &[u64], size: usize, window: usize) -> Vec<u64> {
    group_with_scopes(state, &Rules::default(), order, size, window)
}

/// [`group_by_similarity`] with the configuration's scopes.
pub fn group_with_scopes(
    state: &State,
    rules: &Rules,
    order: &[u64],
    size: usize,
    window: usize,
) -> Vec<u64> {
    let Some(&seed) = order.first() else {
        return vec![];
    };
    let prio = |n: u64| state.queued.get(&n).map_or(0, |e| e.priority);
    let scopes = |n: u64| -> std::collections::BTreeSet<String> {
        pr_scopes(rules, state.pull_requests.get(&n))
            .0
            .into_iter()
            .collect()
    };
    let mut batch = vec![seed];
    let mut dirs = changed_dirs(state.pull_requests.get(&seed));
    let mut sc = scopes(seed);
    let mut candidates: Vec<(usize, u64)> = order
        .iter()
        .copied()
        .enumerate()
        .skip(1)
        .take(window.saturating_sub(1))
        .collect();
    while batch.len() < size && !candidates.is_empty() {
        let key = |n: u64| {
            (
                prio(n),
                share(&scopes(n), &sc),
                share(&changed_dirs(state.pull_requests.get(&n)), &dirs),
            )
        };
        let best = candidates
            .iter()
            .enumerate()
            .max_by(|(_, a), (_, b)| {
                let (ka, kb) = (key(a.1), key(b.1));
                ka.0.cmp(&kb.0)
                    .then(ka.1.partial_cmp(&kb.1).unwrap_or(std::cmp::Ordering::Equal))
                    .then(ka.2.partial_cmp(&kb.2).unwrap_or(std::cmp::Ordering::Equal))
                    .then(b.0.cmp(&a.0))
            })
            .map(|(k, _)| k)
            .expect("non-empty");
        let (_, n) = candidates.remove(best);
        dirs.extend(changed_dirs(state.pull_requests.get(&n)));
        sc.extend(scopes(n));
        batch.push(n);
    }
    batch.sort_by_key(|n| order.iter().position(|o| o == n));
    batch
}

/// Starts at most one car: a retry, the next bisection test or the next batch.
fn fill(state: &mut State, rules: &Rules, base: &str, now: Time, out: &mut Vec<Decision>) {
    let slots = rules.merge_queue.max_parallel_checks as usize;
    let busy = state.trains[base].cars.len() + closing_count(state, base);
    let train = &state.trains[base];
    let Some(base_sha) = train.base_sha.clone() else {
        return;
    };
    if let Some(i) = train.cars.iter().position(|c| c.status == CarStatus::Retry) {
        // The retry is already counted among the cars. It is built only once the forge confirmed
        // the close of the attempt it replaces (I4: that draft's CI counts until then).
        if busy <= slots && !retry_waits_for_close(state, &train.cars[i]) {
            let car = train.cars[i].clone();
            let prs: Vec<CarPr> = car
                .prs
                .iter()
                .filter(|p| state.queued.contains_key(&p.number))
                .cloned()
                .collect();
            let c = &mut state.trains.get_mut(base).expect("train").cars[i];
            c.prs = prs.clone();
            c.base_sha = base_sha.clone();
            c.status = CarStatus::Building { since: now };
            c.replaces = None;
            c.stacked_on = None;
            wake(state, now + REPLY_TIMEOUT, out);
            out.push(Decision::BuildCar {
                car: car.id,
                queue: car.queue.clone(),
                base_branch: base.to_string(),
                branch: car.branch.clone().expect("retry branch"),
                base_sha,
                prs,
                own: car.own.clone(),
                stack_on: None,
            });
        }
        return;
    }
    if state.pause.is_some()
        || busy >= slots
        || train.cars.iter().any(|c| {
            matches!(
                c.status,
                CarStatus::Building { .. }
                    | CarStatus::Failed { .. }
                    | CarStatus::Landing { .. }
                    | CarStatus::LandCheck { .. }
            )
        })
    {
        return;
    }
    if train.bisection.is_some() {
        bisect::fill(state, rules, base, &base_sha, out);
        return;
    }
    let mode = train.mode;
    // A barrier runs alone: nothing starts while one is in the train (serial and parallel
    // mode; an isolated queue builds no dependencies, docs: queue modes).
    if mode != Mode::Isolated && train.cars.iter().any(|c| c.barrier) {
        return;
    }
    let order = waiting_order(state, rules, base);
    let Some(&first) = order.first() else {
        return;
    };
    let queue = state.queued[&first].queue.clone();
    let Some(rule) = rule(rules, &queue) else {
        return;
    };
    let (seed_scopes, seed_barrier) = pr_scopes(rules, state.pull_requests.get(&first));
    if seed_barrier && mode != Mode::Isolated && !train.cars.is_empty() {
        return; // the barrier waits until every batch ahead of it completed
    }
    // A batch never mixes queue rules; it looks ahead for pull requests of the same rule (docs:
    // priority). A barrier shares a batch only with barriers; in parallel mode only pull
    // requests with exactly the seed's scopes share a batch (docs: scopes).
    let group: Vec<u64> = order
        .into_iter()
        .filter(|n| state.queued[n].queue == queue)
        .filter(|n| {
            let (sc, barrier) = pr_scopes(rules, state.pull_requests.get(n));
            barrier == seed_barrier && (mode != Mode::Parallel || sc == seed_scopes)
        })
        .collect();
    let total = group.len() as u32;
    let size = batch_size(rule.batch_size, total, (slots - busy) as u32);
    if total < rule.batch_size.min() {
        let since = group
            .iter()
            .map(|n| state.queued[n].waiting_since)
            .min()
            .unwrap_or(now);
        let deadline = since + rule.batch_max_wait_time;
        if now < deadline {
            wake(state, deadline, out);
            return;
        }
    }
    let window = size.max(1) as usize + slots;
    let batch = group_with_scopes(state, rules, &group, size.max(1) as usize, window);
    let train = &state.trains[base];
    let mut scopes: Vec<String> = batch
        .iter()
        .flat_map(|n| pr_scopes(rules, state.pull_requests.get(n)).0)
        .collect();
    scopes.sort();
    scopes.dedup();
    let barrier = seed_barrier;
    // The cars this one is built on: serial, the last car (which carries everything ahead);
    // isolated, none; parallel, the last car ahead in each of the batch's scopes.
    let deps: Vec<u64> = match mode {
        Mode::Serial => train.cars.last().map(|c| vec![c.id]).unwrap_or_default(),
        Mode::Isolated => vec![],
        Mode::Parallel => {
            let mut d: Vec<u64> = scopes
                .iter()
                .filter_map(|s| {
                    train
                        .cars
                        .iter()
                        .rev()
                        .find(|c| c.barrier || c.scopes.contains(s))
                        .map(|c| c.id)
                })
                .collect();
            if barrier {
                d = train.cars.iter().map(|c| c.id).collect();
            }
            d.sort();
            d.dedup();
            d
        }
    };
    // The pull requests ahead: those of the cars it depends on that are still queued (landed
    // ones are part of the base now), each once, in their order.
    let mut prs: Vec<CarPr> = Vec::new();
    for c in train.cars.iter().filter(|c| deps.contains(&c.id)) {
        for p in &c.prs {
            if state.queued.contains_key(&p.number) && !prs.iter().any(|x| x.number == p.number) {
                prs.push(p.clone());
            }
        }
    }
    for n in &batch {
        prs.push(CarPr {
            number: *n,
            head_sha: state.queued[n].head_sha.clone(),
        });
    }
    // `fast-forward` and `merge-batch` land the batch branch itself: in serial mode each car is
    // built on the branch of the car ahead.
    let stack_on = (mode == Mode::Serial && rule.method().lands_branch())
        .then(|| train.cars.last().and_then(|c| c.head_sha.clone()))
        .flatten();
    let no_cars = train.cars.is_empty();
    let id = state.next_id();
    let pr = state.pull_requests.get(&batch[0]).cloned();
    let up_to_date = pr.as_ref().is_some_and(|p| {
        !p.behind_base && p.base_tip.as_ref() == Some(&base_sha) && p.head_tree.is_some()
    });
    let alone = no_cars && batch.len() == 1;
    let in_place = in_place_allowed(rules, rule) && alone && up_to_date;
    // Direct merge, already-up-to-date case (docs: merge-queue/direct-merge): the pull request
    // is alone at the front, contains the base tip, the queue would test through a draft
    // (not in place), its CI is single-step, the method is not `merge-batch`, and its own CI
    // already meets the merge conditions. Then a queue run would test the same change on the
    // same base again: it is checked where it is and lands. (The scope case is not built.)
    let direct = !in_place
        && alone
        && up_to_date
        && !rule.two_step()
        && rule.method() != schlange_config::rules::MergeMethod::MergeBatch
        && pr.as_ref().is_some_and(|p| {
            let ctx = crate::engine::context(state, p.number);
            let conds = crate::gates::merge_conditions(state, rule, base);
            eval::eval_list(&conds, p, &ctx, &mut Vec::new()).0
        });
    let in_place = in_place || direct;
    let mut car = Car {
        id,
        queue: queue.clone(),
        branch: None,
        draft: None,
        in_place,
        base_sha: base_sha.clone(),
        prs: prs.clone(),
        own: batch.clone(),
        head_sha: None,
        tree: None,
        started: now,
        status: CarStatus::Building { since: now },
        test: None,
        attempt: 0,
        measured: false,
        drifted: false,
        replaces: None,
        deps,
        stacked_on: if in_place { None } else { stack_on.clone() },
        scopes,
        barrier,
    };
    if in_place {
        let pr = pr.expect("checked");
        car.head_sha = Some(pr.head_sha.clone());
        car.tree = pr.head_tree.clone();
        car.draft = Some(pr.number);
        car.status = CarStatus::Running;
        out.push(Decision::CheckInPlace {
            car: id,
            number: pr.number,
            head_sha: pr.head_sha,
            direct,
        });
    } else {
        let branch = branch_name(&rule.queue_branch_prefix, base, id);
        car.branch = Some(branch.clone());
        wake(state, now + REPLY_TIMEOUT, out);
        out.push(Decision::BuildCar {
            car: id,
            queue,
            base_branch: base.to_string(),
            branch,
            base_sha,
            prs,
            own: batch.clone(),
            stack_on,
        });
    }
    for n in &batch {
        if let Some(e) = state.queued.get_mut(n) {
            e.status = EntryStatus::InCar { car: id };
        }
    }
    state.trains.get_mut(base).expect("train").cars.push(car);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_batch_size_divides_the_backlog_by_free_slots() {
        let r = BatchSize::Range { min: 1, max: 2 };
        // Docs: five checks, 1..2: three queued → batches of one; ten → batches of two.
        assert_eq!(batch_size(r, 3, 5), 1);
        assert_eq!(batch_size(r, 10, 5), 2);
        // After two batches of two, eight are left for four free slots: still two.
        assert_eq!(batch_size(r, 8, 4), 2);
        assert_eq!(batch_size(r, 2, 1), 2);
        assert_eq!(batch_size(BatchSize::Range { min: 2, max: 10 }, 1, 5), 2);
        assert_eq!(batch_size(BatchSize::Fixed(5), 1, 5), 5);
    }

    #[test]
    fn auto_timeout_needs_history_and_has_a_floor() {
        let mut s = State {
            now: 10 * 86400,
            ..State::default()
        };
        assert_eq!(auto_timeout(&s, "q", "main"), None);
        let runs: Vec<Sample> = (0..19)
            .map(|k| Sample {
                at: s.now - k * 3600,
                secs: 600,
            })
            .collect();
        s.history.insert(history_key("q", "main"), runs);
        assert_eq!(
            auto_timeout(&s, "q", "main"),
            None,
            "19 runs are not enough"
        );
        s.history
            .get_mut(&history_key("q", "main"))
            .unwrap()
            .push(Sample {
                at: s.now,
                secs: 1200,
            });
        // Twenty runs: the 95th percentile of 19 × 600 s and 1 × 1200 s, times the margin.
        assert_eq!(auto_timeout(&s, "q", "main"), Some(1800));
        // Old runs fall out of the window.
        s.now += 8 * 86400;
        assert_eq!(auto_timeout(&s, "q", "main"), None);
        // Short runs: at least 60 s.
        let quick: Vec<Sample> = (0..20).map(|_| Sample { at: s.now, secs: 5 }).collect();
        s.history.insert(history_key("q", "dev"), quick);
        assert_eq!(auto_timeout(&s, "q", "dev"), Some(60));
    }
}
