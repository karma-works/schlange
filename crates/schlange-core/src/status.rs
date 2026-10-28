//! A read-only view of the queue in the shape of the documented merge queue status API
//! (`GET /repos/{owner}/{repository}/merge-queue/status`): the batches being checked with their
//! pull requests, the pull requests waiting, the mode and the pause. Field names follow the
//! documented response; the values are Schlange's own (car ids for batch ids, our status
//! codes mapped onto the documented ones). Extra fields Schlange knows are added under their
//! own names (`held`, `freezes`).

use serde::Serialize;

use schlange_config::queue_extras::priority_alias;
use schlange_config::rules::Rules;

use crate::engine::{EntryStatus, Freeze, State};
use crate::model::Time;
use crate::queue::{CarStatus, has_live_deps, pr_scopes, waiting_order};
use crate::time::format_utc;

#[derive(Clone, Debug, Serialize)]
pub struct QueueStatus {
    pub mode: String,
    pub batches: Vec<Batch>,
    pub waiting_pull_requests: Vec<StatusPullRequest>,
    pub pause: Option<PauseStatus>,
    /// Scheduled freezes known to the queue (active or not).
    pub freezes: Vec<FreezeStatus>,
    /// Base branches whose queue halted, with the reason.
    pub halted: Vec<(String, String)>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PauseStatus {
    pub paused: bool,
    pub reason: String,
    pub paused_at: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct FreezeStatus {
    pub id: u64,
    pub reason: String,
    pub start: String,
    pub end: Option<String>,
    pub matching_conditions: Vec<String>,
    pub exclude_conditions: Vec<String>,
    pub active: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct BatchStatus {
    pub code: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub struct ChecksSummary {
    pub passed: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct Batch {
    pub id: String,
    /// The queue branch (or `#n` when checked in place).
    pub name: String,
    pub parent_ids: Vec<String>,
    pub status: BatchStatus,
    pub started_at: Option<String>,
    pub scopes: Vec<String>,
    pub queue_rule_name: String,
    pub checks_summary: ChecksSummary,
    pub pull_requests: Vec<StatusPullRequest>,
    pub base_branch: String,
    pub queue_pull_request_number: Option<u64>,
}

#[derive(Clone, Debug, Serialize)]
pub struct StatusPullRequest {
    pub number: u64,
    pub title: String,
    pub queued_at: String,
    pub priority_alias: String,
    pub priority_rule_name: Option<String>,
    pub scopes: Vec<String>,
    pub all_scopes: bool,
    pub queue_rule_name: String,
    pub is_frozen: bool,
    /// Why it may not merge now (pause, freeze, merge protection), if anything holds it.
    pub held: Option<String>,
}

fn time(state: &State, t: Time) -> String {
    format_utc(state.host.epoch as i64 + t as i64)
}

fn pull(state: &State, rules: &Rules, n: u64) -> Option<StatusPullRequest> {
    let e = state.queued.get(&n)?;
    let pr = state.pull_requests.get(&n);
    let (scopes, barrier) = pr_scopes(rules, pr);
    let held = state.landing_holds.get(&n).cloned();
    Some(StatusPullRequest {
        number: n,
        title: pr.map(|p| p.title.clone()).unwrap_or_default(),
        queued_at: time(state, e.since),
        priority_alias: priority_alias(e.priority),
        priority_rule_name: e.priority_rule.clone(),
        scopes,
        all_scopes: barrier,
        queue_rule_name: e.queue.clone(),
        is_frozen: held.as_deref().is_some_and(|h| h.starts_with("frozen")),
        held,
    })
}

fn freeze(state: &State, f: &Freeze) -> FreezeStatus {
    FreezeStatus {
        id: f.id,
        reason: f.reason.clone(),
        start: time(state, f.start),
        end: f.end.map(|e| time(state, e)),
        matching_conditions: f.matching.clone(),
        exclude_conditions: f.exclude.clone(),
        active: f.active(state.now),
    }
}

/// The status of every queue of the repository.
pub fn queue_status(state: &State, rules: &Rules) -> QueueStatus {
    let mut batches = Vec::new();
    let mut waiting = Vec::new();
    for (base, train) in &state.trains {
        let mut t = train.clone();
        t.mode = crate::queue::mode(rules);
        for (i, c) in t.cars.iter().enumerate() {
            let held = c.own.iter().any(|n| state.landing_holds.contains_key(n));
            let code = match &c.status {
                _ if c.test.is_some() => "bisecting",
                CarStatus::Building { .. } => "preparing",
                CarStatus::Running | CarStatus::Confirming { .. } => "running",
                CarStatus::Green if held => "frozen",
                CarStatus::Green if has_live_deps(&t, i) => "waiting_for_previous_batches",
                CarStatus::Green
                | CarStatus::Verifying { .. }
                | CarStatus::Landing { .. }
                | CarStatus::LandCheck { .. } => "waiting_for_merge",
                CarStatus::Failed { .. } => "failed",
                CarStatus::Retry => "waiting_for_requeue",
            };
            let checks = c
                .draft
                .and_then(|d| state.pull_requests.get(&d))
                .map(|d| d.current_checks().into_iter().cloned().collect::<Vec<_>>())
                .unwrap_or_default();
            let parents: Vec<String> = match t.mode {
                crate::queue::Mode::Serial => t.cars[..i]
                    .last()
                    .map(|p| vec![p.id.to_string()])
                    .unwrap_or_default(),
                _ => c.deps.iter().map(u64::to_string).collect(),
            };
            batches.push(Batch {
                id: c.id.to_string(),
                name: c
                    .branch
                    .clone()
                    .unwrap_or_else(|| format!("#{}", c.draft.unwrap_or(0))),
                parent_ids: parents,
                status: BatchStatus { code },
                started_at: (!matches!(c.status, CarStatus::Building { .. }))
                    .then(|| time(state, c.started)),
                scopes: c.scopes.clone(),
                queue_rule_name: c.queue.clone(),
                checks_summary: ChecksSummary {
                    passed: checks
                        .iter()
                        .filter(|r| r.conclusion == Some(crate::model::Conclusion::Success))
                        .count(),
                    total: checks.len(),
                },
                pull_requests: c
                    .own
                    .iter()
                    .filter_map(|n| pull(state, rules, *n))
                    .collect(),
                base_branch: base.clone(),
                queue_pull_request_number: c.draft.filter(|_| !c.in_place),
            });
        }
        for n in waiting_order(state, rules, base) {
            waiting.extend(pull(state, rules, n));
        }
        for (n, e) in &state.queued {
            if e.base == *base
                && matches!(e.status, EntryStatus::Held { .. } | EntryStatus::Bisecting)
            {
                waiting.extend(pull(state, rules, *n));
            }
        }
    }
    QueueStatus {
        mode: rules.merge_queue.mode.clone(),
        batches,
        waiting_pull_requests: waiting,
        pause: state.pause.as_ref().map(|p| PauseStatus {
            paused: true,
            reason: p.reason.clone(),
            paused_at: time(state, p.at),
        }),
        freezes: state.freezes.values().map(|f| freeze(state, f)).collect(),
        halted: state
            .trains
            .iter()
            .filter_map(|(b, t)| t.halted.clone().map(|h| (b.clone(), h)))
            .collect(),
    }
}
