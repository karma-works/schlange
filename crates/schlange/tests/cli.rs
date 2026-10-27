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

/// Critic 1abc (S-CLI): a usage error must not exit 2, which means "valid but unsupported";
/// it exits 64 (EX_USAGE), as JSON when `--json` was asked for. Help and version exit 0.
#[test]
fn usage_errors_exit_64() {
    let (code, _) = run(&["config", "validate"]);
    assert_eq!(code, 64);
    let (code, _) = run(&["no-such-command"]);
    assert_eq!(code, 64);
    let (code, out) = run(&["config", "validate", "--json", "--no-such-flag", "x.yml"]);
    assert_eq!(code, 64);
    let v: serde_json::Value = serde_json::from_str(&out).expect("JSON on stdout");
    assert_eq!(v["exit_code"], 64);
    assert!(
        v["error"].as_str().unwrap().contains("--no-such-flag"),
        "{v}"
    );
    assert_eq!(run(&["--help"]).0, 0);
    assert_eq!(run(&["--version"]).0, 0);
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

fn doctor_check(out: &str, name: &str) -> serde_json::Value {
    let v: serde_json::Value = serde_json::from_str(out).expect(out);
    v["checks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == name)
        .cloned()
        .unwrap_or_else(|| panic!("no check {name}: {out}"))
}

/// Critic 1abc: the doctor reads the repository's `.mergify.yml` and validates it, and it does
/// not count `partial` features as supported.
#[test]
fn doctor_validates_the_configuration_and_counts_partial_apart() {
    let (code, out) = run(&[
        "doctor",
        "--json",
        "--offline",
        "--config",
        &format!("{GOLDEN}/bad-schema.yml"),
    ]);
    assert_eq!(code, 1, "{out}");
    assert_eq!(doctor_check(&out, "configuration")["status"], "FAIL");
    let (code, out) = run(&[
        "doctor",
        "--json",
        "--offline",
        "--config",
        &format!("{GOLDEN}/arztagent.yml"),
    ]);
    assert_eq!(code, 0, "{out}");
    let c = doctor_check(&out, "configuration");
    assert_eq!(c["status"], "WARN", "{c}");
    assert!(
        c["detail"].as_str().unwrap().contains("not supported"),
        "{c}"
    );
    // Without --config the doctor finds `.mergify.yml` in the working directory.
    let dir = std::env::temp_dir().join(format!("schlange-doctor-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join(".mergify.yml"), "queue_rules: 3\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_schlange"))
        .args(["doctor", "--json", "--offline"])
        .current_dir(&dir)
        .env_remove("SCHLANGE_SCHEMA")
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).into_owned();
    let c = doctor_check(&text, "configuration");
    assert!(
        c["detail"].as_str().unwrap().contains(".mergify.yml"),
        "{c}"
    );
    assert_eq!(c["status"], "FAIL", "{c}");
    std::fs::remove_dir_all(&dir).ok();
    // Partial features are counted apart from conformant ones.
    let (_, out) = run(&["doctor", "--json", "--offline"]);
    let m = doctor_check(&out, "support-matrix");
    let detail = m["detail"].as_str().unwrap();
    assert!(
        detail.contains("partial") && detail.contains("conformant"),
        "{detail}"
    );
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
