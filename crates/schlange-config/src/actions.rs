//! Typed view of the actions of `pull_request_rules` (docs: workflow/actions and the schema's
//! `ActionsModel`), with `defaults.actions` filled in (docs: configuration/file-format,
//! "defaults": the rule's own option wins, then `defaults`, then the schema's default).
//!
//! `queue` stays in [`crate::rules::PullRequestRule::queue`]; everything else is a
//! [`RuleAction`]. Default texts (close message, copy titles, the `post_check` title) are
//! Schlange's own wording, not Mergify's.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::conditions::{self, Condition, ParseError};

/// How a rule action reacts to a rule that keeps matching (docs: workflow, "How rules are
/// evaluated").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Trigger {
    /// Runs once when the rule starts to match, again only after it stopped matching.
    Edge,
    /// Runs on every evaluation and holds the pull request in the state it describes.
    Level,
}

/// Who `dismiss_reviews` dismisses for one review state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dismiss {
    None,
    All,
    Users(Vec<String>),
    FromRequestedReviewers,
}

/// Whether a copy waits for the merge (`backport`) or runs at once (`copy`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyKind {
    Backport,
    Copy,
}

impl CopyKind {
    pub fn as_str(self) -> &'static str {
        match self {
            CopyKind::Backport => "backport",
            CopyKind::Copy => "copy",
        }
    }

    /// The head branch of the copy on `destination` (docs: `mergify/bp/<branch>/pr-<n>` and
    /// `mergify/copy/<branch>/pr-<n>`).
    pub fn head_branch(self, destination: &str, number: u64) -> String {
        match self {
            CopyKind::Backport => format!("mergify/bp/{destination}/pr-{number}"),
            CopyKind::Copy => format!("mergify/copy/{destination}/pr-{number}"),
        }
    }
}

/// Options of `backport` and `copy`.
#[derive(Clone, Debug, PartialEq)]
pub struct CopySpec {
    pub kind: CopyKind,
    pub branches: Vec<String>,
    pub regexes: Vec<String>,
    pub ignore_conflicts: bool,
    pub assignees: Vec<String>,
    pub labels: Vec<String>,
    pub label_conflicts: String,
    pub title: String,
    pub body: String,
    /// `check` and/or `comment`.
    pub report_check: bool,
    pub report_comment: bool,
}

impl CopySpec {
    pub fn new(kind: CopyKind) -> Self {
        CopySpec {
            kind,
            branches: vec![],
            regexes: vec![],
            ignore_conflicts: true,
            assignees: vec![],
            labels: vec![],
            label_conflicts: "conflicts".into(),
            title: match kind {
                CopyKind::Backport => "{{ title }} (backport of #{{ number }})".into(),
                CopyKind::Copy => "{{ title }} (copy of #{{ number }})".into(),
            },
            body: match kind {
                CopyKind::Backport => "{{ body }}\n\n---\nBackport of #{{ number }} to `{{ destination_branch }}`, made by the merge queue.\n".into(),
                CopyKind::Copy => "{{ body }}\n\n---\nCopy of #{{ number }} for `{{ destination_branch }}`, made by the merge queue.\n".into(),
            },
            report_check: true,
            report_comment: false,
        }
    }
}

/// One `github_actions` workflow dispatch.
#[derive(Clone, Debug, PartialEq)]
pub struct Dispatch {
    pub workflow: String,
    pub reference: Option<String>,
    /// Input values: templates (strings), numbers and booleans as text.
    pub inputs: BTreeMap<String, Value>,
}

/// One action of a pull request rule, other than `queue`.
#[derive(Clone, Debug, PartialEq)]
pub enum RuleAction {
    Label {
        add: Vec<String>,
        remove: Vec<String>,
        remove_all: bool,
        toggle: Vec<String>,
    },
    Comment {
        message: String,
    },
    Assign {
        add: Vec<String>,
        remove: Vec<String>,
    },
    RequestReviews {
        /// Login and weight (weights only matter with `random_count`).
        users: Vec<(String, u32)>,
        teams: Vec<(String, u32)>,
        users_from_teams: Vec<(String, u32)>,
        random_count: Option<u32>,
    },
    Review {
        /// `APPROVE`, `REQUEST_CHANGES` or `COMMENT`.
        kind: String,
        message: Option<String>,
    },
    DismissReviews {
        approved: Dismiss,
        changes_requested: Dismiss,
        message: Option<String>,
        /// `when: always`; otherwise only after new commits (`synchronize`).
        always: bool,
    },
    Close {
        message: String,
    },
    Edit {
        draft: Option<bool>,
    },
    DeleteHeadBranch {
        force: bool,
    },
    PostCheck {
        title: String,
        summary: String,
        success_conditions: Option<Vec<Condition>>,
        neutral_conditions: Option<Vec<Condition>>,
    },
    Update,
    Rebase,
    Squash {
        /// `all-commits`, `first-commit` or `title+body`.
        commit_message: String,
    },
    Copy(CopySpec),
    Merge {
        method: Option<String>,
    },
    GithubActions {
        dispatch: Vec<Dispatch>,
    },
}

impl RuleAction {
    /// The action's name in the configuration.
    pub fn name(&self) -> &'static str {
        match self {
            RuleAction::Label { .. } => "label",
            RuleAction::Comment { .. } => "comment",
            RuleAction::Assign { .. } => "assign",
            RuleAction::RequestReviews { .. } => "request_reviews",
            RuleAction::Review { .. } => "review",
            RuleAction::DismissReviews { .. } => "dismiss_reviews",
            RuleAction::Close { .. } => "close",
            RuleAction::Edit { .. } => "edit",
            RuleAction::DeleteHeadBranch { .. } => "delete_head_branch",
            RuleAction::PostCheck { .. } => "post_check",
            RuleAction::Update => "update",
            RuleAction::Rebase => "rebase",
            RuleAction::Squash { .. } => "squash",
            RuleAction::Copy(c) => c.kind.as_str(),
            RuleAction::Merge { .. } => "merge",
            RuleAction::GithubActions { .. } => "github_actions",
        }
    }

    /// Docs (workflow): `assign`, `dismiss_reviews`, `edit`, `label`, `post_check`, `rebase`,
    /// `request_reviews`, `review`, `squash` and `update` run on every evaluation; the others
    /// once per match.
    pub fn trigger(&self) -> Trigger {
        match self {
            RuleAction::Assign { .. }
            | RuleAction::DismissReviews { .. }
            | RuleAction::Edit { .. }
            | RuleAction::Label { .. }
            | RuleAction::PostCheck { .. }
            | RuleAction::Rebase
            | RuleAction::RequestReviews { .. }
            | RuleAction::Review { .. }
            | RuleAction::Squash { .. }
            | RuleAction::Update => Trigger::Level,
            _ => Trigger::Edge,
        }
    }

    /// Docs (rule syntax, "Combining rules"): these run at most once per evaluation; the first
    /// matching rule in the file wins.
    pub fn exclusive(&self) -> bool {
        matches!(
            self,
            RuleAction::Close { .. }
                | RuleAction::DeleteHeadBranch { .. }
                | RuleAction::Merge { .. }
                | RuleAction::Rebase
                | RuleAction::Squash { .. }
                | RuleAction::Update
        )
    }
}

/// The order actions of one rule run in (the schema's order).
pub const ACTION_NAMES: [&str; 18] = [
    "assign",
    "backport",
    "close",
    "comment",
    "copy",
    "delete_head_branch",
    "dismiss_reviews",
    "edit",
    "github_actions",
    "label",
    "merge",
    "post_check",
    "queue",
    "rebase",
    "request_reviews",
    "review",
    "squash",
    "update",
];

/// The default `close` message (Schlange's own words).
pub const CLOSE_MESSAGE: &str = "A rule closed this pull request: {{ title }}";

/// The default `post_check` title and summary (Schlange's own; legacy template syntax).
pub const POST_CHECK_TITLE: &str = "Rule '{{ check_rule_name }}': {{ check_status }}";
pub const POST_CHECK_SUMMARY: &str = "{{ check_conditions }}";

/// Layers `defaults` under `own`: every key of `own` wins, missing keys come from `defaults`.
/// A `null` action (`comment:` without options) takes the defaults whole.
pub fn with_defaults(own: &Value, defaults: Option<&Value>) -> Value {
    match (own, defaults) {
        (Value::Object(o), Some(Value::Object(d))) => {
            let mut out = d.clone();
            for (k, v) in o {
                out.insert(k.clone(), v.clone());
            }
            Value::Object(out)
        }
        (Value::Null, Some(d)) => d.clone(),
        _ => own.clone(),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionError {
    pub pointer: String,
    pub message: String,
}

fn strings(v: &Value, key: &str) -> Vec<String> {
    match v.get(key) {
        Some(Value::Array(items)) => items
            .iter()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect(),
        Some(Value::String(s)) => vec![s.clone()],
        _ => vec![],
    }
}

/// Logins with weights: a list (weight 1) or a mapping login → weight.
fn weighted(v: &Value, key: &str) -> Vec<(String, u32)> {
    match v.get(key) {
        Some(Value::Object(m)) => m
            .iter()
            .map(|(k, w)| (k.clone(), w.as_u64().unwrap_or(1).clamp(1, 65535) as u32))
            .collect(),
        _ => strings(v, key).into_iter().map(|s| (s, 1)).collect(),
    }
}

fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(Value::as_str).map(str::to_string)
}

fn bool_or(v: &Value, key: &str, default: bool) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn dismiss(v: &Value, key: &str) -> Dismiss {
    match v.get(key) {
        None | Some(Value::Null) | Some(Value::Bool(true)) => Dismiss::All,
        Some(Value::Bool(false)) => Dismiss::None,
        Some(Value::String(s)) if s == "from_requested_reviewers" => {
            Dismiss::FromRequestedReviewers
        }
        Some(Value::Array(_)) => Dismiss::Users(strings(v, key)),
        Some(_) => Dismiss::All,
    }
}

/// Parses the actions of one rule (without `queue`), with `defaults.actions` layered under
/// them. `pointer` is the rule's `actions` pointer. Unknown action names are skipped: the
/// schema stage reports them.
pub fn parse(
    actions: &Value,
    defaults: Option<&Value>,
    pointer: &str,
    errors: &mut Vec<ActionError>,
    cond_errors: &mut Vec<ParseError>,
) -> Vec<RuleAction> {
    let Some(map) = actions.as_object() else {
        return vec![];
    };
    let mut out = Vec::new();
    for name in ACTION_NAMES {
        let Some(own) = map.get(name) else { continue };
        if name == "queue" {
            continue;
        }
        let p = format!("{pointer}/{name}");
        let v = with_defaults(own, defaults.and_then(|d| d.get(name)));
        let null = Value::Object(Default::default());
        let v = if v.is_null() { &null } else { &v };
        let action = match name {
            "label" => RuleAction::Label {
                add: strings(v, "add"),
                remove: strings(v, "remove"),
                remove_all: bool_or(v, "remove_all", false),
                toggle: strings(v, "toggle"),
            },
            "comment" => match opt_str(v, "message") {
                Some(message) => RuleAction::Comment { message },
                None => {
                    errors.push(ActionError {
                        pointer: format!("{p}/message"),
                        message: "`comment` needs a `message` (in the rule or in `defaults`)"
                            .into(),
                    });
                    continue;
                }
            },
            "assign" => {
                let mut add = strings(v, "users");
                for u in strings(v, "add_users") {
                    if !add.contains(&u) {
                        add.push(u);
                    }
                }
                RuleAction::Assign {
                    add,
                    remove: strings(v, "remove_users"),
                }
            }
            "request_reviews" => RuleAction::RequestReviews {
                users: weighted(v, "users"),
                teams: weighted(v, "teams"),
                users_from_teams: weighted(v, "users_from_teams"),
                random_count: v
                    .get("random_count")
                    .and_then(Value::as_u64)
                    .map(|n| n as u32),
            },
            "review" => RuleAction::Review {
                kind: opt_str(v, "type").unwrap_or_else(|| "APPROVE".into()),
                message: opt_str(v, "message"),
            },
            "dismiss_reviews" => RuleAction::DismissReviews {
                approved: dismiss(v, "approved"),
                changes_requested: dismiss(v, "changes_requested"),
                message: opt_str(v, "message"),
                always: opt_str(v, "when").as_deref() == Some("always"),
            },
            "close" => RuleAction::Close {
                message: opt_str(v, "message").unwrap_or_else(|| CLOSE_MESSAGE.into()),
            },
            "edit" => RuleAction::Edit {
                draft: v.get("draft").and_then(Value::as_bool),
            },
            "delete_head_branch" => RuleAction::DeleteHeadBranch {
                force: bool_or(v, "force", false),
            },
            "post_check" => {
                let conds = |key: &str, errors: &mut Vec<ParseError>| {
                    v.get(key)
                        .filter(|c| !c.is_null())
                        .map(|c| conditions::parse_list(c, &format!("{p}/{key}"), errors))
                };
                RuleAction::PostCheck {
                    title: opt_str(v, "title").unwrap_or_else(|| POST_CHECK_TITLE.into()),
                    summary: opt_str(v, "summary").unwrap_or_else(|| POST_CHECK_SUMMARY.into()),
                    success_conditions: conds("success_conditions", cond_errors),
                    neutral_conditions: conds("neutral_conditions", cond_errors),
                }
            }
            "update" => RuleAction::Update,
            "rebase" => RuleAction::Rebase,
            "squash" => RuleAction::Squash {
                commit_message: opt_str(v, "commit_message")
                    .unwrap_or_else(|| "all-commits".into()),
            },
            "backport" | "copy" => {
                let kind = if name == "backport" {
                    CopyKind::Backport
                } else {
                    CopyKind::Copy
                };
                let mut c = CopySpec::new(kind);
                c.branches = strings(v, "branches");
                c.regexes = strings(v, "regexes");
                c.ignore_conflicts = bool_or(v, "ignore_conflicts", true);
                c.assignees = strings(v, "assignees");
                c.labels = strings(v, "labels");
                if let Some(l) = opt_str(v, "label_conflicts") {
                    c.label_conflicts = l;
                }
                if let Some(t) = opt_str(v, "title") {
                    c.title = t;
                }
                if let Some(b) = opt_str(v, "body") {
                    c.body = b;
                }
                let modes = strings(v, "report_mode");
                if !modes.is_empty() {
                    c.report_check = modes.iter().any(|m| m == "check");
                    c.report_comment = modes.iter().any(|m| m == "comment");
                }
                for (i, r) in c.regexes.iter().enumerate() {
                    if fancy_regex::Regex::new(r).is_err() {
                        errors.push(ActionError {
                            pointer: format!("{p}/regexes/{i}"),
                            message: format!("`{r}` is not a valid regular expression"),
                        });
                    }
                }
                RuleAction::Copy(c)
            }
            "merge" => RuleAction::Merge {
                method: opt_str(v, "method"),
            },
            "github_actions" => {
                let dispatch = v
                    .get("workflow")
                    .and_then(|w| w.get("dispatch"))
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(|d| {
                                Some(Dispatch {
                                    workflow: match d.get("workflow")? {
                                        Value::String(s) => s.clone(),
                                        Value::Number(n) => n.to_string(),
                                        _ => return None,
                                    },
                                    reference: opt_str(d, "ref"),
                                    inputs: d
                                        .get("inputs")
                                        .and_then(Value::as_object)
                                        .map(|m| {
                                            m.iter().map(|(k, v)| (k.clone(), v.clone())).collect()
                                        })
                                        .unwrap_or_default(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                RuleAction::GithubActions { dispatch }
            }
            _ => continue,
        };
        out.push(action);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(actions: Value, defaults: Option<Value>) -> Vec<RuleAction> {
        let mut e = Vec::new();
        let mut c = Vec::new();
        let out = parse(&actions, defaults.as_ref(), "/a", &mut e, &mut c);
        assert!(e.is_empty() && c.is_empty(), "{e:?} {c:?}");
        out
    }

    #[test]
    fn rule_options_win_over_defaults() {
        let a = one(
            json!({ "comment": { "message": "own" }, "close": null }),
            Some(json!({ "comment": { "message": "default" }, "close": { "message": "bye" } })),
        );
        assert_eq!(
            a,
            vec![
                RuleAction::Close {
                    message: "bye".into()
                },
                RuleAction::Comment {
                    message: "own".into()
                }
            ]
        );
        let a = one(
            json!({ "comment": null }),
            Some(json!({ "comment": { "message": "from defaults" } })),
        );
        assert_eq!(
            a,
            vec![RuleAction::Comment {
                message: "from defaults".into()
            }]
        );
    }

    #[test]
    fn comment_without_a_message_is_an_error() {
        let mut e = Vec::new();
        let mut c = Vec::new();
        let out = parse(&json!({ "comment": null }), None, "/a", &mut e, &mut c);
        assert!(out.is_empty());
        assert_eq!(e[0].pointer, "/a/comment/message");
    }

    #[test]
    fn triggers_and_exclusivity_follow_the_docs() {
        let a = one(
            json!({ "label": { "toggle": ["x"] }, "comment": { "message": "m" }, "update": null,
                    "backport": { "branches": ["stable"], "report_mode": ["comment"] } }),
            None,
        );
        let by = |n: &str| a.iter().find(|x| x.name() == n).unwrap();
        assert_eq!(by("label").trigger(), Trigger::Level);
        assert_eq!(by("comment").trigger(), Trigger::Edge);
        assert_eq!(by("update").trigger(), Trigger::Level);
        assert!(by("update").exclusive());
        assert!(!by("label").exclusive());
        let RuleAction::Copy(c) = by("backport") else {
            panic!()
        };
        assert_eq!(c.branches, vec!["stable"]);
        assert!(c.report_comment && !c.report_check);
        assert_eq!(c.kind.head_branch("stable", 7), "mergify/bp/stable/pr-7");
    }

    #[test]
    fn assign_merges_the_legacy_users_key_and_reviews_take_weights() {
        let a = one(
            json!({ "assign": { "users": ["a"], "add_users": ["a", "b"], "remove_users": ["c"] },
                    "request_reviews": { "users": { "x": 3, "y": 1 }, "random_count": 1 } }),
            None,
        );
        assert_eq!(
            a[0],
            RuleAction::Assign {
                add: vec!["a".into(), "b".into()],
                remove: vec!["c".into()]
            }
        );
        assert!(
            matches!(&a[1], RuleAction::RequestReviews { users, random_count: Some(1), .. }
            if users == &vec![("x".to_string(), 3), ("y".to_string(), 1)])
        );
    }
}
