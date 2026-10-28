//! `extends` (docs: configuration/sharing, "Extending configuration files"): a configuration
//! names another repository of the same owner; that repository's configuration is loaded
//! first and the local one is merged over it.
//!
//! How Schlange merges (our reading of the docs):
//!
//! - `shared` and `scopes` are never taken from the extended configuration.
//! - `defaults`, `commands_restrictions`, `merge_queue` and `merge_protections_settings` are
//!   merged key by key, recursively; a value the local configuration sets wins.
//! - `pull_request_rules`, `queue_rules`, `priority_rules` and `merge_protections` are merged
//!   by rule name: the extended rules come first, in their order, then the local ones; a local
//!   rule with the name of an extended one replaces it (at the local rule's position).
//! - Any other key: the local value if set, else the extended one.
//! - One level only: an extended configuration that itself uses `extends` is an error, and so
//!   is a configuration that extends its own repository.
//!
//! Fetching the other repository's file is the host's job ([`resolve`] takes a loader); the
//! visibility rule of the docs (the source must be at least as visible) is not checked.

use serde_json::{Map, Value};

const NEVER_MERGED: &[&str] = &["shared", "scopes", "extends"];
const MERGED_BY_KEY: &[&str] = &[
    "defaults",
    "commands_restrictions",
    "merge_queue",
    "merge_protections_settings",
];
const MERGED_BY_NAME: &[&str] = &[
    "pull_request_rules",
    "queue_rules",
    "priority_rules",
    "merge_protections",
];

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExtendsError {
    #[error("`extends` must name a repository (a string)")]
    NotAName,
    #[error("`extends: {0}`: a configuration cannot extend its own repository")]
    Itself(String),
    #[error(
        "`extends: {0}`: the extended configuration uses `extends` itself; only one level is supported"
    )]
    Nested(String),
    #[error("`extends: {name}`: {message}")]
    Load { name: String, message: String },
}

/// The repository named by `extends`, if any.
pub fn target(config: &Value) -> Result<Option<String>, ExtendsError> {
    match config.get("extends") {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.trim().to_string())),
        Some(_) => Err(ExtendsError::NotAName),
    }
}

/// Merges `local` over `remote` (both whole configurations).
pub fn merge(local: &Value, remote: &Value) -> Value {
    let empty = Map::new();
    let l = local.as_object().unwrap_or(&empty);
    let r = remote.as_object().unwrap_or(&empty);
    let mut out = Map::new();
    let mut keys: Vec<&String> = r.keys().collect();
    for k in l.keys() {
        if !keys.contains(&k) {
            keys.push(k);
        }
    }
    for key in keys {
        let lv = l.get(key).filter(|v| !v.is_null());
        let rv = r.get(key).filter(|v| !v.is_null());
        let merged = if NEVER_MERGED.contains(&key.as_str()) {
            lv.cloned()
        } else if MERGED_BY_KEY.contains(&key.as_str()) {
            match (lv, rv) {
                (Some(a), Some(b)) => Some(deep(a, b)),
                (a, b) => a.or(b).cloned(),
            }
        } else if MERGED_BY_NAME.contains(&key.as_str()) {
            match (lv, rv) {
                (Some(a), Some(b)) => Some(by_name(a, b)),
                (a, b) => a.or(b).cloned(),
            }
        } else {
            lv.or(rv).cloned()
        };
        if let Some(v) = merged {
            out.insert(key.clone(), v);
        }
    }
    Value::Object(out)
}

/// `local` over `remote`, recursively for mappings.
fn deep(local: &Value, remote: &Value) -> Value {
    match (local, remote) {
        (Value::Object(l), Value::Object(r)) => {
            let mut out = r.clone();
            for (k, v) in l {
                let m = match r.get(k) {
                    Some(rv) => deep(v, rv),
                    None => v.clone(),
                };
                out.insert(k.clone(), m);
            }
            Value::Object(out)
        }
        _ => local.clone(),
    }
}

fn rule_name(v: &Value) -> Option<&str> {
    v.get("name").and_then(Value::as_str)
}

/// Extended rules first, then local ones; a local rule replaces an extended one of its name.
fn by_name(local: &Value, remote: &Value) -> Value {
    let (Value::Array(l), Value::Array(r)) = (local, remote) else {
        return local.clone();
    };
    let local_names: Vec<&str> = l.iter().filter_map(rule_name).collect();
    let mut out: Vec<Value> = r
        .iter()
        .filter(|x| rule_name(x).is_none_or(|n| !local_names.contains(&n)))
        .cloned()
        .collect();
    out.extend(l.iter().cloned());
    Value::Array(out)
}

/// Resolves `extends` of `config` (from repository `own`): loads the named repository's
/// configuration with `load` (repository name → parsed configuration) and merges. A
/// configuration without `extends` comes back unchanged.
pub fn resolve(
    config: &Value,
    own: Option<&str>,
    load: impl FnOnce(&str) -> Result<Value, String>,
) -> Result<Value, ExtendsError> {
    let Some(name) = target(config)? else {
        return Ok(config.clone());
    };
    if own.is_some_and(|o| o == name || o.rsplit('/').next() == Some(name.as_str())) {
        return Err(ExtendsError::Itself(name));
    }
    let remote = load(&name).map_err(|message| ExtendsError::Load {
        name: name.clone(),
        message,
    })?;
    if target(&remote).ok().flatten().is_some() {
        return Err(ExtendsError::Nested(name));
    }
    Ok(merge(config, &remote))
}

/// Parses YAML `source` and resolves its `extends` with `load` (repository name → YAML text).
pub fn resolved_value(
    source: &str,
    own: Option<&str>,
    load: impl FnOnce(&str) -> Result<String, String>,
) -> Result<Value, crate::rules::RulesError> {
    let parse = |text: &str| {
        crate::yaml::parse(text).map(|(v, _)| v).map_err(|errs| {
            errs.into_iter()
                .map(|e| e.message)
                .collect::<Vec<_>>()
                .join("; ")
        })
    };
    let value = parse(source).map_err(crate::rules::RulesError::Yaml)?;
    Ok(resolve(&value, own, |name| parse(&load(name)?))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rules_merge_by_name_and_sections_by_key() {
        let remote = json!({
            "shared": { "x": 1 },
            "defaults": { "actions": { "comment": { "message": "remote", "bot_account": "r" } } },
            "merge_queue": { "max_parallel_checks": 2, "mode": "serial" },
            "queue_rules": [{ "name": "default", "batch_size": 3 }, { "name": "hotfix" }],
            "pull_request_rules": [{ "name": "a", "conditions": [] }, { "name": "b", "conditions": [] }],
        });
        let local = json!({
            "extends": "shared-config",
            "defaults": { "actions": { "comment": { "message": "local" } } },
            "merge_queue": { "max_parallel_checks": 4 },
            "queue_rules": [{ "name": "default", "batch_size": 1 }],
            "pull_request_rules": [{ "name": "b", "conditions": ["base = main"] }, { "name": "c", "conditions": [] }],
        });
        let m = resolve(&local, Some("o/app"), |n| {
            assert_eq!(n, "shared-config");
            Ok(remote.clone())
        })
        .unwrap();
        assert!(m.get("shared").is_none(), "shared is never merged");
        assert_eq!(m["extends"], json!("shared-config"), "the local key stays");
        assert_eq!(
            m["defaults"]["actions"]["comment"],
            json!({ "message": "local", "bot_account": "r" })
        );
        assert_eq!(
            m["merge_queue"],
            json!({ "max_parallel_checks": 4, "mode": "serial" })
        );
        let names = |k: &str| {
            m[k].as_array()
                .unwrap()
                .iter()
                .map(|r| r["name"].as_str().unwrap().to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(names("queue_rules"), vec!["hotfix", "default"]);
        assert_eq!(m["queue_rules"][1]["batch_size"], json!(1));
        assert_eq!(names("pull_request_rules"), vec!["a", "b", "c"]);
        assert_eq!(
            m["pull_request_rules"][1]["conditions"],
            json!(["base = main"])
        );
    }

    #[test]
    fn one_level_and_not_itself() {
        let local = json!({ "extends": "app" });
        assert_eq!(
            resolve(&local, Some("o/app"), |_| Ok(json!({}))),
            Err(ExtendsError::Itself("app".into()))
        );
        let local = json!({ "extends": "base" });
        assert_eq!(
            resolve(&local, Some("o/app"), |_| Ok(json!({ "extends": "other" }))),
            Err(ExtendsError::Nested("base".into()))
        );
        assert!(matches!(
            resolve(&local, None, |_| Err("not found".into())),
            Err(ExtendsError::Load { .. })
        ));
        let plain = json!({ "queue_rules": [] });
        assert_eq!(resolve(&plain, None, |_| unreachable!()).unwrap(), plain);
    }
}
