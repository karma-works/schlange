//! The GitHub client and the webhook endpoint against the fake, over HTTP.

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;

use schlange_github::api::{MergeOutcome, PullMergeOutcome, RepoApi};
use schlange_github::client::{BudgetConfig, Client, ClientConfig, Error, Priority, RetryPolicy};
use schlange_github::jwt::AppKey;
use schlange_github::webhook::sign;
use schlange_github::{RepoName, doctor};
use schlange_github_fake::{FakeConfig, FakeGithub, FaultKind, FaultRule, TEST_APP_KEY, Tester};

fn client(gh: &FakeGithub, budget: BudgetConfig) -> Client {
    Client::new(ClientConfig {
        api_base: gh.url(),
        app_id: gh.config().app_id.to_string(),
        key: Arc::new(AppKey::from_pem(TEST_APP_KEY).unwrap()),
        retry: RetryPolicy::fast(),
        budget,
        timeout: Duration::from_secs(5),
        allow_insecure_http: true,
    })
}

fn roomy() -> BudgetConfig {
    BudgetConfig {
        capacity: 1000.0,
        refill_per_sec: 1000.0,
        cosmetic_reserve: 10.0,
    }
}

fn api(gh: &FakeGithub, c: &Client) -> RepoApi {
    let cfg = gh.config();
    RepoApi {
        client: c.clone(),
        repo: RepoName::parse(&cfg.full_name()).unwrap(),
        installation: cfg.installation_id,
    }
}

#[test]
fn installation_tokens_are_cached_and_etags_save_requests() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    let main1 = a.branch_sha("main", Priority::Normal).unwrap();
    let main2 = a.branch_sha("main", Priority::Normal).unwrap();
    assert_eq!(main1, main2);
    let s = c.stats();
    assert_eq!(s.tokens_issued, 1, "one token for both calls");
    assert_eq!(s.not_modified, 1, "the second read was a 304");
    assert_eq!(s.limit, Some(5000));
    // A wrong key is refused.
    let other = Client::new(ClientConfig {
        app_id: "999".into(),
        ..c.config().clone()
    });
    assert!(matches!(
        other.app(),
        Err(Error::Status { status: 401, .. })
    ));
    gh.shutdown();
}

#[test]
fn pagination_follows_link_headers() {
    let gh = FakeGithub::start(FakeConfig::default());
    for i in 0..120 {
        gh.open_pr(
            "someone",
            &format!("pr {i}"),
            &[(format!("f{i}"), "x".into())],
            false,
            &[],
        );
    }
    let c = client(&gh, roomy());
    let pulls = api(&gh, &c).open_pulls(Priority::Normal).unwrap();
    assert_eq!(pulls.len(), 120);
    assert!(
        gh.requests()
            .iter()
            .filter(|r| r.starts_with("GET /repos/karma-works/arzt-agent/pulls 200"))
            .count()
            >= 2
    );
    gh.shutdown();
}

#[test]
fn reads_retry_5xx_and_404_but_writes_report_an_unknown_outcome() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    gh.add_fault(FaultRule::new(
        "GET",
        "/git/ref/heads/main",
        FaultKind::Status(502),
        2,
    ));
    assert!(
        a.branch_sha("main", Priority::Normal).is_ok(),
        "two 502s, then OK"
    );
    gh.add_fault(FaultRule::new(
        "GET",
        "/git/ref/heads/main",
        FaultKind::Status(404),
        2,
    ));
    assert!(
        a.branch_sha("main", Priority::Normal).is_ok(),
        "a short 404 storm"
    );
    gh.add_fault(FaultRule::new(
        "GET",
        "/git/ref/heads/main",
        FaultKind::Status(404),
        50,
    ));
    let e = a.branch_sha("main", Priority::Normal).unwrap_err();
    assert!(matches!(e, Error::NotFound { .. }) && e.is_provisional());
    gh.clear_faults();
    // A write that fails with 502 is not repeated: its effect may have happened.
    let main = a.branch_sha("main", Priority::Normal).unwrap();
    gh.add_fault(FaultRule::new("POST", "/git/refs", FaultKind::Lie(502), 1));
    let e = a.create_ref("x", &main, Priority::Normal).unwrap_err();
    assert!(matches!(e, Error::Ambiguous { .. }), "{e}");
    assert_eq!(
        gh.branch_sha("x").as_deref(),
        Some(main.as_str()),
        "it happened"
    );
    assert_eq!(c.stats().ambiguous, 1);
    gh.shutdown();
}

#[test]
fn rate_limits_block_everyone_and_cosmetics_are_deferred() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    a.branch_sha("main", Priority::Normal).unwrap();
    gh.add_fault(FaultRule::new(
        "*",
        "/",
        FaultKind::RateLimit { retry_after: 1 },
        1,
    ));
    let t = std::time::Instant::now();
    a.branch_sha("main", Priority::Landing).unwrap();
    assert!(
        t.elapsed() >= Duration::from_millis(900),
        "waited for Retry-After"
    );
    // While blocked, a cosmetic call is refused at once.
    gh.add_fault(FaultRule::new(
        "*",
        "/",
        FaultKind::SecondaryLimit { retry_after: 2 },
        1,
    ));
    let e = a.comment(1, "x", Priority::Cosmetic).unwrap_err();
    assert!(matches!(e, Error::Deferred { .. }));
    let e = a.comment(1, "x", Priority::Cosmetic).unwrap_err();
    assert!(matches!(e, Error::Deferred { .. }), "still blocked");
    assert!(c.stats().rate_limited >= 2);
    gh.shutdown();

    // A small budget: cosmetics stop at the reserve, normal reads and landing go on.
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(
        &gh,
        BudgetConfig {
            capacity: 6.0,
            refill_per_sec: 0.001,
            cosmetic_reserve: 4.0,
        },
    );
    let a = api(&gh, &c);
    let n = gh.open_pr("u", "t", &[("a".into(), "1".into())], false, &[]);
    assert!(a.add_labels(n, &["x"], Priority::Cosmetic).is_ok() || c.stats().deferred > 0);
    let mut deferred = false;
    for _ in 0..3 {
        deferred |= matches!(
            a.add_labels(n, &["y"], Priority::Cosmetic),
            Err(Error::Deferred { .. })
        );
    }
    assert!(deferred, "cosmetics hit the reserve");
    for _ in 0..4 {
        a.pull(n, Priority::Landing).unwrap();
    }
    gh.shutdown();
}

#[test]
fn queue_operations_on_the_fake() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    let p1 = gh.open_pr("u", "one", &[("a.txt".into(), "1\n".into())], false, &[]);
    let p2 = gh.open_pr("u", "two", &[("a.txt".into(), "2\n".into())], false, &[]);
    let main = a.branch_sha("main", Priority::Normal).unwrap();
    a.create_ref("mergify/merge-queue/0123456789", &main, Priority::Normal)
        .unwrap();
    let h1 = a.pull(p1, Priority::Normal).unwrap().head.sha;
    let h2 = a.pull(p2, Priority::Normal).unwrap().head.sha;
    let m = a
        .merge_into(
            "mergify/merge-queue/0123456789",
            &h1,
            "m1",
            Priority::Normal,
        )
        .unwrap();
    assert!(matches!(m, MergeOutcome::Merged { .. }));
    assert_eq!(
        a.merge_into(
            "mergify/merge-queue/0123456789",
            &h1,
            "again",
            Priority::Normal
        )
        .unwrap(),
        MergeOutcome::NothingToMerge
    );
    assert_eq!(
        a.merge_into(
            "mergify/merge-queue/0123456789",
            &h2,
            "m2",
            Priority::Normal
        )
        .unwrap(),
        MergeOutcome::Conflict
    );
    let d = a
        .create_pull(
            "q",
            "mergify/merge-queue/0123456789",
            "main",
            "b",
            true,
            Priority::Normal,
        )
        .unwrap();
    assert!(d.draft);
    assert_eq!(d.user.login, "schlange[bot]");
    // Read-compare-patch: a stale expectation is refused, a fast-forward goes through.
    let tip = a
        .branch_sha("mergify/merge-queue/0123456789", Priority::Normal)
        .unwrap();
    assert!(matches!(
        a.update_ref_cas("main", &tip, &tip, Priority::Normal),
        Err(schlange_github::api::CasError::Stale { .. })
    ));
    a.update_ref_cas(
        "mergify/merge-queue/0123456789",
        &tip,
        &tip,
        Priority::Normal,
    )
    .unwrap();
    assert!(matches!(
        a.update_ref_cas(
            "mergify/merge-queue/0123456789",
            &tip,
            &main,
            Priority::Normal
        ),
        Err(schlange_github::api::CasError::NotFastForward)
    ));
    // Squash with the expected head; a moved head is refused.
    assert_eq!(
        a.squash_pull(p1, &h2, Some("x"), None, Priority::Landing)
            .unwrap(),
        PullMergeOutcome::HeadMoved
    );
    assert!(matches!(
        a.squash_pull(p1, &h1, Some("one (#1)"), None, Priority::Landing)
            .unwrap(),
        PullMergeOutcome::Merged { .. }
    ));
    assert!(matches!(
        a.squash_pull(p1, &h1, Some("one (#1)"), None, Priority::Landing)
            .unwrap(),
        PullMergeOutcome::NotMergeable(_)
    ));
    // Checks, statuses, comments, reactions, labels, permissions, files, compare.
    let id = a
        .create_check_run(
            &json!({"name": "q", "head_sha": h2, "status": "in_progress"}),
            Priority::Cosmetic,
        )
        .unwrap();
    a.update_check_run(id, &json!({"conclusion": "neutral"}), Priority::Cosmetic)
        .unwrap();
    gh.report_status(p2, "legacy", "failure");
    gh.report_check(p2, "ci", Some("success"));
    assert_eq!(a.check_runs(&h2, Priority::Normal).unwrap().len(), 2);
    assert_eq!(a.check_suites(&h2, Priority::Normal).unwrap().len(), 2);
    assert_eq!(
        a.combined_status(&h2, Priority::Normal).unwrap().state,
        "failure"
    );
    let cid = a.comment(p2, "hello", Priority::Cosmetic).unwrap();
    a.react_to_comment(cid, "+1", Priority::Cosmetic).unwrap();
    a.add_labels(p2, &["l1"], Priority::Cosmetic).unwrap();
    a.remove_label(p2, "l1", Priority::Cosmetic).unwrap();
    a.remove_label(p2, "never-there", Priority::Cosmetic)
        .unwrap();
    assert!(gh.pr(p2).unwrap().labels.is_empty());
    gh.set_collaborator("karma-works", "admin");
    assert_eq!(
        a.collaborator_permission("karma-works", Priority::Normal)
            .unwrap(),
        "admin"
    );
    assert_eq!(
        a.collaborator_permission("nobody", Priority::Normal)
            .unwrap(),
        "none"
    );
    assert_eq!(
        a.pull_files(p2, Priority::Normal).unwrap(),
        (vec!["a.txt".to_string()], 1)
    );
    let main_now = a.branch_sha("main", Priority::Normal).unwrap();
    assert_eq!(
        a.compare(&main_now, &h2, Priority::Normal)
            .unwrap()
            .behind_by,
        1
    );
    assert!(matches!(
        a.branch_protection("main", Priority::Normal),
        Err(Error::Status { status: 403, .. })
    ));
    assert_eq!(
        a.file_content("README.md", "main", Priority::Normal)
            .unwrap()
            .as_deref(),
        Some("hello\n")
    );
    assert_eq!(
        a.file_content("nope", "main", Priority::Normal).unwrap(),
        None
    );
    a.delete_ref("mergify/merge-queue/0123456789", Priority::Normal)
        .unwrap();
    assert!(matches!(
        a.delete_ref("mergify/merge-queue/0123456789", Priority::Normal),
        Err(Error::NotFound { .. })
    ));
    gh.shutdown();
}

#[test]
fn the_webhook_endpoint_checks_signatures_and_drops_repeated_deliveries() {
    let t = Tester::new("queue_rules:\n  - name: default\n").start();
    let url = t.service.as_ref().unwrap().webhook_url();
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .build(),
    );
    let body = json!({"action": "edited", "number": 999,
        "repository": {"full_name": "karma-works/arzt-agent"},
        "pull_request": {"number": 999, "user": {"login": "x"}, "head": {"ref": "h", "sha": "s"}, "base": {"ref": "main", "sha": "b"}}})
    .to_string();
    let post = |sig: &str, id: &str| {
        agent
            .post(&url)
            .header("x-github-event", "pull_request")
            .header("x-github-delivery", id)
            .header("x-hub-signature-256", sig)
            .send(body.as_bytes())
            .unwrap()
            .status()
            .as_u16()
    };
    let secret = t.gh.config().webhook_secret;
    assert_eq!(post("sha256=00", "d1"), 401);
    assert_eq!(post(&sign(b"wrong", body.as_bytes()), "d1"), 401);
    assert_eq!(post(&sign(&secret, body.as_bytes()), "d1"), 202);
    assert_eq!(
        post(&sign(&secret, body.as_bytes()), "d1"),
        200,
        "duplicate"
    );
    assert_eq!(post(&sign(&secret, body.as_bytes()), "d2"), 202);
    let health = agent
        .get(url.replace("/webhook", "/healthz"))
        .call()
        .unwrap()
        .status();
    assert_eq!(health.as_u16(), 200);
    t.shutdown();
}

#[test]
fn doctor_checks_against_the_fake() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let repo = RepoName::parse(&gh.config().full_name()).unwrap();
    let status = |checks: &[doctor::Check], name: &str| {
        checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| c.status)
            .unwrap()
    };
    let cc = schlange_github_fake::tester::config_check();
    let checks = doctor::github_checks(&c, std::slice::from_ref(&repo), None, 86_400, Some(&cc));
    assert_eq!(status(&checks, "github-app"), "OK");
    // No .mergify.yml on the default branch yet.
    assert_eq!(status(&checks, "configuration"), "FAIL");
    gh.push_base(&[(
        ".mergify.yml".into(),
        "pull_request_rules:\n  - name: q\n    conditions:\n      - base = main\n    actions: {}\n"
            .into(),
    )]);
    let checks = doctor::github_checks(&c, std::slice::from_ref(&repo), None, 86_400, Some(&cc));
    assert_ne!(status(&checks, "configuration"), "FAIL");
    gh.push_base(&[(
        ".mergify.yml".into(),
        "pull_request_rules:\n  - name: q\n    actions: {}\n".into(),
    )]);
    let checks = doctor::github_checks(&c, std::slice::from_ref(&repo), None, 86_400, Some(&cc));
    assert_eq!(status(&checks, "configuration"), "FAIL");
    let d = checks.iter().find(|c| c.name == "configuration").unwrap();
    assert!(
        d.detail.contains("invalid") && d.detail.contains("conditions"),
        "{}",
        d.detail
    );
    let checks = doctor::github_checks(&c, std::slice::from_ref(&repo), None, 86_400, None);
    assert_eq!(status(&checks, "configuration"), "FAIL");
    // Exactly the required permissions; workflows (optional) is missing: WARN.
    assert_eq!(status(&checks, "app-permissions"), "WARN");
    assert_eq!(status(&checks, "ruleset-exception"), "OK");
    assert_eq!(status(&checks, "rate-limit"), "OK");
    assert_eq!(status(&checks, "webhooks"), "WARN", "no deliveries yet");

    let mut perms = gh.config().permissions;
    perms.insert("contents".into(), "read".into());
    perms.insert("administration".into(), "read".into());
    gh.set_permissions(perms);
    gh.add_ruleset(json!({"id": 1, "name": "main", "target": "branch", "enforcement": "active",
                          "bypass_actors": [{"actor_id": 1, "actor_type": "Team", "bypass_mode": "always"}]}));
    let checks =
        doctor::github_checks(&c, std::slice::from_ref(&repo), Some(77), 86_400, Some(&cc));
    assert_eq!(status(&checks, "app-permissions"), "FAIL");
    let p = checks.iter().find(|c| c.name == "app-permissions").unwrap();
    assert!(p.detail.contains("contents: read → write") && p.detail.contains("administration"));
    assert_eq!(status(&checks, "ruleset-exception"), "FAIL");
    let r = checks
        .iter()
        .find(|c| c.name == "ruleset-exception")
        .unwrap();
    assert!(r.detail.contains("Risk"), "{}", r.detail);
    gh.add_ruleset(json!({"id": 2, "name": "other", "target": "branch", "enforcement": "active",
                          "bypass_actors": [{"actor_id": 4242, "actor_type": "Integration", "bypass_mode": "always"}]}));

    // An unreachable app.
    let bad = Client::new(ClientConfig {
        app_id: "1".into(),
        ..c.config().clone()
    });
    let checks = doctor::github_checks(&bad, &[repo], None, 86_400, Some(&cc));
    assert_eq!(status(&checks, "github-app"), "FAIL");
    gh.shutdown();
}

#[test]
fn the_invariant_checker_catches_untested_landings_and_leftover_branches() {
    // Not Schlange: a squash done by hand through the API, never tested by the queue.
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    let n = gh.open_pr("u", "t", &[("a".into(), "1".into())], false, &[]);
    let head = gh.pr(n).unwrap().head_sha;
    a.squash_pull(n, &head, Some("t"), None, Priority::Landing)
        .unwrap();
    let v = gh.invariant_violations(true);
    assert!(v.iter().any(|v| v.starts_with("I0")), "{v:?}");
    a.create_ref("mergify/merge-queue/abcdef0123", &head, Priority::Normal)
        .unwrap();
    let v = gh.invariant_violations(true);
    assert!(v.iter().any(|v| v.starts_with("I3")), "{v:?}");
    gh.shutdown();
}

/// Security (critic 1abc, conditions-files): `git mv` of a constitution file to an ordinary path
/// must not slip through Arztagent's files gate. GitHub lists a rename once, with
/// `previous_filename`; the docs count a renamed file at both of its paths.
#[test]
fn a_renamed_constitution_file_still_trips_arztagents_files_gate() {
    let mut cfg = FakeConfig::default();
    cfg.base_files.insert(
        ".github/workflows/ci.yml".into(),
        "on: [pull_request]\n".into(),
    );
    let gh = FakeGithub::start(cfg);
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    let n = gh.open_pr(
        "arztagent-bot",
        "t",
        &[("b.txt".into(), "b\n".into())],
        false,
        &[],
    );
    gh.move_in_pr(n, ".github/workflows/ci.yml", "docs/ci.yml");
    let (files, listed) = a.pull_files(n, Priority::Normal).unwrap();
    assert_eq!(listed, 2, "a rename is one entry");
    for path in [".github/workflows/ci.yml", "docs/ci.yml", "b.txt"] {
        assert!(files.iter().any(|f| f == path), "{path} missing: {files:?}");
    }
    let yaml = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../schlange-config/tests/golden/arztagent.yml"
    ))
    .unwrap();
    let rules = schlange_config::rules::Rules::from_yaml(&yaml).unwrap();
    let mut pr = schlange_core::model::PullRequest::new(n, "arztagent-bot", "main", "pr", "h");
    pr.files = files;
    let rule = &rules.pull_request_rules[0];
    let (matches, _) = schlange_core::eval::eval_list(
        &rule.conditions,
        &pr,
        &schlange_core::eval::Context::default(),
        &mut vec![],
    );
    assert!(
        !matches,
        "`{}` must not queue a moved constitution file",
        rule.name
    );
    gh.shutdown();
}

#[test]
fn a_rate_limited_write_is_handed_back_not_repeated() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    let main = a.branch_sha("main", Priority::Normal).unwrap();
    gh.add_fault(FaultRule::new(
        "POST",
        "/git/refs",
        FaultKind::RateLimit { retry_after: 1 },
        1,
    ));
    let e = a.create_ref("x", &main, Priority::Landing).unwrap_err();
    assert!(matches!(e, Error::RateLimited { .. }), "{e:?}");
    let posts = gh
        .requests()
        .iter()
        .filter(|r| r.starts_with("POST /repos/karma-works/arzt-agent/git/refs"))
        .count();
    assert_eq!(posts, 1, "the write was not sent again after the wait");
    assert!(gh.branch_sha("x").is_none());
    assert_eq!(c.stats().writes_rate_limited, 1);
    // A read is waited out and repeated.
    gh.add_fault(FaultRule::new(
        "GET",
        "/git/ref/heads/main",
        FaultKind::RateLimit { retry_after: 1 },
        1,
    ));
    assert_eq!(a.branch_sha("main", Priority::Landing).unwrap(), main);
    gh.shutdown();
}

#[test]
fn renamed_repositories_are_followed_and_other_hosts_are_refused() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    let main = a.branch_sha("main", Priority::Normal).unwrap();
    gh.rename_repo("arzt-agent-renamed");
    // The old name still works: 301 for reads, 307 for writes, on the same host.
    assert_eq!(a.branch_sha("main", Priority::Normal).unwrap(), main);
    a.create_ref("after-rename", &main, Priority::Normal)
        .unwrap();
    assert!(gh.branch_sha("after-rename").is_some());
    assert!(c.stats().redirects >= 2);
    // Absolute URLs off the API host are refused before anything is sent.
    let e = c
        .get_json::<serde_json::Value>(
            "https://evil.example/repos/x",
            schlange_github::client::Auth::Installation(77),
            Priority::Normal,
        )
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    // Plain HTTP without the test switch: refused.
    let strict = Client::new(ClientConfig {
        allow_insecure_http: false,
        ..c.config().clone()
    });
    assert!(matches!(strict.app(), Err(Error::Refused(_))));
    gh.shutdown();
}

#[test]
fn link_headers_to_another_host_are_not_followed() {
    // A one-shot server whose answer points its next page at another host.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut s, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let _ = s.read(&mut buf);
        let body = "[1, 2]";
        let _ = write!(
            s,
            "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\nlink: <https://evil.example/next?page=2>; rel=\"next\"\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        );
    });
    let c = Client::new(ClientConfig {
        api_base: format!("http://{addr}"),
        app_id: "1".into(),
        key: Arc::new(AppKey::from_pem(TEST_APP_KEY).unwrap()),
        retry: RetryPolicy::fast(),
        budget: roomy(),
        timeout: Duration::from_secs(5),
        allow_insecure_http: true,
    });
    let e = c
        .get_paginated::<u64>(
            "/items",
            schlange_github::client::Auth::None,
            Priority::Normal,
            10,
            Some,
        )
        .unwrap_err();
    assert!(matches!(e, Error::Refused(_)), "{e:?}");
    srv.join().unwrap();
}

#[test]
fn doctor_accepts_a_read_only_app_for_shadow_mode_only() {
    let read_only = schlange_github::permissions::SHADOW_PERMISSIONS
        .iter()
        .map(|(a, b, _)| (a.to_string(), b.to_string()))
        .collect();
    let gh = FakeGithub::start(FakeConfig {
        permissions: read_only,
        ..FakeConfig::default()
    });
    let c = client(&gh, roomy());
    let repo = RepoName::parse(&gh.config().full_name()).unwrap();
    let find = |checks: &[doctor::Check], name: &str| {
        checks
            .iter()
            .find(|c| c.name == name)
            .map(|c| (c.status, c.detail.clone()))
            .unwrap()
    };
    let cc = schlange_github_fake::tester::config_check();
    let shadow = doctor::github_checks_for(
        &c,
        std::slice::from_ref(&repo),
        None,
        86_400,
        Some(&cc),
        true,
    );
    let (status, detail) = find(&shadow, "app-permissions");
    assert_eq!(status, "OK", "{detail}");
    assert_eq!(find(&shadow, "ruleset-exception").0, "SKIP");
    // The same app cannot run the queue.
    let acting = doctor::github_checks(&c, std::slice::from_ref(&repo), None, 86_400, Some(&cc));
    let (status, detail) = find(&acting, "app-permissions");
    assert_eq!(status, "FAIL");
    assert!(detail.contains("contents: read → write"), "{detail}");
}
