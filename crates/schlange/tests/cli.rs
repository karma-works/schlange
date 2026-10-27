//! Exit codes of the CLI (AGENTS.md): 0 ok, 1 invalid/failed, 2 unsupported with --strict,
//! 3 environment problem.

use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn run(args: &[&str]) -> (i32, String) {
    let out = Command::new(env!("CARGO_BIN_EXE_schlange"))
        .args(args)
        .current_dir(root())
        .env_remove("SCHLANGE_SCHEMA")
        .output()
        .unwrap();
    (
        out.status.code().unwrap(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

const GOLDEN: &str = "crates/schlange-config/tests/golden";

#[test]
fn validate_exit_codes() {
    assert_eq!(
        run(&["config", "validate", &format!("{GOLDEN}/arztagent.yml")]).0,
        0
    );
    let (code, out) = run(&["config", "validate", &format!("{GOLDEN}/bad-schema.yml")]);
    assert_eq!(code, 1);
    assert!(out.contains("bad-schema.yml:3:19: error[schema]"), "{out}");
    assert_eq!(
        run(&[
            "config",
            "validate",
            "--strict",
            &format!("{GOLDEN}/unsupported.yml")
        ])
        .0,
        2
    );
    let (code, out) = run(&[
        "config",
        "validate",
        "--json",
        &format!("{GOLDEN}/bad-yaml.yml"),
    ]);
    assert_eq!(code, 1);
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["findings"][0]["line"], 4);
}

#[test]
fn missing_schema_is_exit_3() {
    let code = run(&[
        "config",
        "validate",
        "--schema",
        "/nonexistent/schema.json",
        &format!("{GOLDEN}/arztagent.yml"),
    ])
    .0;
    assert_eq!(code, 3);
}

#[test]
fn sim_and_doctor() {
    let (code, out) = run(&["sim", "run", "--json", "scenarios/p0-queue-conditions.yaml"]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["passed"], true);
    assert_eq!(run(&["doctor", "--json"]).0, 0);
    assert_eq!(run(&["support-matrix"]).0, 0);
}

#[test]
fn sim_run_on_real_git() {
    let (code, out) = run(&[
        "sim",
        "run",
        "--json",
        "--forge",
        "localgit",
        "scenarios/p1-semantic-conflict-compile.yaml",
    ]);
    assert_eq!(code, 0, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["passed"], true);
    assert_eq!(v["forge"], "localgit");
}

/// The critic's `merge-method.yml` (phase 1a): `reset_on_external_merge: never`,
/// `allow_queue_branch_edit: true` and `max_checks_retries: 2` were silently ignored. Now the
/// edit option is rejected with `rebase` (as Mergify does), and without it `--strict` flags
/// only what is not built (`rebase`), not the options that now are.
#[test]
fn critic_merge_method_config_is_rejected_or_flagged_precisely() {
    let (code, out) = run(&[
        "config",
        "validate",
        "scenarios/regressions/configs/merge-method.yml",
    ]);
    assert_eq!(code, 1, "{out}");
    assert!(out.contains("allow_queue_branch_edit"), "{out}");
    let (code, out) = run(&[
        "config",
        "validate",
        "--strict",
        "scenarios/regressions/configs/merge-method-no-edit.yml",
    ]);
    assert_eq!(code, 2, "{out}");
    assert!(out.contains("rebase"), "{out}");
    for built in ["reset_on_external_merge", "max_checks_retries"] {
        assert!(!out.contains(built), "{built} is built now:\n{out}");
    }
}
