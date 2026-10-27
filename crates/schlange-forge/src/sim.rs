//! The simulated forge: pull requests and check runs in memory, a virtual clock, and CI as a
//! script (duration and conclusion per check). Fault injection (lost, duplicated, reordered
//! webhooks; 5xx; delayed reads) is not built yet.

use std::collections::{BTreeMap, BTreeSet};

use schlange_core::model::{CheckRun, Conclusion, PullRequest, Time};
use schlange_core::{Decision, Event};

use crate::{Forge, ForgeError};

/// A virtual clock that only moves when the simulation advances it.
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtualClock {
    now: Time,
}

impl VirtualClock {
    pub fn now(&self) -> Time {
        self.now
    }

    /// Moves forward to `t`; the clock never goes back.
    pub fn advance_to(&mut self, t: Time) {
        self.now = self.now.max(t);
    }
}

/// How one CI check behaves on a pull request.
#[derive(Clone, Debug)]
pub struct CiScript {
    pub check: String,
    pub app: Option<String>,
    pub duration: Time,
    /// `None`: the check never reports.
    pub conclusion: Option<Conclusion>,
}

/// Things a scenario does to the forge at a point in time.
#[derive(Clone, Debug)]
pub enum Action {
    Open(Box<PullRequest>, Vec<CiScript>),
    Push {
        number: u64,
        files: Option<Vec<String>>,
    },
    Label {
        number: u64,
        label: String,
    },
    Unlabel {
        number: u64,
        label: String,
    },
    Draft {
        number: u64,
    },
    Ready {
        number: u64,
    },
    Close {
        number: u64,
    },
}

enum Pending {
    Act(Action),
    Deliver(Event),
}

/// What the simulated forge recorded from the engine's decisions.
#[derive(Clone, Debug, Default)]
pub struct QueuePlaceholder {
    /// Pull requests the engine queued, by queue name. No queue pull requests, no merges yet.
    pub queued: BTreeMap<u64, String>,
}

#[derive(Default)]
pub struct SimForge {
    clock: VirtualClock,
    pending: BTreeMap<(Time, u64), Pending>,
    order: u64,
    check_seq: u64,
    pull_requests: BTreeMap<u64, PullRequest>,
    ci: BTreeMap<u64, Vec<CiScript>>,
    pushes: BTreeMap<u64, u32>,
    pub queue: QueuePlaceholder,
    pub executed: Vec<(Time, Decision)>,
}

impl SimForge {
    pub fn new() -> Self {
        Self::default()
    }

    /// Schedules a scenario action at `at`.
    pub fn schedule(&mut self, at: Time, action: Action) {
        self.push_pending(at, Pending::Act(action));
    }

    fn push_pending(&mut self, at: Time, p: Pending) {
        self.order += 1;
        self.pending.insert((at, self.order), p);
    }

    fn deliver(&mut self, at: Time, event: Event) {
        self.push_pending(at, Pending::Deliver(event));
    }

    /// Starts the CI scripts of a pull request on its current head.
    fn start_ci(&mut self, number: u64, at: Time) {
        let Some(pr) = self.pull_requests.get(&number) else {
            return;
        };
        let head = pr.head_sha.clone();
        for script in self.ci.get(&number).cloned().unwrap_or_default() {
            self.check_seq += 1;
            let mut run = CheckRun {
                name: script.check.clone(),
                app: script.app.clone(),
                head_sha: head.clone(),
                conclusion: None,
                seq: self.check_seq,
            };
            if let Some(pr) = self.pull_requests.get_mut(&number) {
                pr.checks.push(run.clone());
            }
            self.deliver(
                at,
                Event::CheckRun {
                    number,
                    run: run.clone(),
                },
            );
            if let Some(c) = script.conclusion {
                run.conclusion = Some(c);
                self.deliver(at + script.duration, Event::CheckRun { number, run });
            }
        }
    }

    fn act(&mut self, at: Time, action: Action) -> Option<Event> {
        match action {
            Action::Open(pr, ci) => {
                let number = pr.number;
                self.ci.insert(number, ci);
                self.pull_requests.insert(number, (*pr).clone());
                self.start_ci(number, at);
                Some(Event::PullRequestOpened { pull_request: *pr })
            }
            Action::Push { number, files } => {
                let n = self.pushes.entry(number).or_default();
                *n += 1;
                let pr = self.pull_requests.get_mut(&number)?;
                pr.head_sha = format!("{}-{}", pr.head_ref, n);
                if let Some(f) = files {
                    pr.files = f;
                }
                let ev = Event::PullRequestSynchronized {
                    number,
                    head_sha: pr.head_sha.clone(),
                    files: pr.files.clone(),
                };
                self.start_ci(number, at);
                Some(ev)
            }
            Action::Label { number, label } => {
                self.pull_requests
                    .get_mut(&number)?
                    .labels
                    .insert(label.clone());
                Some(Event::Labeled { number, label })
            }
            Action::Unlabel { number, label } => {
                self.pull_requests.get_mut(&number)?.labels.remove(&label);
                Some(Event::Unlabeled { number, label })
            }
            Action::Draft { number } => {
                self.pull_requests.get_mut(&number)?.draft = true;
                Some(Event::ConvertedToDraft { number })
            }
            Action::Ready { number } => {
                self.pull_requests.get_mut(&number)?.draft = false;
                Some(Event::ReadyForReview { number })
            }
            Action::Close { number } => {
                self.pull_requests.get_mut(&number)?.closed = true;
                Some(Event::Closed { number })
            }
        }
    }

    /// Pull request numbers known to the forge.
    pub fn numbers(&self) -> BTreeSet<u64> {
        self.pull_requests.keys().copied().collect()
    }
}

impl Forge for SimForge {
    fn now(&self) -> Time {
        self.clock.now()
    }

    fn next_event(&mut self) -> Option<(Time, Event)> {
        loop {
            let (&key, _) = self.pending.iter().next()?;
            let pending = self.pending.remove(&key)?;
            self.clock.advance_to(key.0);
            match pending {
                Pending::Deliver(ev) => {
                    if let Event::CheckRun { number, run } = &ev
                        && let Some(pr) = self.pull_requests.get_mut(number)
                    {
                        match pr.checks.iter_mut().find(|c| c.seq == run.seq) {
                            Some(existing) => *existing = run.clone(),
                            None => pr.checks.push(run.clone()),
                        }
                    }
                    return Some((key.0, ev));
                }
                Pending::Act(action) => {
                    if let Some(ev) = self.act(key.0, action) {
                        return Some((key.0, ev));
                    }
                }
            }
        }
    }

    fn pull_request(&self, number: u64) -> Option<PullRequest> {
        self.pull_requests.get(&number).cloned()
    }

    fn execute(&mut self, decision: &Decision) -> Result<(), ForgeError> {
        match decision {
            Decision::Enqueue { number, queue, .. } => {
                if !self.pull_requests.contains_key(number) {
                    return Err(ForgeError::UnknownPullRequest(*number));
                }
                self.queue.queued.insert(*number, queue.clone());
            }
            Decision::Dequeue { number, .. } => {
                self.queue.queued.remove(number);
            }
            Decision::EvaluationError { .. } => {}
        }
        self.executed.push((self.clock.now(), decision.clone()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_only_moves_forward() {
        let mut c = VirtualClock::default();
        c.advance_to(10);
        c.advance_to(5);
        assert_eq!(c.now(), 10);
    }

    #[test]
    fn ci_runs_report_in_time_order() {
        let mut f = SimForge::new();
        let pr = PullRequest {
            number: 1,
            author: "a".into(),
            base: "main".into(),
            head_ref: "b".into(),
            head_sha: "b-0".into(),
            title: String::new(),
            draft: false,
            closed: false,
            labels: Default::default(),
            files: vec![],
            dependabot_update_types: vec![],
            checks: vec![],
        };
        let ci = vec![
            CiScript {
                check: "slow".into(),
                app: None,
                duration: 300,
                conclusion: Some(Conclusion::Success),
            },
            CiScript {
                check: "fast".into(),
                app: None,
                duration: 60,
                conclusion: Some(Conclusion::Failure),
            },
            CiScript {
                check: "never".into(),
                app: None,
                duration: 1,
                conclusion: None,
            },
        ];
        f.schedule(10, Action::Open(Box::new(pr), ci));
        let mut times = Vec::new();
        while let Some((t, _)) = f.next_event() {
            times.push(t);
        }
        // open, three pending runs at 10, fast at 70, slow at 310; `never` never completes.
        assert_eq!(times, vec![10, 10, 10, 10, 70, 310]);
        assert_eq!(f.now(), 310);
    }
}
