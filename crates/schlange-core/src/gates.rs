//! Queue controls and P2 gates, without I/O:
//!
//! - **Pause** (docs: merge-queue/pause): an operator pauses the whole merge queue with a
//!   reason. Pull requests can still join, nothing new is checked and nothing merges; running
//!   queue checks are cancelled (their cars are closed, the pull requests stay queued with
//!   `PR_CHECKS_STOPPED_BECAUSE_MERGE_QUEUE_PAUSE`). A check in place cannot be cancelled: it
//!   keeps running, and a failure there still dequeues. A landing already sent finishes (it is
//!   one compare-and-swap per commit; the engine never interrupts it).
//! - **Freeze** (docs: merge-protections/freeze): during its window a freeze holds back the
//!   merge of every queued pull request matching its conditions (all of its exceptions let one
//!   through). Checks still run; the car waits green at the front.
//! - **Merge protections** (`merge_protections`): when a rule's `if` holds on the pull request,
//!   its `success_conditions` must hold too, or the pull request does not land (I1).
//! - **Priorities** (`priority_rules`): the first matching rule gives a queued pull request its
//!   priority (`medium`, 2000, without a match); higher ones go first. While a pull request
//!   waits, its priority follows its attributes (our reading: the docs say the rules are
//!   evaluated when it enters the queue).
//! - **Required checks** (`branch_protection_injection_mode`): the checks the base branch's
//!   protection requires join the queue conditions (`queue`, the default) or only the merge
//!   conditions (`merge`), or nothing (`none`).

use std::borrow::Cow;

use schlange_config::conditions::{Atom, Condition, Operator};
use schlange_config::rules::{InjectionMode, QueueRule, Rules};

use crate::engine::{self, Control, Decision, EntryStatus, Pause, State};
use crate::model::{PullRequest, RequiredCheck, Time};

/// Applies an operator's control change.
pub(crate) fn apply(state: &mut State, control: &Control, out: &mut Vec<Decision>) {
    match control {
        Control::Pause { reason, by } => {
            state.pause = Some(Pause {
                reason: reason.clone(),
                at: state.now,
                by: by.clone(),
            });
        }
        Control::Unpause => state.pause = None,
        Control::Freeze { freeze } => {
            for t in [Some(freeze.start), freeze.end].into_iter().flatten() {
                if t > state.now {
                    crate::queue::wake(state, t, out);
                }
            }
            state.freezes.insert(freeze.id, freeze.clone());
        }
        Control::Unfreeze { id } => {
            state.freezes.remove(id);
        }
    }
}

/// The first matching priority rule for the pull request: priority, rule name, interruption.
pub fn priority_of(state: &State, rules: &Rules, number: u64) -> (u32, Option<String>, bool) {
    let default = (schlange_config::queue_extras::DEFAULT_PRIORITY, None, false);
    let Some(pr) = state.pull_requests.get(&number) else {
        return default;
    };
    let ctx = engine::context(state, number);
    for r in &rules.priority_rules {
        if crate::eval::eval_list(&r.conditions, pr, &ctx, &mut Vec::new()).0 {
            return (
                r.priority,
                Some(r.name.clone()),
                r.allow_checks_interruption,
            );
        }
    }
    default
}

pub(crate) fn assign_priority(state: &mut State, rules: &Rules, number: u64) {
    let (p, name, interrupt) = priority_of(state, rules, number);
    if let Some(e) = state.queued.get_mut(&number) {
        e.priority = p;
        e.priority_rule = name;
        e.interrupt = interrupt;
    }
}

/// Re-evaluates the priority of every waiting pull request.
pub(crate) fn refresh_priorities(state: &mut State, rules: &Rules) {
    if rules.priority_rules.is_empty() {
        return;
    }
    let waiting: Vec<u64> = state
        .queued
        .iter()
        .filter(|(_, e)| matches!(e.status, EntryStatus::Waiting | EntryStatus::Held { .. }))
        .map(|(n, _)| *n)
        .collect();
    for n in waiting {
        assign_priority(state, rules, n);
    }
}

/// Required checks are typed atoms: check names are literal values, and app IDs cannot be
/// confused with app slugs or supplied through a condition string.
pub fn check_conditions(checks: &[RequiredCheck]) -> Vec<Condition> {
    checks
        .iter()
        .map(|c| {
            Condition::Atom(Atom {
                negate: false,
                length: false,
                attribute: "check-success".into(),
                operator: Some(Operator::Eq),
                value: Some(c.context().into()),
                integration_id: c.integration_id(),
            })
        })
        .collect()
}

/// The queue conditions of a rule on `base`, with the required checks of the base branch's
/// protection when the injection mode is `queue`.
pub fn queue_conditions<'a>(
    state: &State,
    rule: &'a QueueRule,
    base: &str,
) -> Cow<'a, [Condition]> {
    match state.required_checks.get(base) {
        Some(checks) if rule.injection_mode == InjectionMode::Queue => {
            let mut v = rule.queue_conditions.clone();
            v.extend(check_conditions(checks));
            Cow::Owned(v)
        }
        _ => Cow::Borrowed(&rule.queue_conditions),
    }
}

/// The merge conditions of a rule (without `merge_conditions`: the queue conditions), with the
/// required checks when the injection mode is `queue` or `merge`.
pub fn merge_conditions<'a>(
    state: &State,
    rule: &'a QueueRule,
    base: &str,
) -> Cow<'a, [Condition]> {
    let own: &'a [Condition] = if rule.merge_conditions.is_empty() {
        &rule.queue_conditions
    } else {
        &rule.merge_conditions
    };
    match state.required_checks.get(base) {
        Some(checks) if rule.injection_mode != InjectionMode::None => {
            let mut v = own.to_vec();
            v.extend(check_conditions(checks));
            Cow::Owned(v)
        }
        _ => Cow::Borrowed(own),
    }
}

/// Why the pull request may not land now, if anything holds it: the pause, a freeze, a merge
/// protection. The second value is when to look again (a freeze's end, or a minute for
/// protections that read the clock).
pub fn landing_hold(
    state: &State,
    rules: &Rules,
    pr: &PullRequest,
    now: Time,
) -> Option<(String, Option<Time>)> {
    if let Some(p) = &state.pause {
        return Some((format!("the merge queue is paused: {}", p.reason), None));
    }
    let ctx = engine::context(state, pr.number);
    for f in state.freezes.values() {
        if f.holds(pr, &ctx, now) {
            let until = f
                .end
                .map(|e| {
                    format!(
                        "until {}",
                        crate::time::format_utc(state.host.epoch as i64 + e as i64)
                    )
                })
                .unwrap_or_else(|| "until it is removed".into());
            return Some((format!("frozen by `{}` ({until})", f.reason), f.end));
        }
    }
    for m in &rules.merge_protections {
        if crate::eval::eval_list(&m.when, pr, &ctx, &mut Vec::new()).0
            && !crate::eval::eval_list(&m.success_conditions, pr, &ctx, &mut Vec::new()).0
        {
            let clock = m.success_conditions.iter().chain(&m.when).any(|c| {
                c.atoms()
                    .iter()
                    .any(|a| a.attribute == "schedule" || a.attribute.starts_with("current-"))
            });
            return Some((
                format!("merge protection `{}` is not satisfied", m.name),
                clock.then_some(now + 60),
            ));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::Freeze;

    #[test]
    fn freeze_matching_and_exceptions() {
        let mut pr = PullRequest::new(1, "a", "main", "b", "s");
        let f = Freeze {
            id: 1,
            reason: "release".into(),
            start: 10,
            end: Some(20),
            matching: vec!["base = main".into()],
            exclude: vec!["label = hotfix".into(), "label = urgent".into()],
        };
        let ctx = crate::eval::Context::default();
        assert!(!f.holds(&pr, &ctx, 5), "not started");
        assert!(f.holds(&pr, &ctx, 10));
        assert!(!f.holds(&pr, &ctx, 20), "ended");
        pr.labels.insert("hotfix".into());
        assert!(f.holds(&pr, &ctx, 15), "exceptions are combined with AND");
        pr.labels.insert("urgent".into());
        assert!(!f.holds(&pr, &ctx, 15));
        pr.base = "dev".into();
        pr.labels.clear();
        assert!(!f.holds(&pr, &ctx, 15), "does not match");
    }
}

#[cfg(test)]
mod required_check_tests {
    use super::*;
    use crate::model::{CheckRun, Conclusion, RequiredCheck};

    #[test]
    fn required_check_pin_requires_the_numeric_integration_on_the_current_head() {
        let check: RequiredCheck = serde_json::from_value(serde_json::json!({
            "context": "lint", "integration_id": 42
        }))
        .unwrap();
        let conditions = check_conditions(&[check]);
        let Condition::Atom(atom) = &conditions[0] else {
            panic!("injected atom")
        };
        assert_eq!(
            crate::explain::atom_text(atom),
            "check-success = lint (integration 42)"
        );
        let mut pr = PullRequest::new(1, "bot", "main", "topic", "head");
        let holds = |p: &PullRequest| {
            crate::eval::eval_list(
                &conditions,
                p,
                &crate::eval::Context::default(),
                &mut vec![],
            )
            .0
        };
        pr.checks.push(CheckRun {
            name: "lint".into(),
            app: Some("wrong-app".into()),
            app_id: Some(7),
            head_sha: "head".into(),
            conclusion: Some(Conclusion::Success),
            seq: 1,
            ..Default::default()
        });
        assert!(
            !holds(&pr),
            "another integration cannot satisfy the ruleset"
        );
        pr.checks[0].app_id = None;
        pr.checks[0].status = true;
        assert!(
            !holds(&pr),
            "an unverified commit status cannot satisfy an app pin"
        );
        pr.checks[0].status = false;
        pr.checks[0].app_id = Some(42);
        assert!(holds(&pr));
        pr.checks[0].head_sha = "old-head".into();
        assert!(!holds(&pr));
    }
}
