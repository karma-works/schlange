//! Scenarios: a YAML description of a repository's pull requests, CI and events over time,
//! run against the simulated forge. Output: an event log and an invariant report.
//!
//! ```yaml
//! name: example
//! config: ../crates/schlange-config/tests/golden/arztagent.yml   # relative to this file
//! ci:                                   # every pull request runs these checks
//!   - check: ci-ok
//!     duration: 3m
//!     conclusion: success               # or failure, cancelled, …, none (never reports)
//! pull_requests:
//!   - number: 1
//!     author: arztagent-bot
//!     files: [src/lib.rs]
//!     checks: { ci-ok: failure }        # per-PR override
//! events:
//!   - { at: 0s, open: 1 }
//!   - { at: 10m, label: { pr: 1, name: halt } }
//! expect:
//!   - { pr: 1, queued: true, queue: default }
//! ```

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use schlange_config::rules::{Rules, parse_duration};
use schlange_core::model::{Conclusion, PullRequest, Time};
use schlange_core::{Decision, Event, State, step};

use crate::Forge;
use crate::sim::{Action, CiScript, SimForge};

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

fn success() -> Outcome {
    Outcome::Reports(Conclusion::Success)
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
    files: Vec<String>,
    #[serde(default)]
    dependabot_update_type: Option<String>,
    #[serde(default)]
    checks: BTreeMap<String, Outcome>,
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
    files: Option<Vec<String>>,
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
}

#[derive(Debug, Deserialize)]
struct StepSpec {
    at: String,
    #[serde(flatten)]
    action: StepAction,
}

#[derive(Debug, Deserialize, Serialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    pub pr: u64,
    pub queued: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ScenarioSpec {
    name: String,
    #[serde(default)]
    description: String,
    config: String,
    #[serde(default)]
    ci: Vec<CiSpec>,
    pull_requests: Vec<PrSpec>,
    events: Vec<StepSpec>,
    #[serde(default)]
    expect: Vec<Expectation>,
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
    pub actual_queued: bool,
    pub actual_queue: Option<String>,
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct InvariantResult {
    pub id: &'static str,
    pub status: &'static str,
    pub note: &'static str,
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
    /// Parts of the queue that are placeholders in this phase.
    pub placeholders: Vec<&'static str>,
}

const INVARIANTS: [(&str, &str); 6] = [
    ("I0", "base commits equal tested trees; needs merging"),
    (
        "I1",
        "no landing on false conditions or changed head; needs merging",
    ),
    (
        "I2",
        "every queued pull request is merged or dequeued with a reason; needs the queue",
    ),
    (
        "I3",
        "temporary branches equal used branches after quiescence; needs queue branches",
    ),
    (
        "I4",
        "running checks within max_parallel_checks; needs speculation",
    ),
    (
        "I5",
        "same end state with disturbed webhook delivery; needs fault injection",
    ),
];

/// Formats virtual time as `+h:mm:ss`.
pub fn clock(t: Time) -> String {
    format!("+{}:{:02}:{:02}", t / 3600, (t / 60) % 60, t % 60)
}

fn describe_event(ev: &Event) -> String {
    match ev {
        Event::PullRequestOpened { pull_request: p } => format!(
            "#{} opened by {} on {}{}{}",
            p.number,
            p.author,
            p.base,
            if p.draft { " (draft)" } else { "" },
            if p.files.is_empty() {
                String::new()
            } else {
                format!(", files: {}", p.files.join(" "))
            }
        ),
        Event::PullRequestSynchronized {
            number, head_sha, ..
        } => format!("#{number} pushed, head {head_sha}"),
        Event::ReadyForReview { number } => format!("#{number} ready for review"),
        Event::ConvertedToDraft { number } => format!("#{number} converted to draft"),
        Event::Labeled { number, label } => format!("#{number} labeled {label}"),
        Event::Unlabeled { number, label } => format!("#{number} unlabeled {label}"),
        Event::CheckRun { number, run } => match run.conclusion {
            None => format!("#{number} check `{}` started on {}", run.name, run.head_sha),
            Some(c) => format!(
                "#{number} check `{}` completed: {}",
                run.name,
                serde_json::to_value(c)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default()
            ),
        },
        Event::Closed { number } => format!("#{number} closed"),
    }
}

fn describe_decision(d: &Decision) -> String {
    match d {
        Decision::Enqueue {
            number,
            queue,
            rule,
        } => {
            format!("enqueue #{number} into `{queue}` (rule `{rule}`)")
        }
        Decision::Dequeue {
            number,
            queue,
            reason,
        } => format!(
            "dequeue #{number} from `{queue}`: {}{}",
            reason.code.map(|c| format!("{c}: ")).unwrap_or_default(),
            reason.text
        ),
        Decision::EvaluationError { number, message } => {
            format!("#{number}: condition not evaluable, counts as false: {message}")
        }
    }
}

/// Loads and runs a scenario file.
pub fn run_file(path: &Path) -> Result<RunReport, ScenarioError> {
    let text = std::fs::read_to_string(path).map_err(|source| ScenarioError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let fmt = |message: String| ScenarioError::Format {
        path: path.to_path_buf(),
        message,
    };
    let spec: ScenarioSpec = serde_yaml_ng::from_str(&text).map_err(|e| fmt(e.to_string()))?;
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

    let mut forge = SimForge::new();
    let ci_default: Vec<(String, Option<String>, Time, Outcome)> = spec
        .ci
        .iter()
        .map(|c| {
            parse_duration(&c.duration)
                .map(|d| (c.check.clone(), c.app.clone(), d, c.conclusion))
                .ok_or_else(|| {
                    fmt(format!(
                        "check `{}`: bad duration `{}`",
                        c.check, c.duration
                    ))
                })
        })
        .collect::<Result<_, _>>()?;
    let mut prs: BTreeMap<u64, (PullRequest, Vec<CiScript>)> = BTreeMap::new();
    for p in &spec.pull_requests {
        for name in p.checks.keys() {
            if !ci_default.iter().any(|(c, ..)| c == name) {
                return Err(fmt(format!("#{}: check `{name}` is not in `ci`", p.number)));
            }
        }
        let ci = ci_default
            .iter()
            .map(|(check, app, duration, outcome)| {
                let outcome = p.checks.get(check).copied().unwrap_or(*outcome);
                CiScript {
                    check: check.clone(),
                    app: app.clone(),
                    duration: *duration,
                    conclusion: match outcome {
                        Outcome::Reports(c) => Some(c),
                        Outcome::Never => None,
                    },
                }
            })
            .collect();
        let pr = PullRequest {
            number: p.number,
            author: p.author.clone(),
            base: p.base.clone(),
            head_ref: format!("pr-{}", p.number),
            head_sha: format!("pr-{}-0", p.number),
            title: p.title.clone(),
            draft: p.draft,
            closed: false,
            labels: p.labels.clone(),
            files: p.files.clone(),
            dependabot_update_types: p.dependabot_update_type.iter().cloned().collect(),
            checks: vec![],
        };
        prs.insert(p.number, (pr, ci));
    }
    for s in &spec.events {
        let at = parse_duration(&s.at).ok_or_else(|| fmt(format!("bad time `{}`", s.at)))?;
        let known = |n: u64| {
            if prs.contains_key(&n) {
                Ok(n)
            } else {
                Err(fmt(format!("event at {}: unknown pull request #{n}", s.at)))
            }
        };
        let action = match &s.action {
            StepAction::Open(n) => {
                let (pr, ci) = prs.get(&known(*n)?).cloned().expect("checked");
                Action::Open(Box::new(pr), ci)
            }
            StepAction::Push(p) => Action::Push {
                number: known(p.pr)?,
                files: p.files.clone(),
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
        };
        forge.schedule(at, action);
    }

    let mut state = State::default();
    let mut log = Vec::new();
    while let Some((t, event)) = forge.next_event() {
        log.push(LogEntry {
            t,
            kind: "event",
            text: describe_event(&event),
        });
        let (next, decisions) = step(&state, &rules, &event, t);
        state = next;
        for d in decisions {
            log.push(LogEntry {
                t,
                kind: "decision",
                text: describe_decision(&d),
            });
            forge.execute(&d).map_err(|e| fmt(e.to_string()))?;
        }
    }

    let expectations: Vec<ExpectationResult> = spec
        .expect
        .iter()
        .map(|e| {
            let actual_queue = forge.queue.queued.get(&e.pr).cloned();
            let actual_queued = actual_queue.is_some();
            let ok = actual_queued == e.queued && (e.queue.is_none() || e.queue == actual_queue);
            ExpectationResult {
                expected: e.clone(),
                actual_queued,
                actual_queue,
                ok,
            }
        })
        .collect();
    let invariants = INVARIANTS
        .iter()
        .map(|(id, note)| InvariantResult {
            id,
            status: "not_checked",
            note,
        })
        .collect();
    Ok(RunReport {
        scenario: spec.name,
        description: spec.description,
        config: spec.config,
        passed: !expectations.is_empty() && expectations.iter().all(|e| e.ok),
        end_time: forge.now(),
        log,
        expectations,
        invariants,
        placeholders: vec![
            "queue: entries are recorded only; no queue pull requests, speculation, batches, bisection or merges",
            "merge_conditions are not evaluated",
            "dequeue reasons: Mergify codes for these paths are not determined yet",
            "fault injection: none",
        ],
    })
}

impl RunReport {
    pub fn to_text(&self) -> String {
        let mut out = format!("scenario {} (config {})\n", self.scenario, self.config);
        out.push_str("\nevent log\n");
        for e in &self.log {
            out.push_str(&format!("  {}  {:<8}  {}\n", clock(e.t), e.kind, e.text));
        }
        out.push_str("\nexpectations\n");
        for e in &self.expectations {
            out.push_str(&format!(
                "  {}  #{} queued={}{} (actual queued={}{})\n",
                if e.ok { "PASS" } else { "FAIL" },
                e.expected.pr,
                e.expected.queued,
                e.expected
                    .queue
                    .as_ref()
                    .map(|q| format!(" queue={q}"))
                    .unwrap_or_default(),
                e.actual_queued,
                e.actual_queue
                    .as_ref()
                    .map(|q| format!(" queue={q}"))
                    .unwrap_or_default(),
            ));
        }
        out.push_str("\ninvariants\n");
        for i in &self.invariants {
            out.push_str(&format!("  {}  {}  ({})\n", i.id, i.status, i.note));
        }
        out.push_str("\nplaceholders\n");
        for p in &self.placeholders {
            out.push_str(&format!("  - {p}\n"));
        }
        out.push_str(&format!(
            "\nresult: {} at {}\n",
            if self.passed { "PASS" } else { "FAIL" },
            clock(self.end_time)
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn phase0_scenario_passes() {
        let path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios/p0-queue-conditions.yaml");
        let report = run_file(&path).unwrap();
        let failed: Vec<_> = report.expectations.iter().filter(|e| !e.ok).collect();
        assert!(failed.is_empty(), "{}\n{failed:?}", report.to_text());
        assert!(report.log.iter().any(|e| e.text.starts_with("dequeue #5")));
    }
}
