//! Clean-room runtime contract, written from the options read by Schlange's engine.
//!
//! This is deliberately a subset, not a bundled copy of the upstream schema. Unknown
//! options fail closed. Development `config validate` continues using the pinned schema.
use std::collections::BTreeMap;
use std::path::Path;

use serde_json::{Value, json};

use crate::schema::{Attribute, Schema, SchemaError};

pub const NAME: &str = "embedded-schlange-runtime";

fn object(properties: Value, required: &[&str]) -> Value {
    json!({"type":"object", "properties":properties, "required":required,
           "additionalProperties":false})
}
fn list(items: Value) -> Value {
    json!({"type":"array", "items":items})
}
fn nullable(value: Value) -> Value {
    json!({"anyOf":[{"type":"null"},value]})
}
fn choice(values: &[&str]) -> Value {
    json!({"enum":values})
}
fn text() -> Value {
    json!({"type":"string"})
}
fn flag() -> Value {
    json!({"type":"boolean"})
}
fn uint(minimum: u64) -> Value {
    json!({"type":"integer","minimum":minimum,"maximum":u32::MAX})
}
fn conditions() -> Value {
    json!({"$ref":"#/$defs/conditions"})
}
fn names() -> Value {
    json!({"anyOf":[text(),list(text())]})
}

/// The embedded validator accepts exactly the structural options this engine reads.
pub fn schema() -> Result<Schema, SchemaError> {
    let format = object(
        json!({
            "title":choice(&["inherit","pr-title"]),
            "body":choice(&["inherit","pr-body","empty"]),
            "trailers":list(choice(&["co-authored-by","approved-by","merged-by"]))
        }),
        &[],
    );
    let queue = object(
        json!({
            "name":text(), "queue_conditions":conditions(), "merge_conditions":conditions(),
            "merge_method":choice(&["squash","merge","rebase","fast-forward","merge-batch"]),
            "queue_branch_merge_method":choice(&["merge","fast-forward"]),
            "update_method":choice(&["merge","rebase"]),
            "branch_protection_injection_mode":choice(&["queue","merge","none"]),
            "batch_size":{"anyOf":[uint(1),object(json!({"min":uint(1),"max":uint(1)}),&["min","max"])]},
            "batch_max_wait_time":{"anyOf":[uint(0),text()]},
            "checks_timeout":{"anyOf":[{"type":"null"},uint(0),text()]},
            "batch_max_failure_resolution_attempts":nullable(uint(0)),
            "queue_branch_prefix":text(), "allow_queue_branch_edit":flag(),
            "max_checks_retries":uint(0), "commit_message_format":nullable(format),
            "commit_message_template":nullable(text())
        }),
        &["name"],
    );
    let dismiss =
        json!({"anyOf":[{"type":"null"},flag(),{"const":"from_requested_reviewers"},list(text())]});
    let weighted = json!({"anyOf":[names(),{"type":"object","additionalProperties":uint(1)}]});
    let copy = object(
        json!({
            "branches":names(),"regexes":names(),"ignore_conflicts":flag(),
            "assignees":names(),"labels":names(),"label_conflicts":text(),
            "title":text(),"body":text(),"report_mode":list(choice(&["check","comment"]))
        }),
        &[],
    );
    let empty = object(json!({}), &[]);
    let dispatch = object(
        json!({
            "workflow":{"anyOf":[text(),uint(1)]},"ref":text(),
            "inputs":{"type":"object"}
        }),
        &["workflow"],
    );
    let actions = object(
        json!({
            "queue":nullable(object(json!({"name":text()}),&[])),
            "label":nullable(object(json!({"add":names(),"remove":names(),"toggle":names(),"remove_all":flag()}),&[])),
            "comment":nullable(object(json!({"message":text()}),&[])),
            "assign":nullable(object(json!({"users":names(),"add_users":names(),"remove_users":names()}),&[])),
            "request_reviews":nullable(object(json!({"users":weighted,"teams":weighted,"users_from_teams":weighted,"random_count":uint(1)}),&[])),
            "review":nullable(object(json!({"type":choice(&["APPROVE","REQUEST_CHANGES","COMMENT"]),"message":text()}),&[])),
            "dismiss_reviews":nullable(object(json!({"approved":dismiss,"changes_requested":dismiss,"message":text(),"when":choice(&["always","synchronize"])}),&[])),
            "close":nullable(object(json!({"message":text()}),&[])),
            "edit":nullable(object(json!({"draft":flag()}),&[])),
            "delete_head_branch":nullable(object(json!({"force":flag()}),&[])),
            "post_check":nullable(object(json!({"title":text(),"summary":text(),"success_conditions":conditions(),"neutral_conditions":conditions()}),&[])),
            "update":nullable(empty.clone()),"rebase":nullable(empty),
            "squash":nullable(object(json!({"commit_message":choice(&["all-commits","title+body"])}),&[])),
            "backport":nullable(copy.clone()),"copy":nullable(copy),
            "merge":nullable(object(json!({"method":choice(&["squash","merge","rebase","fast-forward","merge-batch"])}),&[])),
            "github_actions":nullable(object(json!({"workflow":object(json!({"dispatch":list(dispatch)}),&[])}),&[]))
        }),
        &[],
    );
    let restrictions: BTreeMap<_, _> = crate::rules::COMMANDS
        .iter()
        .map(|name| (*name, object(json!({"conditions":conditions()}), &[])))
        .collect();
    let filter = object(json!({"include":list(text()),"exclude":list(text())}), &[]);
    let mut root = object(
        json!({
            "queue_rules":list(queue),
            "pull_request_rules":list(object(json!({"name":text(),"conditions":conditions(),"actions":actions,
                "disabled":nullable(object(json!({"reason":text()}),&["reason"]))}),&["name","conditions","actions"])),
            "commands_restrictions":object(json!(restrictions),&[]),
            "merge_queue":object(json!({"max_parallel_checks":uint(1),"mode":choice(&["serial","parallel","isolated"]),
                "reset_on_external_merge":choice(&["always","never"]),"skip_intermediate_results":flag(),
                "queued_label":nullable(text()),"checking_label":nullable(text()),"dequeued_label":nullable(text()),
                "status_comments":choice(&["all","outcomes","none"]),"queue_controls_comment":flag()}),&[]),
            "priority_rules":list(object(json!({"name":text(),"conditions":conditions(),
                "priority":{"anyOf":[uint(0),choice(&["low","medium","high"])]},"allow_checks_interruption":flag()}),&["name","conditions"])),
            "merge_protections":list(object(json!({"name":text(),"description":text(),"if":conditions(),"success_conditions":conditions()}),&["name","if","success_conditions"])),
            "scopes":object(json!({"source":object(json!({"files":{"type":"object","additionalProperties":filter}}),&["files"]),
                "barrier_files":filter,"merge_queue_scope":text()}),&["source"]),
            "defaults":object(json!({"actions":actions}),&[]),
            "extends":text(),"shared":true
        }),
        &[],
    );
    root["$defs"] = json!({
        "conditions":{"anyOf":[{"type":"boolean"},list(json!({"$ref":"#/$defs/condition"}))]},
        "condition":{"anyOf":[text(),{"const":true},
            object(json!({"and":conditions()}),&["and"]),
            object(json!({"or":conditions()}),&["or"]),
            object(json!({"not":{"$ref":"#/$defs/condition"}}),&["not"])]}
    });
    let mut schema = Schema::from_value(Path::new(NAME), root)?;
    schema.condition_keys = [
        "conditions",
        "queue_conditions",
        "merge_conditions",
        "if",
        "success_conditions",
        "neutral_conditions",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    // Attribute contracts mirror `schlange_core::eval::attribute`. Unsupported attributes
    // are rejected even when they exist upstream, so a runtime gate cannot silently pass.
    let scalar = [
        "number",
        "author",
        "base",
        "head",
        "title",
        "sender",
        "sender-permission",
        "queue-position",
        "queue-name",
        "queued-at",
        "queue-merge-started-at",
        "queue-dequeue-reason",
    ];
    let lists = [
        "label",
        "files",
        "dependabot-update-type",
        "depends-on",
        "check",
        "check-success",
        "check-failure",
        "check-neutral",
        "check-skipped",
        "check-cancelled",
        "check-timed-out",
        "check-stale",
        "check-pending",
    ];
    let bools = [
        "draft",
        "closed",
        "merged",
        "from-fork",
        "mergify-configuration-changed",
    ];
    for name in scalar.into_iter().chain(lists) {
        schema.attributes.insert(
            name.into(),
            Attribute {
                operators: Some(
                    ["=", "!=", "~=", "*=", ">=", ">", "<=", "<"]
                        .into_iter()
                        .map(str::to_owned)
                        .collect(),
                ),
                negate: true,
                length: lists.contains(&name),
                length_required: false,
            },
        );
    }
    for name in bools {
        schema.attributes.insert(
            name.into(),
            Attribute {
                operators: Some(vec![]),
                negate: true,
                ..Default::default()
            },
        );
    }
    Ok(schema)
}
