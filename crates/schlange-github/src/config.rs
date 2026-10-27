//! The configuration as `serve` and `doctor` load it: `.mergify.yml` (or `.mergify/config.yml`,
//! `.github/mergify.yml`) from the repository's **default branch** only, through the same
//! pipeline as `schlange config validate`: YAML (stage 1), Mergify's schema (2), conditions
//! (3), semantics (4). Findings of stage 5 (features Schlange does not support yet) are
//! warnings: the configuration is used, and the warnings are logged and shown by the doctor.
//!
//! An invalid configuration is never applied: `serve` keeps the last valid one and reports
//! the errors (log, `/status`, doctor). A `pull_request_rules` entry without `conditions` is
//! a schema error, so it can never match every pull request.

use std::path::Path;

use schlange_config::rules::Rules;
use schlange_config::validate::Severity;
use schlange_config::{FeatureMap, Schema};

use crate::api::RepoApi;
use crate::client::{Error, Priority};

/// Where Mergify looks for its configuration, in order.
pub const CONFIG_PATHS: [&str; 3] = [".mergify.yml", ".mergify/config.yml", ".github/mergify.yml"];

/// The validator with the pinned schema and the feature map.
pub struct ConfigCheck {
    pub schema: Schema,
    pub features: FeatureMap,
}

/// A configuration that passed stages 1–4.
#[derive(Clone, Debug)]
pub struct Loaded {
    pub rules: Rules,
    /// Stage 5: what Schlange does not support yet (the configuration is still used).
    pub warnings: Vec<String>,
}

impl ConfigCheck {
    /// The schema from `explicit`, `$SCHLANGE_SCHEMA` or `.research/mergify/schema.json`
    /// above `start`, and the embedded feature map.
    pub fn locate(explicit: Option<&Path>, start: &Path) -> Result<ConfigCheck, String> {
        let path = schlange_config::schema::locate(explicit, start)
            .ok_or_else(|| schlange_config::schema::SchemaError::NotFound.to_string())?;
        let schema = Schema::load(&path).map_err(|e| e.to_string())?;
        let features = FeatureMap::embedded().map_err(|e| e.to_string())?;
        Ok(ConfigCheck { schema, features })
    }

    /// Validates a configuration text; `Err` lists the errors (stages 1–4).
    pub fn check(&self, file: &str, text: &str) -> Result<Loaded, String> {
        let report = schlange_config::validate(file, text, &self.schema, &self.features, false);
        let at = |f: &schlange_config::validate::Finding| match (f.line, f.column) {
            (Some(l), Some(c)) => format!("{file}:{l}:{c}"),
            _ => format!("{file} {}", f.pointer),
        };
        let errors: Vec<String> = report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Error)
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
        let rules = Rules::from_yaml(text).map_err(|e| format!("{file}: {e}"))?;
        let warnings = report
            .findings
            .iter()
            .filter(|f| f.severity == Severity::Unsupported)
            .map(|f| format!("{}: {}", at(f), f.message))
            .collect();
        Ok(Loaded { rules, warnings })
    }
}

/// Reads the configuration from the default branch: `(path, text)`, `None` when there is none.
pub fn read_from_default_branch(
    api: &RepoApi,
    prio: Priority,
) -> Result<Option<(String, String)>, Error> {
    let branch = api.repository(prio)?.default_branch;
    for path in CONFIG_PATHS {
        if let Some(t) = api.file_content(path, &branch, prio)? {
            return Ok(Some((path.to_string(), t)));
        }
    }
    Ok(None)
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
