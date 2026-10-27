//! The validation pipeline: YAML → schema → conditions → (semantics, missing) → support.

use std::collections::BTreeMap;

use serde::Serialize;
use serde_json::Value;

use crate::conditions::{self, Operator};
use crate::features::FeatureMap;
use crate::schema::Schema;
use crate::support::{self, SupportRow};
use crate::yaml::{self, Position, Spans};

/// Exit codes of `schlange config validate`.
pub mod exit {
    pub const VALID: u8 = 0;
    pub const INVALID: u8 = 1;
    pub const UNSUPPORTED: u8 = 2;
    /// No schema, unreadable file or other environment problem.
    pub const ENVIRONMENT: u8 = 3;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Stage {
    Yaml,
    Schema,
    Conditions,
    Semantics,
    Support,
}

impl Stage {
    fn as_str(self) -> &'static str {
        match self {
            Stage::Yaml => "yaml",
            Stage::Schema => "schema",
            Stage::Conditions => "conditions",
            Stage::Semantics => "semantics",
            Stage::Support => "support",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Severity {
    Error,
    /// Valid for Mergify, not supported by Schlange.
    Unsupported,
}

#[derive(Clone, Debug, Serialize)]
pub struct Finding {
    pub stage: Stage,
    pub severity: Severity,
    pub pointer: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub feature: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct Report {
    pub file: String,
    pub valid: bool,
    pub strict: bool,
    pub exit_code: u8,
    pub findings: Vec<Finding>,
    /// Every inventory entry the configuration uses, with its feature and status.
    pub support: Vec<SupportRow>,
    /// Stages not built yet; reported so that "valid" is not over-read.
    pub stages_missing: Vec<&'static str>,
}

fn at(pos: Option<Position>) -> (Option<usize>, Option<usize>) {
    (pos.map(|p| p.line), pos.map(|p| p.column))
}

/// Validates one configuration text.
pub fn validate(
    file: &str,
    source: &str,
    schema: &Schema,
    features: &FeatureMap,
    strict: bool,
) -> Report {
    let mut findings = Vec::new();
    let mut support_rows = Vec::new();
    match yaml::parse(source) {
        Err(errors) => {
            for e in errors {
                let (line, column) = at(e.position);
                findings.push(Finding {
                    stage: Stage::Yaml,
                    severity: Severity::Error,
                    pointer: e.pointer,
                    line,
                    column,
                    message: e.message,
                    feature: None,
                });
            }
        }
        Ok((value, spans)) => {
            for v in schema.validate(&value) {
                let pos = if v.at_key {
                    spans.key(&v.pointer)
                } else {
                    spans.value(&v.pointer)
                };
                let (line, column) = at(pos);
                findings.push(Finding {
                    stage: Stage::Schema,
                    severity: Severity::Error,
                    pointer: v.pointer,
                    line,
                    column,
                    message: v.message,
                    feature: None,
                });
            }
            check_conditions(&value, "", schema, &spans, &mut findings);
            if !findings.iter().any(|f| f.stage == Stage::Schema) {
                crate::semantics::check(&value, &spans, &mut findings);
            }
            if !findings.iter().any(|f| f.severity == Severity::Error) {
                let uses = support::collect_uses(&value, &schema.condition_keys, features);
                support_rows = support::report(&uses, features);
                for row in support_rows.iter().filter(|r| !r.supported) {
                    let pointer = row.pointer.clone().unwrap_or_default();
                    let (line, column) = if row.pointer.is_some() {
                        at(spans.key(&pointer))
                    } else {
                        (None, None)
                    };
                    let what = match row.id.strip_prefix("default:") {
                        Some(path) => format!("default `{path}` is in effect but"),
                        None => format!("`{}` is", row.id),
                    };
                    findings.push(Finding {
                        stage: Stage::Support,
                        severity: Severity::Unsupported,
                        pointer,
                        line,
                        column,
                        message: format!(
                            "{what} not supported by Schlange yet (feature {}, {:?}, {}: {})",
                            row.feature,
                            row.priority,
                            row.status.as_str(),
                            row.reason
                                .as_ref()
                                .map_or_else(String::new, |r| r.describe())
                        ),
                        feature: Some(row.feature.clone()),
                    });
                }
            }
        }
    }
    let has_errors = findings.iter().any(|f| f.severity == Severity::Error);
    let has_unsupported = findings.iter().any(|f| f.severity == Severity::Unsupported);
    let exit_code = if has_errors {
        exit::INVALID
    } else if strict && has_unsupported {
        exit::UNSUPPORTED
    } else {
        exit::VALID
    };
    Report {
        file: file.to_string(),
        valid: !has_errors,
        strict,
        exit_code,
        findings,
        support: support_rows,
        stages_missing: vec![],
    }
}

/// Stage 3: parses every condition list and checks attributes, operators, modifiers and regexes.
fn check_conditions(
    value: &Value,
    pointer: &str,
    schema: &Schema,
    spans: &Spans,
    findings: &mut Vec<Finding>,
) {
    match value {
        Value::Object(obj) => {
            for (key, child) in obj {
                let child_ptr = format!("{pointer}/{}", yaml::escape(key));
                if pointer.is_empty() && key == "shared" {
                    continue;
                }
                if schema.condition_keys.contains(key) {
                    check_list(child, &child_ptr, schema, spans, findings);
                } else {
                    check_conditions(child, &child_ptr, schema, spans, findings);
                }
            }
        }
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                check_conditions(item, &format!("{pointer}/{i}"), schema, spans, findings);
            }
        }
        _ => {}
    }
}

fn check_list(
    value: &Value,
    pointer: &str,
    schema: &Schema,
    spans: &Spans,
    findings: &mut Vec<Finding>,
) {
    let mut push = |ptr: &str, message: String| {
        let (line, column) = at(spans.value(ptr));
        findings.push(Finding {
            stage: Stage::Conditions,
            severity: Severity::Error,
            pointer: ptr.to_string(),
            line,
            column,
            message,
            feature: None,
        });
    };
    let mut errors = Vec::new();
    conditions::parse_list(value, pointer, &mut errors);
    for e in errors {
        push(&e.pointer, e.message);
    }
    // Atoms keep no pointer; find each string again to report its position.
    let mut strings = Vec::new();
    collect_strings(value, pointer, &mut strings);
    for (ptr, text) in strings {
        let Ok(atom) = conditions::parse_atom(&text) else {
            continue; // already reported
        };
        let Some(attr) = schema.attributes.get(&atom.attribute) else {
            push(&ptr, format!("unknown attribute `{}`", atom.attribute));
            continue;
        };
        if atom.negate && !attr.negate {
            push(&ptr, format!("`{}` cannot be negated", atom.attribute));
        }
        if atom.length && !attr.length {
            push(&ptr, format!("`{}` has no length (`#`)", atom.attribute));
        }
        if attr.length_required && !atom.length {
            push(
                &ptr,
                format!("`{}` can only be used with `#`", atom.attribute),
            );
        }
        if let Some(allowed) = &attr.operators {
            match atom.operator {
                None if !allowed.is_empty() && !atom.length => push(
                    &ptr,
                    format!(
                        "`{}` needs an operator ({})",
                        atom.attribute,
                        allowed.join(" ")
                    ),
                ),
                None => {}
                Some(op) if allowed.is_empty() => push(
                    &ptr,
                    format!(
                        "`{}` takes no operator (write `{}` or `-{}`), found `{}`",
                        atom.attribute,
                        atom.attribute,
                        atom.attribute,
                        op.as_str()
                    ),
                ),
                Some(op) if !allowed.iter().any(|a| a == op.as_str()) && !atom.length => push(
                    &ptr,
                    format!(
                        "operator `{}` is not allowed for `{}` (allowed: {})",
                        op.as_str(),
                        atom.attribute,
                        allowed.join(" ")
                    ),
                ),
                Some(_) => {}
            }
        }
        if atom.length
            && let Some(v) = &atom.value
            && v.parse::<i64>().is_err()
        {
            push(
                &ptr,
                format!("length of `{}` needs a number, found `{v}`", atom.attribute),
            );
        }
        if atom.operator == Some(Operator::Regex)
            && let Some(v) = &atom.value
            && let Err(e) = fancy_regex::Regex::new(v)
        {
            push(&ptr, format!("regex does not compile: {e}"));
        }
    }
}

fn collect_strings(value: &Value, pointer: &str, out: &mut Vec<(String, String)>) {
    match value {
        Value::String(s) => out.push((pointer.to_string(), s.clone())),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                collect_strings(item, &format!("{pointer}/{i}"), out);
            }
        }
        Value::Object(map) => {
            for (k, v) in map {
                collect_strings(v, &format!("{pointer}/{}", yaml::escape(k)), out);
            }
        }
        _ => {}
    }
}

impl Report {
    /// Human-readable output: one line per finding, `file:line:column: severity[stage] pointer: message`.
    pub fn to_text(&self) -> String {
        let mut out = String::new();
        let mut unsupported: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for f in &self.findings {
            if f.severity == Severity::Unsupported {
                let entry = match (f.line, f.column) {
                    (Some(l), Some(c)) => format!("{} ({l}:{c})", short(&f.message)),
                    _ => short(&f.message),
                };
                unsupported
                    .entry(f.feature.as_deref().unwrap_or("?"))
                    .or_default()
                    .push(entry);
                continue;
            }
            let loc = match (f.line, f.column) {
                (Some(l), Some(c)) => format!("{}:{l}:{c}", self.file),
                _ => self.file.clone(),
            };
            let ptr = if f.pointer.is_empty() {
                "/"
            } else {
                &f.pointer
            };
            out.push_str(&format!(
                "{loc}: error[{}] {ptr}: {}\n",
                f.stage.as_str(),
                f.message
            ));
        }
        for (feature, entries) in &unsupported {
            let row = self.support.iter().find(|r| r.feature == *feature);
            let (prio, status) = row.map_or(("?".to_string(), "?"), |r| {
                (format!("{:?}", r.priority), r.status.as_str())
            });
            out.push_str(&format!(
                "{}: unsupported[support] feature {feature} ({prio}, {status}): {}\n",
                self.file,
                entries.join(", ")
            ));
        }
        let errors = self
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
            .count();
        let unsupported = self.findings.len() - errors;
        let verdict = match self.exit_code {
            exit::VALID if unsupported == 0 => "valid".to_string(),
            exit::VALID => format!(
                "valid for Mergify; {unsupported} entries not supported by Schlange yet (--strict makes this exit 2)"
            ),
            exit::UNSUPPORTED => {
                format!("valid for Mergify, not supported by Schlange ({unsupported} entries)")
            }
            _ => format!("invalid ({errors} errors)"),
        };
        if self.stages_missing.is_empty() {
            out.push_str(&format!("{}: {verdict}\n", self.file));
        } else {
            out.push_str(&format!(
                "{}: {verdict}; stages not built yet: {}\n",
                self.file,
                self.stages_missing.join(", ")
            ));
        }
        out
    }
}

/// The inventory ID (or `default` path) a support message is about.
fn short(message: &str) -> String {
    let start = message.find('`').map_or(0, |i| i + 1);
    let end = message[start..]
        .find('`')
        .map_or(message.len(), |i| start + i);
    let id = &message[start..end];
    if message.starts_with("default") {
        format!("default:{id}")
    } else {
        id.to_string()
    }
}
