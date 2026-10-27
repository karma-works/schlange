//! Typed view of the parts of a configuration the engine uses in phase 0: queue rules with
//! their conditions and pull request rules with the `queue` action.

use serde_json::Value;

use crate::conditions::{self, Condition, ParseError};

#[derive(Clone, Debug)]
pub struct QueueRule {
    pub name: String,
    pub queue_conditions: Vec<Condition>,
    pub merge_conditions: Vec<Condition>,
}

#[derive(Clone, Debug)]
pub struct PullRequestRule {
    pub name: String,
    pub conditions: Vec<Condition>,
    /// `Some(None)`: action `queue` without a name; `Some(Some(q))`: queue `q`.
    pub queue: Option<Option<String>>,
}

#[derive(Clone, Debug, Default)]
pub struct Rules {
    pub queue_rules: Vec<QueueRule>,
    pub pull_request_rules: Vec<PullRequestRule>,
}

#[derive(Debug, thiserror::Error)]
pub enum RulesError {
    #[error("{0}")]
    Yaml(String),
    #[error("{}", .0.iter().map(|e| format!("{}: {}", e.pointer, e.message)).collect::<Vec<_>>().join("; "))]
    Conditions(Vec<ParseError>),
    #[error("{pointer}: {message}")]
    Shape { pointer: String, message: String },
}

impl Rules {
    /// Parses YAML text. Schema validation is a separate step (`validate`); this only needs
    /// the shape it reads.
    pub fn from_yaml(source: &str) -> Result<Self, RulesError> {
        let (value, _) = crate::yaml::parse(source).map_err(|errs| {
            RulesError::Yaml(
                errs.into_iter()
                    .map(|e| e.message)
                    .collect::<Vec<_>>()
                    .join("; "),
            )
        })?;
        Self::from_value(&value)
    }

    pub fn from_value(value: &Value) -> Result<Self, RulesError> {
        let mut errors = Vec::new();
        let mut rules = Rules::default();
        for (i, q) in array(value, "queue_rules")?.iter().enumerate() {
            let p = format!("/queue_rules/{i}");
            rules.queue_rules.push(QueueRule {
                name: name(q, &p)?,
                queue_conditions: conditions::parse_list(
                    q.get("queue_conditions").unwrap_or(&Value::Null),
                    &format!("{p}/queue_conditions"),
                    &mut errors,
                ),
                merge_conditions: conditions::parse_list(
                    q.get("merge_conditions").unwrap_or(&Value::Null),
                    &format!("{p}/merge_conditions"),
                    &mut errors,
                ),
            });
        }
        for (i, r) in array(value, "pull_request_rules")?.iter().enumerate() {
            let p = format!("/pull_request_rules/{i}");
            let queue = r
                .get("actions")
                .and_then(|a| a.as_object())
                .and_then(|a| a.get("queue"))
                .map(|q| q.get("name").and_then(Value::as_str).map(str::to_string));
            rules.pull_request_rules.push(PullRequestRule {
                name: name(r, &p)?,
                conditions: conditions::parse_list(
                    r.get("conditions").unwrap_or(&Value::Null),
                    &format!("{p}/conditions"),
                    &mut errors,
                ),
                queue,
            });
        }
        if errors.is_empty() {
            Ok(rules)
        } else {
            Err(RulesError::Conditions(errors))
        }
    }
}

fn array<'a>(value: &'a Value, key: &str) -> Result<&'a [Value], RulesError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(RulesError::Shape {
            pointer: format!("/{key}"),
            message: "must be a list".into(),
        }),
    }
}

fn name(v: &Value, pointer: &str) -> Result<String, RulesError> {
    v.get("name")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| RulesError::Shape {
            pointer: pointer.to_string(),
            message: "needs a `name`".into(),
        })
}

/// Parses a duration: `90`, `90s`, `3m`, `1h`, `5 minutes`, `1 hour 30 minutes`. Returns seconds.
pub fn parse_duration(text: &str) -> Option<u64> {
    let text = text.trim();
    if let Ok(n) = text.parse::<u64>() {
        return Some(n);
    }
    let mut total = 0u64;
    let mut number: Option<u64> = None;
    let mut rest = text;
    let mut matched = false;
    while !rest.is_empty() {
        rest = rest.trim_start();
        let digits = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        if digits > 0 {
            number = Some(rest[..digits].parse().ok()?);
            rest = &rest[digits..];
            continue;
        }
        let word_end = rest
            .find(|c: char| c.is_ascii_digit() || c.is_whitespace())
            .unwrap_or(rest.len());
        let unit = &rest[..word_end];
        let factor = match unit {
            "s" | "sec" | "secs" | "second" | "seconds" => 1,
            "m" | "min" | "mins" | "minute" | "minutes" => 60,
            "h" | "hour" | "hours" => 3600,
            "d" | "day" | "days" => 86400,
            _ => return None,
        };
        total += number.take()? * factor;
        matched = true;
        rest = &rest[word_end..];
    }
    (matched && number.is_none()).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations() {
        assert_eq!(parse_duration("90"), Some(90));
        assert_eq!(parse_duration("90s"), Some(90));
        assert_eq!(parse_duration("3m"), Some(180));
        assert_eq!(parse_duration("5 minutes"), Some(300));
        assert_eq!(parse_duration("1 hour 30 minutes"), Some(5400));
        assert_eq!(parse_duration("1h30m"), Some(5400));
        assert_eq!(parse_duration("soon"), None);
        assert_eq!(parse_duration("5"), Some(5));
        assert_eq!(parse_duration("5 parsecs"), None);
    }

    #[test]
    fn arztagent_rules() {
        let src = include_str!("../tests/golden/arztagent.yml");
        let rules = Rules::from_yaml(src).unwrap();
        assert_eq!(rules.queue_rules.len(), 1);
        assert_eq!(rules.queue_rules[0].name, "default");
        assert_eq!(rules.queue_rules[0].queue_conditions.len(), 7);
        assert_eq!(rules.pull_request_rules.len(), 2);
        assert_eq!(rules.pull_request_rules[0].queue, Some(None));
    }
}
