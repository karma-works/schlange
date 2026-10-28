//! Where the configuration lives in a repository, and the check of a configuration that a
//! pull request changes (the built-in protection "configuration changed").
//!
//! - The file names are searched in a fixed order; the first one found is read. More than one
//!   of them at once is an error (docs: configuration/file-format, "Configuration File Name").
//! - The configuration is read from the default branch only; a pull request's own copy never
//!   evaluates that pull request (MQ-52). [`assess_change`] only validates the new file and
//!   reports the result; it does not activate it (MQ-19).
//!
//! Everything here is pure: callers hand in what exists and the file texts.

use serde::{Deserialize, Serialize};

use crate::rules::Rules;
use crate::validate::{Finding, Severity};

/// The configuration file names, in the order they are searched.
pub const NAMES: [&str; 6] = [
    ".mergify.yml",
    ".mergify.yaml",
    ".mergify/config.yml",
    ".mergify/config.yaml",
    ".github/mergify.yml",
    ".github/mergify.yaml",
];

/// Name of the check reported for a pull request that changes the configuration.
pub const CHECK_CHANGED: &str = "Configuration changed";
/// Name of the check reported for a pull request that deletes the configuration.
pub const CHECK_DELETED: &str = "Configuration has been deleted";

/// Whether `path` is one of the configuration file names.
pub fn is_config_path(path: &str) -> bool {
    NAMES.contains(&path)
}

/// Which configuration file to read, given which paths exist.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Located {
    /// No configuration file.
    None,
    /// Exactly one file.
    One(&'static str),
    /// More than one: an error. `reads` is the one the search order would pick.
    Several {
        found: Vec<&'static str>,
        reads: &'static str,
    },
}

impl Located {
    /// The error text for [`Located::Several`].
    pub fn error(&self) -> Option<String> {
        match self {
            Located::Several { found, reads } => Some(format!(
                "more than one configuration file: {}; keep only one (the search order would \
                 read `{reads}`)",
                found.join(", ")
            )),
            _ => None,
        }
    }
}

/// Searches the names in order.
pub fn locate(exists: impl Fn(&str) -> bool) -> Located {
    let found: Vec<&'static str> = NAMES.iter().copied().filter(|n| exists(n)).collect();
    match found.as_slice() {
        [] => Located::None,
        [one] => Located::One(one),
        [first, ..] => Located::Several {
            reads: first,
            found: found.clone(),
        },
    }
}

/// The result of checking a configuration a pull request changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfigChange {
    /// The configuration paths the pull request touches.
    pub paths: Vec<String>,
    /// The pull request removes the configuration (and leaves none behind).
    pub deleted: bool,
    /// `None`: the new configuration validates; otherwise why it does not.
    pub error: Option<String>,
    /// Findings and notes for the check's summary.
    pub summary: String,
}

impl ConfigChange {
    /// The check name this result is reported under.
    pub fn check_name(&self) -> &'static str {
        if self.deleted {
            CHECK_DELETED
        } else {
            CHECK_CHANGED
        }
    }

    pub fn ok(&self) -> bool {
        self.error.is_none()
    }
}

/// Checks the configuration after a pull request's changes.
///
/// `changed` are the paths the pull request changes; `at_head` returns a file's text at the
/// pull request's head (`None` when it does not exist there). `validate` validates one text
/// and returns its errors (stages 1 to 4; findings Schlange does not support are notes, not
/// errors, because the file is valid for Mergify). Returns `None` when no configuration path
/// is changed.
pub fn assess_change(
    changed: &[String],
    at_head: impl Fn(&str) -> Option<String>,
    validate: impl Fn(&str, &str) -> Vec<Finding>,
) -> Option<ConfigChange> {
    let paths: Vec<String> = changed
        .iter()
        .filter(|p| is_config_path(p))
        .cloned()
        .collect();
    if paths.is_empty() {
        return None;
    }
    let located = locate(|n| at_head(n).is_some());
    let mut change = ConfigChange {
        paths: paths.clone(),
        deleted: false,
        error: None,
        summary: String::new(),
    };
    match located {
        Located::None => {
            change.deleted = true;
            change.summary = format!(
                "The pull request removes the configuration ({}). Removing it is accepted.",
                paths.join(", ")
            );
        }
        Located::Several { .. } => {
            let e = located.error().unwrap_or_default();
            change.summary = e.clone();
            change.error = Some(e);
        }
        Located::One(path) => {
            let text = at_head(path).unwrap_or_default();
            let findings = validate(path, &text);
            let errors: Vec<&Finding> = findings
                .iter()
                .filter(|f| f.severity == Severity::Error)
                .collect();
            let notes: Vec<&Finding> = findings
                .iter()
                .filter(|f| f.severity != Severity::Error)
                .collect();
            let line = |f: &&Finding| match f.line {
                Some(l) => format!("- {path}:{l}: {} ({})", f.message, f.pointer),
                None => format!("- {path}: {} ({})", f.message, f.pointer),
            };
            let mut summary = if errors.is_empty() {
                format!("The new configuration in `{path}` is valid.")
            } else {
                format!(
                    "The new configuration in `{path}` does not validate:\n{}",
                    errors.iter().map(line).collect::<Vec<_>>().join("\n")
                )
            };
            if !notes.is_empty() {
                summary.push_str(&format!(
                    "\n\nValid, but not supported by Schlange yet:\n{}",
                    notes.iter().map(line).collect::<Vec<_>>().join("\n")
                ));
            }
            if !errors.is_empty() {
                change.error = Some(format!(
                    "{} error(s) in `{path}`; first: {}",
                    errors.len(),
                    errors[0].message
                ));
            }
            change.summary = summary;
        }
    }
    Some(change)
}

/// Validation without the Mergify schema (stages 1, 3 and 4): YAML, the shape and the
/// conditions the engine reads, and the semantic rules. Used where the schema is not at hand
/// (the simulated forge; `serve` without `SCHLANGE_SCHEMA`).
pub fn validate_without_schema(file: &str, text: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    match crate::yaml::parse(text) {
        Err(errors) => {
            for e in errors {
                findings.push(Finding {
                    stage: crate::validate::Stage::Yaml,
                    severity: Severity::Error,
                    pointer: e.pointer,
                    line: e.position.map(|p| p.line),
                    column: e.position.map(|p| p.column),
                    message: e.message,
                    feature: None,
                });
            }
        }
        Ok((value, spans)) => {
            if let Err(e) = Rules::from_value(&value) {
                findings.push(Finding {
                    stage: crate::validate::Stage::Conditions,
                    severity: Severity::Error,
                    pointer: String::new(),
                    line: None,
                    column: None,
                    message: e.to_string(),
                    feature: None,
                });
            }
            crate::semantics::check(&value, &spans, &mut findings);
        }
    }
    let _ = file;
    findings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn files(list: &[(&str, &str)]) -> BTreeMap<String, String> {
        list.iter()
            .map(|(p, c)| (p.to_string(), c.to_string()))
            .collect()
    }

    #[test]
    fn search_order_and_two_files() {
        assert_eq!(locate(|_| false), Located::None);
        assert_eq!(
            locate(|p| p == ".github/mergify.yml"),
            Located::One(".github/mergify.yml")
        );
        let l = locate(|p| p == ".mergify/config.yaml" || p == ".mergify.yaml");
        assert_eq!(
            l,
            Located::Several {
                found: vec![".mergify.yaml", ".mergify/config.yaml"],
                reads: ".mergify.yaml"
            }
        );
        assert!(l.error().unwrap().contains(".mergify.yaml"));
        // Every name in the documented order.
        for (i, n) in NAMES.iter().enumerate() {
            let later: Vec<&str> = NAMES[i..].to_vec();
            assert_eq!(locate(|p| later.contains(&p)).error().is_some(), i < 5);
            assert!(is_config_path(n));
        }
        assert!(!is_config_path("mergify.yml"));
    }

    #[test]
    fn change_checks() {
        let good = "queue_rules:\n  - name: default\n";
        let bad = "queue_rules:\n  - name: a\n  - name: a\n";
        let v = validate_without_schema;
        let none = assess_change(&["src/a.rs".into()], |_| None, v);
        assert!(none.is_none(), "untouched configuration: no check");
        let head = files(&[(".mergify.yml", good)]);
        let c = assess_change(&[".mergify.yml".into()], |p| head.get(p).cloned(), v).unwrap();
        assert!(c.ok() && !c.deleted, "{c:?}");
        assert_eq!(c.check_name(), CHECK_CHANGED);
        let head = files(&[(".mergify.yml", bad)]);
        let c = assess_change(&[".mergify.yml".into()], |p| head.get(p).cloned(), v).unwrap();
        assert!(!c.ok());
        assert!(c.summary.contains("duplicate name"), "{}", c.summary);
        let c = assess_change(&[".mergify.yml".into()], |_| None, v).unwrap();
        assert!(c.ok() && c.deleted);
        assert_eq!(c.check_name(), CHECK_DELETED);
        // A second file next to the existing one.
        let head = files(&[(".mergify.yml", good), (".github/mergify.yml", good)]);
        let c =
            assess_change(&[".github/mergify.yml".into()], |p| head.get(p).cloned(), v).unwrap();
        assert!(!c.ok() && !c.deleted);
        let c = assess_change(
            &[".mergify.yml".into()],
            |_| Some("queue_rules: [\n".into()),
            v,
        )
        .unwrap();
        assert!(!c.ok(), "a YAML error fails the check");
    }
}
