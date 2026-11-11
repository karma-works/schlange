//! Golden files for `schlange config validate`: every `tests/golden/*.yml` is validated and the
//! text and JSON output compared with `*.txt` and `*.json`. `UPDATE_GOLDEN=1` rewrites them.
//! Needs the pinned schema (`tools/fetch-mergify-sources`).

use std::path::{Path, PathBuf};

use schlange_config::validate::exit;
use schlange_config::{FeatureMap, Schema, validate};

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn schema() -> Schema {
    let path = schlange_config::schema::locate(None, &root())
        .expect("schema missing: run tools/fetch-mergify-sources");
    Schema::load(&path).expect("schema loads")
}

fn check(path: &Path, actual: &str) {
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::write(path, actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(path)
        .unwrap_or_else(|_| panic!("{} missing; run with UPDATE_GOLDEN=1", path.display()));
    assert_eq!(expected, actual, "golden file {} differs", path.display());
}

#[test]
fn golden_files() {
    let schema = schema();
    let features = FeatureMap::embedded().unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let mut cases: Vec<_> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|e| e == "yml"))
        .collect();
    cases.sort();
    assert!(cases.len() >= 5);
    for case in cases {
        let name = case.file_name().unwrap().to_string_lossy().into_owned();
        let source = std::fs::read_to_string(&case).unwrap();
        let report = validate(&name, &source, &schema, &features, false);
        check(&case.with_extension("txt"), &report.to_text());
        let json = serde_json::to_string_pretty(&report).unwrap() + "\n";
        check(&case.with_extension("json"), &json);
        let expected_exit = if name.starts_with("bad-") {
            exit::INVALID
        } else {
            exit::VALID
        };
        assert_eq!(report.exit_code, expected_exit, "{name}");
    }
}

#[test]
fn arztagent_is_valid_and_strict_exit_codes() {
    let schema = schema();
    let features = FeatureMap::embedded().unwrap();
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden");
    let arzt = std::fs::read_to_string(dir.join("arztagent.yml")).unwrap();
    let report = validate("arztagent.yml", &arzt, &schema, &features, false);
    assert_eq!(report.exit_code, exit::VALID);
    let strict_arzt = validate("arztagent.yml", &arzt, &schema, &features, true);
    assert_eq!(
        strict_arzt.exit_code,
        exit::VALID,
        "{}",
        strict_arzt.to_text()
    );
    assert!(
        report
            .findings
            .iter()
            .all(|f| f.severity != schlange_config::validate::Severity::Error)
    );
    // Every attribute and action Arztagent uses is found and belongs to a P1 feature.
    for id in [
        "attr:files",
        "attr:dependabot-update-type",
        "attr:sender",
        "action:queue",
        "enum:merge_method:squash",
        "default:merge_queue.max_parallel_checks",
        "default:queue_rules.checks_timeout",
    ] {
        let row = report
            .support
            .iter()
            .find(|r| r.id == id)
            .unwrap_or_else(|| panic!("{id} not used"));
        assert_eq!(
            row.priority,
            schlange_config::features::Priority::P1,
            "{id}"
        );
    }
    let unsupported = std::fs::read_to_string(dir.join("unsupported.yml")).unwrap();
    let strict = validate("unsupported.yml", &unsupported, &schema, &features, true);
    assert_eq!(strict.exit_code, exit::UNSUPPORTED);
}

/// An option the schema marks as experimental is never silently supported: every such
/// inventory entry belongs to a feature that reports it as unsupported, and explicit settings
/// stay visible even where they look "off".
#[test]
fn experimental_options_are_not_silently_supported() {
    let features = FeatureMap::embedded().unwrap();
    let experimental: Vec<&str> = features
        .inventory
        .entries
        .iter()
        .filter(|e| e.experimental)
        .map(|e| e.id.as_str())
        .collect();
    assert!(
        experimental.contains(&"config:conflict_resolvers"),
        "the pinned schema marks conflict_resolvers as experimental: {experimental:?}"
    );
    for id in &experimental {
        let owner = features
            .owner(id)
            .unwrap_or_else(|| panic!("{id} has no feature"));
        assert!(
            owner.support(id).is_some(),
            "{id} is experimental but {} supports it",
            owner.id
        );
    }

    let schema = schema();
    for (src, expected) in [
        (
            "conflict_resolvers:\n  - auto: true\n",
            "config:conflict_resolvers.auto",
        ),
        (
            "conflict_resolvers:\n  - paths: [\"*.lock\"]\n    strategy: text\n",
            "config:conflict_resolvers.strategy",
        ),
        (
            "conflict_resolvers:\n  - package-manager: npm\n    directories: [\"/\"]\n",
            "config:conflict_resolvers",
        ),
        (
            "conflict_resolvers:\n  - language: markdown\n",
            "config:conflict_resolvers",
        ),
        // An explicit empty list is still an explicit choice of an experimental section.
        ("conflict_resolvers: []\n", "config:conflict_resolvers"),
    ] {
        let report = validate("t.yml", src, &schema, &features, true);
        assert_eq!(
            report.exit_code,
            exit::UNSUPPORTED,
            "{src}: {}",
            report.to_text()
        );
        assert!(
            report
                .support
                .iter()
                .any(|r| r.id == expected && !r.supported),
            "{src}: {}",
            report.to_text()
        );
    }
}

/// The schema dropped the experimental `merge_queue.batch_pull_request` (now a setting of
/// Mergify's merge queue settings API, not of the configuration file) and
/// `merge_queue.resolve_conflicts` (superseded by the top-level `conflict_resolvers`). A
/// configuration that still sets them is invalid, whatever the value: never accepted, never
/// silently ignored.
#[test]
fn removed_experimental_merge_queue_options_are_invalid() {
    let schema = schema();
    let features = FeatureMap::embedded().unwrap();
    for id in [
        "config:merge_queue.batch_pull_request",
        "config:merge_queue.resolve_conflicts",
    ] {
        assert!(!features.contains(id), "{id} is back in the inventory");
    }
    for (setting, key) in [
        ("batch_pull_request: true", "batch_pull_request"),
        ("batch_pull_request: false", "batch_pull_request"),
        ("batch_pull_request: null", "batch_pull_request"),
        ("resolve_conflicts: {}", "resolve_conflicts"),
        (
            "resolve_conflicts: {pull_requests: all, comment: true}",
            "resolve_conflicts",
        ),
        ("resolve_conflicts: null", "resolve_conflicts"),
    ] {
        let src = format!("merge_queue:\n  {setting}\n");
        for strict in [false, true] {
            let report = validate("t.yml", &src, &schema, &features, strict);
            assert_eq!(
                report.exit_code,
                exit::INVALID,
                "{setting}: {}",
                report.to_text()
            );
            assert!(
                report.to_text().contains(&format!("unknown key `{key}`")),
                "{setting}: {}",
                report.to_text()
            );
        }
    }
}

/// Critic 1abc: `--strict` exits 2 only when the configuration uses something Schlange does
/// not support. A default or a value that switches a feature off is not a use; one that
/// switches it on is.
#[test]
fn strict_exits_2_only_for_real_gaps() {
    let schema = schema();
    let features = FeatureMap::embedded().unwrap();
    let strict = |src: &str| validate("t.yml", src, &schema, &features, true);
    assert_eq!(strict("{}\n").exit_code, exit::VALID);
    let off = strict(
        "queue_rules:\n  - name: default\n    merge_method: squash\n    update_method: null\n",
    );
    assert_eq!(off.exit_code, exit::VALID, "{}", off.to_text());
    let on = strict(
        "queue_rules:\n  - name: default\n    merge_method: squash\n    update_method: rebase\n",
    );
    assert_eq!(on.exit_code, exit::UNSUPPORTED, "{}", on.to_text());
    let plain = strict(
        "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - check-success = ci\n\
         pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n",
    );
    // Off-valued defaults (`update_method: null`, `queue_branch_merge_method: null`) are not
    // uses, and Mergify's default `branch_protection_injection_mode: queue` is built (the
    // checks rulesets require join the queue conditions): a plain queue configuration has no
    // gap.
    let gaps: Vec<&str> = plain
        .support
        .iter()
        .filter(|r| !r.supported)
        .map(|r| r.id.as_str())
        .collect();
    assert!(gaps.is_empty(), "{}", plain.to_text());
    assert_eq!(plain.exit_code, exit::VALID, "{}", plain.to_text());
    let none = strict(
        "queue_rules:\n  - name: default\n    branch_protection_injection_mode: none\n    merge_method: squash\n    queue_conditions:\n      - check-success = ci\n",
    );
    assert_eq!(none.exit_code, exit::VALID, "{}", none.to_text());
}

/// Critic e8a0022 M3: an unsupported attribute is reported at its condition's own line (also
/// inside `or`), not at the line of the list's key.
#[test]
fn unsupported_attributes_point_at_their_condition() {
    let schema = schema();
    let features = FeatureMap::embedded().unwrap();
    let src = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - check-success = ci\n      - schedule = Mon-Fri 09:00-17:00\n      - or:\n          - check-success = a\n          - \"#approved-reviews-by >= 1\"\n";
    let report = validate("t.yml", src, &schema, &features, true);
    let text = report.to_text();
    assert!(text.contains("attr:schedule (6:9)"), "{text}");
    assert!(text.contains("attr:approved-reviews-by (9:13)"), "{text}");
}
