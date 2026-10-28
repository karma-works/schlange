//! The P2 parts of a configuration the queue engine reads: `priority_rules`,
//! `merge_protections`, `scopes`, and the merge and update methods of a queue rule. Defaults
//! follow Mergify's schema; the texts are ours.

use serde_json::Value;

use crate::conditions::{self, Condition, ParseError};
use crate::rules::RulesError;

/// The priority a pull request gets when no priority rule matches (docs: always `medium`).
pub const DEFAULT_PRIORITY: u32 = 2000;

/// `low`, `medium`, `high` as numbers (docs: 1000, 2000, 3000).
pub fn priority_value(v: &Value) -> Option<u32> {
    match v {
        Value::String(s) => match s.as_str() {
            "low" => Some(1000),
            "medium" => Some(2000),
            "high" => Some(3000),
            _ => None,
        },
        Value::Number(n) => n.as_u64().map(|n| n as u32),
        Value::Null => Some(DEFAULT_PRIORITY),
        _ => None,
    }
}

/// The alias of a priority value, for status output: the keyword when it is one.
pub fn priority_alias(p: u32) -> String {
    match p {
        1000 => "low".into(),
        2000 => "medium".into(),
        3000 => "high".into(),
        n => n.to_string(),
    }
}

/// One `priority_rules` entry.
#[derive(Clone, Debug)]
pub struct PriorityRule {
    pub name: String,
    pub conditions: Vec<Condition>,
    pub priority: u32,
    pub allow_checks_interruption: bool,
}

/// One `merge_protections` entry: when `if` holds, `success_conditions` must hold before the
/// pull request may merge.
#[derive(Clone, Debug)]
pub struct MergeProtection {
    pub name: String,
    pub description: Option<String>,
    pub when: Vec<Condition>,
    pub success_conditions: Vec<Condition>,
}

/// A file filter: `include` globs, optional `exclude` globs (regular expressions after
/// translation, anchored).
#[derive(Clone, Debug, Default)]
pub struct FileFilter {
    pub include: Vec<String>,
    pub exclude: Vec<String>,
}

impl FileFilter {
    pub fn matches(&self, path: &str) -> bool {
        let hit = |list: &[String]| {
            list.iter().any(|re| {
                fancy_regex::Regex::new(re)
                    .ok()
                    .and_then(|r| r.is_match(path).ok())
                    .unwrap_or(false)
            })
        };
        hit(&self.include) && !hit(&self.exclude)
    }
}

/// `scopes`: where scopes come from. Only the `files` source is evaluated by Schlange; a
/// `manual` source is parsed (`manual = true`) and gives every pull request no scope.
#[derive(Clone, Debug, Default)]
pub struct Scopes {
    pub files: Vec<(String, FileFilter)>,
    pub manual: bool,
    pub barrier_files: Option<FileFilter>,
    pub merge_queue_scope: String,
}

impl Scopes {
    /// The scopes a pull request with these changed files touches, and whether it is a
    /// barrier (it changes a barrier file: it impacts every scope).
    pub fn of(&self, files: &[String]) -> (Vec<String>, bool) {
        let mut out: Vec<String> = self
            .files
            .iter()
            .filter(|(_, f)| files.iter().any(|p| f.matches(p)))
            .map(|(n, _)| n.clone())
            .collect();
        out.sort();
        let barrier = self
            .barrier_files
            .as_ref()
            .is_some_and(|b| files.iter().any(|p| b.matches(p)));
        (out, barrier)
    }
}

/// `merge_method` of a queue rule, resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MergeMethod {
    /// `squash`, and an unset method (Schlange squashes; see `queue-merge-method-squash`).
    #[default]
    Squash,
    Merge,
    Rebase,
    FastForward,
    MergeBatch,
}

impl MergeMethod {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "squash" => MergeMethod::Squash,
            "merge" => MergeMethod::Merge,
            "rebase" => MergeMethod::Rebase,
            "fast-forward" => MergeMethod::FastForward,
            "merge-batch" => MergeMethod::MergeBatch,
            _ => return None,
        })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MergeMethod::Squash => "squash",
            MergeMethod::Merge => "merge",
            MergeMethod::Rebase => "rebase",
            MergeMethod::FastForward => "fast-forward",
            MergeMethod::MergeBatch => "merge-batch",
        }
    }

    /// Methods that land the batch branch itself: a car is built on top of the car ahead
    /// (stacked), so landing it moves the base onto (or merges) exactly that branch.
    pub fn lands_branch(self) -> bool {
        matches!(self, MergeMethod::FastForward | MergeMethod::MergeBatch)
    }

    /// Methods that is the default `update_method` `rebase` for (docs: `fast-forward`).
    pub fn default_update(self) -> UpdateMethod {
        if self == MergeMethod::FastForward {
            UpdateMethod::Rebase
        } else {
            UpdateMethod::Merge
        }
    }
}

/// `update_method`: how a pull request checked in place is brought up to date.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum UpdateMethod {
    Merge,
    Rebase,
}

impl UpdateMethod {
    pub fn as_str(self) -> &'static str {
        match self {
            UpdateMethod::Merge => "merge",
            UpdateMethod::Rebase => "rebase",
        }
    }
}

/// `branch_protection_injection_mode` (default `queue`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum InjectionMode {
    /// Required checks join the queue and the merge conditions.
    #[default]
    Queue,
    /// Required checks join the merge conditions only.
    Merge,
    /// Not injected.
    None,
}

fn shape(pointer: &str, message: &str) -> RulesError {
    RulesError::Shape {
        pointer: pointer.to_string(),
        message: message.to_string(),
    }
}

pub(crate) fn priority_rules(
    value: &Value,
    errors: &mut Vec<ParseError>,
) -> Result<Vec<PriorityRule>, RulesError> {
    let Some(list) = value.get("priority_rules").filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    let Some(items) = list.as_array() else {
        return Err(shape("/priority_rules", "must be a list"));
    };
    let mut out = Vec::new();
    for (i, r) in items.iter().enumerate() {
        let p = format!("/priority_rules/{i}");
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(&p, "needs a `name`"))?
            .to_string();
        let conditions = conditions::parse_list(
            r.get("conditions").unwrap_or(&Value::Null),
            &format!("{p}/conditions"),
            errors,
        );
        let priority =
            priority_value(r.get("priority").unwrap_or(&Value::Null)).ok_or_else(|| {
                shape(
                    &format!("{p}/priority"),
                    "must be low, medium, high or a number",
                )
            })?;
        let allow_checks_interruption = r
            .get("allow_checks_interruption")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push(PriorityRule {
            name,
            conditions,
            priority,
            allow_checks_interruption,
        });
    }
    Ok(out)
}

pub(crate) fn merge_protections(
    value: &Value,
    errors: &mut Vec<ParseError>,
) -> Result<Vec<MergeProtection>, RulesError> {
    let Some(list) = value.get("merge_protections").filter(|v| !v.is_null()) else {
        return Ok(vec![]);
    };
    let Some(items) = list.as_array() else {
        return Err(shape("/merge_protections", "must be a list"));
    };
    let mut out = Vec::new();
    for (i, r) in items.iter().enumerate() {
        let p = format!("/merge_protections/{i}");
        let name = r
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(|| shape(&p, "needs a `name`"))?
            .to_string();
        out.push(MergeProtection {
            name,
            description: r
                .get("description")
                .and_then(Value::as_str)
                .map(str::to_string),
            when: conditions::parse_list(
                r.get("if").unwrap_or(&Value::Null),
                &format!("{p}/if"),
                errors,
            ),
            success_conditions: conditions::parse_list(
                r.get("success_conditions").unwrap_or(&Value::Null),
                &format!("{p}/success_conditions"),
                errors,
            ),
        });
    }
    Ok(out)
}

fn file_filter(v: &Value, p: &str) -> Result<FileFilter, RulesError> {
    let list = |key: &str| -> Result<Vec<String>, RulesError> {
        match v.get(key) {
            None | Some(Value::Null) => Ok(vec![]),
            Some(Value::Array(items)) => items
                .iter()
                .map(|g| {
                    let g = g
                        .as_str()
                        .ok_or_else(|| shape(&format!("{p}/{key}"), "must be a list of globs"))?;
                    crate::glob::glob_to_regex(g)
                        .map_err(|e| shape(&format!("{p}/{key}"), &format!("glob `{g}`: {e}")))
                })
                .collect(),
            Some(_) => Err(shape(&format!("{p}/{key}"), "must be a list of globs")),
        }
    };
    Ok(FileFilter {
        include: list("include")?,
        exclude: list("exclude")?,
    })
}

pub(crate) fn scopes(value: &Value) -> Result<Option<Scopes>, RulesError> {
    let Some(s) = value.get("scopes").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let mut out = Scopes {
        merge_queue_scope: s
            .get("merge_queue_scope")
            .and_then(Value::as_str)
            .unwrap_or("merge-queue")
            .to_string(),
        ..Scopes::default()
    };
    match s.get("source") {
        None | Some(Value::Null) => return Ok(None),
        Some(src) => {
            if let Some(files) = src.get("files").and_then(Value::as_object) {
                for (name, f) in files {
                    out.files.push((
                        name.clone(),
                        file_filter(f, &format!("/scopes/source/files/{name}"))?,
                    ));
                }
            }
            if src.get("manual").is_some() {
                out.manual = true;
            }
        }
    }
    if let Some(b) = s.get("barrier_files").filter(|v| !v.is_null()) {
        let f = file_filter(b, "/scopes/barrier_files")?;
        if f.include.is_empty() {
            return Err(shape(
                "/scopes/barrier_files",
                "needs at least one `include` pattern",
            ));
        }
        out.barrier_files = Some(f);
    }
    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use crate::rules::Rules;

    use super::*;

    #[test]
    fn priorities_protections_scopes() {
        let src = "priority_rules:\n  - name: urgent\n    conditions: [label = urgent]\n    priority: high\n    allow_checks_interruption: true\n  - name: crit\n    conditions: [label = crit]\n    priority: 5000\n  - name: plain\n    conditions: []\n\
                   merge_protections:\n  - name: hours\n    if: [base = main]\n    success_conditions: [label = ok]\n\
                   scopes:\n  source:\n    files:\n      api:\n        include: ['api/**']\n      web:\n        include: ['web/**']\n        exclude: ['web/docs/**']\n  barrier_files:\n    include: [Makefile]\n\
                   queue_rules:\n  - name: q\n    merge_method: fast-forward\n";
        let r = Rules::from_yaml(src).unwrap();
        assert_eq!(r.priority_rules.len(), 3);
        assert_eq!(r.priority_rules[0].priority, 3000);
        assert!(r.priority_rules[0].allow_checks_interruption);
        assert_eq!(r.priority_rules[1].priority, 5000);
        assert_eq!(r.priority_rules[2].priority, DEFAULT_PRIORITY);
        assert_eq!(r.merge_protections.len(), 1);
        let s = r.scopes.as_ref().unwrap();
        assert_eq!(
            s.of(&["api/x.rs".into(), "web/docs/a.md".into()]),
            (vec!["api".to_string()], false)
        );
        assert_eq!(s.of(&["Makefile".into()]), (vec![], true));
        assert_eq!(r.queue_rules[0].method(), MergeMethod::FastForward);
        assert_eq!(r.queue_rules[0].update(), UpdateMethod::Rebase);
        assert_eq!(priority_alias(3000), "high");
    }
}
