//! Coverage test of the feature map (`.agents/skills/verify-schlange/features/`):
//! - every inventory entry belongs to exactly one feature file, and every pattern matches;
//! - every referenced problem ID (`MQ-…`, `W-…`) exists in `docs/research/`;
//! - every P1 feature names at least one `MQ-` ID;
//! - every feature file has exactly the four sections of the verification skill;
//! - the Schlange features of KONZEPT.md, section 8, exist.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use schlange_config::FeatureMap;
use schlange_config::features::{FEATURE_DIR, Priority};

const SECTIONS: [&str; 4] = [
    "## Sub-features",
    "## How to get to it (user POV)",
    "## Driving it with schlange",
    "## Gotchas",
];

const S_FEATURES: [&str; 8] = [
    "S-CLI",
    "S-EXPLAIN",
    "S-SIM",
    "S-SIMULATE",
    "S-MCP",
    "S-SHADOW",
    "S-AUDIT",
    "S-DOCTOR",
];

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn map() -> FeatureMap {
    FeatureMap::load_dir(&root().join(FEATURE_DIR)).expect("feature map loads")
}

/// IDs defined in the first column of the research tables (`| MQ-01 |`, `| W-01 |`).
fn known_problem_ids() -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for file in ["merge-queue-probleme.md", "mergify-schwaechen.md"] {
        let text = std::fs::read_to_string(root().join("docs/research").join(file)).unwrap();
        for line in text.lines() {
            if let Some(cell) = line.strip_prefix("| ") {
                let id = cell.split(' ').next().unwrap_or("");
                if id.starts_with("MQ-") || id.starts_with("W-") {
                    ids.insert(id.to_string());
                }
            }
        }
    }
    ids
}

#[test]
fn every_inventory_entry_has_exactly_one_feature() {
    let map = map();
    assert!(map.inventory.total > 700, "inventory looks truncated");
    assert_eq!(map.inventory.total, map.inventory.entries.len());
    let problems = map.coverage_problems();
    assert!(
        problems.is_empty(),
        "coverage problems:\n{}",
        problems.join("\n")
    );
}

#[test]
fn problem_ids_exist() {
    let known = known_problem_ids();
    assert_eq!(known.iter().filter(|i| i.starts_with("MQ-")).count(), 53);
    assert_eq!(known.iter().filter(|i| i.starts_with("W-")).count(), 26);
    let mut unknown = Vec::new();
    for f in &map().features {
        for p in &f.problems {
            if !known.contains(p) {
                unknown.push(format!("{}: {p}", f.id));
            }
        }
    }
    assert!(unknown.is_empty(), "unknown problem IDs: {unknown:?}");
}

#[test]
fn p1_features_name_an_mq_case() {
    let missing: Vec<_> = map()
        .features
        .iter()
        .filter(|f| f.priority == Priority::P1 && !f.problems.iter().any(|p| p.starts_with("MQ-")))
        .map(|f| f.id.clone())
        .collect();
    assert!(missing.is_empty(), "P1 features without MQ ID: {missing:?}");
}

#[test]
fn feature_files_have_the_four_sections() {
    for f in &map().features {
        let h2: Vec<&str> = f.body.lines().filter(|l| l.starts_with("## ")).collect();
        assert_eq!(
            h2, SECTIONS,
            "{}: sections must be exactly {SECTIONS:?}",
            f.file
        );
        assert!(
            f.body.trim_start().starts_with("# "),
            "{}: needs an H1",
            f.file
        );
    }
}

#[test]
fn schlange_features_exist() {
    let map = map();
    for id in S_FEATURES {
        assert!(map.by_id(id).is_some(), "{id} missing");
    }
}

#[test]
fn embedded_map_matches_the_files() {
    let files = map();
    let embedded = FeatureMap::embedded().unwrap();
    let ids = |m: &FeatureMap| {
        m.features
            .iter()
            .map(|f| (f.id.clone(), f.status))
            .collect::<Vec<_>>()
    };
    assert_eq!(
        ids(&files),
        ids(&embedded),
        "rebuild: the embedded map is stale"
    );
}

#[test]
fn readme_lists_every_feature() {
    let readme = std::fs::read_to_string(root().join(FEATURE_DIR).join("README.md")).unwrap();
    for f in &map().features {
        assert!(
            readme.contains(&format!("]({})", f.file)),
            "README.md misses {}",
            f.file
        );
    }
}
