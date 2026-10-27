//! Scenarios as YAML: a repository, pull requests, CI and events over time, run against the
//! simulated forge (`runner`). Output: an event log, expectations and an invariant report.
//!
//! ```yaml
//! name: example
//! config: ../crates/schlange-config/tests/golden/arztagent.yml   # relative to this file
//! repo:                                 # optional; default: main with README.md
//!   base: main
//!   files: { README.md: "hello\n" }
//! seed: 1                               # flake seed
//! faults: standard                      # standard (default) | none | {seed, drop, ...}
//! forge_faults: standard                # none (default) | standard: lost, late, duplicated
//!                                       # and lying replies, base races, lagging reads, CI
//!                                       # timing; added to the disturbed run (I5)
//! ci:                                   # every pull request and queue draft runs these
//!   - check: ci-ok
//!     duration: 3m
//!     conclusion: success               # or failure, cancelled, …, none (never reports)
//!     runs_on: both                     # both | pull_request | queue
//!     fail_when_contains: [BUG]         # fail when a file contains a marker
//!     fail_when_all: [A, B]             # fail when both markers are present (MQ-01)
//!     fail_when_prs: [1, 2]             # fail when both pull requests are present
//!     hang_when_contains: HANG          # never report when a file contains a marker
//!     flake_rate: 0.1
//!     run: sh ci/compile.sh             # a real command in a checkout of the tested commit:
//!                                       # exit 0 = conclusion, 125 = hangs, other = failure
//! pull_requests:
//!   - number: 1
//!     author: arztagent-bot
//!     files: [src/lib.rs]               # or a map path → content
//!     checks: { ci-ok: failure }        # per-PR override, on the pull request only
//! events:
//!   - { at: 0s, open: 1 }
//!   - { at: 10m, label: { pr: 1, name: halt } }
//!   - { at: 11m, push: { pr: 1, files: { src/lib.rs: "v2\n" } } }
//!   - { at: 12m, push_base: { files: { hotfix.txt: "x\n" } } }
//!   - { at: 13m, merge_manually: 2 }
//!   - { at: 14m, push_queue_branch: 3 }
//!   - { at: 15m, comment: { pr: 1, by: alice, body: "@mergifyio queue" } }
//!   - { at: 16m, tick: { pr: 1, by: alice, checkbox: queue } }   # queue | requeue
//!   - { at: 17m, edit_body: { pr: 1, body: "Depends-On: #2" } }
//!   - { at: 18m, push: { pr: 1, by: mallory } }       # a push by someone else
//! permissions: { alice: write, mallory: read }      # sender-permission; others: none
//! config_in_repo: true                  # the configuration is .mergify.yml on the base
//! expect:
//!   - { pr: 1, state: merged }          # merged | dequeued | queued | open | closed
//!   - { pr: 2, state: dequeued, reason: CHECKS_FAILED }
//!   - { pr: 3, queued: false }          # phase-0 form: in the queue at the end?
//! expect_merged_order: [1, 3]
//! expect_base: { src/lib.rs: "v2\n" }
//! expect_log: ["build car"]
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use schlange_config::rules::{Rules, parse_duration};
use schlange_core::model::{Conclusion, Time};

use crate::ci::{CiScript, RunsOn};
use crate::git::Tree;
pub use crate::runner::{
    EndState, ExpectState, Expectation, ExpectationResult, InvariantResult, LogEntry, RunReport,
    Scenario, clock, run,
};
use crate::sim::{Action, Faults, PrDef};

#[derive(Debug, thiserror::Error)]
pub enum ScenarioError {
    #[error("cannot read {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("scenario {path}: {message}")]
    Format { path: PathBuf, message: String },
    #[error("configuration {path}: {message}")]
    Config { path: PathBuf, message: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    Reports(Conclusion),
    Never,
}

impl<'de> Deserialize<'de> for Outcome {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        if s == "none" || s == "never" {
            return Ok(Outcome::Never);
        }
        let c: Conclusion = serde_json::from_value(serde_json::Value::String(s.clone()))
            .map_err(|_| serde::de::Error::custom(format!("unknown conclusion `{s}`")))?;
        Ok(Outcome::Reports(c))
    }
}

impl Outcome {
    fn conclusion(self) -> Option<Conclusion> {
        match self {
            Outcome::Reports(c) => Some(c),
            Outcome::Never => None,
        }
    }
}

fn success() -> Outcome {
    Outcome::Reports(Conclusion::Success)
}

/// One marker or a list of them.
#[derive(Debug, Deserialize, Default)]
#[serde(untagged)]
enum OneOrMany {
    #[default]
    None,
    One(String),
    Many(Vec<String>),
}

impl OneOrMany {
    fn list(&self) -> Vec<String> {
        match self {
            OneOrMany::None => vec![],
            OneOrMany::One(s) => vec![s.clone()],
            OneOrMany::Many(v) => v.clone(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CiSpec {
    check: String,
    #[serde(default)]
    app: Option<String>,
    duration: String,
    #[serde(default = "success")]
    conclusion: Outcome,
    #[serde(default)]
    runs_on: Option<String>,
    #[serde(default)]
    fail_when_contains: OneOrMany,
    #[serde(default)]
    fail_when_all: Vec<String>,
    #[serde(default)]
    fail_when_prs: Vec<u64>,
    #[serde(default)]
    hang_when_contains: OneOrMany,
    #[serde(default)]
    flake_rate: f64,
    #[serde(default)]
    run: Option<String>,
}

/// Files: a list of paths (content generated per pull request) or a map path → content.
#[derive(Debug, Deserialize, Clone)]
#[serde(untagged)]
enum FilesSpec {
    List(Vec<String>),
    Map(BTreeMap<String, String>),
}

impl FilesSpec {
    fn resolve(&self, generated: impl Fn(&str) -> String) -> Vec<(String, String)> {
        match self {
            FilesSpec::List(paths) => paths.iter().map(|p| (p.clone(), generated(p))).collect(),
            FilesSpec::Map(m) => m.iter().map(|(p, c)| (p.clone(), c.clone())).collect(),
        }
    }
}

fn main_branch() -> String {
    "main".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PrSpec {
    number: u64,
    author: String,
    #[serde(default)]
    title: String,
    #[serde(default = "main_branch")]
    base: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    labels: BTreeSet<String>,
    #[serde(default)]
    files: Option<FilesSpec>,
    #[serde(default)]
    dependabot_update_type: Option<String>,
    #[serde(default)]
    checks: BTreeMap<String, Outcome>,
    #[serde(default)]
    body: String,
    #[serde(default)]
    fork: bool,
    #[serde(default)]
    approvals: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommentSpec {
    pr: u64,
    by: String,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TickSpec {
    pr: u64,
    by: String,
    checkbox: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EditBodySpec {
    pr: u64,
    body: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LabelSpec {
    pr: u64,
    name: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushSpec {
    pr: u64,
    #[serde(default)]
    files: Option<FilesSpec>,
    #[serde(default)]
    by: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PushBaseSpec {
    #[serde(default)]
    branch: Option<String>,
    files: FilesSpec,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum StepAction {
    Open(u64),
    Push(PushSpec),
    Label(LabelSpec),
    Unlabel(LabelSpec),
    Draft(u64),
    Ready(u64),
    Close(u64),
    PushBase(PushBaseSpec),
    MergeManually(u64),
    PushQueueBranch(u64),
    Comment(CommentSpec),
    Tick(TickSpec),
    EditBody(EditBodySpec),
}

#[derive(Debug, Deserialize)]
struct StepSpec {
    at: String,
    #[serde(flatten)]
    action: StepAction,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RepoSpec {
    #[serde(default = "main_branch")]
    base: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FaultSpec {
    #[serde(default)]
    seed: u64,
    #[serde(default)]
    drop: f64,
    #[serde(default)]
    duplicate: f64,
    #[serde(default)]
    delay: f64,
    #[serde(default)]
    max_delay: Option<String>,
    #[serde(default)]
    reconcile_every: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum FaultsSpec {
    Named(String),
    Custom(FaultSpec),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioSpec {
    name: String,
    #[serde(default)]
    description: String,
    config: String,
    #[serde(default)]
    repo: Option<RepoSpec>,
    #[serde(default)]
    seed: u64,
    #[serde(default)]
    faults: Option<FaultsSpec>,
    #[serde(default)]
    forge_faults: Option<String>,
    #[serde(default)]
    max_time: Option<String>,
    #[serde(default)]
    ci: Vec<CiSpec>,
    pull_requests: Vec<PrSpec>,
    events: Vec<StepSpec>,
    #[serde(default)]
    expect: Vec<Expectation>,
    #[serde(default)]
    expect_merged_order: Option<Vec<u64>>,
    #[serde(default)]
    expect_base: BTreeMap<String, String>,
    #[serde(default)]
    expect_log: Vec<String>,
    #[serde(default)]
    permissions: BTreeMap<String, String>,
    #[serde(default)]
    config_in_repo: bool,
}

/// Content a pull request writes to a path given only by name.
pub fn generated_content(path: &str, number: u64) -> String {
    format!("{path} changed by #{number}\n")
}

/// Default base tree of a scenario.
pub fn default_base_files() -> Tree {
    [(
        "README.md".to_string(),
        "Schlange test repository\n".to_string(),
    )]
    .into()
}

/// Parses a scenario from YAML; `dir` resolves the configuration path.
pub fn parse(text: &str, path: &Path) -> Result<Scenario, ScenarioError> {
    let fmt = |message: String| ScenarioError::Format {
        path: path.to_path_buf(),
        message,
    };
    let spec: ScenarioSpec = serde_yaml_ng::from_str(text).map_err(|e| fmt(e.to_string()))?;
    let config_path = path.parent().unwrap_or(Path::new(".")).join(&spec.config);
    let config_text =
        std::fs::read_to_string(&config_path).map_err(|source| ScenarioError::Io {
            path: config_path.clone(),
            source,
        })?;
    let rules = Rules::from_yaml(&config_text).map_err(|e| ScenarioError::Config {
        path: config_path.clone(),
        message: e.to_string(),
    })?;
    let dur = |what: &str, s: &str| {
        parse_duration(s).ok_or_else(|| fmt(format!("{what}: bad duration `{s}`")))
    };

    let mut scripts = Vec::new();
    for c in &spec.ci {
        let mut s = CiScript::new(
            &c.check,
            dur(&format!("check `{}`", c.check), &c.duration)?,
            c.conclusion.conclusion(),
        );
        s.app = c.app.clone();
        s.runs_on = match c.runs_on.as_deref() {
            None | Some("both") => RunsOn::Both,
            Some("pull_request") => RunsOn::PullRequest,
            Some("queue") => RunsOn::Queue,
            Some(o) => return Err(fmt(format!("check `{}`: unknown runs_on `{o}`", c.check))),
        };
        s.fail_when_contains = c.fail_when_contains.list();
        s.fail_when_all = c.fail_when_all.clone();
        s.fail_when_prs = c.fail_when_prs.clone();
        s.hang_when_contains = c.hang_when_contains.list();
        s.flake_rate = c.flake_rate;
        s.run = c.run.clone();
        scripts.push(s);
    }

    let mut prs = Vec::new();
    for p in &spec.pull_requests {
        for name in p.checks.keys() {
            if !scripts.iter().any(|s| &s.check == name) {
                return Err(fmt(format!("#{}: check `{name}` is not in `ci`", p.number)));
            }
        }
        let n = p.number;
        let files = p
            .files
            .as_ref()
            .map(|f| f.resolve(|path| generated_content(path, n)))
            .unwrap_or_else(|| vec![(format!("pr-{n}.txt"), generated_content("pr", n))]);
        let pr_scripts = scripts
            .iter()
            .map(|s| {
                let mut s = s.clone();
                if let Some(o) = p.checks.get(&s.check) {
                    s.conclusion = o.conclusion();
                }
                s
            })
            .collect();
        prs.push(PrDef {
            number: n,
            author: p.author.clone(),
            title: if p.title.is_empty() {
                format!("Pull request {n}")
            } else {
                p.title.clone()
            },
            base: p.base.clone(),
            draft: p.draft,
            labels: p.labels.clone(),
            files,
            dependabot_update_types: p.dependabot_update_type.iter().cloned().collect(),
            scripts: pr_scripts,
            body: p.body.clone(),
            fork: p.fork,
            approvals: p.approvals.clone(),
        });
    }
    let (base, mut base_files) = match &spec.repo {
        Some(r) if !r.files.is_empty() => (r.base.clone(), r.files.clone()),
        Some(r) => (r.base.clone(), default_base_files()),
        None => ("main".to_string(), default_base_files()),
    };
    if spec.config_in_repo
        && !base_files
            .keys()
            .any(|p| schlange_config::config_file::is_config_path(p))
    {
        base_files.insert(".mergify.yml".into(), config_text.clone());
    }
    let setup = crate::runner::Setup {
        permissions: spec.permissions.clone(),
        config_in_repo: spec.config_in_repo,
    };

    let mut steps = Vec::new();
    for s in &spec.events {
        let at = dur("event", &s.at)?;
        let known = |n: u64| {
            if prs.iter().any(|p| p.number == n) {
                Ok(n)
            } else {
                Err(fmt(format!("event at {}: unknown pull request #{n}", s.at)))
            }
        };
        let action = match &s.action {
            StepAction::Open(n) => Action::Open(known(*n)?),
            StepAction::Push(p) => {
                let n = known(p.pr)?;
                let files = p
                    .files
                    .as_ref()
                    .map(|f| f.resolve(|path| format!("{path} pushed at {} by #{n}\n", s.at)))
                    .unwrap_or_default();
                match &p.by {
                    None => Action::Push { number: n, files },
                    Some(by) => Action::PushAs {
                        number: n,
                        by: by.clone(),
                        files,
                    },
                }
            }
            StepAction::Comment(c) => Action::Comment {
                number: known(c.pr)?,
                by: c.by.clone(),
                body: c.body.clone(),
            },
            StepAction::Tick(t) => Action::Tick {
                number: known(t.pr)?,
                by: t.by.clone(),
                checkbox: match t.checkbox.as_str() {
                    "queue" => crate::sim::CheckBox::Queue,
                    "requeue" => crate::sim::CheckBox::Requeue,
                    o => return Err(fmt(format!("unknown checkbox `{o}` (queue or requeue)"))),
                },
            },
            StepAction::EditBody(e) => Action::EditBody {
                number: known(e.pr)?,
                body: e.body.clone(),
            },
            StepAction::Label(l) => Action::Label {
                number: known(l.pr)?,
                label: l.name.clone(),
            },
            StepAction::Unlabel(l) => Action::Unlabel {
                number: known(l.pr)?,
                label: l.name.clone(),
            },
            StepAction::Draft(n) => Action::Draft { number: known(*n)? },
            StepAction::Ready(n) => Action::Ready { number: known(*n)? },
            StepAction::Close(n) => Action::Close { number: known(*n)? },
            StepAction::PushBase(b) => Action::PushBase {
                branch: b.branch.clone().unwrap_or_else(|| base.clone()),
                files: b
                    .files
                    .resolve(|path| format!("{path} pushed directly at {}\n", s.at)),
            },
            StepAction::MergeManually(n) => Action::MergeManually(known(*n)?),
            StepAction::PushQueueBranch(n) => Action::PushQueueBranch(known(*n)?),
        };
        steps.push((at, action));
    }

    let forge_faults = match spec.forge_faults.as_deref() {
        None | Some("none") => None,
        Some("standard") => Some(crate::faults::ForgeFaults::standard(
            spec.seed.wrapping_add(7),
        )),
        Some(n) => return Err(fmt(format!("unknown forge_faults `{n}`"))),
    };
    let faults = match &spec.faults {
        None => Some(Faults::standard(spec.seed.wrapping_add(1))),
        Some(FaultsSpec::Named(n)) if n == "none" => None,
        Some(FaultsSpec::Named(n)) if n == "standard" => {
            Some(Faults::standard(spec.seed.wrapping_add(1)))
        }
        Some(FaultsSpec::Named(n)) => return Err(fmt(format!("unknown faults `{n}`"))),
        Some(FaultsSpec::Custom(f)) => Some(Faults {
            seed: f.seed,
            drop: f.drop,
            duplicate: f.duplicate,
            delay: f.delay,
            max_delay: f
                .max_delay
                .as_deref()
                .map(|d| dur("max_delay", d))
                .transpose()?
                .unwrap_or(30),
            reconcile_every: f
                .reconcile_every
                .as_deref()
                .map(|d| dur("reconcile_every", d))
                .transpose()?
                .unwrap_or(60),
        }),
    };
    let max_time: Time = spec
        .max_time
        .as_deref()
        .map(|d| dur("max_time", d))
        .transpose()?
        .unwrap_or(7 * 86400);

    Ok(Scenario {
        name: spec.name,
        description: spec.description,
        config: spec.config,
        rules,
        base,
        base_files,
        queue_scripts: scripts,
        prs,
        steps,
        expect: spec.expect,
        expect_merged_order: spec.expect_merged_order,
        expect_base: spec.expect_base,
        expect_log: spec.expect_log,
        faults,
        forge_faults,
        flake_seed: spec.seed,
        max_time,
        setup,
    })
}

/// Loads and runs a scenario file.
pub fn run_file(path: &Path) -> Result<RunReport, ScenarioError> {
    let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let scenario = parse(&text, path)?;
    Ok(run(&scenario))
}
