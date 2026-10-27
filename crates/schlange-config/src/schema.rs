//! Stage 2: the pinned Mergify JSON Schema, plus what stage 3 needs from it (attributes with
//! their operators and modifiers, and which keys hold condition lists).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use jsonschema::error::ValidationErrorKind;
use serde_json::Value;

/// Where the schema is searched when no path is given: below the current directory and its
/// ancestors (the repository keeps it in `.research/`, never checked in).
pub const DEFAULT_RELATIVE_PATH: &str = ".research/mergify/schema.json";

/// Environment variable with an explicit schema path.
pub const ENV_VAR: &str = "SCHLANGE_SCHEMA";

#[derive(Debug, thiserror::Error)]
pub enum SchemaError {
    #[error(
        "no Mergify schema found: pass --schema <path>, set {ENV_VAR}, or run \
         tools/fetch-mergify-sources (expects {DEFAULT_RELATIVE_PATH}). The schema is not \
         embedded in the binary until its licence is cleared."
    )]
    NotFound,
    #[error("cannot read schema {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("schema {path} is not valid JSON: {source}")]
    Json {
        path: PathBuf,
        source: serde_json::Error,
    },
    #[error("schema {path} does not compile: {message}")]
    Compile { path: PathBuf, message: String },
}

/// Finds the schema: explicit path, then the environment, then `.research/` upwards from `start`.
pub fn locate(explicit: Option<&Path>, start: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    if let Some(p) = std::env::var_os(ENV_VAR) {
        return Some(PathBuf::from(p));
    }
    start
        .ancestors()
        .map(|d| d.join(DEFAULT_RELATIVE_PATH))
        .find(|p| p.is_file())
}

/// A condition attribute as the schema describes it.
#[derive(Clone, Debug, Default)]
pub struct Attribute {
    /// `None`: the schema names no operators, anything is accepted.
    pub operators: Option<Vec<String>>,
    pub negate: bool,
    pub length: bool,
    pub length_required: bool,
}

pub struct Schema {
    pub path: PathBuf,
    pub raw: Value,
    validator: jsonschema::Validator,
    pub attributes: BTreeMap<String, Attribute>,
    /// Object keys whose value is a condition list (`conditions`, `queue_conditions`, …).
    pub condition_keys: BTreeSet<String>,
}

/// A schema violation at a JSON pointer.
#[derive(Clone, Debug)]
pub struct Violation {
    pub pointer: String,
    /// True when the pointer names a key (unknown property) rather than a value.
    pub at_key: bool,
    pub message: String,
}

impl Schema {
    pub fn load(path: &Path) -> Result<Self, SchemaError> {
        let text = std::fs::read_to_string(path).map_err(|source| SchemaError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let raw: Value = serde_json::from_str(&text).map_err(|source| SchemaError::Json {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_value(path, raw)
    }

    pub fn from_value(path: &Path, raw: Value) -> Result<Self, SchemaError> {
        let validator = jsonschema::validator_for(&raw).map_err(|e| SchemaError::Compile {
            path: path.to_path_buf(),
            message: e.to_string(),
        })?;
        let attributes = attributes(&raw);
        let condition_keys = condition_keys(&raw);
        Ok(Schema {
            path: path.to_path_buf(),
            raw,
            validator,
            attributes,
            condition_keys,
        })
    }

    pub fn validate(&self, instance: &Value) -> Vec<Violation> {
        let mut out = Vec::new();
        for error in self.validator.iter_errors(instance) {
            let base = error.instance_path().as_str().to_string();
            match error.kind() {
                ValidationErrorKind::AdditionalProperties { unexpected } => {
                    for key in unexpected {
                        out.push(Violation {
                            pointer: format!("{base}/{}", crate::yaml::escape(key)),
                            at_key: true,
                            message: format!("unknown key `{key}`"),
                        });
                    }
                }
                ValidationErrorKind::AnyOf { context }
                | ValidationErrorKind::OneOfNotValid { context } => {
                    out.push(Violation {
                        pointer: base,
                        at_key: false,
                        message: best_alternative(&error.to_string(), context),
                    });
                }
                _ => out.push(Violation {
                    pointer: base,
                    at_key: false,
                    message: error.to_string(),
                }),
            }
        }
        out
    }
}

/// For `anyOf`/`oneOf` failures, the message of the most useful alternative: skip the
/// "is not of type null" branch of nullable options and prefer the deepest errors.
fn best_alternative(
    fallback: &str,
    context: &[Vec<jsonschema::ValidationError<'static>>],
) -> String {
    let mut messages: Vec<String> = context
        .iter()
        .flatten()
        .map(|e| e.to_string())
        .filter(|m| !m.ends_with("is not of type \"null\""))
        .collect();
    messages.dedup();
    match messages.len() {
        0 => fallback.to_string(),
        1..=3 => messages.join("; "),
        _ => format!(
            "{} (and {} more)",
            messages[..2].join("; "),
            messages.len() - 2
        ),
    }
}

fn attributes(raw: &Value) -> BTreeMap<String, Attribute> {
    let props = raw
        .pointer("/$defs/PullRequestAttributes/properties")
        .and_then(Value::as_object);
    let mut out = BTreeMap::new();
    for (name, node) in props.into_iter().flatten() {
        let operators = node
            .get("x-allowed-operators")
            .and_then(Value::as_array)
            .map(|ops| {
                ops.iter()
                    .filter_map(|o| o.as_str().map(str::to_string))
                    .collect()
            });
        let mut attr = Attribute {
            operators,
            ..Default::default()
        };
        match node.get("x-modifiers").and_then(Value::as_array) {
            Some(mods) => {
                for m in mods {
                    match m.get("type").and_then(Value::as_str) {
                        Some("negate") => attr.negate = true,
                        Some("length") => {
                            attr.length = true;
                            attr.length_required =
                                m.get("required").and_then(Value::as_bool) == Some(true);
                        }
                        _ => {}
                    }
                }
            }
            None => {
                attr.negate = true;
                attr.length = true;
            }
        }
        out.insert(name.clone(), attr);
    }
    out
}

fn is_condition_ref(node: &Value) -> bool {
    let name_of = |n: &Value| {
        n.get("$ref")
            .and_then(Value::as_str)
            .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
    };
    let check = |n: &Value| {
        name_of(n)
            .is_some_and(|name| name.ends_with("ConditionsModel") || name == "ListOfRuleConditions")
    };
    check(node)
        || ["anyOf", "oneOf"].iter().any(|k| {
            node.get(*k)
                .and_then(Value::as_array)
                .is_some_and(|alts| alts.iter().any(check))
        })
}

fn condition_keys(raw: &Value) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let defs = raw.get("$defs").and_then(Value::as_object);
    let roots = defs
        .into_iter()
        .flat_map(|d| d.values())
        .chain(std::iter::once(raw));
    for def in roots {
        for (key, prop) in def
            .get("properties")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
        {
            if is_condition_ref(prop) {
                out.insert(key.clone());
            }
        }
    }
    out
}
