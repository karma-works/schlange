//! Stage 4: semantics the schema cannot express. Only rules the documentation states are
//! checked: unique rule names, references to existing queues, readable durations.

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
