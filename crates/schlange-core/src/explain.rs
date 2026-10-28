//! `schlange explain <pr>` (S-EXPLAIN, W-13, MQ-43): why a pull request is (not) queued, not
//! merged, or was dequeued, read from the engine state alone (no I/O).
//!
//! The explanation lists, in this order:
//!
//! - where it stands: merged, queued (queue, rule, position, the pull requests ahead, the batch
//!   it rides in with its draft and status), waiting for a `queue` command's conditions,
//!   dequeued, or not queued;
//! - what holds it: the holds of `State::holds` (`Depends-On`, `Merge-After`, a changed
//!   configuration, a waiting command, a manual dequeue), the pause, freezes and merge
//!   protections that keep it from landing, a halted queue;
//! - every `queue` rule of `pull_request_rules` and every queue rule it could enter, each
//!   condition as a tree with its result: an `and`, `or` or `not` shows which of its
//!   sub-conditions failed, an atom shows the attribute's value the engine saw;
//! - the last dequeue reason (code and text) and the code of the last time its checks ended.
//!
//! [`Explanation::short`] is the one-line form the status comment carries.

use serde::Serialize;

use schlange_config::conditions::{Atom, Condition};
use schlange_config::rules::Rules;

use crate::engine::{EntryStatus, Outcome, State, context, is_queue_pull_request};
use crate::eval::{self, AttrValue, Context};
use crate::model::PullRequest;
use crate::protections::Hold;
use crate::queue::CarStatus;

/// One condition with its result. `result` is `None` when it could not be evaluated (an
/// unknown counts as false, as in the engine).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CondResult {
    /// The condition as written (an atom), or `and`, `or`, `not`, `true`.
    pub condition: String,
    pub result: Option<bool>,
    /// The attribute's value the engine saw (atoms only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<CondResult>,
}

impl CondResult {
    pub fn holds(&self) -> bool {
        self.result == Some(true)
    }

    /// The atoms that make this condition fail (the failing sub-conditions of `and`, `or`,
    /// `not`), as text.
    pub fn failing(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_failing(true, true, &mut out);
        out
    }

    /// As [`Self::failing`], without the values (for texts that must not change with every
    /// check run that reports).
    pub fn failing_plain(&self) -> Vec<String> {
        let mut out = Vec::new();
        self.collect_failing(true, false, &mut out);
        out
    }

    fn collect_failing(&self, want: bool, values: bool, out: &mut Vec<String>) {
        if self.result == Some(want) {
            return;
        }
        match self.condition.as_str() {
            "and" if !self.children.is_empty() => {
                if want {
                    for c in &self.children {
                        c.collect_failing(true, values, out);
                    }
                } else {
                    // `and` should be false: it holds, every child holds.
                    out.push(format!("not ({})", self.render_inline()));
                }
            }
            "or" if !self.children.is_empty() => {
                if want {
                    out.push(self.render_inline());
                } else {
                    for c in &self.children {
                        c.collect_failing(false, values, out);
                    }
                }
            }
            "not" if self.children.len() == 1 => {
                self.children[0].collect_failing(!want, values, out)
            }
            _ => {
                let text = if want {
                    clip(&self.condition)
                } else {
                    format!("not ({})", clip(&self.condition))
                };
                out.push(match (&self.value, &self.error) {
                    (_, Some(e)) => format!("{text} (cannot be evaluated: {e})"),
                    (Some(v), None) if values => format!("{text} (is {v})"),
                    _ => text,
                });
            }
        }
    }

    fn render_inline(&self) -> String {
        match self.condition.as_str() {
            "and" | "or" if !self.children.is_empty() => format!(
                "{} of [{}]",
                if self.condition == "and" {
                    "all"
                } else {
                    "one"
                },
                self.children
                    .iter()
                    .map(|c| c.render_inline())
                    .collect::<Vec<_>>()
                    .join("; ")
            ),
            "not" if self.children.len() == 1 => {
                format!("not {}", self.children[0].render_inline())
            }
            _ => clip(&self.condition),
        }
    }
}

/// A list of conditions (an implicit `and`) with its results.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ListResult {
    pub holds: bool,
    pub conditions: Vec<CondResult>,
}

impl ListResult {
    pub fn failing(&self) -> Vec<String> {
        self.conditions.iter().flat_map(|c| c.failing()).collect()
    }

    pub fn failing_plain(&self) -> Vec<String> {
        self.conditions
            .iter()
            .flat_map(|c| c.failing_plain())
            .collect()
    }
}

/// One `pull_request_rules` rule with a `queue` action and the queue rules it may queue into.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct RuleResult {
    pub rule: String,
    /// The queue the action names; `None`: the first matching queue rule.
    pub queue: Option<String>,
    pub conditions: ListResult,
}

/// One queue rule as the pull request sees it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueueRuleResult {
    pub queue: String,
    pub queue_conditions: ListResult,
    /// Judged on the checks of the batch's draft while it rides in one, else on the pull
    /// request's own checks.
    pub merge_conditions: ListResult,
}

/// The batch a queued pull request rides in.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct BatchInfo {
    pub car: u64,
    /// The pull requests of the batch, in order (those of earlier batches it builds on first).
    pub pull_requests: Vec<u64>,
    /// The pull requests this batch checks.
    pub own: Vec<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub draft: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    pub in_place: bool,
    pub status: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Place {
    pub queue: String,
    pub rule: String,
    pub base: String,
    /// 0 for the first pull request of its queue on this base.
    pub position: usize,
    /// The pull requests ahead of it in the same queue and base.
    pub ahead: Vec<u64>,
    /// `waiting`, `in-batch`, `bisecting`, `held`.
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub held_on: Option<Vec<u64>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<BatchInfo>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub queued_by: Option<String>,
    pub priority: u32,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Dequeue {
    pub code: Option<String>,
    pub text: String,
    pub assumed: bool,
}

/// Why a pull request is where it is.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Explanation {
    pub number: u64,
    pub title: String,
    /// `merged`, `queued`, `waiting-for-conditions` (a `queue` command waits), `dequeued`,
    /// `held`, `not-queued`, `closed`, `queue-draft`, `unknown`.
    pub state: String,
    /// One sentence.
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub place: Option<Place>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub holds: Vec<Hold>,
    /// What keeps a queued pull request from landing now: the pause, a freeze, a merge
    /// protection, a halted queue.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub landing_holds: Vec<String>,
    /// A `queue` command waiting for the queue conditions: who asked.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waiting_command: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub rules: Vec<RuleResult>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub queue_rules: Vec<QueueRuleResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_dequeue: Option<Dequeue>,
    /// The code of the last time its queue checks ended (`queue-dequeue-reason`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checks_ended: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub merged_as: Option<String>,
}

/// Renders an atom as written in a configuration.
pub fn atom_text(a: &Atom) -> String {
    let mut s = String::new();
    if a.negate {
        s.push('-');
    }
    if a.length {
        s.push('#');
    }
    s.push_str(&a.attribute);
    if let (Some(op), Some(v)) = (a.operator, &a.value) {
        s.push(' ');
        s.push_str(op.as_str());
        s.push(' ');
        s.push_str(v);
    }
    s
}

fn value_text(v: &AttrValue) -> String {
    const MAX: usize = 8;
    match v {
        AttrValue::Bool(b) => b.to_string(),
        AttrValue::Str(s) => format!("`{s}`"),
        AttrValue::List(l) if l.is_empty() => "[]".into(),
        AttrValue::List(l) => {
            let mut shown: Vec<String> = l.iter().take(MAX).map(|x| format!("`{x}`")).collect();
            if l.len() > MAX {
                shown.push(format!("… {} more", l.len() - MAX));
            }
            format!("[{}]", shown.join(", "))
        }
        AttrValue::Time(None) => "unset".into(),
        AttrValue::Time(Some(t)) => crate::time::format_utc(*t),
        AttrValue::Permission(r) => eval::PERMISSIONS
            .get(*r)
            .map(|p| p.to_string())
            .unwrap_or_else(|| r.to_string()),
    }
}

/// The value of a check attribute: the current runs it selects with their state (for `#`,
/// how many), or every current run when it selects none.
fn check_value(atom: &Atom, pr: &PullRequest) -> Option<String> {
    use crate::model::Conclusion::*;
    let kind = atom.attribute.as_str();
    let selects: fn(Option<crate::model::Conclusion>) -> bool = match kind {
        "check" => |_| true,
        "check-success" => |c| c == Some(Success),
        "check-failure" => |c| matches!(c, Some(Failure | Cancelled | TimedOut | ActionRequired)),
        "check-neutral" => |c| c == Some(Neutral),
        "check-skipped" => |c| c == Some(Skipped),
        "check-cancelled" => |c| c == Some(Cancelled),
        "check-timed-out" => |c| c == Some(TimedOut),
        "check-stale" => |c| c == Some(Stale),
        "check-pending" => |c| c.is_none(),
        _ => return None,
    };
    let runs = pr.current_checks();
    let show = |r: &&crate::model::CheckRun| {
        let state = match r.conclusion {
            None => "pending".to_string(),
            Some(c) => serde_json::to_value(c)
                .ok()
                .and_then(|v| v.as_str().map(str::to_string))
                .unwrap_or_default(),
        };
        format!("{}: {state}", r.name)
    };
    let selected: Vec<String> = runs
        .iter()
        .filter(|r| selects(r.conclusion))
        .map(show)
        .collect();
    if atom.length {
        return Some(selected.len().to_string());
    }
    Some(if !selected.is_empty() {
        format!("[{}]", selected.join(", "))
    } else if runs.is_empty() {
        "no checks".into()
    } else {
        format!(
            "none; current: {}",
            runs.iter().map(show).collect::<Vec<_>>().join(", ")
        )
    })
}

/// A condition shortened for one-line summaries.
fn clip(text: &str) -> String {
    const MAX: usize = 100;
    if text.chars().count() > MAX {
        format!("{}…", text.chars().take(MAX).collect::<String>())
    } else {
        text.to_string()
    }
}

/// Evaluates a condition tree with every node's result (three-valued, as `eval::eval`).
pub fn evaluate(cond: &Condition, pr: &PullRequest, ctx: &Context) -> CondResult {
    match cond {
        Condition::Atom(a) => {
            let (result, error) = match eval::eval_atom(a, pr, ctx) {
                Ok(r) => (Some(r), None),
                Err(e) => (None, Some(e.to_string())),
            };
            let value = check_value(a, pr).or_else(|| {
                eval::attribute(pr, ctx, &a.attribute)
                    .ok()
                    .map(|v| match (&v, a.length) {
                        (AttrValue::List(l), true) => l.len().to_string(),
                        _ => value_text(&v),
                    })
            });
            CondResult {
                condition: atom_text(a),
                result,
                value,
                error,
                children: vec![],
            }
        }
        Condition::And(cs) | Condition::Or(cs) => {
            let children: Vec<CondResult> = cs.iter().map(|c| evaluate(c, pr, ctx)).collect();
            let and = matches!(cond, Condition::And(_));
            let decisive = Some(!and);
            let result = if children.iter().any(|c| c.result == decisive) {
                decisive
            } else if children.iter().any(|c| c.result.is_none()) {
                None
            } else {
                Some(and)
            };
            CondResult {
                condition: if and { "and" } else { "or" }.into(),
                result,
                value: None,
                error: None,
                children,
            }
        }
        Condition::Not(c) => {
            let child = evaluate(c, pr, ctx);
            CondResult {
                condition: "not".into(),
                result: child.result.map(|r| !r),
                value: None,
                error: None,
                children: vec![child],
            }
        }
        Condition::Always => CondResult {
            condition: "true".into(),
            result: Some(true),
            value: None,
            error: None,
            children: vec![],
        },
    }
}

pub fn evaluate_list(list: &[Condition], pr: &PullRequest, ctx: &Context) -> ListResult {
    let conditions: Vec<CondResult> = list.iter().map(|c| evaluate(c, pr, ctx)).collect();
    ListResult {
        holds: conditions.iter().all(CondResult::holds),
        conditions,
    }
}

fn car_status(s: &CarStatus) -> String {
    match s {
        CarStatus::Building { .. } => "being built".into(),
        CarStatus::Running => "checks running".into(),
        CarStatus::Green => "checks passed, waiting to merge".into(),
        CarStatus::Failed { code, .. } => format!("checks failed ({code})"),
        CarStatus::Verifying { .. } => "checks passed, reading GitHub before merging".into(),
        CarStatus::Confirming { .. } => "checks look failed, confirming with a fresh read".into(),
        CarStatus::Landing { .. } => "merging".into(),
        CarStatus::LandCheck { .. } => "merge outcome unknown, reading GitHub".into(),
        CarStatus::Retry => "checks failed, retried on a fresh draft".into(),
    }
}

fn list(ns: &[u64]) -> String {
    ns.iter()
        .map(|n| format!("#{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// The queue rules a pull request may enter under `rules`, with its conditions evaluated.
fn queue_rule_results(
    state: &State,
    rules: &Rules,
    pr: &PullRequest,
    ctx: &Context,
    only: Option<&str>,
) -> Vec<QueueRuleResult> {
    let entry = state.queued.get(&pr.number);
    // While it rides in a batch the merge conditions see the batch's checks.
    let car_pr = entry.and_then(|e| match e.status {
        EntryStatus::InCar { car } => state
            .trains
            .get(&e.base)
            .and_then(|t| t.cars.iter().find(|c| c.id == car))
            .and_then(|c| crate::queue::car_view(state, c, pr.number)),
        _ => None,
    });
    rules
        .queue_rules
        .iter()
        .filter(|q| only.is_none_or(|o| o == q.name))
        .map(|q| QueueRuleResult {
            queue: q.name.clone(),
            queue_conditions: evaluate_list(
                &crate::gates::queue_conditions(state, q, &pr.base),
                pr,
                ctx,
            ),
            merge_conditions: evaluate_list(
                &crate::gates::merge_conditions(state, q, &pr.base),
                car_pr.as_ref().unwrap_or(pr),
                ctx,
            ),
        })
        .collect()
}

/// Explains pull request `number` from the engine state under `rules`.
pub fn explain(state: &State, rules: &Rules, number: u64) -> Explanation {
    let mut ex = Explanation {
        number,
        title: String::new(),
        state: "unknown".into(),
        summary: String::new(),
        place: None,
        holds: state.holds.get(&number).cloned().unwrap_or_default(),
        landing_holds: Vec::new(),
        waiting_command: None,
        rules: Vec::new(),
        queue_rules: Vec::new(),
        last_dequeue: None,
        checks_ended: state.checks_ended.get(&number).cloned(),
        merged_as: None,
    };
    if let Some(Outcome::Dequeued { reason }) = state.outcomes.get(&number) {
        ex.last_dequeue = Some(Dequeue {
            code: reason.code.map(str::to_string),
            text: reason.text.clone(),
            assumed: reason.assumed,
        });
    }
    let Some(pr) = state.pull_requests.get(&number) else {
        ex.summary = format!("#{number} is not known to the queue (never read from the forge)");
        return ex;
    };
    ex.title = pr.title.clone();
    if is_queue_pull_request(rules, pr) {
        ex.state = "queue-draft".into();
        ex.summary = format!("#{number} is a draft pull request of the merge queue itself");
        return ex;
    }
    let ctx = context(state, number);
    if let Some(Outcome::Merged { sha }) = state.outcomes.get(&number) {
        ex.state = "merged".into();
        ex.merged_as = Some(sha.clone());
        ex.summary = if sha.is_empty() {
            format!("#{number} was merged by the queue")
        } else {
            format!(
                "#{number} was merged by the queue as {}",
                crate::engine::short(sha)
            )
        };
        return ex;
    }
    if pr.merged {
        ex.state = "merged".into();
        ex.merged_as = pr.merge_commit_sha.clone();
        ex.summary = format!("#{number} was merged outside the queue");
        return ex;
    }
    if pr.closed {
        ex.state = "closed".into();
        ex.summary = format!("#{number} is closed");
        return ex;
    }
    if let Some(e) = state.queued.get(&number) {
        ex.state = "queued".into();
        let mut same: Vec<(u64, u64)> = state
            .queued
            .iter()
            .filter(|(_, o)| o.base == e.base && o.queue == e.queue)
            .map(|(n, o)| (o.seq, *n))
            .collect();
        same.sort();
        let ahead: Vec<u64> = if e.status == EntryStatus::Waiting {
            // The engine's order: the pull requests in batches on this base, then the waiting
            // ones by priority, queue rule and arrival.
            let mut v: Vec<u64> = state
                .trains
                .get(&e.base)
                .map(|t| t.cars.iter().flat_map(|c| c.own.clone()).collect())
                .unwrap_or_default();
            v.extend(
                crate::queue::waiting_order(state, rules, &e.base)
                    .into_iter()
                    .take_while(|n| *n != number),
            );
            v.retain(|n| *n != number);
            v
        } else {
            same.iter()
                .take_while(|(_, n)| *n != number)
                .map(|(_, n)| *n)
                .collect()
        };
        let (status, held_on, batch) = match &e.status {
            EntryStatus::Waiting => ("waiting", None, None),
            EntryStatus::Bisecting => ("bisecting", None, None),
            EntryStatus::Held { on } => ("held", Some(on.clone()), None),
            EntryStatus::InCar { car } => {
                let c = state
                    .trains
                    .get(&e.base)
                    .and_then(|t| t.cars.iter().find(|c| c.id == *car));
                (
                    "in-batch",
                    None,
                    c.map(|c| BatchInfo {
                        car: c.id,
                        pull_requests: c.prs.iter().map(|p| p.number).collect(),
                        own: c.own.clone(),
                        draft: c.draft.filter(|_| !c.in_place),
                        branch: c.branch.clone(),
                        in_place: c.in_place,
                        status: car_status(&c.status),
                    }),
                )
            }
        };
        if let Some(p) = &state.pause {
            ex.landing_holds
                .push(format!("the merge queue is paused: {}", p.reason));
        } else if let Some(h) = state.landing_holds.get(&number) {
            ex.landing_holds.push(h.clone());
        }
        if let Some(h) = state.trains.get(&e.base).and_then(|t| t.halted.clone()) {
            ex.landing_holds
                .push(format!("the queue of `{}` is halted: {h}", e.base));
        }
        ex.summary = match (&batch, status) {
            (Some(b), _) => format!(
                "#{number} is queued in `{}` and being checked in batch {} ({}){}",
                e.queue,
                b.car,
                b.status,
                b.draft
                    .map(|d| format!(" on draft #{d}"))
                    .unwrap_or_default()
            ),
            (None, "held") => format!(
                "#{number} is queued in `{}` but held: it conflicts with {} ahead",
                e.queue,
                list(held_on.as_deref().unwrap_or(&[]))
            ),
            (None, "bisecting") => format!(
                "#{number} is queued in `{}`, part of a failed batch being split",
                e.queue
            ),
            _ if ahead.is_empty() => {
                format!(
                    "#{number} is first in queue `{}`, waiting for a batch",
                    e.queue
                )
            }
            _ => format!(
                "#{number} is queued in `{}` at position {}, behind {}",
                e.queue,
                ahead.len() + 1,
                list(&ahead)
            ),
        };
        if let Some(h) = ex.landing_holds.first() {
            ex.summary.push_str(&format!("; it may not merge yet: {h}"));
        }
        ex.place = Some(Place {
            queue: e.queue.clone(),
            rule: e.rule.clone(),
            base: e.base.clone(),
            position: ahead.len(),
            ahead,
            status: status.into(),
            held_on,
            batch,
            queued_by: e.by.clone(),
            priority: e.priority,
        });
        ex.queue_rules = queue_rule_results(state, rules, pr, &ctx, Some(&e.queue));
        return ex;
    }
    // Not queued: every rule that could queue it, every queue rule it could enter.
    for r in &rules.pull_request_rules {
        let Some(target) = &r.queue else { continue };
        ex.rules.push(RuleResult {
            rule: r.name.clone(),
            queue: target.clone(),
            conditions: evaluate_list(&r.conditions, pr, &ctx),
        });
    }
    let pending = state.pending_commands.get(&number);
    ex.queue_rules = queue_rule_results(
        state,
        rules,
        pr,
        &ctx,
        pending.and_then(|p| p.queue.as_deref()),
    );
    let failing_queue = || -> String {
        ex.queue_rules
            .iter()
            .map(|q| {
                let f = q.queue_conditions.failing();
                format!(
                    "queue `{}`: {}",
                    q.queue,
                    if f.is_empty() {
                        "its conditions hold".into()
                    } else {
                        f.join("; ")
                    }
                )
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    if let Some(p) = pending {
        ex.state = "waiting-for-conditions".into();
        ex.waiting_command = Some(p.by.clone());
        ex.summary = format!(
            "@{} asked to queue #{number}; it waits for {}",
            p.by,
            failing_queue()
        );
        return ex;
    }
    if !ex.holds.is_empty() {
        ex.state = "held".into();
        ex.summary = format!(
            "#{number} is held: {}",
            ex.holds
                .iter()
                .map(|h| format!("{} ({})", h.text, h.kind))
                .collect::<Vec<_>>()
                .join("; ")
        );
        return ex;
    }
    if let Some(d) = &ex.last_dequeue {
        ex.state = "dequeued".into();
        ex.summary = format!(
            "#{number} left the queue: {}{}",
            d.code
                .as_deref()
                .map(|c| format!("{c}: "))
                .unwrap_or_default(),
            d.text
        );
        if state.blocked_heads.get(&number) == Some(&pr.head_sha) {
            ex.summary
                .push_str("; it rejoins only after a new push or a `queue` command");
        } else if state.blocked_attrs.contains_key(&number) {
            ex.summary
                .push_str("; it rejoins only after its attributes change");
        }
        return ex;
    }
    ex.state = "not-queued".into();
    let matching: Vec<&RuleResult> = ex.rules.iter().filter(|r| r.conditions.holds).collect();
    ex.summary = if ex.rules.is_empty() {
        format!("#{number} is not queued: no rule queues it automatically; use a `queue` command")
    } else if matching.is_empty() {
        format!(
            "#{number} is not queued: no rule matches ({})",
            ex.rules
                .iter()
                .map(|r| format!("`{}`: {}", r.rule, r.conditions.failing().join("; ")))
                .collect::<Vec<_>>()
                .join(" | ")
        )
    } else {
        format!(
            "#{number} is not queued: rule `{}` matches, but {}",
            matching[0].rule,
            failing_queue()
        )
    };
    ex
}

fn mark(r: Option<bool>) -> &'static str {
    match r {
        Some(true) => "[ok]",
        Some(false) => "[no]",
        None => "[??]",
    }
}

fn tree_lines(c: &CondResult, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    let mut line = format!("{pad}{} {}", mark(c.result), c.condition);
    if let Some(v) = &c.value {
        line.push_str(&format!("  (value: {v})"));
    }
    if let Some(e) = &c.error {
        line.push_str(&format!("  (error: {e})"));
    }
    out.push(line);
    for ch in &c.children {
        tree_lines(ch, depth + 1, out);
    }
}

fn list_lines(name: &str, l: &ListResult, depth: usize, out: &mut Vec<String>) {
    let pad = "  ".repeat(depth);
    if l.conditions.is_empty() {
        out.push(format!("{pad}{name}: none (always true)"));
        return;
    }
    out.push(format!(
        "{pad}{name}: {}",
        if l.holds { "hold" } else { "do not hold" }
    ));
    for c in &l.conditions {
        tree_lines(c, depth + 1, out);
    }
}

impl Explanation {
    /// The explanation as text, for `schlange explain` (stable: golden tests).
    pub fn to_text(&self) -> String {
        let mut out = vec![
            format!(
                "#{}{}: {}",
                self.number,
                if self.title.is_empty() {
                    String::new()
                } else {
                    format!(" {}", self.title)
                },
                self.state
            ),
            self.summary.clone(),
        ];
        if let Some(p) = &self.place {
            out.push(String::new());
            out.push(format!(
                "queue `{}` on `{}` (rule `{}`{}), position {}",
                p.queue,
                p.base,
                p.rule,
                p.queued_by
                    .as_ref()
                    .map(|b| format!(", queued by @{b}"))
                    .unwrap_or_default(),
                p.position + 1
            ));
            if !p.ahead.is_empty() {
                out.push(format!("  ahead: {}", list(&p.ahead)));
            }
            if let Some(b) = &p.batch {
                out.push(format!(
                    "  batch {}: {} ({}){}{}",
                    b.car,
                    list(&b.own),
                    b.status,
                    b.draft.map(|d| format!(", draft #{d}")).unwrap_or_default(),
                    if b.pull_requests.len() > b.own.len() {
                        format!(", built on {}", {
                            let ahead: Vec<u64> = b
                                .pull_requests
                                .iter()
                                .copied()
                                .filter(|n| !b.own.contains(n))
                                .collect();
                            list(&ahead)
                        })
                    } else {
                        String::new()
                    }
                ));
            }
        }
        if !self.landing_holds.is_empty() {
            out.push(String::new());
            out.push("may not merge now:".into());
            for h in &self.landing_holds {
                out.push(format!("  - {h}"));
            }
        }
        if !self.holds.is_empty() {
            out.push(String::new());
            out.push("holds:".into());
            for h in &self.holds {
                out.push(format!(
                    "  - {} {}: {}",
                    if h.failed { "[no]" } else { "[..]" },
                    h.kind,
                    h.text
                ));
            }
        }
        if let Some(by) = &self.waiting_command {
            out.push(String::new());
            out.push(format!(
                "waiting `queue` command by @{by}: it runs once the queue conditions hold"
            ));
        }
        if !self.rules.is_empty() {
            out.push(String::new());
            out.push("pull_request_rules with a `queue` action:".into());
            for r in &self.rules {
                out.push(format!(
                    "  rule `{}` -> {}",
                    r.rule,
                    r.queue
                        .as_ref()
                        .map(|q| format!("queue `{q}`"))
                        .unwrap_or_else(|| "first matching queue".into())
                ));
                list_lines("conditions", &r.conditions, 2, &mut out);
            }
        }
        if !self.queue_rules.is_empty() {
            out.push(String::new());
            out.push("queue rules:".into());
            for q in &self.queue_rules {
                out.push(format!("  queue `{}`", q.queue));
                list_lines("queue_conditions", &q.queue_conditions, 2, &mut out);
                list_lines("merge_conditions", &q.merge_conditions, 2, &mut out);
            }
        }
        if let Some(d) = &self.last_dequeue {
            out.push(String::new());
            out.push(format!(
                "last dequeue: {}{}{}",
                d.code.as_deref().unwrap_or("(no code)"),
                if d.assumed { " (code assumed)" } else { "" },
                format_args!(": {}", d.text)
            ));
        }
        if let Some(c) = &self.checks_ended {
            out.push(format!("last checks ended: {c}"));
        }
        let mut s = out.join("\n");
        s.push('\n');
        s
    }

    /// One line for the status comment: where it stands and what it waits for.
    pub fn short(&self) -> String {
        let s = &self.summary;
        const MAX: usize = 300;
        if s.chars().count() > MAX {
            let cut: String = s.chars().take(MAX).collect();
            format!("{cut}…")
        } else {
            s.clone()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failing_atoms_of_nested_trees() {
        let rules = Rules::from_yaml(
            "queue_rules:\n  - name: default\n    queue_conditions:\n      - or:\n          - label = a\n          - and:\n              - label = b\n              - -draft\n      - not:\n          label = halt\n",
        )
        .unwrap();
        let mut pr = PullRequest::new(3, "x", "main", "f", "h");
        pr.labels.insert("halt".into());
        pr.labels.insert("b".into());
        pr.draft = true;
        let l = evaluate_list(
            &rules.queue_rules[0].queue_conditions,
            &pr,
            &Context::default(),
        );
        assert!(!l.holds);
        let f = l.failing();
        assert_eq!(
            f,
            vec![
                "one of [label = a; all of [label = b; -draft]]".to_string(),
                "not (label = halt) (is [`b`, `halt`])".to_string()
            ],
            "{f:?}"
        );
    }
}
