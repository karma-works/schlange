//! The feature map: one Markdown file per feature with YAML frontmatter, plus the generated
//! inventory (`_inventory.json`). Every inventory entry belongs to exactly one feature; the
//! support matrix of the validator (stage 5) is derived from the features' `status` and, for
//! `partial` features, their `unsupported` list.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

/// Directory of the feature map, relative to the repository root.
pub const FEATURE_DIR: &str = ".agents/skills/verify-schlange/features";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Deserialize, Serialize)]
pub enum Priority {
    P1,
    P2,
    P3,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Missing,
    Partial,
    Conformant,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Missing => "missing",
            Status::Partial => "partial",
            Status::Conformant => "conformant",
        }
    }

    /// Schlange supports a feature once it is at least partly built. Entries of a `partial`
    /// feature can still be unsupported; see [`FeatureMap::support`].
    pub fn is_supported(self) -> bool {
        !matches!(self, Status::Missing)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    pub id: String,
    pub title: String,
    pub priority: Priority,
    pub arztagent: bool,
    pub status: Status,
    /// Inventory IDs or patterns (`*` matches any text).
    #[serde(default)]
    pub inventory: Vec<String>,
    /// Patterns of the feature's own inventory entries that are not built yet (only for
    /// `partial` features). Stage 5 reports a used entry matching one as unsupported.
    #[serde(default)]
    pub unsupported: Vec<String>,
    /// Problem IDs (`MQ-…`, `W-…`).
    #[serde(default)]
    pub problems: Vec<String>,
    pub source: String,
    #[serde(skip)]
    pub file: String,
    #[serde(skip)]
    pub body: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct InventoryEntry {
    pub id: String,
    pub kind: String,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub ty: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Inventory {
    pub counts: BTreeMap<String, usize>,
    pub total: usize,
    pub entries: Vec<InventoryEntry>,
}

#[derive(Debug, thiserror::Error)]
pub enum FeatureError {
    #[error("{file}: {message}")]
    Format { file: String, message: String },
    #[error("cannot read {path}: {source}")]
    Io {
        path: String,
        source: std::io::Error,
    },
}

/// Glob match where `*` matches any (possibly empty) text.
pub fn pattern_matches(pattern: &str, id: &str) -> bool {
    let parts: Vec<&str> = pattern.split('*').collect();
    if parts.len() == 1 {
        return pattern == id;
    }
    let (first, last) = (parts[0], parts[parts.len() - 1]);
    if !id.starts_with(first) || id.len() < first.len() + last.len() || !id.ends_with(last) {
        return false;
    }
    let mut rest = &id[first.len()..id.len() - last.len()];
    for mid in &parts[1..parts.len() - 1] {
        match rest.find(mid) {
            Some(i) => rest = &rest[i + mid.len()..],
            None => return false,
        }
    }
    true
}

/// Parses one feature file.
pub fn parse_feature(file: &str, text: &str) -> Result<Feature, FeatureError> {
    let err = |message: String| FeatureError::Format {
        file: file.to_string(),
        message,
    };
    let rest = text
        .strip_prefix("---\n")
        .ok_or_else(|| err("missing YAML frontmatter".into()))?;
    let end = rest
        .find("\n---\n")
        .ok_or_else(|| err("unterminated YAML frontmatter".into()))?;
    let mut feature: Feature =
        serde_yaml_ng::from_str(&rest[..end]).map_err(|e| err(e.to_string()))?;
    feature.file = file.to_string();
    feature.body = rest[end + 5..].to_string();
    Ok(feature)
}

/// Why stage 5 considers an entry unsupported.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Unsupported {
    /// The owning feature has `status: missing`.
    FeatureMissing,
    /// The owning feature is `partial` and lists the entry as not built.
    NotBuilt { pattern: String },
}

impl Unsupported {
    pub fn describe(&self) -> String {
        match self {
            Unsupported::FeatureMissing => "feature not built".into(),
            Unsupported::NotBuilt { pattern } => {
                format!("not built in this partial feature (`{pattern}`)")
            }
        }
    }
}

impl Feature {
    /// `None` when Schlange supports the (owned) entry `id`, else the reason.
    pub fn support(&self, id: &str) -> Option<Unsupported> {
        if !self.status.is_supported() {
            return Some(Unsupported::FeatureMissing);
        }
        self.unsupported
            .iter()
            .find(|p| pattern_matches(p, id))
            .map(|p| Unsupported::NotBuilt { pattern: p.clone() })
    }
}

pub struct FeatureMap {
    pub features: Vec<Feature>,
    pub inventory: Inventory,
    ids: BTreeSet<String>,
}

impl FeatureMap {
    pub fn new(features: Vec<Feature>, inventory: Inventory) -> Self {
        let ids = inventory.entries.iter().map(|e| e.id.clone()).collect();
        FeatureMap {
            features,
            inventory,
            ids,
        }
    }

    /// Loads the map from a directory (`*.md` except `README.md`, plus `_inventory.json`).
    pub fn load_dir(dir: &Path) -> Result<Self, FeatureError> {
        let io = |path: &Path, source| FeatureError::Io {
            path: path.display().to_string(),
            source,
        };
        let inv_path = dir.join("_inventory.json");
        let inv_text = std::fs::read_to_string(&inv_path).map_err(|e| io(&inv_path, e))?;
        let inventory: Inventory =
            serde_json::from_str(&inv_text).map_err(|e| FeatureError::Format {
                file: inv_path.display().to_string(),
                message: e.to_string(),
            })?;
        let mut names: Vec<_> = std::fs::read_dir(dir)
            .map_err(|e| io(dir, e))?
            .filter_map(Result::ok)
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".md") && n != "README.md")
            .collect();
        names.sort();
        let mut features = Vec::new();
        for name in names {
            let path = dir.join(&name);
            let text = std::fs::read_to_string(&path).map_err(|e| io(&path, e))?;
            features.push(parse_feature(&name, &text)?);
        }
        Ok(Self::new(features, inventory))
    }

    /// The map embedded at build time (see `build.rs`).
    pub fn embedded() -> Result<Self, FeatureError> {
        let bundle = include_str!(concat!(env!("OUT_DIR"), "/feature_bundle.txt"));
        let mut features = Vec::new();
        let mut inventory = None;
        for chunk in bundle.split("\u{1e}FILE ").filter(|c| !c.is_empty()) {
            let (name, text) = chunk.split_once('\n').unwrap_or((chunk, ""));
            if name == "_inventory.json" {
                inventory = Some(
                    serde_json::from_str(text).map_err(|e| FeatureError::Format {
                        file: name.into(),
                        message: e.to_string(),
                    })?,
                );
            } else {
                features.push(parse_feature(name, text)?);
            }
        }
        let inventory = inventory.ok_or_else(|| FeatureError::Format {
            file: "_inventory.json".into(),
            message: "not embedded".into(),
        })?;
        Ok(Self::new(features, inventory))
    }

    pub fn contains(&self, id: &str) -> bool {
        self.ids.contains(id)
    }

    /// Features whose inventory patterns match `id` (exactly one when the map is consistent).
    pub fn owners(&self, id: &str) -> Vec<&Feature> {
        self.features
            .iter()
            .filter(|f| f.inventory.iter().any(|p| pattern_matches(p, id)))
            .collect()
    }

    pub fn owner(&self, id: &str) -> Option<&Feature> {
        self.owners(id).into_iter().next()
    }

    pub fn by_id(&self, id: &str) -> Option<&Feature> {
        self.features.iter().find(|f| f.id == id)
    }

    /// Consistency problems: entries with zero or several owners, patterns that match
    /// nothing, duplicate feature IDs, file names that differ from the ID.
    pub fn coverage_problems(&self) -> Vec<String> {
        let mut problems = Vec::new();
        for entry in &self.inventory.entries {
            let owners = self.owners(&entry.id);
            match owners.len() {
                1 => {}
                0 => problems.push(format!("{} belongs to no feature", entry.id)),
                _ => problems.push(format!(
                    "{} belongs to several features: {}",
                    entry.id,
                    owners
                        .iter()
                        .map(|f| f.id.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )),
            }
        }
        let mut seen = BTreeSet::new();
        for f in &self.features {
            if !seen.insert(&f.id) {
                problems.push(format!("feature id {} is used twice", f.id));
            }
            if f.file != format!("{}.md", f.id) {
                problems.push(format!("{}: file name must be {}.md", f.file, f.id));
            }
            for p in &f.inventory {
                if !self.ids.iter().any(|id| pattern_matches(p, id)) {
                    problems.push(format!(
                        "{}: pattern `{p}` matches no inventory entry",
                        f.id
                    ));
                }
            }
            if !f.unsupported.is_empty() && f.status != Status::Partial {
                problems.push(format!(
                    "{}: `unsupported` is only allowed with status partial, found {}",
                    f.id,
                    f.status.as_str()
                ));
            }
            for p in &f.unsupported {
                let own = self.ids.iter().any(|id| {
                    pattern_matches(p, id) && f.inventory.iter().any(|i| pattern_matches(i, id))
                });
                if !own {
                    problems.push(format!(
                        "{}: unsupported pattern `{p}` matches none of the feature's own inventory entries",
                        f.id
                    ));
                }
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patterns() {
        assert!(pattern_matches("a:b", "a:b"));
        assert!(!pattern_matches("a:b", "a:bc"));
        assert!(pattern_matches(
            "config:queue_rules.batch_size.*",
            "config:queue_rules.batch_size.max"
        ));
        assert!(!pattern_matches(
            "config:queue_rules.batch_size.*",
            "config:queue_rules.batch_size"
        ));
        assert!(pattern_matches(
            "api:*/scheduled_freeze*",
            "api:GET /repos/{o}/{r}/scheduled_freeze"
        ));
        assert!(pattern_matches(
            "cli:mergify stack *",
            "cli:mergify stack push"
        ));
        assert!(!pattern_matches(
            "cli:mergify stack *",
            "cli:mergify stacks"
        ));
        assert!(pattern_matches("a*b*c", "abc"));
        assert!(!pattern_matches("ab*ba", "aba"));
    }

    #[test]
    fn frontmatter() {
        let text = "---\nid: x\ntitle: \"X\"\npriority: P1\narztagent: true\nstatus: missing\n\
                    inventory:\n  - \"attr:base\"\nproblems:\n  - \"MQ-01\"\nsource: \"s\"\n---\n\n# X\n";
        let f = parse_feature("x.md", text).unwrap();
        assert_eq!(f.priority, Priority::P1);
        assert_eq!(f.status, Status::Missing);
        assert_eq!(f.inventory, vec!["attr:base"]);
        assert!(f.unsupported.is_empty());
        assert_eq!(f.support("attr:base"), Some(Unsupported::FeatureMissing));
        assert!(f.body.contains("# X"));
        assert!(parse_feature("y.md", "# no frontmatter").is_err());
    }

    #[test]
    fn unsupported_entries_of_a_partial_feature() {
        let text = "---\nid: x\ntitle: \"X\"\npriority: P1\narztagent: true\nstatus: partial\n\
                    inventory:\n  - \"attr:*\"\nunsupported:\n  - \"attr:b*\"\nsource: \"s\"\n---\n\n# X\n";
        let f = parse_feature("x.md", text).unwrap();
        assert_eq!(f.support("attr:author"), None);
        assert_eq!(
            f.support("attr:base"),
            Some(Unsupported::NotBuilt {
                pattern: "attr:b*".into()
            })
        );
    }

    #[test]
    fn unsupported_patterns_must_hit_own_entries() {
        let feature = |id: &str, status: &str, inv: &str, unsup: &str| {
            let text = format!(
                "---\nid: {id}\ntitle: \"X\"\npriority: P2\narztagent: false\nstatus: {status}\n\
                 inventory:\n  - \"{inv}\"\nunsupported:\n  - \"{unsup}\"\nsource: \"s\"\n---\n\n# X\n"
            );
            parse_feature(&format!("{id}.md"), &text).unwrap()
        };
        let inventory = Inventory {
            counts: BTreeMap::new(),
            total: 2,
            entries: ["attr:a", "attr:b"]
                .map(|id| InventoryEntry {
                    id: id.into(),
                    kind: "attr".into(),
                    ty: None,
                })
                .to_vec(),
        };
        let map = FeatureMap::new(
            vec![
                feature("fa", "partial", "attr:a", "attr:b"),
                feature("fb", "conformant", "attr:b", "attr:b"),
            ],
            inventory,
        );
        let problems = map.coverage_problems();
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("fa: unsupported pattern `attr:b`")),
            "{problems:?}"
        );
        assert!(
            problems
                .iter()
                .any(|p| p.starts_with("fb: `unsupported` is only allowed")),
            "{problems:?}"
        );
        assert_eq!(problems.len(), 2, "{problems:?}");
    }

    #[test]
    fn embedded_map_is_consistent() {
        let map = FeatureMap::embedded().unwrap();
        assert!(map.features.len() > 50);
        assert_eq!(map.coverage_problems(), Vec::<String>::new());
    }
}
