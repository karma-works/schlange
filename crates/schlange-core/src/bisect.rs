//! Splitting a red batch to find the pull request that broke it (docs: merge-queue/batches,
//! "Handling Batch Failure or Timeout"), in our own words:
//!
//! - Of a failed batch of n pull requests, the first n − 1 are cut into parts: as many as
//!   `max_parallel_checks` (at least 2), each of `ceil((n − 1) / parts)` pull requests.
//! - Test i holds parts 1..i on the base. The last pull request is not tested again.
//! - All tests green: the last pull request is the culprit; the others land.
//! - Test f is the first red one: parts before f land, parts after f and the last pull request
//!   go back to the queue, part f is split again (next round). A red part of one pull request
//!   is the culprit.
//! - `batch_max_failure_resolution_attempts` limits the rounds. A red part of several pull
//!   requests in the last round leaves the queue as a whole. With one parallel check, the last
//!   round tests only the first part; the pull requests after it are dropped untested.
//!
//! Every round tests a strict prefix of the batch, so the split always ends (MQ-21).

use serde::Serialize;

use schlange_config::rules::Rules;

use crate::engine::{CarPr, Decision, DequeueReason, EntryStatus, State, dequeue};
use crate::queue::{Car, CarStatus, branch_name, close, requeue, rule};
use crate::reasons as code;

#[derive(Clone, Debug, Serialize)]
pub struct Bisection {
    pub queue: String,
    /// The red batch being split, in queue order.
    pub batch: Vec<u64>,
    pub parts: Vec<Vec<u64>>,
    /// 1 for the split of the batch that failed in the queue.
    pub round: u32,
    pub code: &'static str,
    pub text: String,
    /// Number of tests in this round.
    pub tests: usize,
    /// Last round with one parallel check: only the first part is tested.
    pub only_first: bool,
    /// The test car that lands the passing parts.
    pub land: Option<u64>,
    /// The part to split next, after `land` has landed.
    pub after_land: Option<(Vec<u64>, u32)>,
}

/// Cuts the pull requests into at most `max(slots, 2)` parts of equal size (the last may be
/// shorter).
pub fn split(head: &[u64], slots: usize) -> Vec<Vec<u64>> {
    if head.is_empty() {
        return vec![];
    }
    let parts = slots.max(2);
    let size = head.len().div_ceil(parts);
    head.chunks(size).map(<[u64]>::to_vec).collect()
}

/// The front car `car` (already removed and closed) failed with `code`.
pub(crate) fn batch_failed(
    state: &mut State,
    rules: &Rules,
    base: &str,
    car: &Car,
    code: &'static str,
    text: &str,
    out: &mut Vec<Decision>,
) {
    let limit = rule(rules, &car.queue).and_then(|r| r.batch_max_failure_resolution_attempts);
    if car.own.len() == 1 {
        dequeue(
            state,
            rules,
            car.own[0],
            DequeueReason::new(code, text),
            out,
        );
    } else if limit == Some(0) {
        for &n in &car.own {
            dequeue(
                state,
                rules,
                n,
                DequeueReason::new(
                    code::BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS,
                    format!("the batch failed and is not split (limit 0): {text}"),
                ),
                out,
            );
        }
    } else {
        start(
            state,
            rules,
            base,
            &car.queue,
            car.own.clone(),
            1,
            code,
            text,
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn start(
    state: &mut State,
    rules: &Rules,
    base: &str,
    queue: &str,
    batch: Vec<u64>,
    round: u32,
    code: &'static str,
    text: &str,
) {
    // Pull requests that left the queue meanwhile (while the passing parts landed) are gone;
    // with fewer than two left there is nothing to split, they wait for a new batch.
    let batch: Vec<u64> = batch
        .into_iter()
        .filter(|n| state.queued.contains_key(n))
        .collect();
    if batch.len() < 2 {
        for n in batch {
            if let Some(e) = state.queued.get_mut(&n) {
                e.status = EntryStatus::Waiting;
            }
        }
        return;
    }
    let slots = rules.merge_queue.max_parallel_checks as usize;
    let limit = rule(rules, queue).and_then(|r| r.batch_max_failure_resolution_attempts);
    let last_round = limit.is_some_and(|l| round >= l);
    let only_first = last_round && slots == 1;
    let parts = split(&batch[..batch.len() - 1], slots);
    let tests = if only_first { 1 } else { parts.len() };
    for n in &batch {
        if let Some(e) = state.queued.get_mut(n) {
            e.status = EntryStatus::Bisecting;
        }
    }
    if let Some(t) = state.trains.get_mut(base) {
        t.bisection = Some(Bisection {
            queue: queue.to_string(),
            batch,
            parts,
            round,
            code,
            text: text.to_string(),
            tests,
            only_first,
            land: None,
            after_land: None,
        });
    }
}

/// Ends the bisection: closes its test cars and puts its pull requests (except `skip`) back.
pub(crate) fn abort(
    state: &mut State,
    base: &str,
    reason: Option<DequeueReason>,
    skip: &[u64],
    out: &mut Vec<Decision>,
) {
    let Some(train) = state.trains.get_mut(base) else {
        return;
    };
    let Some(b) = train.bisection.take() else {
        return;
    };
    let tests: Vec<Car> = train
        .cars
        .iter()
        .filter(|c| c.test.is_some())
        .cloned()
        .collect();
    train.cars.retain(|c| c.test.is_none());
    for c in &tests {
        close(c, out);
    }
    for n in b.batch {
        if !skip.contains(&n) {
            requeue(state, n, reason.clone(), out);
        }
    }
}

/// Builds the next test of the round that does not exist yet.
pub(crate) fn fill(
    state: &mut State,
    rules: &Rules,
    base: &str,
    base_sha: &str,
    out: &mut Vec<Decision>,
) {
    let train = &state.trains[base];
    let Some(b) = train.bisection.clone() else {
        return;
    };
    if b.land.is_some() {
        return;
    }
    let Some(i) = (0..b.tests).find(|i| !train.cars.iter().any(|c| c.test == Some(*i))) else {
        return;
    };
    let own: Vec<u64> = b.parts[..=i].concat();
    let prs: Vec<CarPr> = own
        .iter()
        .filter_map(|n| {
            state.queued.get(n).map(|e| CarPr {
                number: *n,
                head_sha: e.head_sha.clone(),
            })
        })
        .collect();
    let id = state.next_id();
    let prefix = rule(rules, &b.queue)
        .map(|r| r.queue_branch_prefix.clone())
        .unwrap_or_else(|| "mergify/merge-queue/".into());
    let branch = branch_name(&prefix, base, id);
    out.push(Decision::BuildCar {
        car: id,
        queue: b.queue.clone(),
        base_branch: base.to_string(),
        branch: branch.clone(),
        base_sha: base_sha.to_string(),
        prs: prs.clone(),
        own: own.clone(),
    });
    state.trains.get_mut(base).expect("train").cars.push(Car {
        id,
        queue: b.queue.clone(),
        branch: Some(branch),
        draft: None,
        in_place: false,
        base_sha: base_sha.to_string(),
        prs,
        own,
        head_sha: None,
        tree: None,
        started: 0,
        status: CarStatus::Building,
        test: Some(i),
    });
}

/// Removes every test car except `keep`.
fn close_tests(state: &mut State, base: &str, keep: Option<u64>, out: &mut Vec<Decision>) {
    let train = state.trains.get_mut(base).expect("train");
    let gone: Vec<Car> = train
        .cars
        .iter()
        .filter(|c| c.test.is_some() && Some(c.id) != keep)
        .cloned()
        .collect();
    train
        .cars
        .retain(|c| c.test.is_none() || Some(c.id) == keep);
    for c in &gone {
        close(c, out);
    }
}

fn back_to_queue(state: &mut State, numbers: &[u64], out: &mut Vec<Decision>) {
    for &n in numbers {
        requeue(
            state,
            n,
            Some(DequeueReason::assumed(
                code::BATCH_AHEAD_FAILED,
                "tested on top of a failing part while the batch was split",
            )),
            out,
        );
    }
}

fn dequeue_part(
    state: &mut State,
    rules: &Rules,
    b: &Bisection,
    part: &[u64],
    out: &mut Vec<Decision>,
) {
    if part.len() == 1 {
        dequeue(
            state,
            rules,
            part[0],
            DequeueReason::new(b.code, format!("found by splitting the batch: {}", b.text)),
            out,
        );
    } else {
        for &n in part {
            dequeue(
                state,
                rules,
                n,
                DequeueReason::new(
                    code::BATCH_MAX_FAILURE_RESOLUTION_ATTEMPTS,
                    format!("the failing part was not split further (round {})", b.round),
                ),
                out,
            );
        }
    }
}

/// Lets test car `car` land the passing parts; `next` is split afterwards.
fn land_test(state: &mut State, base: &str, car: u64, next: Option<(Vec<u64>, u32)>) {
    let train = state.trains.get_mut(base).expect("train");
    if let Some(b) = train.bisection.as_mut() {
        b.land = Some(car);
        b.after_land = next;
    }
    let train = &state.trains[base];
    let own = train
        .cars
        .iter()
        .find(|c| c.id == car)
        .map(|c| c.own.clone())
        .unwrap_or_default();
    for n in own {
        if let Some(e) = state.queued.get_mut(&n) {
            e.status = EntryStatus::InCar { car };
        }
    }
}

/// Decides a round once its results allow it. Returns whether the train changed.
pub(crate) fn resolve(
    state: &mut State,
    rules: &Rules,
    base: &str,
    out: &mut Vec<Decision>,
) -> bool {
    let train = &state.trains[base];
    let Some(b) = train.bisection.clone() else {
        return false;
    };
    if b.land.is_some() {
        return false;
    }
    let test = |i: usize| train.cars.iter().find(|c| c.test == Some(i)).cloned();
    let slots = rules.merge_queue.max_parallel_checks as usize;
    let mut first_fail = None;
    let mut green = 0;
    for i in 0..b.tests {
        match test(i).map(|c| c.status) {
            Some(CarStatus::Green) => green += 1,
            Some(CarStatus::Failed { .. }) => {
                first_fail = Some(i);
                break;
            }
            None if green > 0 && train.cars.len() >= slots => {
                // Fewer slots than tests: the green prefix holds the slot the next test needs.
                // Land it; the rest (still red on top of it) is split again in this round.
                let keep = test(green - 1).map(|c| c.id).expect("green test");
                let landed: Vec<u64> = b.parts[..green].concat();
                let rest: Vec<u64> = b
                    .batch
                    .iter()
                    .copied()
                    .filter(|n| !landed.contains(n))
                    .collect();
                close_tests(state, base, Some(keep), out);
                let next = (rest.len() > 1).then(|| (rest.clone(), b.round));
                land_test(state, base, keep, next);
                if rest.len() == 1 {
                    if let Some(b) = state
                        .trains
                        .get_mut(base)
                        .and_then(|t| t.bisection.as_mut())
                    {
                        b.batch.retain(|n| landed.contains(n));
                    }
                    dequeue(
                        state,
                        rules,
                        rest[0],
                        DequeueReason::new(
                            b.code,
                            format!("every split without it passed: {}", b.text),
                        ),
                        out,
                    );
                }
                return true;
            }
            _ => return false,
        }
    }
    let last = *b.batch.last().expect("non-empty batch");
    let limit = rule(rules, &b.queue).and_then(|r| r.batch_max_failure_resolution_attempts);
    // Before anything leaves the queue, the bisection keeps only the pull requests it still
    // owns (the landing test's and the part to split next); otherwise a dequeue below would
    // abort it.
    let narrow = |state: &mut State, keep: &[u64]| {
        if let Some(b) = state
            .trains
            .get_mut(base)
            .and_then(|t| t.bisection.as_mut())
        {
            b.batch.retain(|n| keep.contains(n));
        }
    };
    if b.only_first {
        let first = b.parts[0].clone();
        let rest: Vec<u64> = b.batch[first.len()..].to_vec();
        match first_fail {
            None => {
                let keep = test(0).map(|c| c.id);
                close_tests(state, base, keep, out);
                narrow(state, &first);
                land_test(state, base, keep.expect("test 0"), None);
                for n in rest {
                    dequeue(
                        state,
                        rules,
                        n,
                        DequeueReason::new(
                            code::DROPPED_BY_BISECTION_ELIMINATION,
                            "dropped untested: the last round left no test to tell them apart",
                        ),
                        out,
                    );
                }
            }
            Some(_) => {
                close_tests(state, base, None, out);
                state.trains.get_mut(base).expect("train").bisection = None;
                dequeue_part(state, rules, &b, &first, out);
                back_to_queue(state, &rest, out);
            }
        }
        return true;
    }
    match first_fail {
        None => {
            let keep = test(b.tests - 1).map(|c| c.id);
            close_tests(state, base, keep, out);
            narrow(state, &b.batch[..b.batch.len() - 1]);
            match keep {
                Some(k) => land_test(state, base, k, None),
                None => state.trains.get_mut(base).expect("train").bisection = None,
            }
            dequeue(
                state,
                rules,
                last,
                DequeueReason::new(b.code, format!("every split without it passed: {}", b.text)),
                out,
            );
        }
        Some(f) => {
            let keep = if f > 0 {
                test(f - 1).map(|c| c.id)
            } else {
                None
            };
            close_tests(state, base, keep, out);
            let mut after: Vec<u64> = b.parts[f + 1..].concat();
            after.push(last);
            let part = b.parts[f].clone();
            let split_again = part.len() > 1 && !limit.is_some_and(|l| b.round >= l);
            let next = split_again.then(|| (part.clone(), b.round + 1));
            let mut owned: Vec<u64> = b.parts[..f].concat();
            if split_again {
                owned.extend(&part);
            }
            narrow(state, &owned);
            match keep {
                Some(k) => land_test(state, base, k, next.clone()),
                None => state.trains.get_mut(base).expect("train").bisection = None,
            }
            back_to_queue(state, &after, out);
            if !split_again {
                dequeue_part(state, rules, &b, &part, out);
            }
            if keep.is_none()
                && let Some((part, round)) = next
            {
                start(state, rules, base, &b.queue, part, round, b.code, &b.text);
            }
        }
    }
    true
}

/// The passing parts landed; split the failing part, or end.
pub(crate) fn after_landing(state: &mut State, rules: &Rules, base: &str, _out: &mut [Decision]) {
    let Some(b) = state.trains.get_mut(base).and_then(|t| t.bisection.take()) else {
        return;
    };
    if let Some((part, round)) = b.after_land {
        start(state, rules, base, &b.queue, part, round, b.code, &b.text);
    }
}

#[cfg(test)]
mod tests {
    use super::split;

    #[test]
    fn splits_as_documented() {
        // 13 with 3 checks: 12 cut into 3 parts of 4.
        let b: Vec<u64> = (1..=12).collect();
        assert_eq!(
            split(&b, 3),
            vec![vec![1, 2, 3, 4], vec![5, 6, 7, 8], vec![9, 10, 11, 12]]
        );
        // 6 with 3: parts of 2, the last one short.
        assert_eq!(
            split(&[1, 2, 3, 4, 5], 3),
            vec![vec![1, 2], vec![3, 4], vec![5]]
        );
        // 5 with 3: only two parts.
        assert_eq!(split(&[1, 2, 3, 4], 3), vec![vec![1, 2], vec![3, 4]]);
        // Never fewer than two parts, also with one check.
        assert_eq!(split(&[1, 2, 3, 4], 1), vec![vec![1, 2], vec![3, 4]]);
        assert_eq!(split(&[1], 5), vec![vec![1]]);
    }
}
