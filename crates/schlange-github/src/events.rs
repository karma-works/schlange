//! The event log (feature `events`, MQ-43, W-03): every queue and action event of a repository,
//! persisted in the `events` table (or in memory without a data directory), queryable with
//! `schlange events` and `GET /repos/{owner}/{repo}/logs`.
//!
//! The record shape follows the public event log API (`id`, `received_at`, `type`,
//! `pull_request`, `base_ref`, `trigger`, `metadata`); the event types reuse its names where one
//! fits (`action.queue.enter`, `action.queue.leave`, `action.queue.checks_start`,
//! `action.queue.checks_end`, `action.queue.merged`, `queue.pause.create`, …). A decision without
//! a documented type is logged as `schlange.<kind>` with the decision as metadata, so new
//! decisions (for example `pull_request_rules` actions) are logged without a change here.
//! Reads (`Refresh`) and timers are not events. The metadata is Schlange's own.

use std::collections::VecDeque;
use std::sync::Mutex;

use schlange_core::Decision;
use schlange_core::engine::Control;
use schlange_core::model::Time;
use serde_json::{Value, json};

use crate::store::{EventQuery, EventRecord, Store};

/// Events kept in memory without a data directory.
const MEMORY_MAX: usize = 20_000;

/// The event types a decision produces, with the pull requests and base they belong to.
pub fn from_decision(repo: &str, d: &Decision, at: Time, trigger: &str) -> Option<EventRecord> {
    let ev = |kind: &str, prs: Vec<u64>, base: Option<String>, metadata: Value| EventRecord {
        id: 0,
        repository: repo.to_string(),
        at,
        kind: kind.to_string(),
        pull_request: (prs.len() == 1).then(|| prs[0]),
        pull_requests: prs,
        base_ref: base,
        trigger: trigger.to_string(),
        metadata,
    };
    Some(match d {
        Decision::Refresh { .. } | Decision::WakeAt { .. } => return None,
        Decision::Enqueue {
            number,
            queue,
            rule,
        } => ev(
            "action.queue.enter",
            vec![*number],
            None,
            json!({ "queue_name": queue, "rule": rule }),
        ),
        Decision::Dequeue {
            number,
            queue,
            reason,
        } => ev(
            "action.queue.leave",
            vec![*number],
            None,
            json!({ "queue_name": queue, "merged": false, "dequeue_code": reason.code,
                    "reason": reason.text, "code_assumed": reason.assumed }),
        ),
        Decision::ChecksEnded {
            number,
            queue,
            reason,
        } => ev(
            "action.queue.checks_end",
            vec![*number],
            None,
            json!({ "queue_name": queue, "aborted": true, "abort_code": reason.code,
                    "abort_reason": reason.text }),
        ),
        Decision::BuildCar {
            car,
            queue,
            base_branch,
            branch,
            prs,
            own,
            ..
        } => ev(
            "action.queue.checks_start",
            own.clone(),
            Some(base_branch.clone()),
            json!({ "queue_name": queue, "car": car, "branch": branch,
                    "batch": prs.iter().map(|p| p.number).collect::<Vec<_>>() }),
        ),
        Decision::CheckInPlace {
            car,
            number,
            direct,
            ..
        } => ev(
            "action.queue.checks_start",
            vec![*number],
            None,
            json!({ "car": car, "in_place": true, "direct_merge": direct }),
        ),
        Decision::CloseCar { car, branch, draft } => ev(
            "schlange.queue.car_closed",
            vec![],
            None,
            json!({ "car": car, "branch": branch, "draft": draft }),
        ),
        Decision::Land {
            car,
            queue,
            base_branch,
            prs,
            method,
            ..
        } => ev(
            "schlange.queue.landing",
            prs.iter().map(|p| p.number).collect(),
            Some(base_branch.clone()),
            json!({ "queue_name": queue, "car": car, "merge_method": method }),
        ),
        Decision::Merged { number, queue, sha } => ev(
            "action.queue.merged",
            vec![*number],
            None,
            json!({ "queue_name": queue, "sha": sha }),
        ),
        Decision::Halt {
            base_branch,
            reason,
        } => ev(
            "schlange.queue.halt",
            vec![],
            Some(base_branch.clone()),
            json!({ "reason": reason }),
        ),
        Decision::EvaluationError { number, message } => ev(
            "schlange.evaluation_error",
            vec![*number],
            None,
            json!({ "message": message }),
        ),
        Decision::Cosmetic { cosmetic } => ev(
            "schlange.cosmetic",
            vec![cosmetic.number()],
            None,
            json!({ "what": cosmetic.describe() }),
        ),
        #[allow(unreachable_patterns)]
        other => {
            let v = serde_json::to_value(other).unwrap_or(Value::Null);
            let kind = v["type"].as_str().unwrap_or("decision").to_string();
            let number = v["number"].as_u64();
            ev(
                &format!("schlange.{kind}"),
                number.into_iter().collect(),
                None,
                v,
            )
        }
    })
}

/// The event of an operator's control (pause, freeze).
pub fn from_control(repo: &str, c: &Control, at: Time, trigger: &str) -> EventRecord {
    let (kind, metadata) = match c {
        Control::Pause { reason, by } => {
            ("queue.pause.create", json!({ "reason": reason, "by": by }))
        }
        Control::Unpause => ("queue.pause.delete", json!({})),
        Control::Freeze { freeze } => (
            "scheduled_freeze.create",
            serde_json::to_value(freeze).unwrap_or_default(),
        ),
        Control::Unfreeze { id } => ("scheduled_freeze.delete", json!({ "id": id })),
    };
    EventRecord {
        id: 0,
        repository: repo.to_string(),
        at,
        kind: kind.into(),
        pull_request: None,
        pull_requests: vec![],
        base_ref: None,
        trigger: trigger.into(),
        metadata,
    }
}

/// The log: the database when there is one, a bounded ring in memory otherwise.
pub struct EventLog {
    store: Option<Store>,
    memory: Mutex<VecDeque<EventRecord>>,
    next: Mutex<u64>,
}

impl EventLog {
    pub fn new(store: Option<Store>) -> Self {
        EventLog {
            store,
            memory: Mutex::new(VecDeque::new()),
            next: Mutex::new(0),
        }
    }

    pub fn add(&self, mut e: EventRecord) {
        if let Some(s) = &self.store {
            let _ = s.add_event(&e);
            return;
        }
        let mut n = self.next.lock().unwrap_or_else(|p| p.into_inner());
        *n += 1;
        e.id = *n;
        let mut m = self.memory.lock().unwrap_or_else(|p| p.into_inner());
        if m.len() >= MEMORY_MAX {
            m.pop_front();
        }
        m.push_back(e);
    }

    /// Newest first, as `Store::events`.
    pub fn query(&self, repo: &str, q: &EventQuery) -> Result<Vec<EventRecord>, String> {
        if let Some(s) = &self.store {
            return s.events(repo, q).map_err(|e| e.to_string());
        }
        let m = self.memory.lock().unwrap_or_else(|p| p.into_inner());
        Ok(m.iter()
            .rev()
            .filter(|e| e.repository == repo)
            .filter(|e| q.pr.is_none_or(|n| e.pull_requests.contains(&n)))
            .filter(|e| q.since.is_none_or(|t| e.at >= t))
            .filter(|e| q.before.is_none_or(|b| e.id < b))
            .filter(|e| q.kind.as_ref().is_none_or(|k| &e.kind == k))
            .take(q.limit.clamp(1, 10_000) as usize)
            .cloned()
            .collect())
    }
}

/// The event as the public API shows it (`received_at` as RFC 3339).
pub fn api_json(e: &EventRecord) -> Value {
    json!({
        "id": e.id,
        "received_at": crate::time::format_rfc3339(e.at),
        "type": e.kind,
        "repository": e.repository,
        "pull_request": e.pull_request,
        "pull_requests": e.pull_requests,
        "base_ref": e.base_ref,
        "trigger": e.trigger,
        "metadata": e.metadata,
    })
}
