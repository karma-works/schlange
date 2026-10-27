//! The queue of one base branch ("train"): serial mode with cumulative speculation.
//!
//! Car k tests the base plus the batches of cars 1..k. At most `max_parallel_checks` cars
//! exist at a time (I4). Only the front car decides: it lands when green, or its failure is
//! handled when red; a red car behind a running one waits, because the cause may lie ahead.
//! When a car is discarded, every car behind it is discarded too (MQ-23). A red batch of
//! several pull requests is split as described in `bisect`.

use serde::Serialize;
use sha2::{Digest, Sha256};

use schlange_config::rules::{BatchSize, ChecksTimeout, QueueRule, Rules};

use crate::bisect;
use crate::engine::{
    CarPr, Decision, DequeueReason, EntryStatus, LandError, LandedCommit, Outcome, State, dequeue,
    short,
};
use crate::eval::{self, Context};
use crate::model::{Conclusion, PullRequest, Time};
use crate::reasons as code;

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum CarStatus {
    /// `BuildCar` sent, no reply yet.
    Building,
    Running,
    Green,
    Failed {
        code: &'static str,
        text: String,
    },
    /// A fresh read was requested before landing.
    Verifying {
        token: u64,
    },
    Landing,
}

#[derive(Clone, Debug, Serialize)]
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
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Train {
    pub base: String,
    pub base_sha: Option<String>,
    pub cars: Vec<Car>,
    pub halted: Option<String>,
    pub bisection: Option<bisect::Bisection>,
    #[serde(skip)]
    pub base_requested: bool,
}

impl Train {
    pub fn new(base: &str) -> Self {
        Train {
            base: base.to_string(),
            ..Default::default()
        }
    }

    /// Branches of the queue's cars that exist on the forge.
    pub fn branches(&self) -> Vec<String> {
        self.cars.iter().filter_map(|c| c.branch.clone()).collect()
    }
}

pub(crate) fn rule<'a>(rules: &'a Rules, name: &str) -> Option<&'a QueueRule> {
    rules.queue_rules.iter().find(|q| q.name == name)
}

pub(crate) fn wake(state: &mut State, at: Time, out: &mut Vec<Decision>) {
    if state.wakeups.insert(at) {
        out.push(Decision::WakeAt { at });
    }
}

pub fn is_car_draft(state: &State, number: u64) -> bool {
    state.trains.values().any(|t| {
        t.cars
            .iter()
            .any(|c| !c.in_place && c.draft == Some(number))
    })
}

/// Deterministic queue branch name: the prefix and ten hex characters.
pub fn branch_name(prefix: &str, base: &str, car: u64) -> String {
    let digest = Sha256::digest(format!("schlange/{base}/{car}").as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("{prefix}{}", &hex[..10])
}

pub(crate) fn close(car: &Car, out: &mut Vec<Decision>) {
    if !car.in_place {
        out.push(Decision::CloseCar {
            car: car.id,
            branch: car.branch.clone(),
            draft: car.draft,
        });
    }
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
    let Some(train) = state.trains.get_mut(base) else {
        return;
    };
    if from >= train.cars.len() {
        return;
    }
    let removed: Vec<Car> = train.cars.drain(from..).collect();
    for (i, car) in removed.iter().enumerate() {
        close(car, out);
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
    drop_cars_from(
        state,
        base,
        idx,
        Some(DequeueReason::new(
            code::BATCH_REBUILT,
            format!("#{number} left this batch"),
        )),
        Some(DequeueReason::new(
            code::PR_AHEAD_DEQUEUED,
            format!("#{number} ahead in the queue left it"),
        )),
        &[number],
        out,
    );
}

/// Discards every car and a bisection; every pull request waits again.
pub(crate) fn reset(state: &mut State, base: &str, reason: DequeueReason, out: &mut Vec<Decision>) {
    bisect::abort(state, base, Some(reason.clone()), &[], out);
    drop_cars_from(state, base, 0, Some(reason.clone()), Some(reason), &[], out);
}

fn find_car(state: &mut State, car: u64) -> Option<(String, usize)> {
    state.trains.iter().find_map(|(b, t)| {
        t.cars
            .iter()
            .position(|c| c.id == car)
            .map(|i| (b.clone(), i))
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn car_built(
    state: &mut State,
    car: u64,
    branch: &str,
    head_sha: &str,
    tree: &str,
    draft: u64,
    now: Time,
    out: &mut Vec<Decision>,
) {
    let Some((base, i)) = find_car(state, car) else {
        // The car was discarded before the reply arrived: remove what the forge created.
        out.push(Decision::CloseCar {
            car,
            branch: Some(branch.to_string()),
            draft: Some(draft),
        });
        return;
    };
    let c = &mut state.trains.get_mut(&base).expect("found").cars[i];
    c.branch = Some(branch.to_string());
    c.head_sha = Some(head_sha.to_string());
    c.tree = Some(tree.to_string());
    c.draft = Some(draft);
    c.started = now;
    c.status = CarStatus::Running;
    // The queue's own push to its branch is known now (not a foreign push).
    state
        .branches
        .insert(branch.to_string(), head_sha.to_string());
}

pub(crate) fn car_conflict(
    state: &mut State,
    rules: &Rules,
    car: u64,
    number: u64,
    with_base: bool,
    with: &[u64],
    out: &mut Vec<Decision>,
) {
    let Some((base, i)) = find_car(state, car) else {
        return;
    };
    let train = state.trains.get_mut(&base).expect("found");
    let removed: Vec<Car> = train.cars.drain(i..).collect();
    let in_bisection = removed.iter().any(|c| c.test.is_some());
    for c in &removed {
        for &n in &c.own {
            if n != number && !in_bisection {
                requeue(state, n, None, out);
            }
        }
    }
    if in_bisection {
        bisect::abort(state, &base, None, &[number], out);
    }
    if with_base {
        dequeue(
            state,
            rules,
            number,
            DequeueReason::new(
                code::CONFLICT_WITH_BASE_BRANCH,
                "the pull request does not merge onto the base branch",
            ),
            out,
        );
    } else {
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

#[allow(clippy::too_many_arguments)]
pub(crate) fn landed(
    state: &mut State,
    rules: &Rules,
    car: u64,
    commits: &[LandedCommit],
    error: Option<&LandError>,
    _now: Time,
    out: &mut Vec<Decision>,
) {
    let Some((base, i)) = find_car(state, car) else {
        return;
    };
    let train = state.trains.get_mut(&base).expect("found");
    let c = train.cars.remove(i);
    close(&c, out);
    for commit in commits {
        train.base_sha = Some(commit.sha.clone());
    }
    if let Some(last) = commits.last() {
        state.branches.insert(base.clone(), last.sha.clone());
    }
    for commit in commits {
        if let Some(pr) = state.pull_requests.get_mut(&commit.number) {
            pr.merged = true;
            pr.closed = true;
        }
        if let Some(entry) = state.queued.remove(&commit.number) {
            state.outcomes.insert(
                commit.number,
                Outcome::Merged {
                    sha: commit.sha.clone(),
                },
            );
            out.push(Decision::Merged {
                number: commit.number,
                queue: entry.queue,
                sha: commit.sha.clone(),
            });
        }
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
            let landed_tree = commits.last().map(|c| c.tree.clone());
            if landed_tree.is_some() && landed_tree != c.tree {
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
            for n in rest {
                requeue(state, n, None, out);
            }
            let reason = match err {
                LandError::BaseMoved { actual } => {
                    if let Some(t) = state.trains.get_mut(&base) {
                        t.base_sha = Some(actual.clone());
                    }
                    state.branches.insert(base.clone(), actual.clone());
                    DequeueReason::assumed(
                        code::MERGE_QUEUE_RESET,
                        format!("the base branch moved to {} while landing", short(actual)),
                    )
                }
                LandError::HeadChanged { number } => {
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
                    DequeueReason::new(code::BATCH_REBUILT, format!("#{number} left the queue"))
                }
                LandError::Conflict { number } => {
                    dequeue(
                        state,
                        rules,
                        *number,
                        DequeueReason::new(
                            code::PR_UNEXPECTEDLY_FAILED_TO_MERGE,
                            "the squash merge did not apply",
                        ),
                        out,
                    );
                    DequeueReason::new(code::BATCH_REBUILT, format!("#{number} left the queue"))
                }
            };
            reset(state, &base, reason, out);
        }
    }
}

fn halt(state: &mut State, base: &str, reason: String, out: &mut Vec<Decision>) {
    reset(
        state,
        base,
        DequeueReason::new(code::MERGE_QUEUE_RESET, "the queue halted"),
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
}

/// Merge conditions of a queue rule; without `merge_conditions` the queue conditions apply.
fn merge_conditions(rule: &QueueRule) -> &[schlange_config::conditions::Condition] {
    if rule.merge_conditions.is_empty() {
        &rule.queue_conditions
    } else {
        &rule.merge_conditions
    }
}

/// The pull request as the merge conditions see it: its own attributes, the checks of the
/// car's head (the draft pull request, or the pull request itself in place).
fn car_view(state: &State, car: &Car, number: u64) -> Option<PullRequest> {
    let mut pr = state.pull_requests.get(&number)?.clone();
    let head = car.head_sha.clone()?;
    let draft = car.draft.and_then(|d| state.pull_requests.get(&d));
    pr.checks = draft.map(|d| d.checks.clone()).unwrap_or_default();
    pr.head_sha = head;
    Some(pr)
}

pub(crate) fn verdict(state: &State, rules: &Rules, car: &Car, now: Time) -> Verdict {
    let Some(rule) = rule(rules, &car.queue) else {
        return Verdict::Pending;
    };
    let conds = merge_conditions(rule);
    let ctx = Context::default();
    let mut all = true;
    let mut views = Vec::new();
    for &n in &car.own {
        let Some(view) = car_view(state, car, n) else {
            return Verdict::Pending;
        };
        let (ok, _) = eval::eval_list(conds, &view, &ctx, &mut Vec::new());
        all &= ok;
        views.push(view);
    }
    if all {
        return Verdict::Green;
    }
    let runs = views
        .first()
        .map(|v| v.current_checks())
        .unwrap_or_default();
    let referenced = eval::referenced_checks(conds);
    let pending: Vec<&str> = runs
        .iter()
        .filter(|r| r.conclusion.is_none())
        .map(|r| r.name.as_str())
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
    let failed_ref = runs
        .iter()
        .any(|r| referenced.contains(&r.name) && failing(r.conclusion));
    if !runs.is_empty() && (pending.is_empty() || failed_ref) {
        let hard = runs.iter().any(|r| {
            matches!(
                r.conclusion,
                Some(Conclusion::Failure | Conclusion::TimedOut | Conclusion::ActionRequired)
            )
        });
        let cancelled = runs
            .iter()
            .any(|r| r.conclusion == Some(Conclusion::Cancelled));
        let names: Vec<&str> = runs
            .iter()
            .filter(|r| failing(r.conclusion))
            .map(|r| r.name.as_str())
            .collect();
        let at = car.draft.map(|d| format!(" on #{d}")).unwrap_or_default();
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
    if let ChecksTimeout::Fixed(t) = rule.checks_timeout
        && now >= car.started + t
    {
        let missing: Vec<&str> = referenced
            .iter()
            .filter(|n| !runs.iter().any(|r| &r.name == *n))
            .map(String::as_str)
            .collect();
        return Verdict::Failed(
            code::CHECKS_TIMEOUT,
            format!(
                "checks did not pass within {}s; still running: [{}]; never reported: [{}]",
                t,
                pending.join(", "),
                missing.join(", ")
            ),
        );
    }
    Verdict::Pending
}

/// Re-evaluates running cars; returns whether anything changed.
fn evaluate_cars(state: &mut State, rules: &Rules, base: &str, now: Time, out: &mut Vec<Decision>) {
    let cars = state.trains[base].cars.clone();
    for (i, car) in cars.iter().enumerate() {
        if car.status != CarStatus::Running {
            continue;
        }
        let v = verdict(state, rules, car, now);
        let status = match v {
            Verdict::Green => CarStatus::Green,
            Verdict::Failed(code, text) => CarStatus::Failed { code, text },
            Verdict::Pending => {
                if let Some(ChecksTimeout::Fixed(t)) =
                    rule(rules, &car.queue).map(|r| r.checks_timeout)
                {
                    wake(state, car.started + t, out);
                }
                continue;
            }
        };
        state.trains.get_mut(base).expect("train").cars[i].status = status;
    }
}

/// Handles the front car; returns whether the train changed.
fn front(
    state: &mut State,
    rules: &Rules,
    base: &str,
    now: Time,
    token: Option<u64>,
    out: &mut Vec<Decision>,
) -> bool {
    let train = &state.trains[base];
    let Some(car) = train.cars.first().cloned() else {
        return false;
    };
    if let Some(b) = &train.bisection
        && b.land != Some(car.id)
    {
        return false;
    }
    match &car.status {
        CarStatus::Green => {
            let t = state.next_id();
            let mut numbers = car.own.clone();
            numbers.extend(car.draft.filter(|_| !car.in_place));
            let mut branches = vec![base.to_string()];
            branches.extend(car.branch.clone());
            state.trains.get_mut(base).expect("train").cars[0].status =
                CarStatus::Verifying { token: t };
            out.push(Decision::Refresh {
                token: t,
                numbers,
                branches,
            });
            false
        }
        CarStatus::Verifying { token: t } if token == Some(*t) => {
            let v = verdict(state, rules, &car, now);
            let train = state.trains.get_mut(base).expect("train");
            match (v, train.base_sha.clone(), car.tree.clone()) {
                (Verdict::Green, Some(expected), Some(tree)) => {
                    train.cars[0].status = CarStatus::Landing;
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
                    });
                    false
                }
                _ => {
                    train.cars[0].status = CarStatus::Running;
                    true
                }
            }
        }
        CarStatus::Failed { code, text } if car.test.is_none() => {
            let (code, text) = (*code, text.clone());
            drop_cars_from(
                state,
                base,
                1,
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
            let failed = train.cars.remove(0);
            close(&failed, out);
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
    let train = state.trains.get_mut(base).expect("train");
    match (train.base_sha.clone(), known) {
        (_, None) => {
            if !train.base_requested {
                train.base_requested = true;
                let token = state.next_id();
                out.push(Decision::Refresh {
                    token,
                    numbers: vec![],
                    branches: vec![base.to_string()],
                });
            }
        }
        (None, Some(sha)) => train.base_sha = Some(sha),
        (Some(old), Some(sha)) if old != sha => {
            train.base_sha = Some(sha.clone());
            // `reset_on_external_merge: never` is not built: Schlange always rebuilds, which
            // keeps I0. See queue-conflicts.md.
            reset(
                state,
                base,
                DequeueReason::assumed(
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
        _ => {}
    }
    // Foreign pushes to queue branches (MQ-08): the tested state no longer holds.
    loop {
        let train = &state.trains[base];
        let foreign = train
            .cars
            .iter()
            .position(|c| match (&c.branch, &c.head_sha) {
                (Some(b), Some(h)) => state.branches.get(b).is_some_and(|s| s != h),
                _ => false,
            });
        let Some(i) = foreign else { break };
        let car = train.cars[i].clone();
        let branch = car.branch.clone().unwrap_or_default();
        drop_cars_from(
            state,
            base,
            i,
            None,
            Some(DequeueReason::new(
                code::BATCH_AHEAD_BRANCH_EDITED,
                format!("someone else pushed to `{branch}` ahead"),
            )),
            &[],
            out,
        );
        if car.test.is_some() {
            bisect::abort(state, base, None, &[], out);
        }
        for n in car.own {
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

pub fn process_trains(
    state: &mut State,
    rules: &Rules,
    now: Time,
    token: Option<u64>,
    out: &mut Vec<Decision>,
) {
    let bases: Vec<String> = state.trains.keys().cloned().collect();
    for base in bases {
        base_and_foreign_pushes(state, rules, &base, out);
        if state.trains[&base].halted.is_some() {
            continue;
        }
        release_held(state, &base);
        for _ in 0..64 {
            evaluate_cars(state, rules, &base, now, out);
            let mut changed = bisect::resolve(state, rules, &base, out);
            changed |= front(state, rules, &base, now, token, out);
            if !changed {
                break;
            }
        }
        fill(state, rules, &base, now, out);
    }
}

fn waiting_order(state: &State, rules: &Rules, base: &str) -> Vec<u64> {
    let rank = |q: &str| {
        rules
            .queue_rules
            .iter()
            .position(|r| r.name == q)
            .unwrap_or(usize::MAX)
    };
    let mut w: Vec<(usize, u64, u64)> = state
        .queued
        .iter()
        .filter(|(_, e)| e.base == base && e.status == EntryStatus::Waiting)
        .map(|(n, e)| (rank(&e.queue), e.seq, *n))
        .collect();
    w.sort();
    w.into_iter().map(|(_, _, n)| n).collect()
}

/// Whether Mergify's rule for checking in place holds for this queue rule.
pub fn in_place_allowed(rules: &Rules, rule: &QueueRule) -> bool {
    rules.merge_queue.max_parallel_checks == 1
        && rules.merge_queue.mode == "serial"
        && rule.batch_size.max() == 1
        && rule.max_checks_retries == 0
        && !rule.two_step()
}

/// Starts at most one car: the next bisection test or the next batch.
fn fill(state: &mut State, rules: &Rules, base: &str, now: Time, out: &mut Vec<Decision>) {
    let slots = rules.merge_queue.max_parallel_checks as usize;
    let train = &state.trains[base];
    let Some(base_sha) = train.base_sha.clone() else {
        return;
    };
    if train.cars.len() >= slots
        || train.cars.iter().any(|c| {
            matches!(
                c.status,
                CarStatus::Building | CarStatus::Failed { .. } | CarStatus::Landing
            )
        })
    {
        return;
    }
    if train.bisection.is_some() {
        bisect::fill(state, rules, base, &base_sha, out);
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
    let group: Vec<u64> = order
        .into_iter()
        .take_while(|n| state.queued[n].queue == queue)
        .collect();
    let total = group.len() as u32;
    let (size, needed) = match rule.batch_size {
        BatchSize::Fixed(n) => (n, n),
        BatchSize::Range { min, max } => (total.div_ceil(slots as u32).clamp(min, max), min),
    };
    if total < needed {
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
    let batch: Vec<u64> = group.into_iter().take(size.max(1) as usize).collect();
    let train = &state.trains[base];
    let mut prs: Vec<CarPr> = train.cars.last().map(|c| c.prs.clone()).unwrap_or_default();
    for n in &batch {
        prs.push(CarPr {
            number: *n,
            head_sha: state.queued[n].head_sha.clone(),
        });
    }
    let no_cars = train.cars.is_empty();
    let id = state.next_id();
    let pr = state.pull_requests.get(&batch[0]).cloned();
    let in_place = in_place_allowed(rules, rule)
        && no_cars
        && batch.len() == 1
        && pr
            .as_ref()
            .is_some_and(|p| !p.behind_base && p.head_tree.is_some());
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
        status: CarStatus::Building,
        test: None,
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
        });
    } else {
        let branch = branch_name(&rule.queue_branch_prefix, base, id);
        car.branch = Some(branch.clone());
        out.push(Decision::BuildCar {
            car: id,
            queue,
            base_branch: base.to_string(),
            branch,
            base_sha,
            prs,
            own: batch.clone(),
        });
    }
    for n in &batch {
        if let Some(e) = state.queued.get_mut(n) {
            e.status = EntryStatus::InCar { car: id };
        }
    }
    state.trains.get_mut(base).expect("train").cars.push(car);
}
