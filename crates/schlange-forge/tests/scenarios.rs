//! Every scenario under `scenarios/` passes (expectations and invariants), and its event log
//! matches the golden file `tests/golden/<name>.log`. Update with
//! `UPDATE_GOLDEN=1 cargo test -p schlange-forge --test scenarios` and review the diff.

use std::path::{Path, PathBuf};

use schlange_forge::scenario::run_file;

fn scenarios() -> Vec<PathBuf> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scenarios");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("scenarios dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "yaml"))
        .collect();
    files.sort();
    files
}

fn golden(name: &str, actual: &str) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(format!("{name}.log"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().expect("dir")).expect("mkdir");
        std::fs::write(&path, actual).expect("write golden");
        return;
    }
    let expected = std::fs::read_to_string(&path).unwrap_or_else(|_| {
        panic!(
            "missing golden {}; run with UPDATE_GOLDEN=1",
            path.display()
        )
    });
    assert_eq!(
        expected, actual,
        "event log of {name} changed; review and run with UPDATE_GOLDEN=1"
    );
}

#[test]
fn every_scenario_passes_with_invariants_checked() {
    let files = scenarios();
    assert!(files.len() >= 13, "P1 scenarios present");
    for file in files {
        let report = run_file(&file).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        assert!(report.passed, "{}\n{}", file.display(), report.to_text());
        for id in ["I0", "I1", "I2", "I3", "I4"] {
            assert_eq!(
                report.invariant(id).status,
                "checked",
                "{} {id}",
                file.display()
            );
        }
        golden(&report.scenario, &report.log_text());
    }
}

#[test]
fn runs_are_deterministic() {
    for file in scenarios() {
        let a = run_file(&file).expect("runs");
        let b = run_file(&file).expect("runs");
        assert_eq!(a.log_text(), b.log_text(), "{}", file.display());
        assert_eq!(
            format!("{:?}", a.disturbed.map(|d| d.end_state)),
            format!("{:?}", b.disturbed.map(|d| d.end_state))
        );
    }
}
