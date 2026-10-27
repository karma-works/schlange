//! Stage 5: which inventory entries a configuration uses (paths, actions, attributes, enum
//! values and the defaults that take effect), and whether Schlange supports them.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::conditions::{self, Condition};
use crate::features::{FeatureMap, Priority, Status, Unsupported};

/// Sections whose defaults apply even when the configuration does not mention them.
const ALWAYS_ACTIVE: &[&str] = &["merge_queue", "commands_restrictions"];

/// One used inventory entry with its pointer in the configuration (if it is written there).
#[derive(Clone, Debug)]
pub struct Use {
    pub id: String,
    pub pointer: Option<String>,
}

/// Collects the inventory entries a configuration uses.
pub fn collect_uses(
    config: &Value,
    condition_keys: &BTreeSet<String>,
    map: &FeatureMap,
) -> Vec<Use> {
    let mut uses: BTreeMap<String, Option<String>> = BTreeMap::new();
    walk(config, "", "", condition_keys, map, &mut uses);

    for entry in &map.inventory.entries {
        if let Some(path) = entry.id.strip_prefix("default:")
            && (!is_off(&entry.value) || null_chooses(path))
            && default_in_effect(config, path)
        {
            uses.entry(entry.id.clone()).or_insert(None);
        }
    }
    uses.into_iter()
        .map(|(id, pointer)| Use { id, pointer })
        .collect()
}

/// Sections of `defaults` that fill in options of other sections before Mergify's own defaults.
const DEFAULTS_SECTIONS: &[(&str, &str)] = &[
    ("queue_rules.", "defaults.queue_rule."),
    ("pull_request_rules.actions.", "defaults.actions."),
];

/// Whether Mergify's default for `path` (dotted, list levels skipped) takes effect: some object
/// at the parent path exists (an empty `key:` counts) and lacks the key, and no `defaults`
/// section sets it instead. Sections in [`ALWAYS_ACTIVE`] exist even when not written.
fn default_in_effect(config: &Value, path: &str) -> bool {
    for (prefix, defaults) in DEFAULTS_SECTIONS {
        if let Some(rest) = path.strip_prefix(prefix)
            && lookup(config, &format!("{defaults}{rest}")).is_some()
        {
            return false;
        }
    }
    let segs: Vec<&str> = path.split('.').collect();
    let empty = Value::Object(Default::default());
    match config.get(segs[0]) {
        Some(section) => missing_somewhere(section, &segs[1..]),
        None if ALWAYS_ACTIVE.contains(&segs[0]) => missing_somewhere(&empty, &segs[1..]),
        None => false,
    }
}

fn lookup<'a>(config: &'a Value, path: &str) -> Option<&'a Value> {
    path.split('.').try_fold(config, |v, seg| v.get(seg))
}

/// Whether some object reached through `segs[..len-1]` (any list element) lacks the last key.
fn missing_somewhere(value: &Value, segs: &[&str]) -> bool {
    let Some((first, rest)) = segs.split_first() else {
        return false;
    };
    match value {
        Value::Array(items) => items.iter().any(|i| missing_somewhere(i, segs)),
        Value::Object(obj) => match obj.get(*first) {
            None => rest.is_empty(),
            Some(child) => !rest.is_empty() && missing_somewhere(child, rest),
        },
        Value::Null => rest.is_empty(),
        _ => false,
    }
}

/// Options whose `null` default is a choice, not "off": a merge method left unset means the
/// repository's first allowed method (schema), which may not be the one Schlange uses.
fn null_chooses(path: &str) -> bool {
    path.ends_with(".merge_method") || path.ends_with(".method")
}

/// A value that leaves a feature switched off: `null`, `false`, `0`, an empty list or map. A
/// default or an explicit option with such a value does not use the feature (value-level: an
/// option written as `false` is not a gap, the same option written as `true` is).
pub fn is_off(v: &Value) -> bool {
    match v {
        Value::Null | Value::Bool(false) => true,
        Value::Number(n) => n.as_f64() == Some(0.0),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        Value::String(_) | Value::Bool(true) => false,
    }
}

fn add(uses: &mut BTreeMap<String, Option<String>>, map: &FeatureMap, id: String, pointer: &str) {
    if map.contains(&id) {
        uses.entry(id).or_insert_with(|| Some(pointer.to_string()));
    }
}

fn walk(
    value: &Value,
    path: &str,
    pointer: &str,
    condition_keys: &BTreeSet<String>,
    map: &FeatureMap,
    uses: &mut BTreeMap<String, Option<String>>,
) {
    match value {
        Value::Object(obj) => {
            for (key, child) in obj {
                let child_path = if path.is_empty() {
                    key.clone()
                } else {
                    format!("{path}.{key}")
                };
                let child_ptr = format!("{pointer}/{}", crate::yaml::escape(key));
                if !is_off(child) || path.ends_with("actions") {
                    add(uses, map, format!("config:{child_path}"), &child_ptr);
                }
                if path.ends_with("actions") {
                    add(uses, map, format!("action:{key}"), &child_ptr);
                }
                if let Value::String(s) = child {
                    add(uses, map, format!("enum:{key}:{s}"), &child_ptr);
                }
                if let Value::Array(items) = child {
                    for (i, item) in items.iter().enumerate() {
                        if let Value::String(s) = item {
                            add(
                                uses,
                                map,
                                format!("enum:{key}:{s}"),
                                &format!("{child_ptr}/{i}"),
                            );
                        }
                    }
                }
                if path.is_empty() && key == "shared" {
                    continue; // free-form anchors
                }
                if condition_keys.contains(key) {
                    let mut errors = Vec::new();
                    for cond in conditions::parse_list(child, &child_ptr, &mut errors) {
                        add_attributes(&cond, &child_ptr, map, uses);
                    }
                    continue;
                }
                walk(child, &child_path, &child_ptr, condition_keys, map, uses);
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                walk(
                    item,
                    path,
                    &format!("{pointer}/{i}"),
                    condition_keys,
                    map,
                    uses,
                );
            }
        }
        _ => {}
    }
}

fn add_attributes(
    cond: &Condition,
    pointer: &str,
    map: &FeatureMap,
    uses: &mut BTreeMap<String, Option<String>>,
) {
    for atom in cond.atoms() {
        add(uses, map, format!("attr:{}", atom.attribute), pointer);
    }
}

/// One row of the support report.
#[derive(Clone, Debug, Serialize)]
pub struct SupportRow {
    pub id: String,
    pub feature: String,
    pub priority: Priority,
    pub status: Status,
    pub supported: bool,
    /// Why the entry is unsupported (absent when supported).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<Unsupported>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// Maps every use to its feature and status.
pub fn report(uses: &[Use], map: &FeatureMap) -> Vec<SupportRow> {
    uses.iter()
        .filter_map(|u| {
            let f = map.owner(&u.id)?;
            let reason = f.support(&u.id);
            Some(SupportRow {
                id: u.id.clone(),
                feature: f.id.clone(),
                priority: f.priority,
                status: f.status,
                supported: reason.is_none(),
                reason,
                pointer: u.pointer.clone(),
            })
        })
        .collect()
}
