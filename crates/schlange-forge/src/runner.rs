//! Runs a typed scenario against the simulated forge and the engine: webhook deliveries become
//! truth reads, decisions are carried out, the forge's replies go back to the engine, and the
//! invariant checker runs after every step. A scenario with faults runs twice (perfect and
//! disturbed delivery) and the end states are compared (I5).
//!
//! [`run`] uses git in memory; [`run_with`] takes any [`GitStore`], which is how the same
//! scenarios run against real git repositories (`localgit::run`).

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;

use schlange_config::rules::Rules;
use schlange_core::engine::{Outcome, short};
use schlange_core::model::Time;
use schlange_core::{Decision, Event, State, step};

use schlange_core::journal::{Journal, MemoryJournal, is_side_effect};

use crate::ci::{CiScript, Rng};
use crate::faults::{CrashPlan, ForgeFaults, Wire};
use crate::git::{GitStore, Repo, Tree};
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

#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
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
    /// At the end it is held for this reason: a hold kind (`depends-on`, `merge-after`,
    /// `configuration-changed`, `queue-command`, `manually-dequeued`) or text in the reason.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<String>,
    /// Labels it has at the end (others may be there too).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    /// Labels it does not have at the end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub no_labels: Vec<String>,
    /// Text → how many comments contain it (idempotence: exactly that many).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub comments: BTreeMap<String, usize>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// Logins or teams whose review is requested at the end.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_requested: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub draft: Option<bool>,
    /// Checks reported by actions: name → conclusion of the latest report.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub checks: BTreeMap<String, String>,
    /// Reviews by state (`APPROVED`, `DISMISSED`, …) → how many.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub reviews: BTreeMap<String, usize>,
    /// Number of commits on the head at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commits: Option<usize>,
    /// The head is up to date with the base at the end.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub up_to_date: Option<bool>,
}

/// A pull request an action created (`backport`, `copy`), expected at the end.
#[derive(Clone, Debug, Default, Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExpectPr {
    pub head: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    /// Text the title contains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub labels: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// File → text it contains on the head.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub files: BTreeMap<String, String>,
    /// `false`: no such pull request may exist.
    #[serde(default = "yes")]
    pub exists: bool,
}

fn yes() -> bool {
    true
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
    pub expect_prs: Vec<ExpectPr>,
    /// `None`: no disturbed run, I5 is not checked.
    pub faults: Option<Faults>,
    /// Forge faults (late, lost, duplicated, lying replies; races; lagging reads; CI timing)
    /// added to the disturbed run.
    pub forge_faults: Option<ForgeFaults>,
    pub flake_seed: u64,
    /// Virtual time after which the run counts as not settling (I2).
    pub max_time: Time,
    /// Accounts and where the configuration lives.
    pub setup: Setup,
}

/// Repository settings of a scenario beyond its files.
#[derive(Clone, Debug, Default)]
pub struct Setup {
    /// Repository permission per login (`sender-permission`); others have `none`.
    pub permissions: BTreeMap<String, String>,
    /// The configuration is a file on the default branch (`.mergify.yml` unless the base files
    /// hold another configuration name): it is read from there after every step, and a change
    /// that lands applies to every pull request (MQ-19).
    pub config_in_repo: bool,
    /// More branches at the initial commit.
    pub branches: Vec<String>,
    /// Checks the protection of the base branch requires (`branch_protection_injection_mode`).
    pub required_checks: Vec<String>,
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
    /// Per pull request: `merged`, `dequeued CODE`, `queued`, `open` or `closed`. Dequeue
    /// reasons are part of the end state.
    pub outcomes: BTreeMap<u64, String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunReport {
    /// `sim` (git in memory) or `localgit` (real git repositories on disk).
    pub forge: &'static str,
    /// Check runs that were real processes in a checkout.
    pub ci_processes: u64,
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
    /// Why open pull requests are held at the end (built-in protections, commands).
    pub holds: BTreeMap<u64, Vec<schlange_core::protections::Hold>>,
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
pub struct Simulation<G: GitStore = Repo> {
    pub forge: SimForge<G>,
    pub state: State,
    pub checker: Checker,
    pub log: Vec<LogEntry>,
    pub steps: u64,
    pub merged_order: Vec<u64>,
    pub settled: bool,
    pub wire: Wire,
    pub journal: MemoryJournal,
    pub crash: CrashPlan,
    /// Side effects carried out so far.
    pub effects: u64,
    pub crashes: u32,
    crashed: bool,
    crash_rng: Rng,
    intent_seq: u64,
}

pub const LIMITATIONS: [&str; 7] = [
    "scopes: file patterns only (no manual scopes, no capacities); parallel and isolated mode: a batch failure that needs a split rebuilds every other car",
    "skip_intermediate_results: serial mode with squash, merge or rebase only; direct merge behind the base (scopes), batch_pull_request: not built",
    "allow_queue_branch_edit: the edit is accepted; merge-batch and fast-forward land it, squash, merge and rebase halt on the tree check",
    "in place: only for an up-to-date pull request; Mergify would update it first (update_method not built)",
    "priorities: re-evaluated while a pull request waits; a running bisection is never interrupted",
    "rule actions: bot_account, users_from_teams and merge options beyond `method` not built; copies are one cherry-picked commit",
    "commit_message_template (deprecated): not rendered; IANA time zones in Merge-After: not supported",
];

/// Unix time of the simulation's time 0 (2026-01-01T00:00:00Z), for `Merge-After` and the
/// timestamp attributes.
pub const SIM_EPOCH: Time = 1_767_225_600;

/// The repository name the simulation uses for `Depends-On`.
pub const SIM_REPOSITORY: &str = "sim/repo";

fn fresh_state() -> State {
    State {
        host: schlange_core::engine::Host {
            epoch: SIM_EPOCH,
            bot: crate::sim::BOT.into(),
            repository: SIM_REPOSITORY.into(),
        },
        ..State::default()
    }
}

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
            stack_on,
            ..
        } => format!(
            "build car {car} on {branch}: base {}{} + {} (checks {})",
            short(base_sha),
            stack_on
                .as_ref()
                .map(|s| format!(", stacked on {}", short(s)))
                .unwrap_or_default(),
            prs.iter()
                .map(|p| format!("#{}", p.number))
                .collect::<Vec<_>>()
                .join(" "),
            own.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Decision::CheckInPlace {
            car,
            number,
            direct: true,
            ..
        } => format!("car {car}: merge #{number} directly (up to date, its own CI green)"),
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
            method,
            ..
        } => format!(
            "land car {car}: {} {} onto {base_branch} if it is still {}",
            method.as_str(),
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
        Decision::Cosmetic { cosmetic } => cosmetic.describe(),
        Decision::Act { rule, op, .. } => format!("action of `{rule}`: {}", op.describe()),
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
        Event::ActionDone { result, .. } => Some(match result {
            schlange_core::actions::ActionResult::Done { .. } => "action done".to_string(),
            schlange_core::actions::ActionResult::Skipped { message } => {
                format!("action skipped: {message}")
            }
            schlange_core::actions::ActionResult::Refused { message } => {
                format!("action refused: {message}")
            }
            schlange_core::actions::ActionResult::Copied { copies } => format!(
                "copied: {}",
                copies
                    .iter()
                    .map(|c| match (&c.error, c.number) {
                        (Some(e), _) => format!("{} failed ({e})", c.destination),
                        (None, Some(n)) => format!(
                            "{} as #{n}{}",
                            c.destination,
                            if c.conflicts { " with conflicts" } else { "" }
                        ),
                        _ => c.destination.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
        Event::Failed { decision, error } => {
            Some(format!("forge error ({error}) on: {}", describe(decision)))
        }
        _ => None,
    }
}

/// Formats virtual time as `+h:mm:ss`.
pub fn clock(t: Time) -> String {
    format!("+{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

impl Simulation<Repo> {
    /// A simulation with git in memory.
    pub fn new(s: &Scenario, faults: Faults) -> Self {
        Simulation::on(s, faults, Repo::new(), false)
    }
}

impl<G: GitStore> Simulation<G> {
    /// A simulation on the empty git store `git`; `real_ci` runs every check as a process.
    pub fn on(s: &Scenario, faults: Faults, git: G, real_ci: bool) -> Self {
        let mut forge = SimForge::with_git(
            git,
            &s.base,
            s.base_files.clone(),
            s.queue_scripts.clone(),
            s.flake_seed,
        );
        forge.real_ci = real_ci;
        forge.permissions = s.setup.permissions.clone();
        for b in &s.setup.branches {
            forge.add_branch(b);
        }
        if !s.setup.required_checks.is_empty() {
            forge
                .required_checks
                .insert(s.base.clone(), s.setup.required_checks.clone());
        }
        for d in &s.prs {
            forge.define(d.clone());
        }
        for (t, a) in &s.steps {
            forge.schedule(*t, a.clone());
        }
        forge.set_faults(faults);
        Simulation {
            forge,
            state: fresh_state(),
            checker: Checker::new(),
            log: Vec::new(),
            steps: 0,
            merged_order: Vec::new(),
            settled: true,
            wire: Wire::new(ForgeFaults::none()),
            journal: MemoryJournal::default(),
            crash: CrashPlan::none(),
            effects: 0,
            crashes: 0,
            crashed: false,
            crash_rng: Rng::new(0),
            intent_seq: 0,
        }
    }

    /// Adds forge faults: late, lost, duplicated and lying replies, base races, lagging reads,
    /// CI timing.
    pub fn with_forge_faults(mut self, f: ForgeFaults) -> Self {
        self.forge.ci_timing = f.ci.clone();
        self.checker.relaxed_steps |= !f.is_none();
        self.wire = Wire::new(f);
        self
    }

    /// Crashes the engine at these points (and restarts it from its journal).
    pub fn with_crashes(mut self, plan: CrashPlan) -> Self {
        self.crash_rng = Rng::new(plan.seed ^ 0xc4a5_4ed0);
        self.checker.relaxed_steps |= !plan.is_none();
        self.crash = plan;
        self
    }

    /// Whether to crash before (`after: false`) or right after side effect `index`.
    fn should_crash(&mut self, index: u64, after: bool) -> bool {
        // A point fires once: a crash before a side effect does not advance the count.
        if self.crash.points.remove(&(index, after)) {
            return true;
        }
        self.crash.rate > 0.0
            && self.crashes < self.crash.max
            && self.crash_rng.unit() < self.crash.rate
    }

    /// The engine dies: its memory and everything on its way to it are gone.
    fn crash_now(&mut self) {
        self.crashes += 1;
        self.crashed = true;
        self.state = fresh_state();
        let lost = self.forge.drop_engine_inbox();
        self.log(
            "crash",
            format!(
                "engine crashed at side effect {}; {lost} replies, deliveries and timers lost",
                self.effects
            ),
        );
    }

    /// Restarts the engine from its journal and a fresh full read of the forge.
    fn restart(&mut self, rules: &Rules) {
        self.crashed = false;
        let recovered = self.journal.recover().expect("journal readable");
        let in_doubt = match recovered {
            Some(r) => {
                self.state = r.state;
                r.in_doubt
            }
            None => vec![],
        };
        self.log(
            "restart",
            format!(
                "engine restarted from its journal; side effects in doubt: {}",
                if in_doubt.is_empty() {
                    "none".to_string()
                } else {
                    in_doubt
                        .iter()
                        .map(|(_, d)| describe(d))
                        .collect::<Vec<_>>()
                        .join("; ")
                }
            ),
        );
        self.feed(rules, Event::Restarted, None);
        if self.crashed {
            return;
        }
        let known = self.known_branches();
        let ev = self.wire.snapshot_all(&self.forge, &known);
        self.feed(rules, ev, Some("full read after restart".into()));
        if self.crashed {
            return;
        }
        let comments = self.forge.all_comments();
        if !comments.is_empty() {
            self.feed(rules, Event::Comments { comments }, None);
        }
    }

    /// Hands every comment to the engine (it ignores those it has seen).
    fn deliver_comments(&mut self, rules: &Rules) -> bool {
        let comments = self.forge.all_comments();
        if comments.is_empty() {
            return false;
        }
        self.deliver(rules, Event::Comments { comments }, None)
    }

    /// With the configuration in the repository: reads it from the default branch and, when it
    /// changed, switches to it and tells the engine (MQ-19). A configuration that does not
    /// load (two files, invalid text) is logged and the last good one stays.
    fn reload_config(&mut self, rules: &mut Rules, text: &mut Option<String>) {
        let Some(tip) = self.forge.read_branch(&self.forge.default_branch.clone()) else {
            return;
        };
        let tree = self.forge.repo.tree(&tip);
        let located = schlange_config::config_file::locate(|p| tree.contains_key(p));
        let new = match &located {
            schlange_config::config_file::Located::One(p) => tree.get(*p).cloned(),
            schlange_config::config_file::Located::None => None,
            schlange_config::config_file::Located::Several { .. } => {
                let e = located.error().unwrap_or_default();
                if self.log.last().is_none_or(|l| l.text != e) {
                    self.log("config", e);
                }
                return;
            }
        };
        let Some(new) = new else {
            return;
        };
        if text.as_ref() == Some(&new) {
            return;
        }
        match Rules::from_yaml(&new) {
            Ok(r) => {
                let first = text.is_none();
                *text = Some(new);
                if first {
                    *rules = r;
                    return;
                }
                *rules = r;
                self.log(
                    "config",
                    "the configuration on the default branch changed; using it".into(),
                );
                self.deliver(rules, Event::ConfigurationChanged, None);
            }
            Err(e) => {
                *text = Some(new);
                self.log(
                    "config",
                    format!("the configuration on the default branch does not load, keeping the last good one: {e}"),
                );
            }
        }
    }

    /// `feed`, then restarts after a crash (as often as needed).
    pub fn deliver(&mut self, rules: &Rules, event: Event, origin: Option<String>) -> bool {
        let acted = self.feed(rules, event, origin);
        while self.crashed {
            self.restart(rules);
        }
        acted
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
    ///
    /// The state each step produces is journaled before any of its side effects, and every side
    /// effect is journaled before it is carried out (MQ-31). With a crash plan the engine may
    /// die before or after any side effect; `deliver` restarts it.
    pub fn feed(&mut self, rules: &Rules, event: Event, origin: Option<String>) -> bool {
        let mut queue = std::collections::VecDeque::from([event]);
        let mut any = false;
        let mut origin = origin;
        while let Some(ev) = queue.pop_front() {
            if self.crashed {
                break;
            }
            let now = self.forge.now();
            let (next, decisions) = step(&self.state, rules, &ev, now);
            self.state = next;
            self.steps += 1;
            self.journal.checkpoint(&self.state).expect("journal");
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
                if !is_side_effect(d) {
                    self.forge.execute(d);
                    continue;
                }
                let index = self.effects;
                if self.should_crash(index, false) {
                    self.crash_now();
                    return any;
                }
                self.intent_seq += 1;
                self.journal.intent(self.intent_seq, d).expect("journal");
                let replies = self.wire.execute(&mut self.forge, d);
                for (_, note) in std::mem::take(&mut self.forge.notes) {
                    self.log("fault", note);
                }
                for (_, n, m) in std::mem::take(&mut self.forge.landed_messages) {
                    self.log("message", format!("#{n}: {}", m.replace('\n', " ⏎ ")));
                }
                for r in replies {
                    if let Some(text) = describe_reply(&r) {
                        self.log("reply", text);
                    }
                    queue.push_back(r);
                }
                self.journal.done(self.intent_seq).expect("journal");
                self.effects += 1;
                if self.should_crash(index, true) {
                    self.crash_now();
                    return any;
                }
            }
        }
        any
    }

    fn known_branches(&self) -> Vec<String> {
        self.state.branches.keys().cloned().collect()
    }

    pub fn run(&mut self, s: &Scenario) {
        let mut owned_rules = s.rules.clone();
        let mut config_text: Option<String> = None;
        if s.setup.config_in_repo {
            self.reload_config(&mut owned_rules, &mut config_text);
        }
        let rules = &mut owned_rules;
        for (base, checks) in self.forge.required_checks.clone() {
            self.deliver(rules, Event::RequiredChecks { base, checks }, None);
        }
        let initial = self.wire.snapshot_all(&self.forge, &[]);
        self.deliver(rules, initial, None);
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
                    Item::Action(text) => {
                        self.log("action", text);
                        // An operator's control reaches the engine at once (the API of
                        // `serve` hands it to the worker and answers after the step).
                        for control in std::mem::take(&mut self.forge.undelivered_controls) {
                            self.deliver(rules, Event::Control { control }, None);
                        }
                    }
                    Item::Ci(text) => self.log("ci", text),
                    Item::Webhook { hint, text } => {
                        let ev = match &hint {
                            Hint::PullRequest(n) => {
                                self.wire.snapshot(&self.forge, &[*n], &[], None)
                            }
                            Hint::Branch(b) => {
                                self.wire
                                    .snapshot(&self.forge, &[], std::slice::from_ref(b), None)
                            }
                            Hint::Comment(id) => Event::Comments {
                                comments: self.forge.read_comment(*id).into_iter().collect(),
                            },
                        };
                        self.deliver(rules, ev, Some(text));
                    }
                    Item::Tick => {
                        self.deliver(rules, Event::Tick, Some("timer".into()));
                    }
                    Item::Reconcile => {
                        let known = self.known_branches();
                        let ev = self.wire.snapshot_all(&self.forge, &known);
                        self.deliver(rules, ev, Some("reconciliation pass".into()));
                        self.deliver_comments(rules);
                    }
                    Item::Reply(ev) => {
                        if let Some(text) = describe_reply(&ev) {
                            self.log("reply", format!("late: {text}"));
                        }
                        self.deliver(rules, *ev, None);
                    }
                }
                if s.setup.config_in_repo {
                    self.reload_config(rules, &mut config_text);
                }
                self.checker.after_step(&self.forge, &self.state, rules);
            }
            if !self.settled {
                break;
            }
            // Quiescent: a final reconciliation reads everything once more.
            let known = self.known_branches();
            let ev = self.wire.snapshot_all_fresh(&self.forge, &known);
            let mut acted = self.deliver(rules, ev, Some("final reconciliation".into()));
            acted |= self.deliver_comments(rules);
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
        let outcomes = self
            .forge
            .numbers()
            .into_iter()
            .filter(|n| {
                self.forge
                    .read_pull_request(*n)
                    .is_some_and(|p| p.author != crate::sim::BOT)
            })
            .map(|n| (n, self.outcome(n)))
            .collect();
        EndState {
            merged,
            still_queued: self.state.queued.keys().copied().collect(),
            base_trees,
            outcomes,
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

fn check_expectation<G: GitStore>(sim: &Simulation<G>, e: &Expectation) -> ExpectationResult {
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
    let mut actual = actual;
    let mut notes: Vec<String> = Vec::new();
    if let Some(p) = sim.forge.read_pull_request(e.pr) {
        for l in &e.labels {
            if !p.labels.contains(l) {
                ok = false;
                notes.push(format!("lacks label {l}"));
            }
        }
        for l in &e.no_labels {
            if p.labels.contains(l) {
                ok = false;
                notes.push(format!("has label {l}"));
            }
        }
        for (text, want) in &e.comments {
            let have = sim
                .forge
                .comments_on(e.pr)
                .iter()
                .filter(|c| c.body.contains(text.as_str()))
                .count();
            if have != *want {
                ok = false;
                notes.push(format!("{have} comment(s) with {text:?}, expected {want}"));
            }
        }
        for a in &e.assignees {
            if !p.assignees.contains(a) {
                ok = false;
                notes.push(format!("{a} not assigned ({:?})", p.assignees));
            }
        }
        for r in &e.review_requested {
            if !p.requested_reviewers.contains(r) && !p.requested_teams.contains(r) {
                ok = false;
                notes.push(format!(
                    "no review requested from {r} ({:?} {:?})",
                    p.requested_reviewers, p.requested_teams
                ));
            }
        }
        if let Some(d) = e.draft
            && p.draft != d
        {
            ok = false;
            notes.push(format!("draft is {}", p.draft));
        }
        for (name, want) in &e.checks {
            let have = sim
                .forge
                .posted_checks
                .latest(e.pr, name)
                .map(|c| c.conclusion.clone());
            if have.as_deref() != Some(want.as_str()) {
                ok = false;
                notes.push(format!("check {name}: {have:?}"));
            }
        }
        for (st, want) in &e.reviews {
            let have = p.reviews.iter().filter(|r| &r.state == st).count();
            if have != *want {
                ok = false;
                notes.push(format!("{have} review(s) {st}"));
            }
        }
        if let Some(c) = e.commits
            && p.commit_messages.len() != c
        {
            ok = false;
            notes.push(format!("{} commit(s)", p.commit_messages.len()));
        }
        if let Some(u) = e.up_to_date
            && p.behind_base == u
        {
            ok = false;
            notes.push(format!("behind base: {}", p.behind_base));
        }
    }
    if !notes.is_empty() {
        actual = format!("{actual}; {}", notes.join("; "));
    }
    if let Some(h) = &e.held {
        let holds = sim.state.holds.get(&e.pr).cloned().unwrap_or_default();
        ok &= holds
            .iter()
            .any(|x| x.kind == *h || x.text.contains(h.as_str()));
        actual = format!(
            "{actual}; held: [{}]",
            holds
                .iter()
                .map(|x| format!("{}: {}", x.kind, x.text))
                .collect::<Vec<_>>()
                .join("; ")
        );
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

/// Runs a scenario with git in memory: perfect delivery, then (with faults) disturbed delivery
/// for I5.
pub fn run(s: &Scenario) -> RunReport {
    match run_with(s, || Ok::<_, std::convert::Infallible>(Repo::new()), false) {
        Ok(r) => r,
        Err(e) => match e {},
    }
}

/// The engine state at the end of a scenario's undisturbed run in the simulated forge (for
/// `schlange explain --scenario` and `config simulate --scenario`).
pub fn end_state(s: &Scenario) -> State {
    let mut sim = Simulation::on(s, Faults::none(), Repo::new(), false);
    sim.run(s);
    sim.state
}

/// Runs a scenario on git stores made by `make` (one per run: perfect, and disturbed when the
/// scenario has faults). `real_ci`: every check runs as a process in a checkout.
pub fn run_with<G: GitStore, E>(
    s: &Scenario,
    mut make: impl FnMut() -> Result<G, E>,
    real_ci: bool,
) -> Result<RunReport, E> {
    let mut perfect = Simulation::on(s, Faults::none(), make()?, real_ci);
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
            let mut d = Simulation::on(s, f.clone(), make()?, real_ci)
                .with_forge_faults(s.forge_faults.clone().unwrap_or_default());
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
                held: None,
                ..Default::default()
            },
            actual: format!("{:?}", perfect.merged_order),
            ok: &perfect.merged_order == order,
        });
    }
    if !s.expect_base.is_empty() {
        let tree = perfect
            .forge
            .read_branch(&s.base)
            .map(|sha| perfect.forge.repo.tree(&sha))
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
                    held: None,
                    ..Default::default()
                },
                ok: actual.as_deref() == Some(content.as_str()),
                actual: format!("{actual:?}"),
            });
        }
    }
    for x in &s.expect_prs {
        let found = perfect
            .forge
            .numbers()
            .into_iter()
            .filter_map(|n| perfect.forge.read_pull_request(n))
            .find(|p| p.head_ref == x.head);
        let mut why = Vec::new();
        match (&found, x.exists) {
            (None, true) => why.push("missing".to_string()),
            (Some(p), false) => why.push(format!("exists as #{}", p.number)),
            (None, false) => {}
            (Some(p), true) => {
                if let Some(b) = &x.base
                    && &p.base != b
                {
                    why.push(format!("base {}", p.base));
                }
                if let Some(t) = &x.title
                    && !p.title.contains(t.as_str())
                {
                    why.push(format!("title {:?}", p.title));
                }
                for l in &x.labels {
                    if !p.labels.contains(l) {
                        why.push(format!("lacks label {l}"));
                    }
                }
                for a in &x.assignees {
                    if !p.assignees.contains(a) {
                        why.push(format!("{a} not assigned"));
                    }
                }
                let tree = perfect.forge.repo.tree(&p.head_sha);
                for (f, text) in &x.files {
                    if !tree.get(f).is_some_and(|c| c.contains(text.as_str())) {
                        why.push(format!("{f} = {:?}", tree.get(f)));
                    }
                }
            }
        }
        expectations.push(ExpectationResult {
            expected: Expectation {
                queue: Some(format!(
                    "pull request on {}{}",
                    x.head,
                    if x.exists { "" } else { " absent" }
                )),
                ..Default::default()
            },
            ok: why.is_empty(),
            actual: if why.is_empty() {
                "as expected".into()
            } else {
                why.join("; ")
            },
        });
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
                held: None,
                ..Default::default()
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
    Ok(RunReport {
        forge: perfect.forge.repo.kind(),
        ci_processes: perfect.forge.processes,
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
        holds: perfect.state.holds.clone(),
    })
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
        let mut out = format!(
            "scenario {} (config {}, forge {})\n",
            self.scenario, self.config, self.forge
        );
        out.push_str("\nevent log\n");
        for e in &self.log {
            out.push_str(&format!("  {}  {:<8}  {}\n", clock(e.t), e.kind, e.text));
        }
        out.push_str("\noutcomes\n");
        for (n, o) in &self.outcomes {
            out.push_str(&format!("  #{n}: {o}\n"));
        }
        out.push_str(&format!("  merged in order: {:?}\n", self.merged_order));
        if !self.holds.is_empty() {
            out.push_str("\nheld\n");
            for (n, hs) in &self.holds {
                for h in hs {
                    out.push_str(&format!(
                        "  #{n}: {}{}: {}\n",
                        h.kind,
                        if h.failed { " (fails)" } else { "" },
                        h.text
                    ));
                }
            }
        }
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
