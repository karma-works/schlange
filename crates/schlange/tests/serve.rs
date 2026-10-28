//! The real binary: `schlange serve` and `schlange doctor` against the fake GitHub, configured
//! through the environment as in production.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use schlange_github_fake::{CiOn, CiRule, FakeConfig, FakeGithub};

fn key_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../schlange-github-fake/fixtures/test-app-key.pem")
}

fn env_for(cmd: &mut Command, gh: &FakeGithub) {
    let cfg = gh.config();
    cmd.env("SCHLANGE_GITHUB_APP_ID", cfg.app_id.to_string())
        .env("SCHLANGE_GITHUB_PRIVATE_KEY", key_path())
        .env("SCHLANGE_GITHUB_API_URL", gh.url())
        .env("SCHLANGE_REPOS", cfg.full_name())
        .env("SCHLANGE_INSECURE_TEST_API", "true")
        .env_remove("SCHLANGE_SCHEMA");
}

struct Kill(Child);

impl Drop for Kill {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_lands_a_pull_request_and_doctor_checks_the_app() {
    let gh = FakeGithub::start(FakeConfig::default());
    gh.set_ci(vec![CiRule::success("ci", CiOn::Both)]);
    let dir = std::env::temp_dir().join(format!("schlange-serve-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let config = dir.join("mergify.yml");
    std::fs::write(
        &config,
        "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - base = main\n      - check-success = ci\n\
         pull_request_rules:\n  - name: all\n    conditions: []\n    actions:\n      queue:\n",
    )
    .unwrap();
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_schlange"));
    env_for(&mut cmd, &gh);
    cmd.args(["serve", "--config"])
        .arg(&config)
        .env("SCHLANGE_LISTEN", format!("127.0.0.1:{port}"))
        .env("SCHLANGE_WEBHOOK_SECRET", "fake-webhook-secret")
        .env("SCHLANGE_DATA_DIR", dir.join("data"))
        .env("SCHLANGE_ADMIN_TOKEN", "serve-test-admin-token")
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let _child = Kill(cmd.spawn().unwrap());
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let health = format!("http://127.0.0.1:{port}/healthz");
    let deadline = Instant::now() + Duration::from_secs(20);
    while agent.get(&health).call().map(|r| r.status().as_u16()).ok() != Some(200) {
        assert!(Instant::now() < deadline, "serve did not come up");
        std::thread::sleep(Duration::from_millis(20));
    }
    gh.set_webhook_target(Some(format!("http://127.0.0.1:{port}/webhook")));
    let n = gh.open_pr(
        "someone",
        "Add a",
        &[("a.txt".into(), "a\n".into())],
        false,
        &[],
    );
    while !gh.pr(n).unwrap().merged {
        assert!(Instant::now() < deadline, "not merged: {:?}", gh.requests());
        std::thread::sleep(Duration::from_millis(20));
    }
    let v = gh.invariant_violations(false);
    assert!(v.is_empty(), "{v:?}");
    // `/status` shows pull request data: only with the admin token.
    let anon = agent
        .get(format!("http://127.0.0.1:{port}/status"))
        .call()
        .unwrap();
    assert_eq!(anon.status().as_u16(), 401);
    let status = agent
        .get(format!("http://127.0.0.1:{port}/status"))
        .header("authorization", "Bearer serve-test-admin-token")
        .call()
        .unwrap()
        .body_mut()
        .read_to_string()
        .unwrap();
    assert!(status.contains("\"merged\""), "{status}");
    assert!(status.contains("\"persistent\": true"), "{status}");
    assert!(dir.join("data/schlange.db").is_file());

    // `schlange queue`: the queue API of serve with the admin token.
    let queue = |args: &[&str], token: &str| {
        let out = Command::new(env!("CARGO_BIN_EXE_schlange"))
            .arg("queue")
            .args(args)
            .args(["--url", &format!("http://127.0.0.1:{port}")])
            .args(["--repo", &gh.config().full_name()])
            .env("SCHLANGE_ADMIN_TOKEN", token)
            .env_remove("SCHLANGE_ADMIN_TOKEN_FILE")
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };
    let (code, out) = queue(
        &["pause", "--reason", "CI incident"],
        "serve-test-admin-token",
    );
    assert_eq!((code, out.trim()), (Some(0), "paused: CI incident"));
    let (code, out) = queue(&["status"], "serve-test-admin-token");
    assert_eq!(code, Some(0));
    assert!(out.contains("PAUSED: CI incident"), "{out}");
    let (code, out) = queue(&["status", "--json"], "wrong-token-of-16-bytes");
    assert_eq!(code, Some(1), "{out}");
    assert!(out.contains("\"status\": 401"), "{out}");
    let (code, out) = queue(&["unpause"], "serve-test-admin-token");
    assert_eq!((code, out.trim()), (Some(0), "not paused"));

    // `explain`, `events`, `audit`, `config simulate`, `shadow report` against serve's API.
    let cli = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_schlange"))
            .args(args)
            .args(["--url", &format!("http://127.0.0.1:{port}")])
            .args(["--repo", &gh.config().full_name()])
            .env("SCHLANGE_ADMIN_TOKEN", "serve-test-admin-token")
            .env_remove("SCHLANGE_ADMIN_TOKEN_FILE")
            .output()
            .unwrap();
        (
            out.status.code(),
            String::from_utf8_lossy(&out.stdout).to_string(),
        )
    };
    let num = n.to_string();
    let (code, out) = cli(&["explain", &num]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains(&format!("#{n} Add a: merged")), "{out}");
    let (code, out) = cli(&["events", "--pr", &num, "--json"]);
    assert_eq!(code, Some(0), "{out}");
    for t in [
        "action.queue.enter",
        "action.queue.checks_start",
        "action.queue.merged",
    ] {
        assert!(out.contains(t), "{t} missing: {out}");
    }
    let (code, out) = cli(&["events"]);
    assert_eq!(code, Some(0));
    assert!(out.contains("queue.pause.create"), "{out}");
    let (code, out) = cli(&["audit", "--pr", &num]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.starts_with(&format!("#{n} car ")), "{out}");
    let strict = dir.join("strict.yml");
    std::fs::write(
        &strict,
        "queue_rules:\n  - name: default\n    queue_conditions:\n      - label = ready\n\
         pull_request_rules:\n  - name: all\n    conditions: []\n    actions:\n      queue:\n",
    )
    .unwrap();
    let (code, out) = cli(&["config", "simulate", strict.to_str().unwrap(), "--pr", &num]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("merged"), "{out}");
    std::fs::write(&strict, "queue_rules: 3\n").unwrap();
    let (code, _) = cli(&["config", "simulate", strict.to_str().unwrap(), "--pr", &num]);
    assert_eq!(code, Some(1), "an invalid configuration is refused");
    let (code, out) = cli(&["shadow", "report"]);
    assert_eq!(code, Some(0), "{out}");
    assert!(out.contains("0 agree, 0 disagree"), "{out}");
    // The lease: one leader (serve takes it by default).
    assert!(status.contains("\"role\": \"leader\""), "{status}");

    // Doctor with the same environment: the GitHub checks run and none fails. It validates the
    // repository's `.mergify.yml` from the default branch.
    gh.push_base(&[(
        ".mergify.yml".into(),
        std::fs::read_to_string(&config).unwrap(),
    )]);
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_schlange"));
    env_for(&mut cmd, &gh);
    let out = cmd.args(["doctor", "--json"]).output().unwrap();
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let status_of = |name: &str| {
        v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["name"] == name)
            .map(|c| c["status"].as_str().unwrap().to_string())
            .unwrap()
    };
    assert_eq!(status_of("github-app"), "OK", "{v}");
    assert_eq!(status_of("webhooks"), "OK", "{v}");
    assert_eq!(status_of("rate-limit"), "OK");
    assert_ne!(status_of("configuration"), "FAIL", "{v}");
    assert_eq!(out.status.code(), Some(0), "{v}");
    // Offline (the container health check) skips them.
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_schlange"));
    env_for(&mut cmd, &gh);
    let out = cmd.args(["doctor", "--offline"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("SKIP  github-app"));
    gh.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn serve_refuses_plain_http_short_secrets_and_warns_about_secrets_on_the_command_line() {
    let gh = FakeGithub::start(FakeConfig::default());
    let run = |extra: &[&str], insecure: bool, secret: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_schlange"));
        env_for(&mut cmd, &gh);
        if !insecure {
            cmd.env_remove("SCHLANGE_INSECURE_TEST_API");
        }
        cmd.arg("serve")
            .args(extra)
            .env("SCHLANGE_WEBHOOK_SECRET", secret)
            .env("SCHLANGE_LISTEN", "127.0.0.1:0")
            .env(
                "SCHLANGE_DATA_DIR",
                std::env::temp_dir().join("schlange-never-used"),
            );
        cmd.output().unwrap()
    };
    let out = run(&[], false, "fake-webhook-secret");
    assert_eq!(out.status.code(), Some(3));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--insecure-test-api"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = run(&[], true, "short");
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("shorter than 16"));
    let out = run(&["--webhook-secret", "x"], true, "");
    assert!(String::from_utf8_lossy(&out.stderr).contains("visible to other users"));
    gh.shutdown();
}

#[test]
fn serve_without_credentials_is_an_environment_error() {
    let out = Command::new(env!("CARGO_BIN_EXE_schlange"))
        .arg("serve")
        .env_remove("SCHLANGE_GITHUB_APP_ID")
        .env_remove("SCHLANGE_GITHUB_PRIVATE_KEY")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(3));
    assert!(String::from_utf8_lossy(&out.stderr).contains("--app-id"));
}
