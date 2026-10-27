//! Stage 5: which inventory entries a configuration uses (paths, actions, attributes, enum
//! values and the defaults that take effect), and whether Schlange supports them.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use serde_json::Value;

use crate::conditions::{self, Condition};
use crate::features::{FeatureMap, Priority, Status};

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
    let mut set_paths = BTreeSet::new();
    walk(
        config,
        "",
        "",
        condition_keys,
        map,
        &mut uses,
        &mut set_paths,
    );

    // Defaults take effect where the path is not set but its parent section exists.
    for entry in &map.inventory.entries {
        let Some(path) = entry.id.strip_prefix("default:") else {
            continue;
        };
        if set_paths.contains(path) {
            continue;
        }
        let parent = path.rsplit_once('.').map_or("", |(p, _)| p);
        let top = path.split('.').next().unwrap_or("");
        if set_paths.contains(parent) || (parent == top && ALWAYS_ACTIVE.contains(&top)) {
            uses.entry(entry.id.clone()).or_insert(None);
        }
    }
    uses.into_iter()
        .map(|(id, pointer)| Use { id, pointer })
        .collect()
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
    set_paths: &mut BTreeSet<String>,
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
                set_paths.insert(child_path.clone());
                add(uses, map, format!("config:{child_path}"), &child_ptr);
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
                walk(
                    child,
                    &child_path,
                    &child_ptr,
                    condition_keys,
                    map,
                    uses,
                    set_paths,
                );
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
                    set_paths,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pointer: Option<String>,
}

/// Maps every use to its feature and status.
pub fn report(uses: &[Use], map: &FeatureMap) -> Vec<SupportRow> {
    uses.iter()
        .filter_map(|u| {
            let f = map.owner(&u.id)?;
            Some(SupportRow {
                id: u.id.clone(),
                feature: f.id.clone(),
                priority: f.priority,
                status: f.status,
                supported: f.status.is_supported(),
                pointer: u.pointer.clone(),
            })
        })
        .collect()
}
