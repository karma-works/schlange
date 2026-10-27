//! The state machine: `(state, event) → (state, decisions)`, without I/O.
//!
//! Phase 0 covers only the entry of the queue: pull request rules with the `queue` action and
//! the `queue_conditions` of the chosen queue. The queue itself (speculation, batches, merge) is
//! a placeholder: a queued pull request stays queued until its queue conditions stop holding.

use std::collections::BTreeMap;

use serde::Serialize;

use schlange_config::rules::Rules;

use crate::eval::{self, Context};
use crate::model::{CheckRun, PullRequest, Time};

/// What the forge reports.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Event {
    PullRequestOpened {
        pull_request: PullRequest,
    },
    PullRequestSynchronized {
        number: u64,
        head_sha: String,
        files: Vec<String>,
    },
    ReadyForReview {
        number: u64,
    },
    ConvertedToDraft {
        number: u64,
    },
    Labeled {
        number: u64,
        label: String,
    },
    Unlabeled {
        number: u64,
        label: String,
    },
    CheckRun {
        number: u64,
        run: CheckRun,
    },
    Closed {
        number: u64,
    },
}

impl Event {
    pub fn number(&self) -> u64 {
        match self {
            Event::PullRequestOpened { pull_request } => pull_request.number,
            Event::PullRequestSynchronized { number, .. }
            | Event::ReadyForReview { number }
            | Event::ConvertedToDraft { number }
            | Event::Labeled { number, .. }
            | Event::Unlabeled { number, .. }
            | Event::CheckRun { number, .. }
            | Event::Closed { number } => *number,
        }
    }
}

/// Why a pull request left the queue.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct DequeueReason {
    /// Mergify's code when it is known for this path; `None` while it is not determined yet.
    pub code: Option<&'static str>,
    pub text: String,
}

/// What the engine decided. The forge adapter turns decisions into side effects.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Decision {
    Enqueue {
        number: u64,
        queue: String,
        rule: String,
    },
    Dequeue {
        number: u64,
        queue: String,
        reason: DequeueReason,
    },
    /// A condition could not be evaluated; it counts as false.
    EvaluationError { number: u64, message: String },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct QueueEntry {
    pub queue: String,
    pub rule: String,
    pub since: Time,
    pub head_sha: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct State {
    pub pull_requests: BTreeMap<u64, PullRequest>,
    pub queued: BTreeMap<u64, QueueEntry>,
}

/// Applies one event and re-evaluates the affected pull request.
pub fn step(state: &State, rules: &Rules, event: &Event, now: Time) -> (State, Vec<Decision>) {
    let mut next = state.clone();
    apply(&mut next, event);
    let mut decisions = Vec::new();
    let number = event.number();
    if let Some(pr) = next.pull_requests.get(&number).cloned() {
        evaluate(&mut next, rules, &pr, now, &mut decisions);
    }
    (next, decisions)
}

fn apply(state: &mut State, event: &Event) {
    match event {
        Event::PullRequestOpened { pull_request } => {
            state
                .pull_requests
                .insert(pull_request.number, pull_request.clone());
        }
        Event::PullRequestSynchronized {
            number,
            head_sha,
            files,
        } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                pr.head_sha = head_sha.clone();
                pr.files = files.clone();
            }
        }
        Event::ReadyForReview { number } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                pr.draft = false;
            }
        }
        Event::ConvertedToDraft { number } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                pr.draft = true;
            }
        }
        Event::Labeled { number, label } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                pr.labels.insert(label.clone());
            }
        }
        Event::Unlabeled { number, label } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                pr.labels.remove(label);
            }
        }
        Event::CheckRun { number, run } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                match pr.checks.iter_mut().find(|c| c.seq == run.seq) {
                    Some(existing) => *existing = run.clone(),
                    None => pr.checks.push(run.clone()),
                }
            }
        }
        Event::Closed { number } => {
            if let Some(pr) = state.pull_requests.get_mut(number) {
                pr.closed = true;
            }
        }
    }
}

fn evaluate(
    state: &mut State,
    rules: &Rules,
    pr: &PullRequest,
    now: Time,
    out: &mut Vec<Decision>,
) {
    let ctx = Context::default();
    let mut errors = Vec::new();
    let number = pr.number;

    if let Some(entry) = state.queued.get(&number).cloned() {
        let reason = if pr.closed {
            Some(DequeueReason {
                code: None,
                text: "pull request closed (Mergify code not determined yet)".into(),
            })
        } else {
            match rules.queue_rules.iter().find(|q| q.name == entry.queue) {
                None => Some(DequeueReason {
                    code: Some("QUEUE_RULE_MISSING"),
                    text: format!("queue `{}` no longer exists", entry.queue),
                }),
                Some(q) => {
                    let (ok, _) = eval::eval_list(&q.queue_conditions, pr, &ctx, &mut errors);
                    (!ok).then(|| DequeueReason {
                        code: None,
                        text: "queue_conditions no longer hold (Mergify code not determined yet)"
                            .into(),
                    })
                }
            }
        };
        if let Some(reason) = reason {
            state.queued.remove(&number);
            out.push(Decision::Dequeue {
                number,
                queue: entry.queue,
                reason,
            });
        }
    } else if !pr.closed {
        'rules: for rule in &rules.pull_request_rules {
            let Some(target) = &rule.queue else { continue };
            let (matches, _) = eval::eval_list(&rule.conditions, pr, &ctx, &mut errors);
            if !matches {
                continue;
            }
            for q in &rules.queue_rules {
                if target.as_ref().is_some_and(|t| *t != q.name) {
                    continue;
                }
                let (ok, _) = eval::eval_list(&q.queue_conditions, pr, &ctx, &mut errors);
                if ok {
                    state.queued.insert(
                        number,
                        QueueEntry {
                            queue: q.name.clone(),
                            rule: rule.name.clone(),
                            since: now,
                            head_sha: pr.head_sha.clone(),
                        },
                    );
                    out.push(Decision::Enqueue {
                        number,
                        queue: q.name.clone(),
                        rule: rule.name.clone(),
                    });
                    break 'rules;
                }
            }
        }
    }
    let mut seen = Vec::new();
    for e in errors {
        let message = e.to_string();
        if !seen.contains(&message) {
            seen.push(message.clone());
            out.push(Decision::EvaluationError { number, message });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Conclusion;

    const CONFIG: &str = "queue_rules:\n  - name: default\n    queue_conditions:\n      - base = main\n      - -draft\n      - -label = halt\n      - check-success = ci\n\
                          pull_request_rules:\n  - name: auto\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";

    fn pr(n: u64, author: &str) -> PullRequest {
        PullRequest {
            number: n,
            author: author.into(),
            base: "main".into(),
            head_ref: format!("b{n}"),
            head_sha: format!("s{n}"),
            title: String::new(),
            draft: false,
            closed: false,
            labels: Default::default(),
            files: vec![],
            dependabot_update_types: vec![],
            checks: vec![],
        }
    }

    fn green(n: u64, seq: u64) -> Event {
        Event::CheckRun {
            number: n,
            run: CheckRun {
                name: "ci".into(),
                app: None,
                head_sha: format!("s{n}"),
                conclusion: Some(Conclusion::Success),
                seq,
            },
        }
    }

    #[test]
    fn enqueue_then_dequeue_on_halt() {
        let rules = Rules::from_yaml(CONFIG).unwrap();
        let s = State::default();
        let (s, d) = step(
            &s,
            &rules,
            &Event::PullRequestOpened {
                pull_request: pr(1, "bot"),
            },
            0,
        );
        assert!(d.is_empty(), "no check yet");
        let (s, d) = step(&s, &rules, &green(1, 1), 10);
        assert_eq!(
            d,
            vec![Decision::Enqueue {
                number: 1,
                queue: "default".into(),
                rule: "auto".into()
            }]
        );
        // Same event again: idempotent, no second entry (MQ-12).
        let (s, d) = step(&s, &rules, &green(1, 1), 11);
        assert!(d.is_empty());
        let (s, d) = step(
            &s,
            &rules,
            &Event::Labeled {
                number: 1,
                label: "halt".into(),
            },
            20,
        );
        assert!(matches!(&d[..], [Decision::Dequeue { number: 1, .. }]));
        assert!(s.queued.is_empty());
    }

    #[test]
    fn other_author_is_not_queued() {
        let rules = Rules::from_yaml(CONFIG).unwrap();
        let (s, _) = step(
            &State::default(),
            &rules,
            &Event::PullRequestOpened {
                pull_request: pr(2, "human"),
            },
            0,
        );
        let (s, d) = step(&s, &rules, &green(2, 1), 5);
        assert!(d.is_empty());
        assert!(s.queued.is_empty());
    }
}
