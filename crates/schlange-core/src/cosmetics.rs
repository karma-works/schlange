//! What the queue shows on pull requests (docs: merge-queue/lifecycle, "Queue labels" and
//! "Status comments"; commands/queue, "Queueing with a Checkbox"): labels, the status comment,
//! the comment with the "queue this pull request" checkbox, replies and reactions to
//! commands, and the check of a changed configuration.
//!
//! All of it is cosmetic: decided after the queue's own decisions, never read back as a
//! condition (the queue's own labels are left out of the attributes that can block a pull
//! request again, see `engine::fingerprint`), and sent by the adapters with the lowest
//! priority, so it never delays landing (MQ-34). The engine records what it last asked for,
//! so every label change and comment edit is decided once (idempotent under repeated
//! events, MQ-12).
//!
//! The texts are Schlange's own.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use schlange_config::rules::{Rules, StatusComments};

use crate::commands::{CONTROLS_MARKER, QUEUE_BOX, REQUEUE_BOX, STATUS_MARKER, checkbox};
use crate::engine::{Decision, EntryStatus, Outcome, State, is_queue_pull_request, short};
use crate::eval;
use crate::queue;

/// One cosmetic side effect.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Cosmetic {
    /// Add and remove the queue's labels.
    Labels {
        number: u64,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// Create or edit the queue's status comment on the pull request.
    StatusComment { number: u64, body: String },
    /// Create or edit the comment with the "queue this pull request" checkbox.
    ControlsComment { number: u64, body: String },
    /// A reply to a command (`to`: the command's comment, `None` for a checkbox).
    Reply {
        number: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        to: Option<u64>,
        body: String,
    },
    /// A reaction to a command's comment.
    React {
        number: u64,
        comment: u64,
        content: String,
    },
    /// A check run on the pull request's head reporting a built-in protection.
    ProtectionCheck {
        number: u64,
        head_sha: String,
        name: String,
        ok: bool,
        title: String,
        summary: String,
    },
}

impl Cosmetic {
    pub fn number(&self) -> u64 {
        match self {
            Cosmetic::Labels { number, .. }
            | Cosmetic::StatusComment { number, .. }
            | Cosmetic::ControlsComment { number, .. }
            | Cosmetic::Reply { number, .. }
            | Cosmetic::React { number, .. }
            | Cosmetic::ProtectionCheck { number, .. } => *number,
        }
    }

    /// One line for logs.
    pub fn describe(&self) -> String {
        // The text lines without markers, joined.
        let first = |b: &str| {
            b.lines()
                .map(|l| match l.find("<!--") {
                    Some(i) => l[..i].trim(),
                    None => l.trim(),
                })
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" — ")
        };
        match self {
            Cosmetic::Labels {
                number,
                add,
                remove,
            } => {
                let mut parts: Vec<String> = add.iter().map(|l| format!("+{l}")).collect();
                parts.extend(remove.iter().map(|l| format!("-{l}")));
                format!("labels #{number} {}", parts.join(" "))
            }
            Cosmetic::StatusComment { number, body } => {
                format!("status comment #{number}: {}", first(body))
            }
            Cosmetic::ControlsComment { number, body } => {
                format!("controls comment #{number}: {}", first(body))
            }
            Cosmetic::Reply { number, body, .. } => format!("reply #{number}: {}", first(body)),
            Cosmetic::React {
                number,
                comment,
                content,
            } => format!("react {content} to comment {comment} on #{number}"),
            Cosmetic::ProtectionCheck {
                number, name, ok, ..
            } => format!(
                "check `{name}` on #{number}: {}",
                if *ok { "success" } else { "failure" }
            ),
        }
    }
}

/// What the engine last asked the forge to show, per pull request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Applied {
    #[serde(default)]
    pub labels: BTreeMap<u64, BTreeSet<String>>,
    #[serde(default)]
    pub status: BTreeMap<u64, String>,
    #[serde(default)]
    pub controls: BTreeMap<u64, String>,
    #[serde(default)]
    pub checks: BTreeMap<u64, String>,
}

/// The labels the queue manages under `rules`.
pub fn queue_labels(rules: &Rules) -> BTreeSet<String> {
    let mq = &rules.merge_queue;
    [&mq.queued_label, &mq.checking_label, &mq.dequeued_label]
        .into_iter()
        .flatten()
        .cloned()
        .collect()
}

pub(crate) fn emit(out: &mut Vec<Decision>, cosmetic: Cosmetic) {
    out.push(Decision::Cosmetic { cosmetic });
}

fn wanted_labels(state: &State, rules: &Rules, n: u64) -> BTreeSet<String> {
    let mq = &rules.merge_queue;
    let mut want = BTreeSet::new();
    if let Some(e) = state.queued.get(&n) {
        want.extend(mq.queued_label.clone());
        if matches!(e.status, EntryStatus::InCar { .. } | EntryStatus::Bisecting) {
            want.extend(mq.checking_label.clone());
        }
    } else if matches!(state.outcomes.get(&n), Some(Outcome::Dequeued { .. }))
        && !state.pull_requests.get(&n).is_some_and(|p| p.merged)
    {
        want.extend(mq.dequeued_label.clone());
    }
    want
}

/// The status comment's body and whether it reports a final outcome.
fn status_body(state: &State, rules: &Rules, n: u64) -> Option<(String, bool)> {
    let mut lines = vec![STATUS_MARKER.to_string()];
    let outcome;
    if let Some(e) = state.queued.get(&n) {
        outcome = false;
        let by =
            e.by.as_ref()
                .map(|b| format!(" (queued by @{b})"))
                .unwrap_or_default();
        lines.push(format!("**Merge queue** · queue `{}`{by}", e.queue));
        lines.push(String::new());
        lines.push(match &e.status {
            EntryStatus::Waiting => {
                // The short form of `schlange explain` (S-EXPLAIN): its place in the queue.
                let ex = crate::explain::explain(state, rules, n);
                match ex.place.as_ref().map(|p| p.ahead.clone()) {
                    Some(ahead) if !ahead.is_empty() => format!(
                        "Waiting for its turn: position {} in queue `{}`, behind {}.",
                        ahead.len() + 1,
                        e.queue,
                        ahead
                            .iter()
                            .map(|x| format!("#{x}"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    _ => "Waiting for its turn: first in its queue.".to_string(),
                }
            }
            EntryStatus::InCar { car } => {
                let draft = state
                    .trains
                    .get(&e.base)
                    .and_then(|t| t.cars.iter().find(|c| c.id == *car))
                    .and_then(|c| c.draft.filter(|_| !c.in_place));
                match draft {
                    Some(d) => format!("Being checked in draft pull request #{d}."),
                    None => "Being checked.".to_string(),
                }
            }
            EntryStatus::Bisecting => {
                "Part of a failed batch that is being split to find the cause.".to_string()
            }
            EntryStatus::Held { on } => format!(
                "Held: it conflicts with {} ahead in the queue and keeps its place until they \
                 leave or merge.",
                on.iter()
                    .map(|x| format!("#{x}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        });
        if let Some(p) = &state.pause {
            lines.push(String::new());
            lines.push(format!(
                "The merge queue is paused ({}): nothing is checked or merged until it resumes.",
                p.reason
            ));
        } else if let Some(h) = state.landing_holds.get(&n) {
            lines.push(String::new());
            lines.push(format!("It may not merge yet: {h}."));
        }
    } else if let Some(p) = state.pending_commands.get(&n) {
        outcome = false;
        lines.push("**Merge queue** · waiting to enter".to_string());
        lines.push(String::new());
        lines.push(format!(
            "@{} asked to queue this pull request; it enters {} once its queue conditions hold.",
            p.by,
            p.queue
                .as_ref()
                .map(|q| format!("queue `{q}`"))
                .unwrap_or_else(|| "the first matching queue".into())
        ));
        // The short form of `schlange explain`: the conditions it waits for.
        let waits: Vec<String> = crate::explain::explain(state, rules, n)
            .queue_rules
            .iter()
            .flat_map(|q| q.queue_conditions.failing_plain())
            .collect();
        if !waits.is_empty() {
            lines.push(String::new());
            let mut text = waits.join("; ");
            if text.chars().count() > 300 {
                text = format!("{}…", text.chars().take(300).collect::<String>());
            }
            lines.push(format!("Waiting for: {text}"));
        }
    } else {
        match state.outcomes.get(&n)? {
            Outcome::Merged { sha } => {
                outcome = true;
                lines.push("**Merge queue** · merged".to_string());
                lines.push(String::new());
                lines.push(if sha.is_empty() {
                    "Merged by the queue.".to_string()
                } else {
                    format!("Merged by the queue as {}.", short(sha))
                });
            }
            Outcome::Dequeued { reason } => {
                outcome = true;
                lines.push("**Merge queue** · left the queue".to_string());
                lines.push(String::new());
                lines.push(format!("Reason: {}", reason.describe()));
                let open = state
                    .pull_requests
                    .get(&n)
                    .is_some_and(|p| !p.closed && !p.merged);
                if rules.merge_queue.queue_controls_comment && open {
                    lines.push(String::new());
                    lines.push(checkbox(
                        "Put this pull request back into the queue",
                        REQUEUE_BOX,
                    ));
                }
            }
        }
    }
    Some((lines.join("\n"), outcome))
}

/// Whether some queue's conditions hold for an open pull request that nothing queued.
fn ready(state: &State, rules: &Rules, n: u64) -> bool {
    let Some(pr) = state.pull_requests.get(&n) else {
        return false;
    };
    if pr.closed || pr.merged || state.holds.get(&n).is_some_and(|h| !h.is_empty()) {
        return false;
    }
    let ctx = crate::engine::context(state, n);
    rules.queue_rules.iter().any(|q| {
        eval::eval_list(
            &crate::gates::queue_conditions(state, q, &pr.base),
            pr,
            &ctx,
            &mut Vec::new(),
        )
        .0
    })
}

fn controls_body(state: &State, rules: &Rules, n: u64) -> Option<String> {
    let posted = state.cosmetics.controls.contains_key(&n);
    let by = state
        .pending_commands
        .get(&n)
        .map(|p| p.by.clone())
        .or_else(|| state.queued.get(&n).and_then(|e| e.by.clone()));
    if let Some(by) = by {
        // Only edit a comment that exists.
        return posted.then(|| {
            format!("{CONTROLS_MARKER}\n**Merge queue**\n\n@{by} asked to queue this pull request.")
        });
    }
    if state.queued.contains_key(&n)
        || state.outcomes.contains_key(&n)
        || !rules.merge_queue.queue_controls_comment
        || !ready(state, rules, n)
    {
        return None;
    }
    Some(format!(
        "{CONTROLS_MARKER}\n**Merge queue**\n\nThis pull request meets the conditions of a queue, \
         but no rule queues it on its own.\n\n{}",
        checkbox("Add this pull request to the merge queue", QUEUE_BOX)
    ))
}

/// Decides the cosmetic changes after a step.
pub(crate) fn update(state: &mut State, rules: &Rules, out: &mut Vec<Decision>) {
    let numbers: Vec<u64> = state.pull_requests.keys().copied().collect();
    for n in numbers {
        let pr = state.pull_requests[&n].clone();
        if is_queue_pull_request(rules, &pr) || queue::is_car_draft(state, n) {
            continue;
        }
        // Labels.
        let want = wanted_labels(state, rules, n);
        let have = state.cosmetics.labels.get(&n).cloned().unwrap_or_default();
        if want != have {
            let add: Vec<String> = want.difference(&have).cloned().collect();
            let remove: Vec<String> = have.difference(&want).cloned().collect();
            if want.is_empty() {
                state.cosmetics.labels.remove(&n);
            } else {
                state.cosmetics.labels.insert(n, want);
            }
            emit(
                out,
                Cosmetic::Labels {
                    number: n,
                    add,
                    remove,
                },
            );
        }
        // Status comment.
        let level = rules.merge_queue.status_comments;
        if level != StatusComments::None
            && let Some((body, outcome)) = status_body(state, rules, n)
            && (level == StatusComments::All || outcome)
            && state.cosmetics.status.get(&n) != Some(&body)
        {
            state.cosmetics.status.insert(n, body.clone());
            emit(out, Cosmetic::StatusComment { number: n, body });
        }
        // The checkbox comment.
        if let Some(body) = controls_body(state, rules, n)
            && state.cosmetics.controls.get(&n) != Some(&body)
        {
            state.cosmetics.controls.insert(n, body.clone());
            emit(out, Cosmetic::ControlsComment { number: n, body });
        }
        // The check of a changed configuration.
        if let Some(c) = &pr.config_change {
            let key = format!("{}:{}:{}", pr.head_sha, c.check_name(), c.ok());
            if state.cosmetics.checks.get(&n) != Some(&key) {
                state.cosmetics.checks.insert(n, key);
                emit(
                    out,
                    Cosmetic::ProtectionCheck {
                        number: n,
                        head_sha: pr.head_sha.clone(),
                        name: c.check_name().to_string(),
                        ok: c.ok(),
                        title: if c.deleted {
                            "The configuration is removed".into()
                        } else if c.ok() {
                            "The new configuration is valid".into()
                        } else {
                            "The new configuration does not validate".into()
                        },
                        summary: c.summary.clone(),
                    },
                );
            }
        }
    }
}
