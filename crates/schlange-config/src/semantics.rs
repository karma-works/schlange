//! Stage 4: semantics the schema cannot express. Only rules the documentation states are
//! checked: unique rule names, references to existing queues, readable durations, and the
//! `merge_method` combinations Mergify rejects.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::rules::parse_duration;
use crate::validate::{Finding, Severity, Stage};
use crate::yaml::Spans;

fn push(findings: &mut Vec<Finding>, spans: &Spans, pointer: String, message: String) {
    let pos = spans.value(&pointer);
    findings.push(Finding {
        stage: Stage::Semantics,
        severity: Severity::Error,
        line: pos.map(|p| p.line),
        column: pos.map(|p| p.column),
        pointer,
        message,
        feature: None,
    });
}

fn items<'a>(config: &'a Value, key: &str) -> &'a [Value] {
    config
        .get(key)
        .and_then(Value::as_array)
        .map_or(&[], Vec::as_slice)
}

/// Rule lists whose `name` must be unique.
const NAMED_LISTS: &[&str] = &[
    "queue_rules",
    "pull_request_rules",
    "priority_rules",
    "merge_protections",
];

/// Duration options of a queue rule; `checks_timeout` also accepts `auto` and null.
const DURATIONS: &[&str] = &["batch_max_wait_time", "checks_timeout"];

pub fn check(config: &Value, spans: &Spans, findings: &mut Vec<Finding>) {
    for list in NAMED_LISTS {
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for (i, rule) in items(config, list).iter().enumerate() {
            let Some(name) = rule.get("name").and_then(Value::as_str) else {
                continue;
            };
            if let Some(first) = seen.insert(name, i) {
                push(
                    findings,
                    spans,
                    format!("/{list}/{i}/name"),
                    format!("duplicate name `{name}` in {list} (first at /{list}/{first}/name)"),
                );
            }
        }
    }

    let mut queues: Vec<&str> = items(config, "queue_rules")
        .iter()
        .filter_map(|q| q.get("name").and_then(Value::as_str))
        .collect();
    queues.dedup();
    let mut queue_refs = Vec::new();
    for (i, rule) in items(config, "pull_request_rules").iter().enumerate() {
        if let Some(name) = rule.pointer("/actions/queue/name").and_then(Value::as_str) {
            queue_refs.push((format!("/pull_request_rules/{i}/actions/queue/name"), name));
        }
    }
    if let Some(name) = config
        .pointer("/defaults/actions/queue/name")
        .and_then(Value::as_str)
    {
        queue_refs.push(("/defaults/actions/queue/name".into(), name));
    }
    for (pointer, name) in queue_refs {
        if !queues.contains(&name) {
            let known = if queues.is_empty() {
                "no queue_rules are defined".to_string()
            } else {
                format!("known: {}", queues.join(", "))
            };
            push(
                findings,
                spans,
                pointer,
                format!("queue `{name}` does not exist ({known})"),
            );
        }
    }

    let mut rules: Vec<(String, &Value)> = items(config, "queue_rules")
        .iter()
        .enumerate()
        .map(|(i, q)| (format!("/queue_rules/{i}"), q))
        .collect();
    if let Some(d) = config.pointer("/defaults/queue_rule") {
        rules.push(("/defaults/queue_rule".into(), d));
    }
    merge_method_rules(config, spans, findings);

    for (base, rule) in rules {
        for key in DURATIONS {
            let Some(Value::String(text)) = rule.get(*key) else {
                continue;
            };
            if *key == "checks_timeout" && text == "auto" {
                continue;
            }
            match parse_duration(text) {
                None => push(
                    findings,
                    spans,
                    format!("{base}/{key}"),
                    format!(
                        "`{text}` is not a duration (for example `30 seconds`, `5 minutes`, `1 hour`)"
                    ),
                ),
                Some(s) if *key == "checks_timeout" && s < 60 => push(
                    findings,
                    spans,
                    format!("{base}/{key}"),
                    format!("checks_timeout must be at least 60 seconds, found {s} seconds"),
                ),
                Some(_) => {}
            }
        }
    }
}

/// Merge methods that land the queue branch itself; only they can carry a commit pushed to it.
const BRANCH_MERGING_METHODS: &[&str] = &["merge-batch", "fast-forward"];

/// A queue rule option with its pointer, falling back to `defaults.queue_rule`.
fn option<'a>(
    config: &'a Value,
    rule: &'a Value,
    base: &str,
    key: &str,
) -> Option<(&'a Value, String)> {
    match rule.get(key) {
        Some(v) => Some((v, format!("{base}/{key}"))),
        None => config
            .pointer(&format!("/defaults/queue_rule/{key}"))
            .map(|v| (v, format!("/defaults/queue_rule/{key}"))),
    }
}

/// Combinations of `merge_method` with other options that the documentation says are
/// rejected (merge strategies and batches pages):
/// - `allow_queue_branch_edit: true` needs `merge-batch` or `fast-forward`, set explicitly;
/// - `fast-forward` works in serial mode only;
/// - `fast-forward` with `max_checks_retries` > 0 needs `max_parallel_checks` 1.
fn merge_method_rules(config: &Value, spans: &Spans, findings: &mut Vec<Finding>) {
    let mode = config
        .pointer("/merge_queue/mode")
        .and_then(Value::as_str)
        .unwrap_or("serial");
    let parallel_checks = config
        .pointer("/merge_queue/max_parallel_checks")
        .and_then(Value::as_u64)
        .unwrap_or(5);
    for (i, rule) in items(config, "queue_rules").iter().enumerate() {
        let base = format!("/queue_rules/{i}");
        let method = option(config, rule, &base, "merge_method")
            .and_then(|(v, p)| v.as_str().map(|m| (m, p)));
        let method_name = method.as_ref().map(|(m, _)| *m);

        if let Some((Value::Bool(true), pointer)) =
            option(config, rule, &base, "allow_queue_branch_edit")
            && !method_name.is_some_and(|m| BRANCH_MERGING_METHODS.contains(&m))
        {
            let found = match method_name {
                Some(m) => format!("found `{m}`"),
                None => "merge_method is not set, and the repository default is never one of them"
                    .to_string(),
            };
            push(
                findings,
                spans,
                pointer,
                format!(
                    "allow_queue_branch_edit: true needs merge_method `merge-batch` or \
                     `fast-forward` ({found}); other methods merge each pull request at its own \
                     head, so an edit of the queue branch could never land"
                ),
            );
        }

        let Some(("fast-forward", pointer)) = method else {
            continue;
        };
        if mode != "serial" {
            push(
                findings,
                spans,
                pointer.clone(),
                format!(
                    "merge_method `fast-forward` works in serial mode only, but merge_queue.mode \
                     is `{mode}`"
                ),
            );
        }
        let retries = option(config, rule, &base, "max_checks_retries")
            .and_then(|(v, _)| v.as_u64())
            .unwrap_or(0);
        if retries > 0 && parallel_checks > 1 {
            push(
                findings,
                spans,
                pointer,
                format!(
                    "merge_method `fast-forward` with max_checks_retries {retries} needs \
                     merge_queue.max_parallel_checks 1 (found {parallel_checks}): a retried batch \
                     gets a new head, so the batches checked on top of it cannot be fast-forwarded"
                ),
            );
        }
    }
}
