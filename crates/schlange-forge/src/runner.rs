//! Runs a typed scenario against the simulated forge and the engine: webhook deliveries become
//! truth reads, decisions are carried out, the forge's replies go back to the engine, and the
//! invariant checker runs after every step. A scenario with faults runs twice (perfect and
//! disturbed delivery) and the end states are compared (I5).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use schlange_config::rules::Rules;
use schlange_core::engine::{Outcome, short};
use schlange_core::model::Time;
use schlange_core::{Decision, Event, State, step};

use crate::ci::CiScript;
use crate::git::Tree;
use crate::invariants::{Checker, IDS, InvariantState};
use crate::sim::{Action, Faults, Hint, Item, PrDef, SimForge};

/// What a scenario expects of one pull request at the end.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpectState {
    /// Landed by the queue.
    Merged,
    /// Left the queue with a reason.
    Dequeued,
    /// Still queued (only meaningful in a halted queue).
    Queued,
    /// Never queued, still open.
    Open,
    /// Closed or merged by hand.
    Closed,
}

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    pub pr: u64,
    /// Phase-0 form: whether the pull request is in the queue at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ExpectState>,
    /// Dequeue reason code (`CHECKS_FAILED` …) of the last dequeue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// A scenario, from YAML (`scenario`) or the Rust DSL (`dsl`).
#[derive(Clone, Debug)]
pub struct Scenario {
    pub name: String,
    pub description: String,
    pub config: String,
    pub rules: Rules,
    pub base: String,
    pub base_files: Tree,
    /// CI on queue draft pull requests (the global `ci` list).
    pub queue_scripts: Vec<CiScript>,
    pub prs: Vec<PrDef>,
    pub steps: Vec<(Time, Action)>,
    pub expect: Vec<Expectation>,
    pub expect_merged_order: Option<Vec<u64>>,
    pub expect_base: BTreeMap<String, String>,
    pub expect_log: Vec<String>,
    /// `None`: no disturbed run, I5 is not checked.
    pub faults: Option<Faults>,
    pub flake_seed: u64,
    /// Virtual time after which the run counts as not settling (I2).
    pub max_time: Time,
}

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub t: Time,
    pub kind: &'static str,
    pub text: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ExpectationResult {
    pub expected: Expectation,
    pub actual: String,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvariantResult {
    pub id: &'static str,
    /// `checked`, `violated` or `not_checked`.
    pub status: &'static str,
    pub checks: u64,
    pub violations: Vec<String>,
    pub notes: Vec<String>,
}

/// The end state compared for I5.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EndState {
    pub merged: BTreeSet<u64>,
    pub still_queued: BTreeSet<u64>,
    pub base_trees: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    pub scenario: String,
    pub description: String,
    pub config: String,
    pub passed: bool,
    pub end_time: Time,
    pub log: Vec<LogEntry>,
    pub expectations: Vec<ExpectationResult>,
    pub invariants: Vec<InvariantResult>,
    pub end_state: EndState,
    pub merged_order: Vec<u64>,
    /// Per pull request: `merged`, `dequeued CODE`, `queued`, `open`, `closed`.
    pub outcomes: BTreeMap<u64, String>,
    pub disturbed: Option<DisturbedRun>,
    /// What this phase does not build.
    pub limitations: Vec<&'static str>,
    pub steps: u64,
}

#[derive(Debug, Clone, Serialize)]
pub struct DisturbedRun {
    pub faults: String,
    pub dropped: u64,
    pub duplicated: u64,
    pub end_state: EndState,
    pub end_time: Time,
    pub invariant_violations: Vec<String>,
}

/// One simulation with a given delivery.
pub struct Simulation {
    pub forge: SimForge,
    pub state: State,
    pub checker: Checker,
    pub log: Vec<LogEntry>,
    pub steps: u64,
    pub merged_order: Vec<u64>,
    pub settled: bool,
}

pub const LIMITATIONS: [&str; 7] = [
    "merge methods other than squash, queue modes parallel/isolated, priorities, scopes: not built",
    "checks_timeout `auto` applies no timeout (no CI history is kept); fixed durations work",
    "max_checks_retries, skip_intermediate_results, direct merge: not built",
    "reset_on_external_merge `never` behaves like `always`",
    "in place: only for an up-to-date pull request; Mergify would update it first",
    "labels and status comments on pull requests: not built",
    "commands (queue, dequeue, requeue): not built",
];

fn describe(d: &Decision) -> String {
    match d {
        Decision::Enqueue {
            number,
            queue,
            rule,
        } => format!("enqueue #{number} into `{queue}` (rule `{rule}`)"),
        Decision::Dequeue {
            number,
            queue,
            reason,
        } => format!("dequeue #{number} from `{queue}`: {}", reason.describe()),
        Decision::ChecksEnded { number, reason, .. } => {
            format!(
                "checks of #{number} ended, stays queued: {}",
                reason.describe()
            )
        }
        Decision::EvaluationError { number, message } => {
            format!("#{number}: condition not evaluable, counts as false: {message}")
        }
        Decision::BuildCar {
            car,
            branch,
            base_sha,
            prs,
            own,
            ..
        } => format!(
            "build car {car} on {branch}: base {} + {} (checks {})",
            short(base_sha),
            prs.iter()
                .map(|p| format!("#{}", p.number))
                .collect::<Vec<_>>()
                .join(" "),
            own.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Decision::CheckInPlace { car, number, .. } => {
            format!("car {car}: check #{number} in place")
        }
        Decision::CloseCar { car, branch, draft } => format!(
            "close car {car}{}{}",
            branch
                .as_ref()
                .map(|b| format!(", delete {b}"))
                .unwrap_or_default(),
            draft.map(|d| format!(", close #{d}")).unwrap_or_default()
        ),
        Decision::Refresh {
            numbers, branches, ..
        } => format!(
            "read {} before acting",
            numbers
                .iter()
                .map(|n| format!("#{n}"))
                .chain(branches.iter().cloned())
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Decision::Land {
            car,
            base_branch,
            expected_base_sha,
            prs,
            ..
        } => format!(
            "land car {car}: squash {} onto {base_branch} if it is still {}",
            prs.iter()
                .map(|p| format!("#{}", p.number))
                .collect::<Vec<_>>()
                .join(" "),
            short(expected_base_sha)
        ),
        Decision::Merged { number, sha, .. } => format!("#{number} merged as {}", short(sha)),
        Decision::Halt {
            base_branch,
            reason,
        } => format!("HALT queue of {base_branch}: {reason}"),
        Decision::WakeAt { at } => format!("wake at {}", clock(*at)),
    }
}

fn describe_reply(e: &Event) -> Option<String> {
    match e {
        Event::CarBuilt {
            car,
            head_sha,
            draft,
            ..
        } => Some(format!(
            "car {car} built at {}, draft #{draft}",
            short(head_sha)
        )),
        Event::CarConflict {
            car,
            number,
            with_base,
            with,
        } => Some(format!(
            "car {car}: #{number} conflicts with {}",
            if *with_base {
                "the base branch".to_string()
            } else {
                with.iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )),
        Event::Landed {
            car,
            commits,
            error,
        } => Some(format!(
            "car {car} landed {}{}",
            if commits.is_empty() {
                "nothing".into()
            } else {
                commits
                    .iter()
                    .map(|c| format!("#{} as {}", c.number, short(&c.sha)))
                    .collect::<Vec<_>>()
                    .join(", ")
            },
            error
                .as_ref()
                .map(|e| format!("; refused: {e:?}"))
                .unwrap_or_default()
        )),
        _ => None,
    }
}

/// Formats virtual time as `+h:mm:ss`.
pub fn clock(t: Time) -> String {
    format!("+{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

impl Simulation {
    pub fn new(s: &Scenario, faults: Faults) -> Self {
        let mut forge = SimForge::new(
            &s.base,
            s.base_files.clone(),
            s.queue_scripts.clone(),
            s.flake_seed,
        );
        for d in &s.prs {
            forge.define(d.clone());
        }
        for (t, a) in &s.steps {
            forge.schedule(*t, a.clone());
        }
        forge.set_faults(faults);
        Simulation {
            forge,
            state: State::default(),
            checker: Checker::new(),
            log: Vec::new(),
            steps: 0,
            merged_order: Vec::new(),
            settled: true,
        }
    }

    fn log(&mut self, kind: &'static str, text: String) {
        self.log.push(LogEntry {
            t: self.forge.now(),
            kind,
            text,
        });
    }

    /// Hands one event to the engine, carries out its decisions and feeds the replies back.
    /// Returns whether the engine decided anything besides wake-ups.
    pub fn feed(&mut self, rules: &Rules, event: Event, origin: Option<String>) -> bool {
        let mut queue = vec![event];
        let mut any = false;
        let mut origin = origin;
        while !queue.is_empty() {
            let ev = queue.remove(0);
            let now = self.forge.now();
            let (next, decisions) = step(&self.state, rules, &ev, now);
            self.state = next;
            self.steps += 1;
            let mut replies = Vec::new();
            for d in &decisions {
                if !matches!(d, Decision::WakeAt { .. }) {
                    any = true;
                    if let Some(o) = origin.take() {
                        self.log("webhook", o);
                    }
                    self.log("decision", describe(d));
                }
                if let Decision::Merged { number, .. } = d {
                    self.merged_order.push(*number);
                }
                for r in self.forge.execute(d) {
                    if let Some(text) = describe_reply(&r) {
                        self.log("reply", text);
                    }
                    replies.push(r);
                }
            }
            queue.extend(replies);
        }
        any
    }

    fn known_branches(&self) -> Vec<String> {
        self.state.branches.keys().cloned().collect()
    }

    pub fn run(&mut self, s: &Scenario) {
        let rules = &s.rules;
        let initial = self.forge.snapshot_all(&[]);
        self.feed(rules, initial, None);
        let mut rounds = 0;
        loop {
            while let Some((t, item)) = self.forge.next_item() {
                for (_, note) in std::mem::take(&mut self.forge.notes) {
                    self.log("fault", note);
                }
                if t > s.max_time {
                    self.settled = false;
                    break;
                }
                match item {
                    Item::Action(text) => self.log("action", text),
                    Item::Ci(text) => self.log("ci", text),
                    Item::Webhook { hint, text } => {
                        let ev = match &hint {
                            Hint::PullRequest(n) => self.forge.snapshot(&[*n], &[], None),
                            Hint::Branch(b) => {
                                self.forge.snapshot(&[], std::slice::from_ref(b), None)
                            }
                        };
                        self.feed(rules, ev, Some(text));
                    }
                    Item::Tick => {
                        self.feed(rules, Event::Tick, Some("timer".into()));
                    }
                    Item::Reconcile => {
                        let ev = self.forge.snapshot_all(&self.known_branches());
                        self.feed(rules, ev, Some("reconciliation pass".into()));
                    }
                }
                self.checker.after_step(&self.forge, &self.state, rules);
            }
            if !self.settled {
                break;
            }
            // Quiescent: a final reconciliation reads everything once more.
            let ev = self.forge.snapshot_all(&self.known_branches());
            let acted = self.feed(rules, ev, Some("final reconciliation".into()));
            self.checker.after_step(&self.forge, &self.state, rules);
            rounds += 1;
            if (!acted && !self.forge.has_work()) || rounds > 100 {
                break;
            }
        }
        for (_, note) in std::mem::take(&mut self.forge.notes) {
            self.log("fault", note);
        }
        if self.settled {
            self.checker.at_quiescence(&self.forge, &self.state, rules);
        } else {
            self.checker
                .note("I2", format!("stopped at {}", clock(s.max_time)));
            let r = self.checker.results.get_mut("I2").expect("I2");
            r.checks += 1;
            r.violations
                .push(format!("did not settle within {}", clock(s.max_time)));
        }
    }

    pub fn end_state(&self) -> EndState {
        let merged = self
            .state
            .outcomes
            .iter()
            .filter(|(_, o)| matches!(o, Outcome::Merged { .. }))
            .map(|(n, _)| *n)
            .collect();
        let base_trees = self
            .forge
            .base_branches
            .iter()
            .filter_map(|b| {
                self.forge
                    .read_branch(b)
                    .map(|s| (b.clone(), self.forge.repo.tree_id_of(&s)))
            })
            .collect();
        EndState {
            merged,
            still_queued: self.state.queued.keys().copied().collect(),
            base_trees,
        }
    }

    /// `merged`, `dequeued CODE`, `queued`, `open` or `closed` for a pull request.
    pub fn outcome(&self, n: u64) -> String {
        if self.state.queued.contains_key(&n) {
            return "queued".into();
        }
        match self.state.outcomes.get(&n) {
            Some(Outcome::Merged { .. }) => "merged".into(),
            Some(Outcome::Dequeued { reason }) => {
                format!("dequeued {}", reason.code.unwrap_or("-"))
            }
            None => match self.forge.read_pull_request(n) {
                Some(p) if p.closed => "closed".into(),
                Some(_) => "open".into(),
                None => "unknown".into(),
            },
        }
    }
}

fn check_expectation(sim: &Simulation, e: &Expectation) -> ExpectationResult {
    let actual = sim.outcome(e.pr);
    let in_queue = sim.state.queued.get(&e.pr);
    let mut ok = true;
    if let Some(q) = e.queued {
        ok &= in_queue.is_some() == q;
    }
    if let Some(name) = &e.queue {
        let queue = in_queue.map(|x| x.queue.clone()).or_else(|| {
            // A merged or dequeued pull request was in the queue it was enqueued into.
            sim.log.iter().rev().find_map(|l| {
                l.text
                    .strip_prefix(&format!("enqueue #{} into `", e.pr))
                    .and_then(|r| r.split('`').next())
                    .map(str::to_string)
            })
        });
        ok &= queue.as_deref() == Some(name.as_str());
    }
    if let Some(state) = &e.state {
        let word = actual.split(' ').next().unwrap_or("");
        ok &= match state {
            ExpectState::Merged => word == "merged",
            ExpectState::Dequeued => word == "dequeued",
            ExpectState::Queued => word == "queued",
            ExpectState::Open => word == "open",
            ExpectState::Closed => word == "closed",
        };
    }
    if let Some(code) = &e.reason {
        let last = match sim.state.outcomes.get(&e.pr) {
            Some(Outcome::Dequeued { reason }) => reason.code,
            _ => None,
        };
        ok &= last == Some(code.as_str());
    }
    ExpectationResult {
        expected: e.clone(),
        actual,
        ok,
    }
}

fn invariant_results(results: &BTreeMap<&'static str, InvariantState>) -> Vec<InvariantResult> {
    IDS.iter()
        .map(|id| {
            let r = results.get(id).cloned().unwrap_or_default();
            let status = if !r.violations.is_empty() {
                "violated"
            } else if r.checks == 0 && *id == "I5" {
                "not_checked"
            } else {
                "checked"
            };
            InvariantResult {
                id,
                status,
                checks: r.checks,
                violations: r.violations,
                notes: r.notes,
            }
        })
        .collect()
}

/// Runs a scenario: perfect delivery, then (with faults) disturbed delivery for I5.
pub fn run(s: &Scenario) -> RunReport {
    let mut perfect = Simulation::new(s, Faults::none());
    perfect.run(s);
    let mut results = perfect.checker.results.clone();
    let end_state = perfect.end_state();
    let mut disturbed = None;
    match &s.faults {
        None => results
            .get_mut("I5")
            .expect("I5")
            .notes
            .push("no faults configured: no disturbed run".into()),
        Some(f) => {
            let mut d = Simulation::new(s, f.clone());
            d.run(s);
            let d_end = d.end_state();
            let i5 = results.get_mut("I5").expect("I5");
            i5.checks += 1;
            if d_end != end_state {
                i5.violations.push(format!(
                    "end state differs: perfect {end_state:?}, disturbed {d_end:?}"
                ));
            }
            let mut inner = Vec::new();
            for (id, r) in &d.checker.results {
                for v in &r.violations {
                    inner.push(format!("{id}: {v}"));
                }
            }
            if !inner.is_empty() {
                i5.violations.push(format!(
                    "invariants violated in the disturbed run: {inner:?}"
                ));
            }
            i5.notes.push(format!(
                "disturbed run: {} dropped, {} duplicated deliveries",
                d.forge.dropped, d.forge.duplicated
            ));
            disturbed = Some(DisturbedRun {
                faults: format!("{f:?}"),
                dropped: d.forge.dropped,
                duplicated: d.forge.duplicated,
                end_state: d_end,
                end_time: d.forge.now(),
                invariant_violations: inner,
            });
        }
    }
    let mut expectations: Vec<ExpectationResult> = s
        .expect
        .iter()
        .map(|e| check_expectation(&perfect, e))
        .collect();
    if let Some(order) = &s.expect_merged_order {
        expectations.push(ExpectationResult {
            expected: Expectation {
                pr: 0,
                queued: None,
                queue: Some(format!("merged order {order:?}")),
                state: None,
                reason: None,
            },
            actual: format!("{:?}", perfect.merged_order),
            ok: &perfect.merged_order == order,
        });
    }
    if !s.expect_base.is_empty() {
        let tree = perfect
            .forge
            .read_branch(&s.base)
            .map(|sha| perfect.forge.repo.tree(&sha).clone())
            .unwrap_or_default();
        for (path, content) in &s.expect_base {
            let actual = tree.get(path).cloned();
            expectations.push(ExpectationResult {
                expected: Expectation {
                    pr: 0,
                    queued: None,
                    queue: Some(format!("{} has {path} = {content:?}", s.base)),
                    state: None,
                    reason: None,
                },
                ok: actual.as_deref() == Some(content.as_str()),
                actual: format!("{actual:?}"),
            });
        }
    }
    for needle in &s.expect_log {
        let found = perfect.log.iter().any(|l| l.text.contains(needle.as_str()));
        expectations.push(ExpectationResult {
            expected: Expectation {
                pr: 0,
                queued: None,
                queue: Some(format!("log contains {needle:?}")),
                state: None,
                reason: None,
            },
            actual: if found { "found" } else { "missing" }.into(),
            ok: found,
        });
    }
    let invariants = invariant_results(&results);
    let violated = invariants.iter().any(|i| i.status == "violated");
    let outcomes = s
        .prs
        .iter()
        .map(|p| (p.number, perfect.outcome(p.number)))
        .collect();
    RunReport {
        scenario: s.name.clone(),
        description: s.description.clone(),
        config: s.config.clone(),
        passed: !expectations.is_empty() && expectations.iter().all(|e| e.ok) && !violated,
        end_time: perfect.forge.now(),
        log: perfect.log,
        expectations,
        invariants,
        end_state,
        merged_order: perfect.merged_order,
        outcomes,
        disturbed,
        limitations: LIMITATIONS.to_vec(),
        steps: perfect.steps,
    }
}

impl RunReport {
    /// The event log alone, for golden files.
    pub fn log_text(&self) -> String {
        let mut out = String::new();
        for e in &self.log {
            out.push_str(&format!("{}  {:<8}  {}\n", clock(e.t), e.kind, e.text));
        }
        out
    }

    pub fn to_text(&self) -> String {
        let mut out = format!("scenario {} (config {})\n", self.scenario, self.config);
        out.push_str("\nevent log\n");
        for e in &self.log {
            out.push_str(&format!("  {}  {:<8}  {}\n", clock(e.t), e.kind, e.text));
        }
        out.push_str("\noutcomes\n");
        for (n, o) in &self.outcomes {
            out.push_str(&format!("  #{n}: {o}\n"));
        }
        out.push_str(&format!("  merged in order: {:?}\n", self.merged_order));
        out.push_str("\nexpectations\n");
        for e in &self.expectations {
            let what = if e.expected.pr == 0 {
                e.expected.queue.clone().unwrap_or_default()
            } else {
                format!(
                    "#{}{}{}{}{}",
                    e.expected.pr,
                    e.expected
                        .queued
                        .map(|q| format!(" queued={q}"))
                        .unwrap_or_default(),
                    e.expected
                        .queue
                        .as_ref()
                        .map(|q| format!(" queue={q}"))
                        .unwrap_or_default(),
                    e.expected
                        .state
                        .as_ref()
                        .map(|s| format!(" state={s:?}").to_lowercase())
                        .unwrap_or_default(),
                    e.expected
                        .reason
                        .as_ref()
                        .map(|r| format!(" reason={r}"))
                        .unwrap_or_default(),
                )
            };
            out.push_str(&format!(
                "  {}  {what} (actual {})\n",
                if e.ok { "PASS" } else { "FAIL" },
                e.actual
            ));
        }
        out.push_str("\ninvariants\n");
        for i in &self.invariants {
            out.push_str(&format!("  {}  {} ({} checks)\n", i.id, i.status, i.checks));
            for v in &i.violations {
                out.push_str(&format!("      violation: {v}\n"));
            }
            for n in &i.notes {
                out.push_str(&format!("      note: {n}\n"));
            }
        }
        out.push_str("\nnot built in this phase\n");
        for p in &self.limitations {
            out.push_str(&format!("  - {p}\n"));
        }
        out.push_str(&format!(
            "\nresult: {} at {} ({} engine steps)\n",
            if self.passed { "PASS" } else { "FAIL" },
            clock(self.end_time),
            self.steps
        ));
        out
    }

    pub fn invariant(&self, id: &str) -> &InvariantResult {
        self.invariants
            .iter()
            .find(|i| i.id == id)
            .expect("known invariant")
    }
}
