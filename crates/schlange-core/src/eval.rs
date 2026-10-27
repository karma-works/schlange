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
}

/// Context that is not part of the pull request itself.
#[derive(Clone, Debug, Default)]
pub struct Context {
    /// Login of the commenter, for command restrictions.
    pub sender: Option<String>,
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
}

fn checks_where(pr: &PullRequest, pred: impl Fn(Option<Conclusion>) -> bool) -> AttrValue {
    AttrValue::List(
        pr.current_checks()
            .into_iter()
            .filter(|c| pred(c.conclusion))
            .map(|c| c.name.clone())
            .collect(),
    )
}

/// Value of an attribute for a pull request.
pub fn attribute(pr: &PullRequest, ctx: &Context, name: &str) -> Result<AttrValue, EvalError> {
    use Conclusion::*;
    Ok(match name {
        "number" => AttrValue::Str(pr.number.to_string()),
        "author" => AttrValue::Str(pr.author.clone()),
        "base" => AttrValue::Str(pr.base.clone()),
        "head" => AttrValue::Str(pr.head_ref.clone()),
        "title" => AttrValue::Str(pr.title.clone()),
        "draft" => AttrValue::Bool(pr.draft),
        "closed" => AttrValue::Bool(pr.closed),
        "label" => AttrValue::List(pr.labels.iter().cloned().collect()),
        "files" => AttrValue::List(pr.files.clone()),
        "dependabot-update-type" => AttrValue::List(pr.dependabot_update_types.clone()),
        "sender" => match &ctx.sender {
            Some(s) => AttrValue::Str(s.clone()),
            None => AttrValue::Str(String::new()),
        },
        "check" => checks_where(pr, |_| true),
        "check-success" => checks_where(pr, |c| c == Some(Success)),
        "check-failure" => checks_where(pr, |c| {
            matches!(c, Some(Failure | Cancelled | TimedOut | ActionRequired))
        }),
        "check-neutral" => checks_where(pr, |c| c == Some(Neutral)),
        "check-skipped" => checks_where(pr, |c| c == Some(Skipped)),
        "check-cancelled" => checks_where(pr, |c| c == Some(Cancelled)),
        "check-timed-out" => checks_where(pr, |c| c == Some(TimedOut)),
        "check-stale" => checks_where(pr, |c| c == Some(Stale)),
        "check-pending" => checks_where(pr, |c| c.is_none()),
        other => return Err(EvalError::UnsupportedAttribute(other.to_string())),
    })
}

/// Unix-style glob as used by `*=`: `*` within a segment, `**` across segments, `?` one char.
/// Open: Mergify's exact glob semantics (anchoring, `*` across `/`) still need a difference test.
fn glob_to_regex(glob: &str) -> String {
    let mut re = String::from("^");
    let mut chars = glob.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '*' if chars.peek() == Some(&'*') => {
                chars.next();
                re.push_str(".*");
            }
            '*' => re.push_str("[^/]*"),
            '?' => re.push_str("[^/]"),
            c => re.push_str(&fancy_regex::escape(&c.to_string())),
        }
    }
    re.push('$');
    re
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
        Operator::Regex => fancy_regex::Regex::new(expected)
            .map_err(|_| EvalError::BadRegex(expected.to_string()))?
            .is_match(actual)
            .unwrap_or(false),
        Operator::Glob => fancy_regex::Regex::new(&glob_to_regex(expected))
            .map_err(|_| EvalError::BadRegex(expected.to_string()))?
            .is_match(actual)
            .unwrap_or(false),
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

/// Evaluates one atom.
pub fn eval_atom(atom: &Atom, pr: &PullRequest, ctx: &Context) -> Result<bool, EvalError> {
    let value = attribute(pr, ctx, &atom.attribute)?;
    let result = if atom.length {
        let len = match &value {
            AttrValue::List(v) => v.len(),
            AttrValue::Str(s) => s.chars().count(),
            AttrValue::Bool(_) => {
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
        }
    };
    Ok(result != atom.negate)
}

/// Evaluates a condition tree. Errors make the condition false and are returned alongside.
pub fn eval(
    cond: &Condition,
    pr: &PullRequest,
    ctx: &Context,
    errors: &mut Vec<EvalError>,
) -> bool {
    match cond {
        Condition::Atom(a) => eval_atom(a, pr, ctx).unwrap_or_else(|e| {
            errors.push(e);
            false
        }),
        Condition::And(cs) => cs.iter().all(|c| eval(c, pr, ctx, errors)),
        Condition::Or(cs) => cs.iter().any(|c| eval(c, pr, ctx, errors)),
        Condition::Not(c) => !eval(c, pr, ctx, errors),
        Condition::Always => true,
    }
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
        PullRequest {
            number: 7,
            author: "arztagent-bot".into(),
            base: "main".into(),
            head_ref: "feature".into(),
            head_sha: "h2".into(),
            title: "t".into(),
            draft: false,
            closed: false,
            labels: ["ready".to_string()].into(),
            files: vec!["src/main.rs".into(), ".github/workflows/ci.yml".into()],
            dependabot_update_types: vec![],
            checks: vec![],
        }
    }

    fn run(name: &str, sha: &str, c: Option<Conclusion>, seq: u64) -> CheckRun {
        CheckRun {
            name: name.into(),
            app: None,
            head_sha: sha.into(),
            conclusion: c,
            seq,
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
        // Whether `*` crosses `/` like Python's `PurePath.match` is not settled (open).
        assert!(holds("files *= **.rs", &p));
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
