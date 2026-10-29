//! Read-only watchdog observation tracking.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_two_matching_fresh_observations_and_backs_off() {
        let mut w = Watchdog::default();
        assert!(!w.observe(1, Some("car-1"), 10, 60));
        assert!(!w.observe(1, Some("car-1"), 11, 60));
        assert!(!w.observe(1, Some("car-1"), 9, 60));
        assert!(!w.observe(2, Some("car-1"), 70, 60)); // generation changed
        assert!(!w.observe(2, Some("car-2"), 130, 60)); // car changed
        assert!(w.observe(2, Some("car-2"), 190, 60));
        assert!(!w.observe(2, Some("car-2"), 200, 60)); // bounded retries
        assert!(w.observe(2, Some("car-2"), 250, 60));
        assert!(!w.observe(2, None, 310, 60)); // read failure / not actionable
        assert!(!w.observe(2, Some("car-2"), 370, 60));
        assert!(
            !w.observe(2, Some("car-2"), 370 + PROBE_MAX_AGE_SECS + 61, 60),
            "old first observation must expire"
        );
    }

    #[test]
    fn rotation_does_not_stick_on_changing_actionable_state() {
        let mut r = Rotation::default();
        r.advance(true, false);
        assert_eq!(r.cursor, 0);
        r.advance(true, false);
        assert_eq!(r.cursor, 1);
        r.advance(false, false);
        assert_eq!(r.cursor, 2);
        r.advance(true, true);
        assert_eq!(r.cursor, 3);
    }

    #[test]
    fn resumable_reads_are_bounded_and_discarded_on_generation_change() {
        use schlange_core::model::PullRequest;
        use std::cell::Cell;
        use std::time::{Duration, Instant};
        struct Fake {
            allowance: Cell<usize>,
            calls: Cell<usize>,
        }
        impl ProbeReader for Fake {
            fn pr(
                &self,
                n: u64,
                _: &std::sync::Arc<crate::client::ProbeMemo>,
            ) -> Result<PullRequest, crate::client::Error> {
                if self.allowance.get() == 0 {
                    return Err(crate::client::Error::Deferred { path: "pr".into() });
                }
                self.allowance.set(self.allowance.get() - 1);
                self.calls.set(self.calls.get() + 1);
                Ok(PullRequest::new(n, "a", "main", "p", "h"))
            }
            fn branch(
                &self,
                _: &str,
                _: &std::sync::Arc<crate::client::ProbeMemo>,
            ) -> Result<Option<String>, crate::client::Error> {
                Ok(Some("base".into()))
            }
        }
        let reader = Fake {
            allowance: Cell::new(1),
            calls: Cell::new(0),
        };
        let now = Cell::new(Instant::now());
        let clock = || now.get();
        let plan = || ([1, 2].into(), ["main".to_string()].into());
        let mut probe = Probe::default();
        let key = ProbeKey {
            generation: 1,
            base: "main".into(),
            car: 7,
        };
        assert!(matches!(
            probe.read(key.clone(), plan(), &clock, &|| true, &reader),
            ProbeResult::Pending(_)
        ));
        assert_eq!(reader.calls.get(), 1);
        reader.allowance.set(1);
        assert!(matches!(
            probe.read(key.clone(), plan(), &clock, &|| true, &reader),
            ProbeResult::Ready(_, _)
        ));
        assert_eq!(
            reader.calls.get(),
            2,
            "completed PR must not be fetched twice"
        );
        // A new observation must start over, and a changed generation discards its partial read.
        reader.allowance.set(1);
        assert!(matches!(
            probe.read(key.clone(), plan(), &clock, &|| true, &reader),
            ProbeResult::Pending(_)
        ));
        reader.allowance.set(1);
        let changed = ProbeKey {
            generation: 2,
            ..key.clone()
        };
        assert!(matches!(
            probe.read(changed, plan(), &clock, &|| true, &reader),
            ProbeResult::Pending(_)
        ));
        // The clock is measured from the first request, not extended when progress occurs.
        now.set(now.get() + Duration::from_secs(PROBE_MAX_AGE_SECS + 1));
        assert!(matches!(
            probe.read(key, plan(), &clock, &|| true, &reader),
            ProbeResult::Invalid
        ));
    }

    #[test]
    fn expiration_during_a_single_pull_read_discards_the_entire_probe() {
        use std::cell::Cell;
        use std::time::{Duration, Instant};
        struct Slow<'a>(&'a Cell<Instant>);
        impl ProbeReader for Slow<'_> {
            fn pr(
                &self,
                n: u64,
                _: &std::sync::Arc<crate::client::ProbeMemo>,
            ) -> Result<schlange_core::model::PullRequest, crate::client::Error> {
                self.0
                    .set(self.0.get() + Duration::from_secs(PROBE_MAX_AGE_SECS + 1));
                Ok(schlange_core::model::PullRequest::new(
                    n, "a", "main", "p", "h",
                ))
            }
            fn branch(
                &self,
                _: &str,
                _: &std::sync::Arc<crate::client::ProbeMemo>,
            ) -> Result<Option<String>, crate::client::Error> {
                unreachable!()
            }
        }
        let now = Cell::new(Instant::now());
        let mut probe = Probe::default();
        let key = ProbeKey {
            generation: 1,
            base: "main".into(),
            car: 1,
        };
        assert!(matches!(
            probe.read(
                key,
                ([1].into(), ["main".into()].into()),
                &|| now.get(),
                &|| true,
                &Slow(&now)
            ),
            ProbeResult::Invalid
        ));
        assert!(probe.partial.is_none());
    }

    #[test]
    fn partial_probes_for_different_cars_do_not_starve_or_mix() {
        let mut probes = Probes::default();
        let now = std::time::Instant::now();
        let a = ProbeKey {
            generation: 1,
            base: "a".into(),
            car: 1,
        };
        let b = ProbeKey {
            generation: 1,
            base: "b".into(),
            car: 2,
        };
        probes.get(&a, now);
        probes.get(&b, now);
        assert_eq!(probes.entries.len(), 2);
        let c = ProbeKey {
            generation: 2,
            ..a.clone()
        };
        probes.get(&c, now);
        assert_eq!(
            probes.entries.len(),
            1,
            "generation change clears every partial"
        );
        assert!(!probes.entries.contains_key(&b));
    }

    #[test]
    fn partial_probe_cache_evicts_oldest_not_lexically_first() {
        use std::time::{Duration, Instant};
        let mut probes = Probes::default();
        let start = Instant::now();
        let oldest = ProbeKey {
            generation: 1,
            base: "z".into(),
            car: 1,
        };
        probes.get(&oldest, start);
        for n in 0..63 {
            let key = ProbeKey {
                generation: 1,
                base: format!("a{n:02}"),
                car: n,
            };
            probes.get(&key, start + Duration::from_secs(n + 1));
        }
        let next = ProbeKey {
            generation: 1,
            base: "new".into(),
            car: 1,
        };
        probes.get(&next, start + Duration::from_secs(64));
        assert_eq!(probes.entries.len(), 64);
        assert!(!probes.entries.contains_key(&oldest));
        assert!(probes.entries.contains_key(&ProbeKey {
            generation: 1,
            base: "a00".into(),
            car: 0
        }));
    }

    #[test]
    fn candidate_obeys_checks_and_pause_without_mutating_state() {
        use schlange_core::model::{CheckRun, Conclusion, PullRequest};
        use schlange_core::{Decision, Event, State, step};
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\n    queue_conditions: []\n    merge_conditions: [check-success = full]\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let pr = PullRequest::new(1, "a", "main", "pr-1", "h1");
        let snap = Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".into(), Some("b0".into()))].into(),
            token: None,
        };
        let (state, decisions) = step(&State::default(), &rules, &snap, 0);
        let (car, branch) = decisions
            .iter()
            .find_map(|d| match d {
                Decision::BuildCar { car, branch, .. } => Some((*car, branch.clone())),
                _ => None,
            })
            .unwrap();
        let (state, _) = step(
            &state,
            &rules,
            &Event::CarBuilt {
                car,
                branch,
                head_sha: "c1".into(),
                tree: "t1".into(),
                draft: 1000,
            },
            0,
        );
        let mut draft = PullRequest::new(1000, "bot", "main", "queue", "c1");
        let pending = Event::Snapshot {
            pull_requests: vec![draft.clone()],
            branches: Default::default(),
            token: None,
        };
        assert!(candidate_from_snapshot(&state, &rules, &pending, 0, None).is_none());
        draft.checks.push(CheckRun {
            name: "full".into(),
            head_sha: "c1".into(),
            conclusion: Some(Conclusion::Success),
            ..Default::default()
        });
        let green = Event::Snapshot {
            pull_requests: vec![draft],
            branches: Default::default(),
            token: None,
        };
        let before = serde_json::to_value(&state).unwrap();
        assert!(candidate_from_snapshot(&state, &rules, &green, 0, None).is_some());
        assert_eq!(before, serde_json::to_value(&state).unwrap());
        let mut paused = state.clone();
        paused.pause = Some(schlange_core::engine::Pause {
            reason: "maintenance".into(),
            at: 0,
            by: None,
        });
        assert!(candidate_from_snapshot(&paused, &rules, &green, 0, None).is_none());
        let mut frozen = state.clone();
        frozen.freezes.insert(
            1,
            schlange_core::engine::Freeze {
                id: 1,
                reason: "maintenance".into(),
                start: 0,
                end: None,
                matching: vec![],
                exclude: vec![],
            },
        );
        assert!(candidate_from_snapshot(&frozen, &rules, &green, 0, None).is_none());
    }

    #[test]
    fn green_car_behind_pending_car_is_not_actionable() {
        use schlange_core::model::{CheckRun, Conclusion, PullRequest};
        use schlange_core::{Decision, Event, State, step};
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\n    queue_conditions: []\n    merge_conditions: [check-success = full]\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let snap = |n| Event::Snapshot {
            pull_requests: vec![PullRequest::new(
                n,
                "a",
                "main",
                &format!("pr-{n}"),
                &format!("h{n}"),
            )],
            branches: [("main".into(), Some("b0".into()))].into(),
            token: None,
        };
        let (mut state, first) = step(&State::default(), &rules, &snap(1), 0);
        let build = |decisions: &[Decision]| {
            decisions
                .iter()
                .find_map(|d| match d {
                    Decision::BuildCar { car, branch, .. } => Some((*car, branch.clone())),
                    _ => None,
                })
                .expect("car built")
        };
        let (car1, branch1) = build(&first);
        (state, _) = step(
            &state,
            &rules,
            &Event::CarBuilt {
                car: car1,
                branch: branch1,
                head_sha: "c1".into(),
                tree: "t1".into(),
                draft: 1001,
            },
            0,
        );
        let (new_state, second) = step(&state, &rules, &snap(2), 0);
        state = new_state;
        let (car2, branch2) = build(&second);
        (state, _) = step(
            &state,
            &rules,
            &Event::CarBuilt {
                car: car2,
                branch: branch2,
                head_sha: "c2".into(),
                tree: "t2".into(),
                draft: 1002,
            },
            0,
        );
        assert_eq!(targets(&state), vec![("main".to_string(), car1)]);
        let plan = read_plan(&state, "main", car1).unwrap();
        assert!(plan.0.contains(&1) && plan.0.contains(&1001));
        assert!(!plan.0.contains(&2) && !plan.0.contains(&1002));
        let first_draft = PullRequest::new(1001, "bot", "main", "queue-1", "c1");
        let mut second_draft = PullRequest::new(1002, "bot", "main", "queue-2", "c2");
        second_draft.checks.push(CheckRun {
            name: "full".into(),
            head_sha: "c2".into(),
            conclusion: Some(Conclusion::Success),
            ..Default::default()
        });
        let event = Event::Snapshot {
            pull_requests: vec![first_draft, second_draft],
            branches: Default::default(),
            token: None,
        };
        assert!(
            candidate_from_snapshot(&state, &rules, &event, 0, None).is_none(),
            "second car must wait for first"
        );
    }
}

#[derive(Default)]
pub(crate) struct Watchdog {
    previous: Option<(u64, String, u64)>,
    last_reconcile: Option<u64>,
}

const PROBE_MAX_AGE_SECS: u64 = 600;

#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ProbeKey {
    generation: u64,
    base: String,
    car: u64,
}

struct PartialProbe {
    key: ProbeKey,
    started: std::time::Instant,
    numbers: std::collections::VecDeque<u64>,
    branches: std::collections::VecDeque<String>,
    pulls: Vec<schlange_core::model::PullRequest>,
    refs: std::collections::BTreeMap<String, Option<String>>,
    memo: std::sync::Arc<crate::client::ProbeMemo>,
}

#[derive(Default)]
pub(crate) struct Probe {
    partial: Option<PartialProbe>,
    total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

#[derive(Default)]
pub(crate) struct Probes {
    generation: Option<u64>,
    entries: std::collections::BTreeMap<ProbeKey, CachedProbe>,
    total: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

struct CachedProbe {
    inserted: std::time::Instant,
    probe: Probe,
}

impl Probes {
    pub(crate) fn with_total(total: std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self {
            generation: None,
            entries: Default::default(),
            total,
        }
    }
    fn get(&mut self, key: &ProbeKey, now: std::time::Instant) -> &mut Probe {
        if self.generation != Some(key.generation) {
            self.entries.clear();
            self.generation = Some(key.generation);
        }
        self.entries.retain(|_, p| {
            p.probe.partial.as_ref().is_none_or(|p| {
                now >= p.started && now.duration_since(p.started).as_secs() <= PROBE_MAX_AGE_SECS
            })
        });
        if !self.entries.contains_key(key)
            && self.entries.len() >= 64
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, p)| p.inserted)
                .map(|(k, _)| k.clone())
        {
            self.entries.remove(&oldest);
        }
        let total = self.total.clone();
        &mut self
            .entries
            .entry(key.clone())
            .or_insert_with(|| CachedProbe {
                inserted: now,
                probe: Probe {
                    partial: None,
                    total,
                },
            })
            .probe
    }
}

enum ProbeResult {
    Pending(usize),
    Ready(Box<schlange_core::Event>, std::time::Instant),
    Invalid,
}

trait ProbeReader {
    fn pr(
        &self,
        n: u64,
        memo: &std::sync::Arc<crate::client::ProbeMemo>,
    ) -> Result<schlange_core::model::PullRequest, crate::client::Error>;
    fn branch(
        &self,
        name: &str,
        memo: &std::sync::Arc<crate::client::ProbeMemo>,
    ) -> Result<Option<String>, crate::client::Error>;
}

impl ProbeReader for crate::forge::GithubForge {
    fn pr(
        &self,
        n: u64,
        memo: &std::sync::Arc<crate::client::ProbeMemo>,
    ) -> Result<schlange_core::model::PullRequest, crate::client::Error> {
        self.with_probe_memo(memo.clone())
            .read_pr(n, crate::client::Priority::Background)
    }
    fn branch(
        &self,
        name: &str,
        memo: &std::sync::Arc<crate::client::ProbeMemo>,
    ) -> Result<Option<String>, crate::client::Error> {
        self.with_probe_memo(memo.clone())
            .read_branch_checked(name, crate::client::Priority::Background)
    }
}

impl Probe {
    fn read(
        &mut self,
        key: ProbeKey,
        plan: (
            std::collections::BTreeSet<u64>,
            std::collections::BTreeSet<String>,
        ),
        clock: &dyn Fn() -> std::time::Instant,
        valid: &dyn Fn() -> bool,
        reader: &dyn ProbeReader,
    ) -> ProbeResult {
        let fresh = |started: std::time::Instant| {
            let now = clock();
            now >= started && now.duration_since(started).as_secs() <= PROBE_MAX_AGE_SECS && valid()
        };
        if self.partial.as_ref().is_some_and(|p| !fresh(p.started)) {
            self.partial = None;
            return ProbeResult::Invalid;
        }
        if self.partial.as_ref().is_none_or(|p| p.key != key) {
            self.partial = Some(PartialProbe {
                key,
                started: clock(),
                numbers: plan.0.into_iter().collect(),
                branches: plan.1.into_iter().collect(),
                pulls: vec![],
                refs: Default::default(),
                memo: std::sync::Arc::new(crate::client::ProbeMemo::new(self.total.clone())),
            });
        }
        let mut p = self.partial.take().expect("probe initialized");
        loop {
            if !fresh(p.started) {
                return ProbeResult::Invalid;
            }
            if let Some(n) = p.numbers.front().copied() {
                match reader.pr(n, &p.memo) {
                    Ok(pr) if fresh(p.started) => {
                        p.pulls.push(pr);
                        p.numbers.pop_front();
                    }
                    Err(crate::client::Error::Deferred { .. }) if fresh(p.started) => {
                        let completed = p.pulls.len();
                        self.partial = Some(p);
                        return ProbeResult::Pending(completed);
                    }
                    _ => return ProbeResult::Invalid,
                }
            } else if let Some(name) = p.branches.front().cloned() {
                match reader.branch(&name, &p.memo) {
                    Ok(sha) if fresh(p.started) => {
                        p.refs.insert(name, sha);
                        p.branches.pop_front();
                    }
                    Err(crate::client::Error::Deferred { .. }) if fresh(p.started) => {
                        let completed = p.pulls.len();
                        self.partial = Some(p);
                        return ProbeResult::Pending(completed);
                    }
                    _ => return ProbeResult::Invalid,
                }
            } else {
                return ProbeResult::Ready(
                    Box::new(schlange_core::Event::Snapshot {
                        pull_requests: p.pulls,
                        branches: p.refs,
                        token: None,
                    }),
                    p.started + std::time::Duration::from_secs(PROBE_MAX_AGE_SECS),
                );
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct Rotation {
    pub(crate) cursor: usize,
    attempts: u8,
}

impl Rotation {
    pub(crate) fn advance(&mut self, actionable: bool, reconciled: bool) {
        self.attempts = if actionable {
            self.attempts.saturating_add(1)
        } else {
            0
        };
        if !actionable || reconciled || self.attempts >= 2 {
            self.cursor = self.cursor.wrapping_add(1);
            self.attempts = 0;
        }
    }
}

impl Watchdog {
    pub(crate) fn observe(
        &mut self,
        generation: u64,
        candidate: Option<&str>,
        now: u64,
        interval: u64,
    ) -> bool {
        let Some(candidate) = candidate else {
            self.previous = None;
            return false;
        };
        let repeated = self.previous.as_ref().is_some_and(|(g, c, first)| {
            *g == generation
                && c == candidate
                && now >= *first
                && now - *first >= interval
                && now - *first <= PROBE_MAX_AGE_SECS + interval
        });
        if self.previous.as_ref().is_none_or(|(g, c, first)| {
            *g != generation
                || c != candidate
                || now < *first
                || now - *first > PROBE_MAX_AGE_SECS + interval
        }) {
            self.previous = Some((generation, candidate.to_string(), now));
        }
        if repeated
            && self
                .last_reconcile
                .is_none_or(|last| now.saturating_sub(last) >= interval)
        {
            self.last_reconcile = Some(now);
            true
        } else {
            false
        }
    }
}

fn targets(state: &schlange_core::State) -> Vec<(String, u64)> {
    use schlange_core::queue::{CarStatus, has_live_deps};
    state
        .trains
        .iter()
        .flat_map(|(base, train)| {
            train
                .cars
                .iter()
                .enumerate()
                .filter(|(i, car)| {
                    matches!(car.status, CarStatus::Running) && !has_live_deps(train, *i)
                })
                .map(|(_, car)| (base.clone(), car.id))
        })
        .collect()
}

fn read_plan(
    state: &schlange_core::State,
    base: &str,
    id: u64,
) -> Option<(
    std::collections::BTreeSet<u64>,
    std::collections::BTreeSet<String>,
)> {
    let car = state.trains.get(base)?.cars.iter().find(|c| c.id == id)?;
    let mut numbers = std::collections::BTreeSet::new();
    numbers.extend(car.prs.iter().map(|p| p.number));
    numbers.extend(car.own.iter().copied());
    numbers.extend(car.draft);
    let mut branches = std::collections::BTreeSet::from([base.to_string()]);
    branches.extend(car.branch.iter().cloned());
    Some((numbers, branches))
}

/// Inspect one car's independent read. Dry-run decisions are discarded; the caller may only
/// request ordinary reconciliation. Missing reads return no candidate.
pub(crate) enum Sample {
    Pending(usize),
    Done(Option<String>, Option<std::time::Instant>),
}

pub(crate) struct CandidateInput<'a> {
    pub state: &'a schlange_core::State,
    pub rules: &'a schlange_config::rules::Rules,
    pub now: &'a dyn Fn() -> u64,
    pub generation: u64,
    pub cursor: usize,
    pub clock: &'a dyn Fn() -> std::time::Instant,
    pub valid: &'a dyn Fn() -> bool,
}

pub(crate) fn candidate(
    forge: &crate::forge::GithubForge,
    probes: &mut Probes,
    input: CandidateInput<'_>,
) -> Sample {
    let CandidateInput {
        state,
        rules,
        now,
        generation,
        cursor,
        clock,
        valid,
    } = input;
    let choices = targets(state);
    let Some((base, car)) = cursor
        .checked_rem(choices.len())
        .and_then(|i| choices.get(i))
    else {
        probes.entries.clear();
        return Sample::Done(None, None);
    };
    let Some(plan) = read_plan(state, base, *car) else {
        probes.entries.clear();
        return Sample::Done(None, None);
    };
    let key = ProbeKey {
        generation,
        base: base.clone(),
        car: *car,
    };
    let result = probes
        .get(&key, clock())
        .read(key.clone(), plan, clock, valid, forge);
    if !matches!(result, ProbeResult::Pending(_)) {
        probes.entries.remove(&key);
    }
    match result {
        ProbeResult::Pending(completed) => Sample::Pending(completed),
        ProbeResult::Invalid => Sample::Done(None, None),
        ProbeResult::Ready(event, deadline) => Sample::Done(
            candidate_from_snapshot(state, rules, &event, now(), Some((base, *car))),
            Some(deadline),
        ),
    }
}

fn candidate_from_snapshot(
    state: &schlange_core::State,
    rules: &schlange_config::rules::Rules,
    event: &schlange_core::Event,
    now: u64,
    only: Option<(&str, u64)>,
) -> Option<String> {
    use schlange_core::Decision;
    use schlange_core::queue::CarStatus;
    let (next, decisions) = schlange_core::step(state, rules, event, now);
    let mut ready = Vec::new();
    for (base, train) in &state.trains {
        for car in &train.cars {
            if only.is_some_and(|(b, id)| b != base || id != car.id) {
                continue;
            }
            if !matches!(car.status, CarStatus::Running) {
                continue;
            }
            if let Some(next_car) = next
                .trains
                .get(base)
                .and_then(|t| t.cars.iter().find(|c| c.id == car.id))
                && let CarStatus::Verifying { token, .. } = next_car.status
                && decisions
                    .iter()
                    .any(|d| matches!(d, Decision::Refresh { token: t, .. } if *t == token))
            {
                ready.push(format!(
                    "{base}:{}:{}",
                    car.id,
                    car.head_sha.as_deref().unwrap_or("")
                ));
            }
        }
    }
    if ready.is_empty() {
        None
    } else {
        Some(ready.join(","))
    }
}
