//! Evaluates conditions against a pull request.
//!
//! Semantics (docs: configuration/conditions): for list attributes an operator holds when any
//! element matches, except `!=`, which holds when no element matches. `check-success` means the
//! conclusion `success` only; `check-failure` also covers cancelled, timed out and
//! action required. The newest run per check name on the head commit counts.

use schlange_config::conditions::{Atom, Condition, Operator};

use crate::model::{Conclusion, PullRequest};

#[derive(Clone, Debug, PartialEq)]
pub enum AttrValue {
    Bool(bool),
    Str(String),
    List(Vec<String>),
    /// A timestamp in Unix seconds; `None` when unset (then every comparison is false).
    Time(Option<i64>),
    /// A rank on [`PERMISSIONS`].
    Permission(usize),
}

/// GitHub's repository permissions, lowest first (`sender-permission`).
pub const PERMISSIONS: [&str; 6] = ["none", "read", "triage", "write", "maintain", "admin"];

/// The rank of a permission name; `None` for an unknown name.
pub fn permission_rank(p: &str) -> Option<usize> {
    PERMISSIONS
        .iter()
        .position(|x| x.eq_ignore_ascii_case(p.trim()))
}

/// Where a pull request stands in its queue (queue attributes).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct QueueAttrs {
    pub name: String,
    /// 0 for the first pull request of the queue.
    pub position: i64,
    /// Unix seconds.
    pub queued_at: i64,
    /// Unix seconds; `None` until its checks started.
    pub merge_started_at: Option<i64>,
}

/// Context that is not part of the pull request itself.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Login of the commenter, for command restrictions.
    pub sender: Option<String>,
    /// The commenter's repository permission (`sender-permission`); `None` counts as `none`.
    pub sender_permission: Option<String>,
    /// Unix seconds now, for relative timestamps (`… ago`).
    pub now: i64,
    /// `owner/name` of the repository, for `depends-on`.
    pub repository: String,
    /// Set while the pull request is queued.
    pub queue: Option<QueueAttrs>,
    /// The code of the last time its queue checks ended (`queue-dequeue-reason`).
    pub dequeue_reason: Option<String>,
}

/// Replaces the template `{{author}}` (also `{{ author }}`) in condition values with the pull
/// request's author (`commands_restrictions`).
pub fn with_author(list: &[Condition], author: &str) -> Vec<Condition> {
    fn render(c: &Condition, author: &str) -> Condition {
        match c {
            Condition::Atom(a) => {
                let mut a = a.clone();
                if let Some(v) = &a.value {
                    let compact: String = v.chars().filter(|c| !c.is_whitespace()).collect();
                    if compact.contains("{{author}}") {
                        a.value = Some(compact.replace("{{author}}", author));
                    }
                }
                Condition::Atom(a)
            }
            Condition::And(cs) => Condition::And(cs.iter().map(|c| render(c, author)).collect()),
            Condition::Or(cs) => Condition::Or(cs.iter().map(|c| render(c, author)).collect()),
            Condition::Not(c) => Condition::Not(Box::new(render(c, author))),
            Condition::RequiredCheck { .. } => c.clone(),
            Condition::Always => Condition::Always,
        }
    }
    list.iter().map(|c| render(c, author)).collect()
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EvalError {
    #[error("attribute `{0}` is not supported by the evaluator yet")]
    UnsupportedAttribute(String),
    #[error("operator `{op}` cannot be applied to `{attribute}`")]
    BadOperator { attribute: String, op: &'static str },
    #[error("regex `{0}` does not compile")]
    BadRegex(String),
    #[error("`{0}` needs a number")]
    NotANumber(String),
    #[error("`{attribute}` cannot be compared with `{value}`")]
    BadValue { attribute: String, value: String },
    #[error("regex `{pattern}` failed while matching: {message}")]
    RegexRuntime { pattern: String, message: String },
    #[error("glob `{0}` is invalid: {1}")]
    BadGlob(String, String),
    /// The forge listed only part of the changed files: a condition on them cannot be judged.
    #[error("`{0}` cannot be judged: the forge listed only part of the changed files")]
    IncompleteFiles(String),
    #[error("`{0}` cannot be judged: check suite or workflow identity is incomplete")]
    IncompleteChecks(String),
}

/// The conclusions a `check-*` attribute selects; `None` for other attributes.
fn check_predicate(attribute: &str) -> Option<fn(Option<Conclusion>) -> bool> {
    use Conclusion::*;
    Some(match attribute {
        "check" => |_| true,
        "check-success" => |c| c == Some(Success),
        "check-failure" => |c| matches!(c, Some(Failure | Cancelled | TimedOut | ActionRequired)),
        "check-neutral" => |c| c == Some(Neutral),
        "check-skipped" => |c| c == Some(Skipped),
        "check-cancelled" => |c| c == Some(Cancelled),
        "check-timed-out" => |c| c == Some(TimedOut),
        "check-stale" => |c| c == Some(Stale),
        "check-pending" => |c| c.is_none(),
        _ => return None,
    })
}

/// Names of the current runs whose conclusion `pred` selects; with `app`, only runs that app
/// published (`@<app>/<name>`).
fn checks_where(
    pr: &PullRequest,
    app: Option<&str>,
    pred: impl Fn(Option<Conclusion>) -> bool,
) -> AttrValue {
    AttrValue::List(
        pr.current_checks()
            .into_iter()
            .filter(|c| app.is_none_or(|a| c.app.as_deref() == Some(a)))
            .filter(|c| pred(c.conclusion))
            .map(|c| c.name.clone())
            .collect(),
    )
}

/// Whether a check reference from a condition (`name` or `@<app>/<name>`) names this run.
pub fn check_matches(reference: &str, run: &crate::model::CheckRun) -> bool {
    match schlange_config::conditions::qualified(reference) {
        Some((app, name)) => run.name == name && run.app.as_deref() == Some(app),
        None => run.name == reference,
    }
}

/// Value of an attribute for a pull request.
pub fn attribute(pr: &PullRequest, ctx: &Context, name: &str) -> Result<AttrValue, EvalError> {
    if check_predicate(name).is_some() && pr.checks_incomplete() {
        return Err(EvalError::IncompleteChecks(name.to_string()));
    }
    Ok(match name {
        "number" => AttrValue::Str(pr.number.to_string()),
        "author" => AttrValue::Str(pr.author.clone()),
        "base" => AttrValue::Str(pr.base.clone()),
        "head" => AttrValue::Str(pr.head_ref.clone()),
        "title" => AttrValue::Str(pr.title.clone()),
        "draft" => AttrValue::Bool(pr.draft),
        "closed" => AttrValue::Bool(pr.closed),
        "merged" => AttrValue::Bool(pr.merged),
        "label" => AttrValue::List(pr.labels.iter().cloned().collect()),
        "files" | "mergify-configuration-changed" if pr.files_incomplete => {
            return Err(EvalError::IncompleteFiles(name.to_string()));
        }
        "files" => AttrValue::List(pr.files.clone()),
        "dependabot-update-type" => AttrValue::List(pr.dependabot_update_types.clone()),
        "sender" => match &ctx.sender {
            Some(s) => AttrValue::Str(s.clone()),
            None => AttrValue::Str(String::new()),
        },
        "sender-permission" => AttrValue::Permission(
            ctx.sender_permission
                .as_deref()
                .and_then(permission_rank)
                .unwrap_or(0),
        ),
        "from-fork" => AttrValue::Bool(pr.from_fork),
        "depends-on" => AttrValue::List(crate::protections::depends_on_attribute(
            pr,
            &ctx.repository,
        )),
        "mergify-configuration-changed" => AttrValue::Bool(
            pr.files
                .iter()
                .any(|f| schlange_config::config_file::is_config_path(f)),
        ),
        "queue-position" => {
            AttrValue::Str(ctx.queue.as_ref().map_or(-1, |q| q.position).to_string())
        }
        "queue-name" => AttrValue::Str(
            ctx.queue
                .as_ref()
                .map(|q| q.name.clone())
                .unwrap_or_default(),
        ),
        "queued-at" => AttrValue::Time(ctx.queue.as_ref().map(|q| q.queued_at)),
        "queue-merge-started-at" => {
            AttrValue::Time(ctx.queue.as_ref().and_then(|q| q.merge_started_at))
        }
        "queue-dequeue-reason" => {
            AttrValue::Str(ctx.dequeue_reason.clone().unwrap_or_else(|| "NONE".into()))
        }
        other => match check_predicate(other) {
            Some(pred) => checks_where(pr, None, pred),
            None => return Err(EvalError::UnsupportedAttribute(other.to_string())),
        },
    })
}

thread_local! {
    /// Compiled patterns. The engine re-evaluates every pull request after every event, and
    /// compiling a lookahead pattern each time dominates the run time. A pure cache: same
    /// pattern, same regex.
    static REGEXES: std::cell::RefCell<std::collections::HashMap<String, Option<fancy_regex::Regex>>> =
        Default::default();
}

fn regex_match(pattern: &str, shown: &str, actual: &str) -> Result<bool, EvalError> {
    REGEXES.with(|cache| {
        let mut cache = cache.borrow_mut();
        let re = cache
            .entry(pattern.to_string())
            .or_insert_with(|| fancy_regex::Regex::new(pattern).ok());
        match re {
            // A pattern that fails while matching (backtracking limit) is an error, not "no
            // match": the condition then fails closed.
            Some(re) => re.is_match(actual).map_err(|e| EvalError::RegexRuntime {
                pattern: shown.to_string(),
                message: e.to_string(),
            }),
            None => Err(EvalError::BadRegex(shown.to_string())),
        }
    })
}

fn compare_scalar(
    attribute: &str,
    op: Operator,
    actual: &str,
    expected: &str,
) -> Result<bool, EvalError> {
    Ok(match op {
        Operator::Eq => actual == expected,
        Operator::Ne => actual != expected,
        Operator::Regex => regex_match(expected, expected, actual)?,
        Operator::Glob => {
            let re = schlange_config::glob::glob_to_regex(expected)
                .map_err(|e| EvalError::BadGlob(expected.to_string(), e))?;
            regex_match(&re, expected, actual)?
        }
        Operator::Ge | Operator::Gt | Operator::Le | Operator::Lt => {
            let ordering = match (actual.parse::<f64>(), expected.parse::<f64>()) {
                (Ok(a), Ok(e)) => a.partial_cmp(&e),
                _ => Some(actual.cmp(expected)),
            };
            let Some(o) = ordering else {
                return Err(EvalError::NotANumber(attribute.to_string()));
            };
            match op {
                Operator::Ge => o.is_ge(),
                Operator::Gt => o.is_gt(),
                Operator::Le => o.is_le(),
                _ => o.is_lt(),
            }
        }
    })
}

/// Ordering of `actual` against `expected` under `op`.
fn ordered(op: Operator, o: std::cmp::Ordering) -> Option<bool> {
    Some(match op {
        Operator::Eq => o.is_eq(),
        Operator::Ne => o.is_ne(),
        Operator::Ge => o.is_ge(),
        Operator::Gt => o.is_gt(),
        Operator::Le => o.is_le(),
        Operator::Lt => o.is_lt(),
        Operator::Regex | Operator::Glob => return None,
    })
}

/// Whether a condition value is the template `{{ author }}` (spaces inside the braces allowed).
fn is_author_template(v: &str) -> bool {
    let compact: String = v.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "{{author}}"
}

/// Evaluates one atom.
pub fn eval_atom(atom: &Atom, pr: &PullRequest, ctx: &Context) -> Result<bool, EvalError> {
    // `{{ author }}` as a value stands for the pull request's author (docs: conditions,
    // "Matching the Pull Request Author"); numbers, times and permissions reject it below as
    // an invalid value.
    let rendered;
    let atom = match &atom.value {
        Some(v) if is_author_template(v) => {
            rendered = Atom {
                value: Some(pr.author.clone()),
                ..atom.clone()
            };
            &rendered
        }
        _ => atom,
    };
    // `@<app>/<name>` on a `check-*` attribute: only that app's runs, matched by name.
    let qualified = match (&atom.value, check_predicate(&atom.attribute)) {
        (Some(v), Some(pred)) => schlange_config::conditions::qualified(v)
            .map(|(app, name)| (checks_where(pr, Some(app), pred), name.to_string())),
        _ => None,
    };
    let unqualified;
    let (value, atom) = match qualified {
        Some((value, name)) => {
            unqualified = Atom {
                value: Some(name),
                ..atom.clone()
            };
            (value, &unqualified)
        }
        None => (attribute(pr, ctx, &atom.attribute)?, atom),
    };
    let bad_op = |op: Operator| EvalError::BadOperator {
        attribute: atom.attribute.clone(),
        op: op.as_str(),
    };
    let bad_value = |v: &str| EvalError::BadValue {
        attribute: atom.attribute.clone(),
        value: v.to_string(),
    };
    match (&value, atom.operator, &atom.value) {
        (AttrValue::Permission(rank), Some(op), Some(v)) if !atom.length => {
            let expected = permission_rank(v).ok_or_else(|| bad_value(v))?;
            let r = ordered(op, rank.cmp(&expected)).ok_or_else(|| bad_op(op))?;
            return Ok(r != atom.negate);
        }
        (AttrValue::Time(actual), Some(op), Some(v)) if !atom.length => {
            if matches!(
                op,
                Operator::Eq | Operator::Ne | Operator::Regex | Operator::Glob
            ) {
                return Err(bad_op(op));
            }
            let expected =
                crate::time::parse_condition_time(v, ctx.now).ok_or_else(|| bad_value(v))?;
            let r = match actual {
                Some(a) => ordered(op, a.cmp(&expected)).unwrap_or(false),
                None => false,
            };
            return Ok(r != atom.negate);
        }
        (AttrValue::Str(code), Some(op @ (Operator::Eq | Operator::Ne)), Some(v))
            if atom.attribute == "queue-dequeue-reason" && !atom.length =>
        {
            // The documentation writes the codes in kebab case, the schema in upper case.
            let expected = v.trim().to_ascii_uppercase().replace('-', "_");
            let r = (code == &expected) == (op == Operator::Eq);
            return Ok(r != atom.negate);
        }
        (AttrValue::Time(t), None, _) => return Ok(t.is_some() != atom.negate),
        (AttrValue::Permission(r), None, _) => return Ok((*r > 0) != atom.negate),
        (AttrValue::Time(_) | AttrValue::Permission(_), ..) => {
            return Err(EvalError::BadOperator {
                attribute: atom.attribute.clone(),
                op: "#",
            });
        }
        _ => {}
    }
    let result = if atom.length {
        let len = match &value {
            AttrValue::List(v) => v.len(),
            AttrValue::Str(s) => s.chars().count(),
            AttrValue::Bool(_) | AttrValue::Time(_) | AttrValue::Permission(_) => {
                return Err(EvalError::BadOperator {
                    attribute: atom.attribute.clone(),
                    op: "#",
                });
            }
        };
        match (atom.operator, &atom.value) {
            (Some(op), Some(v)) => {
                let expected: f64 = v
                    .parse()
                    .map_err(|_| EvalError::NotANumber(atom.attribute.clone()))?;
                compare_scalar(&atom.attribute, op, &len.to_string(), &expected.to_string())?
            }
            _ => len > 0,
        }
    } else {
        match (&value, atom.operator, &atom.value) {
            (AttrValue::Bool(b), None, _) => *b,
            (AttrValue::List(items), None, _) => !items.is_empty(),
            (AttrValue::Str(s), None, _) => !s.is_empty(),
            (AttrValue::Bool(_), Some(op), _) => {
                return Err(EvalError::BadOperator {
                    attribute: atom.attribute.clone(),
                    op: op.as_str(),
                });
            }
            (AttrValue::Str(s), Some(op), Some(v)) => compare_scalar(&atom.attribute, op, s, v)?,
            (AttrValue::List(items), Some(Operator::Ne), Some(v)) => !items.iter().any(|i| i == v),
            (AttrValue::List(items), Some(op), Some(v)) => {
                let mut any = false;
                for i in items {
                    if compare_scalar(&atom.attribute, op, i, v)? {
                        any = true;
                        break;
                    }
                }
                any
            }
            (_, Some(_), None) => false,
            (AttrValue::Time(_) | AttrValue::Permission(_), ..) => false,
        }
    };
    Ok(result != atom.negate)
}

/// Evaluates a condition tree. An atom that cannot be evaluated is unknown; `and`, `or` and
/// `not` follow three-valued logic (a result is known only when it does not depend on the
/// unknown part), and an unknown result counts as false. So an error fails closed, also under
/// `not`. The errors are returned alongside.
pub fn eval(
    cond: &Condition,
    pr: &PullRequest,
    ctx: &Context,
    errors: &mut Vec<EvalError>,
) -> bool {
    eval3(cond, pr, ctx, errors).unwrap_or(false)
}

/// `Some(result)`, or `None` when the result depends on an atom that could not be evaluated.
fn eval3(
    cond: &Condition,
    pr: &PullRequest,
    ctx: &Context,
    errors: &mut Vec<EvalError>,
) -> Option<bool> {
    match cond {
        Condition::RequiredCheck { context, app_id } => {
            if pr.checks_incomplete()
                || pr.checks.iter().any(|c| {
                    c.head_sha == pr.head_sha
                        && c.name == *context
                        && (c.status || c.app_id.is_none())
                })
            {
                errors.push(EvalError::IncompleteChecks(context.clone()));
                None
            } else {
                let runs: Vec<_> = pr
                    .current_checks()
                    .into_iter()
                    .filter(|c| !c.status && c.name == *context && c.app_id == Some(*app_id))
                    .collect();
                let slugs: std::collections::BTreeSet<_> = runs.iter().map(|c| &c.app).collect();
                Some(
                    !runs.is_empty()
                        && slugs.len() == 1
                        && runs
                            .iter()
                            .all(|c| c.conclusion == Some(Conclusion::Success)),
                )
            }
        }
        Condition::Atom(a) => match eval_atom(a, pr, ctx) {
            Ok(r) => Some(r),
            Err(e) => {
                errors.push(e);
                None
            }
        },
        Condition::And(cs) => {
            let rs: Vec<Option<bool>> = cs.iter().map(|c| eval3(c, pr, ctx, errors)).collect();
            if rs.contains(&Some(false)) {
                Some(false)
            } else if rs.contains(&None) {
                None
            } else {
                Some(true)
            }
        }
        Condition::Or(cs) => {
            let rs: Vec<Option<bool>> = cs.iter().map(|c| eval3(c, pr, ctx, errors)).collect();
            if rs.contains(&Some(true)) {
                Some(true)
            } else if rs.contains(&None) {
                None
            } else {
                Some(false)
            }
        }
        Condition::Not(c) => eval3(c, pr, ctx, errors).map(|r| !r),
        Condition::Always => Some(true),
    }
}

/// Check names a condition list refers to with `check-*` attributes and `=`.
pub fn referenced_checks(list: &[Condition]) -> Vec<String> {
    fn walk(c: &Condition, out: &mut Vec<String>) {
        match c {
            Condition::Atom(a) => {
                if a.attribute.starts_with("check")
                    && let (Some(Operator::Eq), Some(v)) = (a.operator, &a.value)
                    && !out.contains(v)
                {
                    out.push(v.clone());
                }
            }
            Condition::And(cs) | Condition::Or(cs) => cs.iter().for_each(|c| walk(c, out)),
            Condition::Not(c) => walk(c, out),
            Condition::RequiredCheck { context, .. } => {
                if !out.contains(context) {
                    out.push(context.clone());
                }
            }
            Condition::Always => {}
        }
    }
    let mut out = Vec::new();
    list.iter().for_each(|c| walk(c, &mut out));
    out
}

/// Evaluates a condition list (implicit `and`) and reports each condition's result.
pub fn eval_list(
    list: &[Condition],
    pr: &PullRequest,
    ctx: &Context,
    errors: &mut Vec<EvalError>,
) -> (bool, Vec<bool>) {
    let results: Vec<bool> = list.iter().map(|c| eval(c, pr, ctx, errors)).collect();
    (results.iter().all(|r| *r), results)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CheckRun;
    use schlange_config::conditions::parse_atom;

    fn pr() -> PullRequest {
        let mut p = PullRequest::new(7, "arztagent-bot", "main", "feature", "h2");
        p.title = "t".into();
        p.labels = ["ready".to_string()].into();
        p.files = vec!["src/main.rs".into(), ".github/workflows/ci.yml".into()];
        p
    }

    fn run(name: &str, sha: &str, c: Option<Conclusion>, seq: u64) -> CheckRun {
        CheckRun {
            name: name.into(),
            app: None,
            head_sha: sha.into(),
            conclusion: c,
            seq,
            ..Default::default()
        }
    }

    fn holds(cond: &str, pr: &PullRequest) -> bool {
        eval_atom(&parse_atom(cond).unwrap(), pr, &Context::default()).unwrap()
    }

    #[test]
    fn scalars_and_lists() {
        let p = pr();
        assert!(holds("base = main", &p));
        assert!(!holds("base != main", &p));
        assert!(holds("-draft", &p));
        assert!(holds("label = ready", &p));
        assert!(holds("-label = halt", &p));
        assert!(holds("label != halt", &p));
        assert!(!holds("label != ready", &p));
        assert!(holds("#files = 2", &p));
        assert!(holds("#files >= 1", &p));
        assert!(holds("files ~= ^\\.github/", &p));
        assert!(holds("files *= src/*.rs", &p));
        // Docs (data types, globs): `**` is special only as a whole segment; inside one it is
        // two `*`, which stay in their segment.
        assert!(!holds("files *= **.rs", &p));
        assert!(holds("files *= **/*.rs", &p));
        assert!(holds("author ~= ^arztagent", &p));
    }

    #[test]
    fn lookahead_regex_as_in_arztagent() {
        // Negated: no file outside the exceptions matches the constitution pattern.
        let re = "-files ~= ^(?!(?:\\.agents/skills/verify\\-arztagent/features/.*)$)(?:\\.github/.*|\\.agents/skills/.*)$";
        let mut p = pr();
        p.files = vec![
            "src/main.rs".into(),
            ".agents/skills/verify-arztagent/features/x.md".into(),
        ];
        assert!(holds(re, &p), "exception path is allowed");
        p.files.push(".github/workflows/ci.yml".into());
        assert!(!holds(re, &p), "workflow change is constitution");
    }

    /// Critic e8a0022 M1: GitHub lists at most 3000 files. On a truncated list a `files`
    /// condition fails closed, negated or not: a hidden constitution path cannot pass the gate.
    #[test]
    fn a_truncated_file_list_fails_closed() {
        let re = "-files ~= ^\\.github/";
        let mut p = pr();
        p.files = vec!["src/main.rs".into()];
        assert!(holds(re, &p));
        p.files_incomplete = true;
        for c in [
            re,
            "files ~= ^src/",
            "#files >= 1",
            "-mergify-configuration-changed",
        ] {
            let cond = Condition::Atom(parse_atom(c).unwrap());
            let mut errors = vec![];
            assert!(!eval(&cond, &p, &Context::default(), &mut errors), "{c}");
            assert!(
                matches!(errors.first(), Some(EvalError::IncompleteFiles(_))),
                "{c}: {errors:?}"
            );
        }
        assert!(holds("base = main", &p), "other attributes still evaluate");
    }

    #[test]
    fn required_check_app_binding_does_not_accept_another_apps_success() {
        let mut p = pr();
        let cond = Condition::RequiredCheck {
            context: "ci".into(),
            app_id: 42,
        };
        let head = p.head_sha.clone();
        let check = |app_id, conclusion| crate::model::CheckRun {
            name: "ci".into(),
            app: Some(format!("app-{app_id}")),
            app_id: Some(app_id),
            head_sha: head.clone(),
            conclusion: Some(conclusion),
            ..Default::default()
        };
        p.checks = vec![
            check(42, Conclusion::Failure),
            check(43, Conclusion::Success),
        ];
        assert!(!eval(&cond, &p, &Context::default(), &mut vec![]));
        p.checks = vec![
            check(42, Conclusion::Success),
            check(43, Conclusion::Failure),
        ];
        assert!(eval(&cond, &p, &Context::default(), &mut vec![]));
        p.checks = vec![check(43, Conclusion::Success)];
        assert!(!eval(&cond, &p, &Context::default(), &mut vec![]));
    }

    #[test]
    fn app_bound_green_check_cannot_hide_a_red_same_name_status() {
        let mut p = pr();
        let cond = Condition::RequiredCheck {
            context: "ci".into(),
            app_id: 42,
        };
        p.checks = vec![
            crate::model::CheckRun {
                name: "ci".into(),
                app: Some("app".into()),
                app_id: Some(42),
                head_sha: p.head_sha.clone(),
                conclusion: Some(Conclusion::Success),
                ..Default::default()
            },
            crate::model::CheckRun {
                name: "ci".into(),
                status: true,
                head_sha: p.head_sha.clone(),
                conclusion: Some(Conclusion::Failure),
                ..Default::default()
            },
        ];
        assert!(!eval(&cond, &p, &Context::default(), &mut vec![]));
        p.checks[1].app_id = Some(42);
        assert!(!eval(&cond, &p, &Context::default(), &mut vec![]));
    }

    #[test]
    fn bound_identity_cannot_be_split_by_inconsistent_slug() {
        let mut p = pr();
        let cond = Condition::RequiredCheck {
            context: "ci".into(),
            app_id: 42,
        };
        p.checks = vec![
            crate::model::CheckRun {
                name: "ci".into(),
                app: Some("a".into()),
                app_id: Some(42),
                head_sha: p.head_sha.clone(),
                conclusion: Some(Conclusion::Failure),
                ..Default::default()
            },
            crate::model::CheckRun {
                name: "ci".into(),
                app: Some("b".into()),
                app_id: Some(42),
                head_sha: p.head_sha.clone(),
                conclusion: Some(Conclusion::Success),
                ..Default::default()
            },
        ];
        assert!(!eval(&cond, &p, &Context::default(), &mut vec![]));
    }

    /// Table test: every conclusion against every check attribute (MQ-16).
    #[test]
    fn check_semantics_table() {
        use Conclusion::*;
        let table: [(Option<Conclusion>, &[&str]); 9] = [
            (Some(Success), &["check-success"]),
            (Some(Failure), &["check-failure"]),
            (Some(Cancelled), &["check-failure", "check-cancelled"]),
            (Some(TimedOut), &["check-failure", "check-timed-out"]),
            (Some(ActionRequired), &["check-failure"]),
            (Some(Neutral), &["check-neutral"]),
            (Some(Skipped), &["check-skipped"]),
            (Some(Stale), &["check-stale"]),
            (None, &["check-pending"]),
        ];
        let attrs = [
            "check-success",
            "check-failure",
            "check-neutral",
            "check-skipped",
            "check-cancelled",
            "check-timed-out",
            "check-stale",
            "check-pending",
        ];
        for (conclusion, expected) in table {
            let mut p = pr();
            p.checks = vec![run("ci", "h2", conclusion, 1)];
            for attr in attrs {
                let got = holds(&format!("{attr} = ci"), &p);
                assert_eq!(got, expected.contains(&attr), "{conclusion:?} × {attr}");
            }
            assert!(holds("check = ci", &p));
        }
    }

    #[test]
    fn newest_run_on_head_wins() {
        use Conclusion::*;
        let mut p = pr();
        p.checks = vec![
            run("ci", "h1", Some(Success), 1), // old head: ignored (MQ-14)
            run("ci", "h2", Some(Success), 2),
            run("ci", "h2", Some(Failure), 3), // rerun turns red (MQ-17)
        ];
        assert!(!holds("check-success = ci", &p));
        assert!(holds("#check-failure = 1", &p));
        p.checks.push(run("ci", "h2", None, 4)); // second trigger still running
        assert!(holds("check-pending = ci", &p));
        assert!(!holds("check-success = ci", &p));
    }

    fn holds_with(cond: &str, pr: &PullRequest, ctx: &Context) -> bool {
        eval_atom(&parse_atom(cond).unwrap(), pr, ctx).unwrap()
    }

    #[test]
    fn sender_permission_ladder_and_fork() {
        let mut p = pr();
        for (perm, write) in [
            ("none", false),
            ("read", false),
            ("triage", false),
            ("write", true),
            ("maintain", true),
            ("admin", true),
        ] {
            let ctx = Context {
                sender_permission: Some(perm.into()),
                ..Default::default()
            };
            assert_eq!(
                holds_with("sender-permission >= write", &p, &ctx),
                write,
                "{perm}"
            );
            assert_eq!(
                holds_with("sender-permission = triage", &p, &ctx),
                perm == "triage"
            );
            assert_eq!(
                holds_with("sender-permission < read", &p, &ctx),
                perm == "none"
            );
        }
        assert!(
            !holds_with("sender-permission >= read", &p, &Context::default()),
            "unknown permission counts as none"
        );
        assert!(
            eval_atom(
                &parse_atom("sender-permission >= owner").unwrap(),
                &p,
                &Context::default()
            )
            .is_err()
        );
        assert!(!holds("from-fork", &p));
        p.from_fork = true;
        assert!(holds("from-fork", &p));
    }

    #[test]
    fn author_template() {
        let list = vec![Condition::Atom(
            parse_atom("sender = {{ author }}").unwrap(),
        )];
        let r = with_author(&list, "alice");
        let ctx = Context {
            sender: Some("alice".into()),
            ..Default::default()
        };
        let mut errors = Vec::new();
        assert!(eval_list(&r, &pr(), &ctx, &mut errors).0);
        let ctx = Context {
            sender: Some("mallory".into()),
            ..Default::default()
        };
        assert!(!eval_list(&r, &pr(), &ctx, &mut errors).0);
    }

    #[test]
    fn queue_attributes() {
        let p = pr();
        let ctx = Context {
            now: 10_000,
            queue: Some(QueueAttrs {
                name: "default".into(),
                position: 0,
                queued_at: 9_000,
                merge_started_at: None,
            }),
            dequeue_reason: Some("CHECKS_FAILED".into()),
            ..Default::default()
        };
        assert!(holds_with("queue-position = 0", &p, &ctx));
        assert!(holds_with("queue-name = default", &p, &ctx));
        assert!(holds_with("queued-at < 10 minutes ago", &p, &ctx));
        assert!(!holds_with("queued-at < 20 minutes ago", &p, &ctx));
        assert!(holds_with("queued-at > 1970-01-01T02:00:00Z", &p, &ctx));
        assert!(!holds_with("queue-merge-started-at > 1970-01-01", &p, &ctx));
        assert!(holds_with("queue-dequeue-reason = checks-failed", &p, &ctx));
        assert!(holds_with(
            "queue-dequeue-reason != CHECKS_TIMEOUT",
            &p,
            &ctx
        ));
        let none = Context::default();
        assert!(holds_with("queue-position = -1", &p, &none));
        assert!(holds_with("queue-dequeue-reason = none", &p, &none));
        assert!(!holds_with("queued-at > 1970-01-01", &p, &none));
        assert!(eval_atom(&parse_atom("queued-at = 1970-01-01").unwrap(), &p, &ctx).is_err());
    }

    #[test]
    fn protection_attributes() {
        let mut p = pr();
        p.body = "Depends-On: #4\nDepends-On: org/other#5".into();
        let ctx = Context {
            repository: "org/app".into(),
            ..Default::default()
        };
        assert!(holds_with("depends-on = org/app#4", &p, &ctx));
        assert!(holds_with("#depends-on = 2", &p, &ctx));
        assert!(!holds("mergify-configuration-changed", &p));
        p.files.push(".github/mergify.yml".into());
        assert!(holds("mergify-configuration-changed", &p));
    }

    fn eval_str(cond: &str, pr: &PullRequest) -> (bool, Vec<EvalError>) {
        let mut errors = Vec::new();
        let value: serde_json::Value = serde_json::from_str(cond).unwrap();
        let mut perr = Vec::new();
        let list = schlange_config::conditions::parse_list(&value, "", &mut perr);
        assert!(perr.is_empty(), "{perr:?}");
        (
            eval_list(&list, pr, &Context::default(), &mut errors).0,
            errors,
        )
    }

    /// Critic 1abc, globs: `**/*.py` matches files at the root; braces and brackets work.
    #[test]
    fn globs_follow_the_documented_semantics() {
        let mut p = pr();
        p.files = vec!["setup.py".into()];
        assert!(holds("files *= **/*.py", &p), "root file");
        p.files = vec!["web/a/b.tsx".into()];
        assert!(holds("files *= web/**/*.{js,jsx,ts,tsx}", &p));
        assert!(!holds("files *= web/**/*.{js,ts}", &p));
        p.files = vec!["v2.md".into()];
        assert!(holds("files *= v[0-9].md", &p));
        assert!(!holds("files *= v[!0-9].md", &p));
        assert!(
            eval_atom(
                &parse_atom("files *= {a,b").unwrap(),
                &p,
                &Context::default()
            )
            .is_err(),
            "an unbalanced brace is an error, not a literal"
        );
    }

    /// Critic 1abc, conditions-checks: the newest run per app, per workflow and event, and
    /// statuses apart from check runs.
    #[test]
    fn check_runs_are_read_per_app_workflow_and_kind() {
        use Conclusion::*;
        let mut p = pr();
        let mut a = run("pep8", "h2", Some(Success), 1);
        a.app = Some("github-actions".into());
        let mut b = run("pep8", "h2", Some(Failure), 2);
        b.app = Some("other-ci".into());
        p.checks = vec![a.clone(), b.clone()];
        // Two apps: the bare name matches whichever passed; the qualified form pins the app.
        assert!(holds("check-success = pep8", &p));
        assert!(holds("check-success = @github-actions/pep8", &p));
        assert!(!holds("check-success = @other-ci/pep8", &p));
        assert!(holds("check-failure = @other-ci/pep8", &p));
        assert!(!holds("check-success = @nobody/pep8", &p));
        assert_eq!(
            attribute(&p, &Context::default(), "check-success").unwrap(),
            AttrValue::List(vec!["pep8".into()])
        );
        // One check over two workflow files: a newer pass in one does not hide a failure in
        // the other.
        let mut w1 = run("test", "h2", Some(Failure), 3);
        p.required_check_names.insert("test".into());
        w1.app = Some("github-actions".into());
        w1.workflow = Some("ci.yml".into());
        w1.event = Some("pull_request".into());
        let mut w2 = run("test", "h2", Some(Success), 4);
        w2.app = Some("github-actions".into());
        w2.workflow = Some("nightly.yml".into());
        w2.event = Some("pull_request".into());
        p.checks = vec![w1.clone(), w2.clone()];
        assert!(!holds("check-success = test", &p));
        // A re-run of the same workflow and event replaces the earlier run.
        let mut w3 = w1.clone();
        w3.conclusion = Some(Success);
        w3.seq = 5;
        p.checks.push(w3);
        assert!(holds("check-success = test", &p));
        // A commit status and a check run of the same name are numbered apart: a status with
        // a higher id does not replace the check run, nor the other way round.
        let cr = run("lint", "h2", Some(Failure), 7);
        let mut st = run("lint", "h2", Some(Success), 900);
        st.status = true;
        p.checks = vec![cr, st];
        assert!(holds("check-failure = lint", &p));
        assert!(holds("check-success = lint", &p));
        assert_eq!(p.current_checks().len(), 2);
    }

    #[test]
    fn nonrequired_checks_use_newest_suite_not_newest_run() {
        use Conclusion::*;
        let mut p = pr();
        let mut old_suite_late_run = run("KI-Review", "h2", Some(Failure), 100);
        old_suite_late_run.app = Some("github-actions".into());
        old_suite_late_run.suite_created_at = Some("2026-01-01T00:00:00Z".into());
        old_suite_late_run.suite_id = Some(10);
        let mut new_suite_early_run = run("KI-Review", "h2", Some(Success), 90);
        new_suite_early_run.app = Some("github-actions".into());
        new_suite_early_run.suite_created_at = Some("2026-01-02T00:00:00Z".into());
        new_suite_early_run.suite_id = Some(20);
        p.checks = vec![old_suite_late_run, new_suite_early_run];
        assert!(holds("check-success = KI-Review", &p));
        assert!(holds("#check-failure = 0", &p));
        // Suite timestamps can tie at GitHub's exposed resolution. ID ordering is not a
        // documented creation-order guarantee, so do not select a possibly stale success.
        p.checks[0].suite_created_at = p.checks[1].suite_created_at.clone();
        assert!(attribute(&p, &Context::default(), "check-success").is_err());
    }

    #[test]
    fn mixed_suite_metadata_cannot_be_used_to_select_success() {
        use Conclusion::*;
        let mut p = pr();
        let old = run("lint", "h2", Some(Failure), 100);
        let mut newer = run("lint", "h2", Some(Success), 90);
        newer.suite_created_at = Some("2026-01-02T00:00:00Z".into());
        newer.suite_id = Some(20);
        p.checks = vec![old, newer];
        assert!(attribute(&p, &Context::default(), "check-success").is_err());
        assert!(attribute(&p, &Context::default(), "check-failure").is_err());
    }

    #[test]
    fn app_qualified_required_check_preserves_distinct_workflows() {
        use Conclusion::*;
        let mut p = pr();
        p.required_check_names.insert("@github-actions/lint".into());
        let mut red = run("lint", "h2", Some(Failure), 3);
        red.app = Some("github-actions".into());
        red.workflow = Some("a.yml".into());
        red.event = Some("pull_request".into());
        red.suite_created_at = Some("2026-01-01T00:00:00Z".into());
        red.suite_id = Some(1);
        let mut green = red.clone();
        green.conclusion = Some(Success);
        green.workflow = Some("b.yml".into());
        green.suite_id = Some(2);
        green.seq = 4;
        p.checks = vec![red, green];
        assert!(!holds("check-success = lint", &p));
        assert!(holds("check-failure = lint", &p));
        p.checks[0].workflow = None;
        assert!(attribute(&p, &Context::default(), "check-success").is_err());
    }

    /// Critic 1abc, conditions-syntax.
    #[test]
    fn unicode_operators_parse_and_evaluate() {
        let p = pr();
        assert!(holds("#files ≥ 2", &p));
        assert!(holds("#files ≤ 2", &p));
        assert!(!holds("#files ≥ 3", &p));
    }

    #[test]
    fn not_over_an_evaluation_error_fails_closed() {
        let p = pr();
        // `approved-reviews-by` is not evaluated: unknown, so `not` of it is unknown too.
        let (ok, errors) = eval_str(r#"[{"not": "approved-reviews-by = x"}]"#, &p);
        assert!(!ok, "fails closed");
        assert_eq!(errors.len(), 1);
        let (ok, _) = eval_str(
            r#"[{"not": {"and": ["base = main", "approved-reviews-by = x"]}}]"#,
            &p,
        );
        assert!(!ok);
        // Known regardless of the unknown part: `false and ?` is false, so its negation holds.
        let (ok, _) = eval_str(
            r#"[{"not": {"and": ["base = other", "approved-reviews-by = x"]}}]"#,
            &p,
        );
        assert!(ok);
        let (ok, _) = eval_str(
            r#"[{"or": ["base = main", "approved-reviews-by = x"]}]"#,
            &p,
        );
        assert!(ok, "true or ? is true");
    }

    #[test]
    fn a_regex_that_fails_while_matching_is_an_error() {
        let mut p = pr();
        p.title = "a".repeat(40);
        // Catastrophic backtracking hits fancy_regex's backtrack limit.
        let a = parse_atom(r"title ~= ^(?:(a)|(a))*?\1?(?:(a)|(a))*\1?(?=b)").unwrap();
        let r = eval_atom(&a, &p, &Context::default());
        assert!(matches!(r, Err(EvalError::RegexRuntime { .. })), "{r:?}");
        let n = parse_atom(r"-title ~= ^(?:(a)|(a))*?\1?(?:(a)|(a))*\1?(?=b)").unwrap();
        assert!(
            eval_atom(&n, &p, &Context::default()).is_err(),
            "never a silent pass"
        );
    }

    #[test]
    fn author_template_in_values() {
        let mut p = pr();
        p.head_ref = "arztagent-bot".into();
        assert!(holds("head = {{ author }}", &p));
        assert!(holds("head = {{author}}", &p));
        p.labels.insert("arztagent-bot".into());
        assert!(holds("label = {{ author }}", &p));
        p.head_ref = "other".into();
        assert!(!holds("head = {{ author }}", &p));
        assert!(
            eval_atom(
                &parse_atom("#files = {{ author }}").unwrap(),
                &p,
                &Context::default()
            )
            .is_err(),
            "a number rejects it"
        );
    }

    #[test]
    fn unsupported_attribute_is_an_error() {
        let a = parse_atom("approved-reviews-by = x").unwrap();
        assert_eq!(
            eval_atom(&a, &pr(), &Context::default()),
            Err(EvalError::UnsupportedAttribute(
                "approved-reviews-by".into()
            ))
        );
    }
}
