//! Faults between the engine and the forge (KONZEPT.md section 6, MQ-06, MQ-31, MQ-33, MQ-35):
//! the forge's replies are acknowledgements that can come late, twice or never; a side effect
//! can fail with an error before it happens, or happen and still answer with an error (a
//! "lying" 5xx); a push to the base can slip in between "green" and "set ref"; reads can lag
//! behind writes; CI runs are not all alike. And the engine itself can crash at any side
//! effect (`CrashPlan`), losing its memory and whatever was on its way to it.
//!
//! `Wire` sits between the runner and the simulated forge and works on any [`GitStore`], so the
//! same faults run against git in memory and against `localgit`.
//!
//! Lagging reads apply to webhook-driven reads and reconciliation passes. The engine's own
//! `Refresh` reads (verify before landing, confirm a red verdict, resolve an unknown landing)
//! are consistent reads: no engine can keep I1 if the read it lands on may miss a label added
//! a minute earlier. An adapter must serve them without a cache (the GitHub adapter reads them
//! directly at landing priority). This is a requirement on adapters, not an assumption about
//! GitHub's caches.

use std::collections::{BTreeMap, BTreeSet};

use schlange_core::journal::is_side_effect;
use schlange_core::model::{PullRequest, Time};
use schlange_core::{Decision, Event};

use crate::ci::Rng;
use crate::git::{GitStore, Sha};
use crate::sim::{Action, CiTiming, SimForge};

/// Faults of the forge's replies, reads and side effects, and of CI timing. Probabilities are
/// per reply, per side effect or per read.
#[derive(Clone, Debug, PartialEq, Default)]
pub struct ForgeFaults {
    pub seed: u64,
    /// A reply never arrives.
    pub reply_drop: f64,
    /// A reply arrives twice.
    pub reply_duplicate: f64,
    /// A reply arrives late, up to `max_reply_delay`.
    pub reply_delay: f64,
    pub max_reply_delay: Time,
    /// The side effect fails before it happens and the forge answers with an error.
    pub error: f64,
    /// The side effect happens and the forge still answers with an error (MQ-35).
    pub lie: f64,
    /// Someone pushes to the base right before a landing (a real compare-and-swap race, MQ-06).
    pub base_race: f64,
    /// A webhook-driven or reconciliation read returns the state the engine read before, for
    /// an object that changed at most `max_lag` ago (read after write, eventual consistency,
    /// MQ-33).
    pub stale_read: f64,
    pub max_lag: Time,
    /// CI timing: jitter, late check-run creation, delayed cancellation.
    pub ci: CiTiming,
}

impl ForgeFaults {
    /// A reliable, synchronous forge.
    pub fn none() -> Self {
        ForgeFaults::default()
    }

    /// The default disturbance for the fault-mode property tests and scenarios.
    pub fn standard(seed: u64) -> Self {
        ForgeFaults {
            seed,
            reply_drop: 0.1,
            reply_duplicate: 0.1,
            reply_delay: 0.3,
            max_reply_delay: 90,
            error: 0.08,
            lie: 0.08,
            base_race: 0.1,
            stale_read: 0.2,
            max_lag: 30,
            ci: CiTiming {
                seed,
                jitter: 0.3,
                max_create_delay: 90,
                max_cancel_delay: 60,
            },
        }
    }

    pub fn is_none(&self) -> bool {
        *self
            == ForgeFaults {
                seed: self.seed,
                ci: CiTiming {
                    seed: self.ci.seed,
                    ..CiTiming::default()
                },
                ..ForgeFaults::default()
            }
    }
}

/// Where the engine crashes: before or after the side effect with a given index (0-based, in
/// the order the engine sends them), and/or at random with `rate` per side effect.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CrashPlan {
    /// `(index, after)`: crash right before (`false`) or right after (`true`) side effect
    /// `index` is carried out.
    pub points: BTreeSet<(u64, bool)>,
    pub rate: f64,
    pub seed: u64,
    /// At most this many random crashes.
    pub max: u32,
}

impl CrashPlan {
    pub fn none() -> Self {
        CrashPlan::default()
    }

    pub fn at(index: u64, after: bool) -> Self {
        CrashPlan {
            points: [(index, after)].into(),
            ..CrashPlan::default()
        }
    }

    pub fn random(seed: u64, rate: f64, max: u32) -> Self {
        CrashPlan {
            seed,
            rate,
            max,
            ..CrashPlan::default()
        }
    }

    pub fn is_none(&self) -> bool {
        self.points.is_empty() && self.rate == 0.0
    }
}

/// Counters of what the wire did, for reports.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct WireStats {
    pub replies_dropped: u64,
    pub replies_duplicated: u64,
    pub replies_delayed: u64,
    pub errors: u64,
    pub lies: u64,
    pub base_races: u64,
    pub stale_reads: u64,
}

/// The path between the engine and the forge, with `ForgeFaults`.
#[derive(Clone, Debug)]
pub struct Wire {
    pub faults: ForgeFaults,
    rng: Rng,
    /// Per object: what the engine last read, and when that was last the truth. The object
    /// changed after that time, so a stale answer is at most `max_lag` behind the change.
    prs: BTreeMap<u64, (PullRequest, Time)>,
    branches: BTreeMap<String, (Option<Sha>, Time)>,
    races: u64,
    pub stats: WireStats,
}

impl Wire {
    pub fn new(faults: ForgeFaults) -> Self {
        Wire {
            rng: Rng::new(faults.seed ^ 0x51ce_f00d),
            faults,
            prs: BTreeMap::new(),
            branches: BTreeMap::new(),
            races: 0,
            stats: WireStats::default(),
        }
    }

    fn lagging(&mut self, now: Time, fresh_at: Time, consistent: bool) -> bool {
        !consistent
            && now < fresh_at + self.faults.max_lag
            && self.rng.unit() < self.faults.stale_read
    }

    fn read_pr<G: GitStore>(
        &mut self,
        forge: &SimForge<G>,
        n: u64,
        consistent: bool,
    ) -> Option<PullRequest> {
        let truth = forge.read_pull_request(n)?;
        if self.faults.stale_read == 0.0 {
            return Some(truth);
        }
        let now = forge.now();
        if let Some((seen, fresh_at)) = self.prs.get(&n).cloned()
            && seen != truth
            && self.lagging(now, fresh_at, consistent)
        {
            self.stats.stale_reads += 1;
            return Some(seen);
        }
        self.prs.insert(n, (truth.clone(), now));
        Some(truth)
    }

    fn read_branch<G: GitStore>(
        &mut self,
        forge: &SimForge<G>,
        name: &str,
        consistent: bool,
    ) -> Option<Sha> {
        let truth = forge.read_branch(name);
        if self.faults.stale_read == 0.0 {
            return truth;
        }
        let now = forge.now();
        if let Some((seen, fresh_at)) = self.branches.get(name).cloned()
            && seen != truth
            && self.lagging(now, fresh_at, consistent)
        {
            self.stats.stale_reads += 1;
            return seen;
        }
        self.branches.insert(name.to_string(), (truth.clone(), now));
        truth
    }

    /// A read of these pull requests and branches, as the engine gets it. Reads with a token
    /// (the engine's `Refresh`) are consistent; others may lag.
    pub fn snapshot<G: GitStore>(
        &mut self,
        forge: &SimForge<G>,
        numbers: &[u64],
        branches: &[String],
        token: Option<u64>,
    ) -> Event {
        let consistent = token.is_some();
        Event::Snapshot {
            pull_requests: numbers
                .iter()
                .filter_map(|n| self.read_pr(forge, *n, consistent))
                .collect(),
            branches: branches
                .iter()
                .map(|b| (b.clone(), self.read_branch(forge, b, consistent)))
                .collect(),
            token,
        }
    }

    /// A full reconciliation read: every pull request, every branch but pull request heads,
    /// and the branches the engine believes in (`known`).
    pub fn snapshot_all<G: GitStore>(&mut self, forge: &SimForge<G>, known: &[String]) -> Event {
        let mut branches: Vec<String> = forge
            .repo
            .ref_names()
            .into_iter()
            .filter(|r| !r.starts_with("pr-"))
            .collect();
        branches.extend(known.iter().cloned());
        branches.sort();
        branches.dedup();
        self.snapshot(forge, &forge.numbers(), &branches, None)
    }

    /// A full read past the lag window: reads lag by at most `max_lag`, so a reconciliation that
    /// late sees the truth. The runner's final pass at quiescence stands for it.
    pub fn snapshot_all_fresh<G: GitStore>(
        &mut self,
        forge: &SimForge<G>,
        known: &[String],
    ) -> Event {
        let stale = std::mem::replace(&mut self.faults.stale_read, 0.0);
        let ev = self.snapshot_all(forge, known);
        self.faults.stale_read = stale;
        self.prs.clear();
        self.branches.clear();
        ev
    }

    /// Hands a reply to the engine now, later, twice or never. Returns the replies due now.
    fn deliver<G: GitStore>(&mut self, forge: &mut SimForge<G>, ev: Event) -> Vec<Event> {
        let f = &self.faults;
        if f.reply_drop > 0.0 && self.rng.unit() < f.reply_drop {
            self.stats.replies_dropped += 1;
            forge
                .notes
                .push((forge.now(), format!("reply lost: {}", reply_name(&ev))));
            return vec![];
        }
        let now = forge.now();
        if f.reply_duplicate > 0.0 && self.rng.unit() < self.faults.reply_duplicate {
            self.stats.replies_duplicated += 1;
            let at = now + 1 + self.rng.upto(self.faults.max_reply_delay);
            forge.schedule_reply(at, ev.clone());
        }
        if self.faults.reply_delay > 0.0 && self.rng.unit() < self.faults.reply_delay {
            self.stats.replies_delayed += 1;
            let at = now + 1 + self.rng.upto(self.faults.max_reply_delay);
            forge.schedule_reply(at, ev);
            return vec![];
        }
        vec![ev]
    }

    /// Carries out a decision on the forge and returns the replies due now.
    pub fn execute<G: GitStore>(&mut self, forge: &mut SimForge<G>, d: &Decision) -> Vec<Event> {
        if self.faults.is_none() || !is_side_effect(d) {
            return forge.execute(d);
        }
        if let Decision::Land { base_branch, .. } = d
            && self.rng.unit() < self.faults.base_race
        {
            self.races += 1;
            self.stats.base_races += 1;
            let text = forge.act_now(Action::PushBase {
                branch: base_branch.clone(),
                files: vec![(format!("race-{}.txt", self.races), "raced\n".into())],
            });
            forge
                .notes
                .push((forge.now(), format!("push raced the landing: {text}")));
        }
        if self.rng.unit() < self.faults.error {
            self.stats.errors += 1;
            forge.notes.push((
                forge.now(),
                format!("forge error before {}", decision_name(d)),
            ));
            let ev = Event::Failed {
                decision: d.clone(),
                error: "503 before the effect".into(),
            };
            return self.deliver(forge, ev);
        }
        let replies = match d {
            Decision::Refresh {
                token,
                numbers,
                branches,
            } => vec![self.snapshot(forge, numbers, branches, Some(*token))],
            _ => forge.execute(d),
        };
        if self.rng.unit() < self.faults.lie {
            self.stats.lies += 1;
            forge.notes.push((
                forge.now(),
                format!("forge did {} but answered 502", decision_name(d)),
            ));
            let ev = Event::Failed {
                decision: d.clone(),
                error: "502 after the effect".into(),
            };
            return self.deliver(forge, ev);
        }
        replies
            .into_iter()
            .flat_map(|r| self.deliver(forge, r))
            .collect()
    }
}

fn decision_name(d: &Decision) -> String {
    match d {
        Decision::BuildCar { car, .. } => format!("building car {car}"),
        Decision::CheckInPlace { car, .. } => format!("checking car {car} in place"),
        Decision::CloseCar { car, .. } => format!("closing car {car}"),
        Decision::Refresh { token, .. } => format!("read {token}"),
        Decision::Land { car, .. } => format!("landing car {car}"),
        other => format!("{other:?}"),
    }
}

fn reply_name(e: &Event) -> String {
    match e {
        Event::CarBuilt { car, .. } => format!("car {car} built"),
        Event::CarConflict { car, .. } => format!("car {car} conflicts"),
        Event::Landed { car, .. } => format!("car {car} landed"),
        Event::CarClosed { car } => format!("car {car} closed"),
        Event::Snapshot { token, .. } => format!("read {}", token.unwrap_or(0)),
        Event::Failed { decision, .. } => format!("error on {}", decision_name(decision)),
        _ => "reply".into(),
    }
}
