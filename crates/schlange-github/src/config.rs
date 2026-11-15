//! Runtime configuration for `serve` and `doctor`, read from the default branch only.
//! Without an explicit schema, Schlange uses its own embedded structural contract for
//! supported engine options. An explicit upstream schema preserves development validation.
//! Invalid configurations never replace the last valid one.

use std::path::Path;

use schlange_config::rules::Rules;
use schlange_config::validate::Severity;
use schlange_config::{FeatureMap, Schema};

use crate::api::RepoApi;
use crate::client::{Error, Priority};

/// What the search on the default branch found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Found {
    None,
    /// Exactly one file: its path and text.
    One(String, String),
    /// More than one file: the error text (nothing is applied).
    Ambiguous(String),
}

/// Runtime validator: embedded contract or explicitly selected upstream schema.
pub struct ConfigCheck {
    pub schema: Schema,
    pub features: FeatureMap,
}

/// A configuration that passed stages 1–4.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub rules: Rules,
    /// Stage 5 warnings (explicit active unsupported settings are rejected by the embedded validator).
    pub warnings: Vec<String>,
}

impl ConfigCheck {
    /// Use an explicitly supplied schema (argument or environment), otherwise the embedded
    /// clean-room runtime contract. Automatic upstream discovery belongs to `config validate`.
    pub fn locate(explicit: Option<&Path>, _start: &Path) -> Result<ConfigCheck, String> {
        let path = explicit
            .map(Path::to_path_buf)
            .or_else(|| std::env::var_os(schlange_config::schema::ENV_VAR).map(Into::into));
        match path {
            Some(path) => Ok(ConfigCheck {
                schema: Schema::load(&path).map_err(|e| e.to_string())?,
                features: FeatureMap::embedded().map_err(|e| e.to_string())?,
            }),
            None => Self::embedded(),
        }
    }

    pub fn embedded() -> Result<Self, String> {
        Ok(Self {
            schema: schlange_config::runtime::schema().map_err(|e| e.to_string())?,
            features: FeatureMap::embedded().map_err(|e| e.to_string())?,
        })
    }

    pub fn is_embedded(&self) -> bool {
        self.schema.path == Path::new(schlange_config::runtime::NAME)
    }

    /// Validates a configuration text; `Err` lists the errors (stages 1–4). A configuration
    /// with `extends` fails here: use [`ConfigCheck::check_extended`].
    pub fn check(&self, file: &str, text: &str) -> Result<Loaded, String> {
        self.check_extended(file, text, None, |name| {
            Err(format!("repository `{name}` cannot be read here"))
        })
    }

    /// Like `check`, resolving `extends` with `load` (repository name → its configuration
    /// text). The local text is validated against the schema; the merged result is what runs.
    pub fn check_extended(
        &self,
        file: &str,
        text: &str,
        own: Option<&str>,
        load: impl FnOnce(&str) -> Result<String, String>,
    ) -> Result<Loaded, String> {
        let report = schlange_config::validate(file, text, &self.schema, &self.features, false);
        let at = |f: &schlange_config::validate::Finding| match (f.line, f.column) {
            (Some(l), Some(c)) => format!("{file}:{l}:{c}"),
            _ => format!("{file} {}", f.pointer),
        };
        let errors: Vec<String> = report
            .findings
            .iter()
            .filter(|f| {
                f.severity == Severity::Error
                    || (self.is_embedded()
                        && f.severity == Severity::Unsupported
                        && !f.pointer.is_empty())
            })
            .map(|f| format!("{}: {:?}: {}", at(f), f.stage, f.message))
            .collect();
        if !errors.is_empty() {
            let more = errors.len().saturating_sub(10);
            let mut text = errors.into_iter().take(10).collect::<Vec<_>>().join("; ");
            if more > 0 {
                text.push_str(&format!("; and {more} more"));
            }
            return Err(text);
        }
        let rules = Rules::from_yaml_extended(text, own, |name| {
            let remote = load(name)?;
            // The inherited document can contain active options too. Validate it before
            // merging; YAML deserialization alone deliberately ignores many unknown keys.
            if self.is_embedded() {
                self.check(name, &remote)?;
            }
            Ok(remote)
        })
        .map_err(|e| format!("{file}: {e}"))?;
        let warnings = report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Unsupported)
            .map(|f| format!("{}: {}", at(f), f.message))
            .collect();
        Ok(Loaded { rules, warnings })
    }
}

/// The configuration of the repository `text` extends (`extends`), read from that
/// repository's default branch under every documented name; `None` without `extends`.
pub fn extended_text(api: &RepoApi, text: &str, prio: Priority) -> Option<Result<String, String>> {
    let (value, _) = schlange_config::yaml::parse(text).ok()?;
    let name = schlange_config::extends::target(&value).ok()??;
    for file in schlange_config::config_file::NAMES {
        match api.foreign_file(&name, file, prio) {
            Ok(Some(t)) => return Some(Ok(t)),
            Ok(None) => {}
            Err(e) => return Some(Err(format!("repository `{name}`: {e}"))),
        }
    }
    Some(Err(format!(
        "repository `{name}` has no configuration file"
    )))
}

/// Reads the configuration from the default branch under every documented name.
pub fn read_from_default_branch(api: &RepoApi, prio: Priority) -> Result<Found, Error> {
    use schlange_config::config_file::{Located, NAMES, locate};
    let branch = api.repository(prio)?.default_branch;
    let mut found = std::collections::BTreeMap::new();
    for path in NAMES {
        if let Some(t) = api.file_content(path, &branch, prio)? {
            found.insert(path, t);
        }
    }
    let located = locate(|p| found.contains_key(p));
    if let Some(e) = located.error() {
        return Ok(Found::Ambiguous(e));
    }
    Ok(match located {
        Located::One(p) => Found::One(p.to_string(), found.remove(p).unwrap_or_default()),
        _ => Found::None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check() -> Option<ConfigCheck> {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        ConfigCheck::locate(None, &root).ok()
    }

    #[test]
    fn rules_without_conditions_are_rejected_and_unsupported_features_warn() {
        let Some(c) = check() else {
            eprintln!("schema missing: run tools/fetch-mergify-sources");
            return;
        };
        let missing = "pull_request_rules:\n  - name: everything\n    actions:\n      queue:\n";
        let e = c.check(".mergify.yml", missing).unwrap_err();
        assert!(e.contains("conditions"), "{e}");
        let ok = "queue_rules:\n  - name: default\n    queue_conditions:\n      - base = main\n\
                  pull_request_rules:\n  - name: q\n    conditions:\n      - base = main\n    actions:\n      queue:\n";
        let l = c.check(".mergify.yml", ok).unwrap();
        assert_eq!(l.rules.pull_request_rules.len(), 1);
        let bad = "pull_request_rules:\n  - name: q\n    conditions:\n      - nosuchattribute = 1\n    actions: {}\n";
        assert!(c.check(".mergify.yml", bad).is_err());
    }
}
