//! Typed view of the parts of a configuration the engine uses: queue rules with their
//! conditions and queue options, the `merge_queue` settings, and pull request rules with the
//! `queue` action. Defaults follow Mergify's schema (`queue_rules.*`, `merge_queue.*`).

use serde_json::Value;

use crate::conditions::{self, Condition, ParseError};

/// `batch_size`: a fixed size or a `{min, max}` range. Default 1.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BatchSize {
    Fixed(u32),
    Range { min: u32, max: u32 },
}

impl BatchSize {
    pub fn max(self) -> u32 {
        match self {
            BatchSize::Fixed(n) => n,
            BatchSize::Range { max, .. } => max,
        }
    }

    pub fn min(self) -> u32 {
        match self {
            BatchSize::Fixed(n) => n,
            BatchSize::Range { min, .. } => min,
        }
    }
}

/// `checks_timeout`: default `auto`; a fixed duration (at least 60 s); `null` disables it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChecksTimeout {
    Auto,
    Fixed(u64),
    Disabled,
}

#[derive(Clone, Debug)]
pub struct QueueRule {
    pub name: String,
    pub queue_conditions: Vec<Condition>,
    pub merge_conditions: Vec<Condition>,
    /// `None` when unset (Mergify then uses the repository's first allowed method).
    pub merge_method: Option<String>,
    pub batch_size: BatchSize,
    /// Seconds; default 30.
    pub batch_max_wait_time: u64,
    pub checks_timeout: ChecksTimeout,
    /// `None`: no limit (default).
    pub batch_max_failure_resolution_attempts: Option<u32>,
    pub queue_branch_prefix: String,
    pub allow_queue_branch_edit: bool,
    pub max_checks_retries: u32,
}

impl QueueRule {
    /// A queue rule with Mergify's defaults and no conditions.
    pub fn new(name: &str) -> Self {
        QueueRule {
            name: name.to_string(),
            queue_conditions: vec![],
            merge_conditions: vec![],
            merge_method: None,
            batch_size: BatchSize::Fixed(1),
            batch_max_wait_time: 30,
            checks_timeout: ChecksTimeout::Auto,
            batch_max_failure_resolution_attempts: None,
            queue_branch_prefix: "mergify/merge-queue/".into(),
            allow_queue_branch_edit: false,
            max_checks_retries: 0,
        }
    }

    /// Two-step CI: `merge_conditions` set and different from `queue_conditions`.
    pub fn two_step(&self) -> bool {
        !self.merge_conditions.is_empty() && self.merge_conditions != self.queue_conditions
    }
}

/// Top-level `merge_queue` settings.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MergeQueueSettings {
    /// Default 5.
    pub max_parallel_checks: u32,
    /// `serial` (default), `parallel` or `isolated`.
    pub mode: String,
    /// `always` (default) or `never`.
    pub reset_on_external_merge: String,
    pub skip_intermediate_results: bool,
}

impl Default for MergeQueueSettings {
    fn default() -> Self {
        MergeQueueSettings {
            max_parallel_checks: 5,
            mode: "serial".into(),
            reset_on_external_merge: "always".into(),
            skip_intermediate_results: false,
        }
    }
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
    pub merge_queue: MergeQueueSettings,
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
            let mut rule = QueueRule::new(&name(q, &p)?);
            rule.queue_conditions = conditions::parse_list(
                q.get("queue_conditions").unwrap_or(&Value::Null),
                &format!("{p}/queue_conditions"),
                &mut errors,
            );
            rule.merge_conditions = conditions::parse_list(
                q.get("merge_conditions").unwrap_or(&Value::Null),
                &format!("{p}/merge_conditions"),
                &mut errors,
            );
            queue_options(q, &p, &mut rule)?;
            rules.queue_rules.push(rule);
        }
        if let Some(mq) = value.get("merge_queue").filter(|v| !v.is_null()) {
            let s = &mut rules.merge_queue;
            if let Some(n) = uint(mq, "max_parallel_checks", "/merge_queue")? {
                s.max_parallel_checks = n.max(1);
            }
            if let Some(m) = mq.get("mode").and_then(Value::as_str) {
                s.mode = m.to_string();
            }
            if let Some(r) = mq.get("reset_on_external_merge").and_then(Value::as_str) {
                s.reset_on_external_merge = r.to_string();
            }
            if let Some(b) = mq.get("skip_intermediate_results").and_then(Value::as_bool) {
                s.skip_intermediate_results = b;
            }
        }
        for (i, r) in array(value, "pull_request_rules")?.iter().enumerate() {
            let p = format!("/pull_request_rules/{i}");
            // The schema requires `conditions`. Without them a rule must never match every
            // pull request (`serve` validates first; this is the second line).
            if r.get("conditions").is_none_or(Value::is_null) {
                return Err(RulesError::Shape {
                    pointer: format!("{p}/conditions"),
                    message: "a pull request rule needs `conditions` (use `[]` to match every pull request)".into(),
                });
            }
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

fn shape(pointer: &str, message: &str) -> RulesError {
    RulesError::Shape {
        pointer: pointer.to_string(),
        message: message.to_string(),
    }
}

fn uint(v: &Value, key: &str, pointer: &str) -> Result<Option<u32>, RulesError> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x.as_u64().map(|n| Some(n as u32)).ok_or_else(|| {
            shape(
                &format!("{pointer}/{key}"),
                "must be a non-negative integer",
            )
        }),
    }
}

fn duration(v: &Value, pointer: &str) -> Result<u64, RulesError> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .ok_or_else(|| shape(pointer, "must be a duration")),
        Value::String(s) => parse_duration(s).ok_or_else(|| shape(pointer, "must be a duration")),
        _ => Err(shape(pointer, "must be a duration")),
    }
}

/// Reads the options of one queue rule that the queue engine uses.
fn queue_options(q: &Value, p: &str, rule: &mut QueueRule) -> Result<(), RulesError> {
    if let Some(m) = q.get("merge_method").and_then(Value::as_str) {
        rule.merge_method = Some(m.to_string());
    }
    match q.get("batch_size") {
        None | Some(Value::Null) => {}
        Some(Value::Object(o)) => {
            let get = |k: &str| o.get(k).and_then(Value::as_u64).map(|n| n as u32);
            let min = get("min").unwrap_or(1).max(1);
            let max = get("max").unwrap_or(min).max(min);
            rule.batch_size = BatchSize::Range { min, max };
        }
        Some(v) => {
            let n = v.as_u64().ok_or_else(|| {
                shape(
                    &format!("{p}/batch_size"),
                    "must be an integer or {min, max}",
                )
            })?;
            rule.batch_size = BatchSize::Fixed((n as u32).max(1));
        }
    }
    if let Some(v) = q.get("batch_max_wait_time").filter(|v| !v.is_null()) {
        rule.batch_max_wait_time = duration(v, &format!("{p}/batch_max_wait_time"))?;
    }
    match q.get("checks_timeout") {
        None => {}
        Some(Value::Null) => rule.checks_timeout = ChecksTimeout::Disabled,
        Some(Value::String(s)) if s == "auto" => rule.checks_timeout = ChecksTimeout::Auto,
        Some(v) => {
            // Mergify refuses values below 60 seconds; the validator reports that, the engine
            // clamps.
            let secs = duration(v, &format!("{p}/checks_timeout"))?;
            rule.checks_timeout = ChecksTimeout::Fixed(secs.max(60));
        }
    }
    rule.batch_max_failure_resolution_attempts =
        uint(q, "batch_max_failure_resolution_attempts", p)?;
    if let Some(s) = q.get("queue_branch_prefix").and_then(Value::as_str) {
        rule.queue_branch_prefix = s.to_string();
    }
    if let Some(b) = q.get("allow_queue_branch_edit").and_then(Value::as_bool) {
        rule.allow_queue_branch_edit = b;
    }
    if let Some(n) = uint(q, "max_checks_retries", p)? {
        rule.max_checks_retries = n;
    }
    Ok(())
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
        let q = &rules.queue_rules[0];
        assert_eq!(q.batch_size, BatchSize::Fixed(5));
        assert_eq!(q.batch_max_wait_time, 300);
        assert_eq!(q.merge_method.as_deref(), Some("squash"));
        assert_eq!(q.checks_timeout, ChecksTimeout::Auto);
        assert_eq!(q.batch_max_failure_resolution_attempts, None);
        assert_eq!(q.queue_branch_prefix, "mergify/merge-queue/");
        assert!(q.two_step());
        assert_eq!(rules.merge_queue, MergeQueueSettings::default());
    }

    #[test]
    fn queue_options_and_defaults() {
        let src = "merge_queue:\n  max_parallel_checks: 2\n  reset_on_external_merge: never\n\
                   queue_rules:\n  - name: a\n    batch_size: {min: 2, max: 4}\n    checks_timeout: 30 min\n    batch_max_failure_resolution_attempts: 0\n    queue_branch_prefix: mq-\n  - name: b\n    checks_timeout: null\n    batch_max_wait_time: 90\n  - name: c\n    checks_timeout: 10 s\n";
        let r = Rules::from_yaml(src).unwrap();
        assert_eq!(r.merge_queue.max_parallel_checks, 2);
        assert_eq!(r.merge_queue.reset_on_external_merge, "never");
        let [a, b, c] = &r.queue_rules[..] else {
            panic!("three rules")
        };
        assert_eq!(a.batch_size, BatchSize::Range { min: 2, max: 4 });
        assert_eq!(a.checks_timeout, ChecksTimeout::Fixed(1800));
        assert_eq!(a.batch_max_failure_resolution_attempts, Some(0));
        assert_eq!(a.queue_branch_prefix, "mq-");
        assert_eq!(b.checks_timeout, ChecksTimeout::Disabled);
        assert_eq!(b.batch_max_wait_time, 90);
        assert_eq!(b.batch_size, BatchSize::Fixed(1));
        assert_eq!(
            c.checks_timeout,
            ChecksTimeout::Fixed(60),
            "clamped to 60 s"
        );
    }
}
