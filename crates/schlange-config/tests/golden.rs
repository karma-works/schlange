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

/// Critic 1abc: `--strict` exits 2 only when the configuration uses something Schlange does
/// not support. A default or a value that switches a feature off is not a use; one that
/// switches it on is.
#[test]
fn strict_exits_2_only_for_real_gaps() {
    let schema = schema();
    let features = FeatureMap::embedded().unwrap();
    let strict = |src: &str| validate("t.yml", src, &schema, &features, true);
    assert_eq!(strict("{}\n").exit_code, exit::VALID);
    let off = strict("merge_queue:\n  skip_intermediate_results: false\n");
    assert_eq!(off.exit_code, exit::VALID, "{}", off.to_text());
    let on = strict("merge_queue:\n  skip_intermediate_results: true\n");
    assert_eq!(on.exit_code, exit::UNSUPPORTED, "{}", on.to_text());
    let plain = strict(
        "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - check-success = ci\n\
         pull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n",
    );
    // Off-valued defaults (`update_method: null`, `queue_branch_merge_method: null`) are not
    // uses. The one gap left is real: Mergify's default `branch_protection_injection_mode:
    // queue` adds the checks branch protection requires to the queue conditions; Schlange
    // does not read branch protection yet.
    let gaps: Vec<&str> = plain
        .support
        .iter()
        .filter(|r| !r.supported)
        .map(|r| r.id.as_str())
        .collect();
    assert_eq!(
        gaps,
        vec!["default:queue_rules.branch_protection_injection_mode"],
        "{}",
        plain.to_text()
    );
    let none = strict(
        "queue_rules:\n  - name: default\n    branch_protection_injection_mode: none\n    merge_method: squash\n    queue_conditions:\n      - check-success = ci\n",
    );
    assert_eq!(
        none.exit_code,
        exit::UNSUPPORTED,
        "`none` is the missing feature's own value: {}",
        none.to_text()
    );
}
