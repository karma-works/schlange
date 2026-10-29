//! Condition parser: `[-][#]attribute [operator value]` and the combinators `and`, `or`, `not`.
//!
//! The parser knows the grammar only. Whether an attribute exists and which operators it allows
//! comes from the schema ([`crate::schema::Attributes`]).

use serde::Serialize;
use serde_json::Value;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Operator {
    #[serde(rename = "=")]
    Eq,
    #[serde(rename = "!=")]
    Ne,
    #[serde(rename = "~=")]
    Regex,
    #[serde(rename = "*=")]
    Glob,
    #[serde(rename = ">=")]
    Ge,
    #[serde(rename = ">")]
    Gt,
    #[serde(rename = "<=")]
    Le,
    #[serde(rename = "<")]
    Lt,
}

impl Operator {
    /// Canonical spelling, as listed in the schema's `x-allowed-operators`.
    pub fn as_str(self) -> &'static str {
        match self {
            Operator::Eq => "=",
            Operator::Ne => "!=",
            Operator::Regex => "~=",
            Operator::Glob => "*=",
            Operator::Ge => ">=",
            Operator::Gt => ">",
            Operator::Le => "<=",
            Operator::Lt => "<",
        }
    }
}

/// Spellings, longest first so that `>=` wins over `>`.
const OPERATORS: &[(&str, Operator)] = &[
    ("!=", Operator::Ne),
    ("~=", Operator::Regex),
    ("*=", Operator::Glob),
    (">=", Operator::Ge),
    ("<=", Operator::Le),
    ("≠", Operator::Ne),
    ("≥", Operator::Ge),
    ("≤", Operator::Le),
    ("=", Operator::Eq),
    (":", Operator::Eq),
    (">", Operator::Gt),
    ("<", Operator::Lt),
];

/// Splits a qualified check or deployment name `@<github-app-slug>/<name>` into slug and name;
/// `None` for any other shape.
pub fn qualified(value: &str) -> Option<(&str, &str)> {
    let rest = value.strip_prefix('@')?;
    let (slug, name) = rest.split_once('/')?;
    let slug_ok = !slug.is_empty()
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.');
    (slug_ok && !name.is_empty()).then_some((slug, name))
}

/// One condition string.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Atom {
    pub negate: bool,
    pub length: bool,
    pub attribute: String,
    pub operator: Option<Operator>,
    pub value: Option<String>,
}

/// A condition tree.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Condition {
    /// Internal branch-protection requirement, not accepted by the config parser.
    RequiredCheck {
        context: String,
        app_id: u64,
    },
    Atom(Atom),
    And(Vec<Condition>),
    Or(Vec<Condition>),
    Not(Box<Condition>),
    /// The literal `true` some condition lists accept.
    Always,
}

impl Condition {
    /// Every atom in the tree.
    pub fn atoms(&self) -> Vec<&Atom> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }

    fn collect<'a>(&'a self, out: &mut Vec<&'a Atom>) {
        match self {
            Condition::Atom(a) => out.push(a),
            Condition::And(cs) | Condition::Or(cs) => cs.iter().for_each(|c| c.collect(out)),
            Condition::Not(c) => c.collect(out),
            Condition::Always | Condition::RequiredCheck { .. } => {}
        }
    }
}

/// A parse error, with the JSON pointer of the offending element relative to the list.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ParseError {
    pub pointer: String,
    pub message: String,
}

/// Parses one condition string.
pub fn parse_atom(text: &str) -> Result<Atom, String> {
    let mut rest = text.trim();
    let mut negate = false;
    let mut length = false;
    if let Some(r) = rest.strip_prefix('-') {
        negate = true;
        rest = r.trim_start();
    }
    if let Some(r) = rest.strip_prefix('#') {
        length = true;
        rest = r.trim_start();
    }
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_'))
        .unwrap_or(rest.len());
    let attribute = &rest[..end];
    if attribute.is_empty() {
        return Err(format!("condition `{text}` has no attribute"));
    }
    let after = rest[end..].trim_start();
    if after.is_empty() {
        return Ok(Atom {
            negate,
            length,
            attribute: attribute.to_string(),
            operator: None,
            value: None,
        });
    }
    let Some((spelling, operator)) = OPERATORS.iter().find(|(s, _)| after.starts_with(s)) else {
        return Err(format!(
            "condition `{text}`: unknown operator after `{attribute}`"
        ));
    };
    let mut value = after[spelling.len()..].trim();
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        value = &value[1..value.len() - 1];
    } else if value.is_empty() {
        return Err(format!("condition `{text}` has no value"));
    }
    Ok(Atom {
        negate,
        length,
        attribute: attribute.to_string(),
        operator: Some(*operator),
        value: Some(value.to_string()),
    })
}

/// Parses one element of a condition list: a string, `{and: [...]}`, `{or: [...]}`,
/// `{not: ...}` or `true`. `pointer` is the element's JSON pointer, used in errors.
pub fn parse_tree(value: &Value, pointer: &str, errors: &mut Vec<ParseError>) -> Option<Condition> {
    match value {
        Value::String(s) => match parse_atom(s) {
            Ok(a) => Some(Condition::Atom(a)),
            Err(message) => {
                errors.push(ParseError {
                    pointer: pointer.to_string(),
                    message,
                });
                None
            }
        },
        Value::Bool(true) => Some(Condition::Always),
        Value::Object(map) if map.len() == 1 => {
            let (key, inner) = map.iter().next()?;
            let child = format!("{pointer}/{key}");
            match key.as_str() {
                "and" | "or" => {
                    let list = parse_list(inner, &child, errors);
                    Some(if key == "and" {
                        Condition::And(list)
                    } else {
                        Condition::Or(list)
                    })
                }
                "not" => parse_tree(inner, &child, errors).map(|c| Condition::Not(Box::new(c))),
                other => {
                    errors.push(ParseError {
                        pointer: child,
                        message: format!("unknown combinator `{other}` (expected and, or, not)"),
                    });
                    None
                }
            }
        }
        _ => {
            errors.push(ParseError {
                pointer: pointer.to_string(),
                message: "a condition must be a string or one of and, or, not".into(),
            });
            None
        }
    }
}

/// Parses a condition list (implicit `and`).
pub fn parse_list(value: &Value, pointer: &str, errors: &mut Vec<ParseError>) -> Vec<Condition> {
    match value {
        Value::Array(items) => items
            .iter()
            .enumerate()
            .filter_map(|(i, v)| parse_tree(v, &format!("{pointer}/{i}"), errors))
            .collect(),
        Value::Bool(true) => vec![Condition::Always],
        Value::Null => Vec::new(),
        other => parse_tree(other, pointer, errors).into_iter().collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn atom(s: &str) -> Atom {
        parse_atom(s).unwrap()
    }

    #[test]
    fn forms() {
        assert_eq!(
            atom("-draft"),
            Atom {
                negate: true,
                length: false,
                attribute: "draft".into(),
                operator: None,
                value: None
            }
        );
        let a = atom("#check-failure = 0");
        assert!(a.length && !a.negate);
        assert_eq!(a.attribute, "check-failure");
        assert_eq!(a.operator, Some(Operator::Eq));
        assert_eq!(a.value.as_deref(), Some("0"));
        let a = atom("-label = halt");
        assert!(a.negate);
        assert_eq!(a.value.as_deref(), Some("halt"));
        assert_eq!(
            atom("author=dependabot[bot]").value.as_deref(),
            Some("dependabot[bot]")
        );
        assert_eq!(atom("base: main").operator, Some(Operator::Eq));
        assert_eq!(atom("label ≠ x").operator, Some(Operator::Ne));
        assert_eq!(atom("#files ≥ 3").operator, Some(Operator::Ge));
        assert_eq!(atom("#files ≤ 3").operator, Some(Operator::Le));
        assert_eq!(atom("#files≤3").value.as_deref(), Some("3"));
        assert_eq!(atom("#files >= 3").operator, Some(Operator::Ge));
        assert_eq!(atom("#files > 3").operator, Some(Operator::Gt));
        assert_eq!(atom("files *= src/**").operator, Some(Operator::Glob));
        assert_eq!(atom("title = \"a b\"").value.as_deref(), Some("a b"));
    }

    #[test]
    fn qualified_check_names() {
        assert_eq!(
            qualified("@github-actions/pep8"),
            Some(("github-actions", "pep8"))
        );
        assert_eq!(qualified("@a/b/c"), Some(("a", "b/c")));
        assert_eq!(qualified("@/pep8"), None);
        assert_eq!(qualified("@github-actions"), None);
        assert_eq!(qualified("@x/"), None);
        assert_eq!(qualified("pep8"), None);
    }

    #[test]
    fn regex_value_is_kept_verbatim() {
        let a = atom("-files ~= ^(?!(?:a/.*)$)(?:\\.github/.*)$");
        assert_eq!(a.operator, Some(Operator::Regex));
        assert_eq!(a.value.as_deref(), Some("^(?!(?:a/.*)$)(?:\\.github/.*)$"));
    }

    #[test]
    fn errors() {
        assert!(parse_atom("= main").is_err());
        assert!(parse_atom("base = ").is_err());
        assert!(parse_atom("base ? main").is_err());
    }

    #[test]
    fn trees() {
        let mut errs = Vec::new();
        let list = parse_list(
            &json!(["base = main", {"or": ["check-success = KI-Review", "author = dependabot[bot]"]},
                    {"not": {"and": ["draft"]}}, {"xor": []}, 3]),
            "/c",
            &mut errs,
        );
        assert_eq!(list.len(), 3);
        assert!(matches!(&list[1], Condition::Or(v) if v.len() == 2));
        assert_eq!(errs.len(), 2);
        assert_eq!(errs[0].pointer, "/c/3/xor");
        assert_eq!(errs[1].pointer, "/c/4");
        assert_eq!(list[1].atoms().len(), 2);
    }
}
