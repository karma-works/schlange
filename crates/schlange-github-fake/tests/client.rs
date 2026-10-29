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
fn fake_exposes_check_run_suite_identity() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("alice", "suite", &[("a".into(), "new".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
    let run_id = gh.report_check(pr, "ci", Some("success"));
    let c = client(&gh, roomy());
    let repo = api(&gh, &c);
    let suites = repo.check_suites(&head, Priority::Normal).unwrap();
    let suite = suites.iter().find(|s| s.id == run_id).unwrap();
    assert!(suite.created_at.is_some());
    assert!(suite.app.as_ref().and_then(|a| a.id).is_some());
    let runs = repo.check_suite_runs(suite.id, Priority::Normal).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run_id);
    assert_eq!(runs[0].check_suite.as_ref().unwrap().id, suite.id);
    gh.shutdown();
}

#[test]
fn suite_reader_validates_complete_authenticated_run_identity() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "identity",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let run_id = gh.report_check(pr, "ci", Some("success"));
    let c = client(&gh, roomy());
    let runs = api(&gh, &c)
        .validated_suite_runs(&head, Priority::Normal)
        .unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].0.id, run_id);
    assert_eq!(runs[0].1.id, run_id);
    gh.shutdown();
}

#[test]
fn suite_reader_rejects_mismatched_app_and_incomplete_pages() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "identity-bad",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let run_id = gh.report_check(pr, "ci", Some("success"));
    let c = client(&gh, roomy());
    let repo = api(&gh, &c);
    let path = format!("/check-suites/{run_id}/check-runs");
    gh.add_fault(FaultRule::new(
        "GET",
        &path,
        FaultKind::Json(json!({
            "total_count": 1,
            "check_runs": [{"id":run_id,"name":"ci","head_sha":head,"status":"completed",
              "app":{"id":999,"slug":"github-actions"},"check_suite":{"id":run_id}}]
        })),
        1,
    ));
    assert!(repo.validated_suite_runs(&head, Priority::Normal).is_err());
    gh.add_fault(FaultRule::new(
        "GET",
        &path,
        FaultKind::Json(json!({"check_runs":[]})),
        1,
    ));
    assert!(repo.validated_suite_runs(&head, Priority::Normal).is_err());
    gh.shutdown();
}

#[test]
fn suite_reader_visits_second_page_of_suites() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "many-suites",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    for n in 0..101 {
        gh.report_check(pr, &format!("check-{n}"), Some("success"));
    }
    let c = client(&gh, roomy());
    let runs = api(&gh, &c)
        .validated_suite_runs(&head, Priority::Normal)
        .unwrap();
    assert_eq!(runs.len(), 101);
    assert!(
        gh.requests_with_query()
            .iter()
            .any(|r| r.contains(&format!("/commits/{head}/check-suites")) && r.contains("page=2"))
    );
    gh.shutdown();
}

#[test]
fn guarded_build_reports_deferred_work_as_uncertain() {
    use schlange_core::{Decision, engine::CarPr};
    use schlange_github::client::WriteGate;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    struct Deny;
    impl WriteGate for Deny {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Err("input changed".into())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "guarded-build",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    let client = client(&gh, roomy()).with_execution_gate(Arc::new(Deny));
    let mut forge = GithubForge::new(
        api(&gh, &client),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let build = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "queue/guarded".into(),
        base_sha: base,
        prs: vec![CarPr {
            number: pr,
            head_sha: head,
        }],
        own: vec![pr],
        stack_on: None,
    };
    assert!(forge.execute_guarded_once(&build).is_err());
    assert_eq!(forge.pending_count(), 1);
    assert!(gh.branch_sha("queue/guarded").is_none());
    gh.shutdown();
}

#[test]
fn guarded_build_never_deletes_or_adopts_a_preexisting_branch() {
    use schlange_core::{Decision, engine::CarPr};
    use schlange_github::client::WriteGate;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "foreign-branch",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    let plain = client(&gh, roomy());
    api(&gh, &plain)
        .create_ref("queue/foreign", &base, Priority::Normal)
        .unwrap();
    let scoped = plain.with_execution_gate(Arc::new(Allow));
    let mut forge = GithubForge::new(
        api(&gh, &scoped),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let build = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "queue/foreign".into(),
        base_sha: base.clone(),
        prs: vec![CarPr {
            number: pr,
            head_sha: head,
        }],
        own: vec![pr],
        stack_on: None,
    };
    let before = gh.requests_with_query().len();
    assert!(forge.execute_guarded_once(&build).is_err());
    assert_eq!(gh.branch_sha("queue/foreign"), Some(base));
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("DELETE ") || r.contains("/merges"))
    );
    gh.shutdown();
}

#[test]
fn guarded_build_does_not_adopt_an_ambiguous_ref_creation() {
    use schlange_core::{Decision, engine::CarPr};
    use schlange_github::client::WriteGate;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "ambiguous-ref",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    gh.add_fault(FaultRule::new("POST", "/git/refs", FaultKind::Lie(422), 1));
    let client = client(&gh, roomy()).with_execution_gate(Arc::new(Allow));
    let mut forge = GithubForge::new(
        api(&gh, &client),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let build = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "queue/ambiguous".into(),
        base_sha: base.clone(),
        prs: vec![CarPr {
            number: pr,
            head_sha: head,
        }],
        own: vec![pr],
        stack_on: None,
    };
    let before = gh.requests_with_query().len();
    assert!(forge.execute_guarded_once(&build).is_err());
    assert_eq!(gh.branch_sha("queue/ambiguous"), Some(base));
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("DELETE ") || r.contains("/merges"))
    );
    gh.shutdown();
}

#[test]
fn created_ref_receipt_requires_successful_matching_response() {
    use schlange_github::client::WriteGate;
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let base = gh.branch_sha("main").unwrap();
    let c = client(&gh, roomy()).with_execution_gate(Arc::new(Allow));
    let repo = api(&gh, &c);
    let receipt = repo
        .create_ref_receipt("queue/receipt", &base, Priority::Normal)
        .unwrap();
    assert_eq!(receipt.branch(), "queue/receipt");
    assert_eq!(receipt.sha(), base);
    assert_eq!(receipt.repo(), gh.config().full_name());
    gh.add_fault(FaultRule::new(
        "POST",
        "/git/refs",
        FaultKind::JsonStatus(
            201,
            json!({
                "ref":"refs/heads/queue/wrong", "object":{"sha":base}
            }),
        ),
        1,
    ));
    assert!(matches!(
        repo.create_ref_receipt("queue/other", &base, Priority::Normal),
        Err(Error::Ambiguous { .. })
    ));
    gh.shutdown();
}

#[test]
fn ref_receipt_never_retries_an_ambiguous_or_redirected_write() {
    use schlange_github::client::WriteGate;
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    for (kind, branch) in [
        (FaultKind::Lie(500), "queue/receipt-500"),
        (FaultKind::Status(401), "queue/receipt-401"),
        (
            FaultKind::Redirect {
                status: 307,
                location: "/repos/karma-works/arzt-agent/git/refs".into(),
            },
            "queue/receipt-redirect",
        ),
        (
            FaultKind::Redirect {
                status: 308,
                location: "/repos/karma-works/other/git/refs".into(),
            },
            "queue/receipt-redirect-308",
        ),
    ] {
        let gh = FakeGithub::start(FakeConfig::default());
        let base = gh.branch_sha("main").unwrap();
        gh.add_fault(FaultRule::new("POST", "/git/refs", kind, 1));
        let c = client(&gh, roomy()).with_execution_gate(Arc::new(Allow));
        let repo = api(&gh, &c);
        let before = gh.requests_with_query().len();
        assert!(
            repo.create_ref_receipt(branch, &base, Priority::Normal)
                .is_err()
        );
        let writes = gh.requests_with_query()[before..]
            .iter()
            .filter(|r| r.starts_with("POST ") && r.contains("/git/refs"))
            .count();
        assert_eq!(writes, 1, "create-ref must have exactly one attempt");
        gh.shutdown();
    }
}

#[test]
fn required_actions_collision_without_workflow_identity_is_refused() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "required",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    gh.report_check(pr, "ci", Some("failure"));
    gh.report_check(pr, "ci", Some("success"));
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    assert!(
        forge
            .read_pr_with_checks(pr, Priority::Normal, &["ci".into()], &[])
            .is_err()
    );
    gh.shutdown();
}

#[test]
fn suite_run_endpoint_requires_checks_permission() {
    let mut config = FakeConfig::default();
    config.permissions.remove("checks");
    let gh = FakeGithub::start(config);
    let c = client(&gh, roomy());
    assert!(api(&gh, &c).check_suite_runs(1, Priority::Normal).is_err());
    assert!(
        gh.requests_with_query()
            .iter()
            .any(|r| r.contains("/check-suites/1/check-runs") && r.ends_with(" 403"))
    );
    gh.shutdown();
}

#[test]
fn checked_combined_status_refuses_missing_count() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "status-count",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/status"),
        FaultKind::Json(json!({"state":"success", "statuses":[]})),
        1,
    ));
    let c = client(&gh, roomy());
    assert!(
        api(&gh, &c)
            .combined_status_checked(&head, Priority::Normal)
            .is_err()
    );
    gh.shutdown();
}

#[test]
fn checked_combined_status_rejects_duplicate_identity() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "duplicate-status",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/status"),
        FaultKind::Json(json!({"total_count":2,"statuses":[
            {"id":1,"context":"ci","state":"success"},
            {"id":1,"context":"ci","state":"failure"}
        ]})),
        1,
    ));
    let c = client(&gh, roomy());
    assert!(
        api(&gh, &c)
            .combined_status_checked(&head, Priority::Normal)
            .is_err()
    );
    gh.shutdown();
}

#[test]
fn missing_administration_access_never_proves_no_required_checks() {
    let mut cfg = FakeConfig::default();
    cfg.permissions.remove("administration");
    let gh = FakeGithub::start(cfg);
    let c = client(&gh, roomy());
    assert!(
        api(&gh, &c)
            .required_check_contexts("main", Priority::Normal)
            .is_err()
    );
    gh.shutdown();
}

#[test]
fn downscoped_token_without_administration_cannot_prove_requirements() {
    let mut cfg = FakeConfig::default();
    cfg.permissions
        .insert("administration".into(), "read".into());
    cfg.permissions.insert("actions".into(), "read".into());
    let gh = FakeGithub::start(cfg);
    let c = client(&gh, roomy());
    let cfg = gh.config();
    let permissions = [
        ("metadata", "read"),
        ("contents", "read"),
        ("checks", "read"),
        ("actions", "read"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_owned(), v.to_owned()))
    .collect();
    c.set_token_scope(cfg.installation_id, &[cfg.name], &permissions);
    assert!(
        api(&gh, &c)
            .required_check_contexts("main", Priority::Normal)
            .is_err()
    );
    gh.shutdown();
}

#[test]
fn requirements_preserve_ruleset_and_classic_app_bindings() {
    let mut cfg = FakeConfig::default();
    cfg.permissions
        .insert("administration".into(), "read".into());
    cfg.permissions.insert("actions".into(), "read".into());
    let gh = FakeGithub::start(cfg);
    gh.add_fault(FaultRule::new(
        "GET",
        "/branches/main",
        FaultKind::Json(json!({"protected":true})),
        1,
    ));
    gh.add_fault(FaultRule::new("GET", "/rules/branches/main", FaultKind::Json(json!([
        {"type":"required_status_checks", "parameters":{"required_status_checks":[{"context":"ci", "integration_id":42}]}}
    ])), 1));
    gh.add_fault(FaultRule::new(
        "GET",
        "/branches/main/protection",
        FaultKind::Json(json!({
            "required_status_checks":{"checks":[{"context":"other", "app_id":43}]}
        })),
        1,
    ));
    let c = client(&gh, roomy());
    let requirements = api(&gh, &c)
        .required_check_contexts("main", Priority::Normal)
        .unwrap();
    assert!(
        requirements
            .iter()
            .any(|r| r.context == "ci" && r.app_id == Some(42))
    );
    assert!(
        requirements
            .iter()
            .any(|r| r.context == "other" && r.app_id == Some(43))
    );
    gh.shutdown();
}

#[test]
fn branch_rule_pagination_refuses_page_limit() {
    let gh = FakeGithub::start(FakeConfig::default());
    for id in 1..=101 {
        gh.add_ruleset(json!({"id":id}));
    }
    let c = client(&gh, roomy());
    let cfg = gh.config();
    let path = format!(
        "/repos/{}/rules/branches/main?per_page=100",
        cfg.full_name()
    );
    let rules: Result<Vec<serde_json::Value>, _> = c.get_paginated_array_checked(
        &path,
        schlange_github::client::Auth::Installation(cfg.installation_id),
        Priority::Normal,
        1,
    );
    assert!(rules.is_err());
    gh.shutdown();
}

#[test]
fn malformed_required_contexts_fail_closed() {
    let mut cfg = FakeConfig::default();
    cfg.permissions
        .insert("administration".into(), "read".into());
    cfg.permissions.insert("actions".into(), "read".into());
    let gh = FakeGithub::start(cfg);
    let c = client(&gh, roomy());
    for item in [
        json!({"context":"", "integration_id":42}),
        json!({"context":"ci", "integration_id":0}),
    ] {
        gh.add_fault(FaultRule::new(
            "GET",
            "/branches/main",
            FaultKind::Json(json!({"protected":false})),
            1,
        ));
        gh.add_fault(FaultRule::new(
            "GET",
            "/rules/branches/main",
            FaultKind::Json(json!([
                {"type":"required_status_checks", "parameters":{"required_status_checks":[item]}}
            ])),
            1,
        ));
        assert!(
            api(&gh, &c)
                .required_check_contexts("main", Priority::Normal)
                .is_err()
        );
    }
    gh.shutdown();
}

#[test]
fn unprotected_branch_can_be_read_when_scoped_permissions_are_known() {
    let mut cfg = FakeConfig::default();
    cfg.permissions
        .insert("administration".into(), "read".into());
    cfg.permissions.insert("actions".into(), "read".into());
    let gh = FakeGithub::start(cfg);
    let c = client(&gh, roomy());
    assert!(
        api(&gh, &c)
            .required_check_contexts("main", Priority::Normal)
            .unwrap()
            .is_empty()
    );
    gh.shutdown();
}

#[test]
fn base_identity_batch_refuses_suite_failure_then_reads_fresh() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("alice", "batch", &[("a".into(), "new".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
    gh.report_check(pr, "ci", Some("success"));
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let mut state = schlange_core::State::default();
    state
        .pull_requests
        .insert(pr, forge.read_pr(pr, Priority::Normal).unwrap());
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Status(403),
        1,
    ));
    assert!(
        forge
            .read_base_identity_batch("main", 1, &state, Priority::Normal)
            .is_err()
    );
    let batch = forge
        .read_base_identity_batch("main", 1, &state, Priority::Normal)
        .unwrap();
    let schlange_core::Event::CheckIdentityBatch { pull_requests, .. } = batch else {
        panic!("wrong event")
    };
    assert_eq!(pull_requests.len(), 1);
    assert_eq!(pull_requests[0].head_sha, head);
    assert!(
        pull_requests[0]
            .checks
            .iter()
            .any(|c| c.name == "ci" && c.app_id == Some(15368))
    );
    gh.shutdown();
}

#[test]
fn queue_check_name_bound_to_another_app_remains_required() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "named-check",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    gh.add_fault(FaultRule::new("GET", "/rules/branches/main", FaultKind::Json(json!([
        {"type":"required_status_checks", "parameters":{"required_status_checks":[{"context":"queue", "integration_id":55}]}}
    ])), 2));
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let mut state = schlange_core::State::default();
    state
        .pull_requests
        .insert(pr, forge.read_pr(pr, Priority::Normal).unwrap());
    let batch = forge
        .read_base_identity_batch("main", 1, &state, Priority::Normal)
        .unwrap();
    let schlange_core::Event::CheckIdentityBatch { bound_checks, .. } = batch else {
        panic!("wrong event")
    };
    assert!(
        bound_checks
            .iter()
            .any(|r| r.context == "queue" && r.app_id == 55)
    );
    gh.shutdown();
}

#[test]
fn identity_batch_rejects_base_move_during_read() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use schlange_github_fake::Then;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "base-race",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let mut state = schlange_core::State::default();
    state
        .pull_requests
        .insert(pr, forge.read_pr(pr, Priority::Normal).unwrap());
    gh.add_fault(
        FaultRule::new("GET", &format!("/pulls/{pr}"), FaultKind::Status(500), 1)
            .then(Then::PushBase(vec![("new-base".into(), "changed".into())])),
    );
    let error = forge
        .read_base_identity_batch("main", 1, &state, Priority::Normal)
        .unwrap_err();
    assert!(
        error.to_string().contains("pull request base differs"),
        "{error}"
    );
    gh.shutdown();
}

#[test]
fn waiting_queue_comment_scan_holds_unseen_human_commands() {
    use schlange_core::engine::{EntryStatus, QueueEntry};
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "commands",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let mut state = schlange_core::State::default();
    let p = forge.read_pr(pr, Priority::Normal).unwrap();
    state.queued.insert(
        pr,
        QueueEntry {
            queue: "q".into(),
            rule: "q".into(),
            since: 0,
            head_sha: p.head_sha.clone(),
            base: "main".into(),
            seq: 1,
            waiting_since: 0,
            status: EntryStatus::Waiting,
            external_reset: false,
            by: None,
            priority: 2000,
            priority_rule: None,
            interrupt: false,
        },
    );
    state.pull_requests.insert(pr, p);
    let entry = state.queued.remove(&pr).unwrap();
    assert!(
        forge
            .waiting_queue_comments_clear(&state, Priority::Normal)
            .unwrap()
    );
    state.queued.insert(pr, entry);
    gh.set_collaborator("someone", "write");
    let bot = gh.comment(pr, "schlange[bot]", "<!-- schlange:status --> waiting");
    gh.edit_comment(
        bot,
        "someone",
        "<!-- schlange:status --> - [x] <!-- schlange:box:requeue -->",
    )
    .unwrap();
    assert!(
        forge
            .waiting_queue_comments_clear(&state, Priority::Normal)
            .unwrap()
    );
    let command = gh.comment(pr, "alice", "@mergifyio dequeue");
    assert!(
        !forge
            .waiting_queue_comments_clear(&state, Priority::Normal)
            .unwrap()
    );
    let version = gh
        .comments_on(pr)
        .into_iter()
        .find(|c| c.id == command)
        .unwrap()
        .updated_at;
    state
        .seen_comments
        .insert((command, schlange_github::time::format_rfc3339(version)));
    assert!(
        !forge
            .waiting_queue_comments_clear(&state, Priority::Normal)
            .unwrap(),
        "a seen timestamp does not authenticate the original body"
    );
    state.queued.remove(&pr);
    assert!(
        !forge
            .waiting_queue_comments_clear(&state, Priority::Normal)
            .unwrap()
    );
    gh.shutdown();
}

#[test]
fn arztagent_review_uses_newest_authenticated_suite_not_highest_run_id() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("alice", "review", &[("a".into(), "new".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
    let suite = |id, created| json!({"id":id, "head_sha":head, "created_at":created, "app":{"id":15368,"slug":"github-actions"}});
    gh.add_fault(FaultRule::new("GET", &format!("/commits/{head}/check-suites"), FaultKind::Json(json!({"total_count":2,"check_suites":[suite(10,"2026-01-01T00:00:00Z"),suite(11,"2026-01-02T00:00:00Z")]})), 1));
    for (suite_id, run_id, conclusion) in [(10, 900, "failure"), (11, 100, "success")] {
        gh.add_fault(FaultRule::new("GET", &format!("/check-suites/{suite_id}/check-runs"), FaultKind::Json(json!({"total_count":1,"check_runs":[{"id":run_id,"name":"KI-Review","head_sha":head,"status":"completed","conclusion":conclusion,"app":{"id":15368,"slug":"github-actions"},"check_suite":{"id":suite_id}}]})), 1));
    }
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let observed = forge
        .read_pr_with_checks(pr, Priority::Normal, &[], &[])
        .unwrap();
    let checks = observed.current_checks();
    assert_eq!(checks.len(), 1);
    assert_eq!(checks[0].seq, 100);
    assert_eq!(
        checks[0].conclusion,
        Some(schlange_core::model::Conclusion::Success)
    );
    assert!(
        gh.requests_with_query()
            .iter()
            .any(|r| r.contains("/check-suites/11/check-runs") && r.contains("filter=all"))
    );
    gh.shutdown();
}

#[test]
fn incomplete_suite_page_cannot_produce_a_pull_request_snapshot() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "partial",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/commits/{head}/check-suites"),
        FaultKind::Json(json!({"check_suites":[]})),
        1,
    ));
    let c = client(&gh, roomy());
    let forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    assert!(
        forge
            .read_pr_with_checks(pr, Priority::Normal, &[], &[])
            .is_err(),
        "missing suite count cannot prove a complete check set"
    );
    gh.shutdown();
}

#[test]
fn deferred_status_update_reuses_comment_and_recovers_after_deletion() {
    use schlange_core::Decision;
    use schlange_core::cosmetics::Cosmetic;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let id = gh.comment(pr, "schlange[bot]", "<!-- schlange:status -->\nold");
    let c = client(&gh, roomy());
    let mut forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    gh.add_fault(FaultRule::new(
        "PATCH",
        &format!("/issues/comments/{id}"),
        FaultKind::RateLimit { retry_after: 0 },
        1,
    ));
    let update = |body: &str| Decision::Cosmetic {
        cosmetic: Cosmetic::StatusComment {
            number: pr,
            body: format!("<!-- schlange:status -->\n{body}"),
        },
    };
    forge.execute(&update("first"));
    forge.flush_cosmetics();
    assert_eq!(forge.cosmetic_count(), 1, "429 defers the update");
    let after_defer = gh.requests().len();
    forge.flush_cosmetics();
    assert_eq!(forge.cosmetic_count(), 0);
    let retries = &gh.requests()[after_defer..];
    assert!(retries.iter().any(|r| r.starts_with(&format!(
        "PATCH /repos/karma-works/arzt-agent/issues/comments/{id} 200"
    ))));
    assert!(
        !retries
            .iter()
            .any(|r| r.contains(&format!("/issues/{pr}/comments"))),
        "retry must not repeat lookup"
    );
    assert_eq!(
        gh.comments_on(pr)
            .iter()
            .filter(|c| c.body.contains("<!-- schlange:status -->"))
            .count(),
        1
    );
    gh.delete_comment(id);
    forge.execute(&update("replacement"));
    forge.flush_cosmetics();
    let comments = gh.comments_on(pr);
    assert_eq!(
        comments
            .iter()
            .filter(|c| c.body.contains("<!-- schlange:status -->"))
            .count(),
        1
    );
    assert!(
        comments
            .iter()
            .any(|c| c.body.contains("replacement") && c.id != id)
    );
    gh.shutdown();
}

#[test]
fn empty_marker_lookup_reserves_creation_and_ambiguous_create_is_not_duplicated() {
    use schlange_core::Decision;
    use schlange_core::cosmetics::Cosmetic;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let c = client(&gh, roomy());
    let repo = api(&gh, &c);
    repo.open_pulls(Priority::Normal).unwrap(); // installation token before constraining budget
    let mut forge = GithubForge::new(
        repo,
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let decision = Decision::Cosmetic {
        cosmetic: Cosmetic::StatusComment {
            number: pr,
            body: "<!-- schlange:status -->\nnew".into(),
        },
    };
    forge.execute(&decision);
    let constrained = BudgetConfig {
        capacity: 13.0,
        refill_per_sec: 0.0001,
        cosmetic_reserve: 12.0,
    };
    let start = gh.requests().len();
    for _ in 0..2 {
        c.test_set_budget(constrained.clone());
        forge.flush_cosmetics();
    }
    assert_eq!(forge.cosmetic_count(), 1);
    assert!(
        !gh.requests()[start..]
            .iter()
            .any(|r| r.contains(&format!("/issues/{pr}/comments"))),
        "must defer before a lookup that cannot be followed by its write"
    );
    c.test_set_budget(BudgetConfig {
        capacity: 14.0,
        ..constrained
    });
    forge.flush_cosmetics();
    assert_eq!(forge.cosmetic_count(), 0);
    assert_eq!(
        gh.comments_on(pr)
            .iter()
            .filter(|c| c.body.contains("<!-- schlange:status -->"))
            .count(),
        1
    );

    let other = gh.open_pr("someone", "second", &[("b".into(), "b".into())], false, &[]);
    gh.add_fault(FaultRule::new(
        "POST",
        &format!("/issues/{other}/comments"),
        FaultKind::Lie(502),
        1,
    ));
    let other_decision = Decision::Cosmetic {
        cosmetic: Cosmetic::StatusComment {
            number: other,
            body: "<!-- schlange:status -->\nsecond".into(),
        },
    };
    c.test_set_budget(roomy());
    forge.execute(&other_decision);
    forge.flush_cosmetics();
    forge.execute(&other_decision);
    forge.flush_cosmetics();
    assert_eq!(
        gh.comments_on(other)
            .iter()
            .filter(|c| c.body.contains("<!-- schlange:status -->"))
            .count(),
        1,
        "ambiguous create must be found on the next lookup"
    );
    gh.shutdown();
}

#[test]
fn redirected_write_is_fenced_against_actual_destination() {
    use schlange_github::client::WriteGate;
    struct Gate;
    impl WriteGate for Gate {
        fn permit(&self, _: &str, path: &str) -> Result<(), String> {
            if path.contains("/other-repo/") {
                Err("destination is not leased".into())
            } else {
                Ok(())
            }
        }
    }
    for code in [307, 308] {
        let gh = FakeGithub::start(FakeConfig::default());
        gh.open_pr(
            "alice",
            "redirect",
            &[("a".into(), "old".into())],
            false,
            &[],
        );
        let before = gh.requests_with_query().len();
        gh.add_fault(FaultRule::new(
            "DELETE",
            "/git/refs/heads/pr-1",
            FaultKind::Redirect {
                status: code,
                location: "/repos/karma-works/other-repo/git/refs/heads/pr-1".into(),
            },
            1,
        ));
        let c = client(&gh, roomy());
        let repo = api(&gh, &c);
        c.set_write_gate(Some(Arc::new(Gate)));
        let error = repo.delete_ref("pr-1", Priority::Normal).unwrap_err();
        assert!(
            error.to_string().contains("destination is not leased"),
            "{error}"
        );
        assert!(
            !gh.requests_with_query()[before..]
                .iter()
                .any(|r| r.contains("/other-repo/"))
        );
        assert!(gh.branch_sha("pr-1").is_some());
        gh.shutdown();
    }
}

#[test]
fn write_rechecks_lease_after_budget_wait() {
    use schlange_github::client::WriteGate;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Gate(AtomicBool);
    impl WriteGate for Gate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("lease lost".into())
            }
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    gh.open_pr("alice", "budget", &[("a".into(), "old".into())], false, &[]);
    let c = client(&gh, roomy());
    let repo = api(&gh, &c);
    repo.open_pulls(Priority::Normal).unwrap();
    c.test_set_budget(BudgetConfig {
        capacity: 1.0,
        refill_per_sec: 0.5,
        cosmetic_reserve: 0.0,
    });
    repo.open_pulls(Priority::Normal).unwrap();
    let gate = Arc::new(Gate(AtomicBool::new(true)));
    c.set_write_gate(Some(gate.clone()));
    let before = gh.requests_with_query().len();
    let worker = std::thread::spawn(move || repo.delete_ref("pr-1", Priority::Normal));
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while c.test_budget_waits() == 0 {
        assert!(
            std::time::Instant::now() < deadline,
            "write did not reach budget wait"
        );
        std::thread::yield_now();
    }
    gate.0.store(false, Ordering::SeqCst);
    c.test_set_budget(roomy());
    assert!(
        worker
            .join()
            .unwrap()
            .unwrap_err()
            .to_string()
            .contains("lease lost")
    );
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("DELETE "))
    );
    assert!(gh.branch_sha("pr-1").is_some());
    gh.shutdown();
}

#[test]
fn retrying_write_rechecks_fence_before_each_http_attempt() {
    use schlange_github::client::WriteGate;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Gate(AtomicUsize);
    impl WriteGate for Gate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("ingress changed".into())
            }
        }
    }
    for code in [500, 401] {
        let gh = FakeGithub::start(FakeConfig::default());
        let _pr = gh.open_pr(
            "alice",
            "write gate",
            &[("a".into(), "old".into())],
            false,
            &[],
        );
        let original = gh.branch_sha("pr-1").unwrap();
        gh.add_fault(FaultRule::new(
            "DELETE",
            "/git/refs/heads/pr-1",
            FaultKind::Status(code),
            1,
        ));
        let c = client(&gh, roomy());
        let repo = api(&gh, &c);
        c.set_write_gate(Some(Arc::new(Gate(AtomicUsize::new(0)))));
        let before = gh.requests_with_query().len();
        let error = repo.delete_ref("pr-1", Priority::Normal).unwrap_err();
        assert!(error.to_string().contains("ingress changed"), "{error}");
        assert_eq!(gh.branch_sha("pr-1").unwrap(), original);
        assert_eq!(
            gh.requests_with_query()[before..]
                .iter()
                .filter(|r| r.starts_with("DELETE "))
                .count(),
            1
        );
        if code == 401 {
            assert_eq!(
                gh.requests_with_query()[before..]
                    .iter()
                    .filter(|r| r.starts_with("POST ") && r.contains("/access_tokens"))
                    .count(),
                2,
                "initial token and replacement must both be issued"
            );
        }
        gh.shutdown();
    }
}

#[test]
fn reserved_comment_write_still_obeys_fence_and_refunds_on_refusal() {
    use schlange_core::Decision;
    use schlange_core::cosmetics::Cosmetic;
    use schlange_github::client::WriteGate;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    struct LostLease;
    impl WriteGate for LostLease {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Err("lease lost".into())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let c = client(&gh, roomy());
    let repo = api(&gh, &c);
    repo.open_pulls(Priority::Normal).unwrap();
    let mut forge = GithubForge::new(
        repo.clone(),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    c.set_write_gate(Some(Arc::new(LostLease)));
    c.test_set_budget(BudgetConfig {
        capacity: 14.0,
        refill_per_sec: 0.0001,
        cosmetic_reserve: 12.0,
    });
    let before = gh.requests().len();
    forge.execute(&Decision::Cosmetic {
        cosmetic: Cosmetic::StatusComment {
            number: pr,
            body: "<!-- schlange:status -->\ntest".into(),
        },
    });
    forge.flush_cosmetics();
    assert!(
        !gh.requests()[before..]
            .iter()
            .any(|r| r.starts_with("POST ")),
        "fenced write must not be sent"
    );
    assert!(gh.comments_on(pr).is_empty());
    assert!(
        repo.issue_comments(pr, Priority::Cosmetic).is_ok(),
        "unused reserved token was refunded"
    );
    gh.shutdown();
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
        Err(Error::NotFound { .. })
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
    perms.insert("members".into(), "read".into());
    gh.set_permissions(perms);
    gh.add_ruleset(json!({"id": 1, "name": "main", "target": "branch", "enforcement": "active",
                          "bypass_actors": [{"actor_id": 1, "actor_type": "Team", "bypass_mode": "always"}]}));
    let checks =
        doctor::github_checks(&c, std::slice::from_ref(&repo), Some(77), 86_400, Some(&cc));
    assert_eq!(status(&checks, "app-permissions"), "FAIL");
    let p = checks.iter().find(|c| c.name == "app-permissions").unwrap();
    assert!(p.detail.contains("contents: read → write") && p.detail.contains("members"));
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

#[test]
fn per_execution_write_gate_is_isolated_and_cannot_bypass_shared_gate() {
    use schlange_github::client::WriteGate;
    struct Gate(&'static str);
    impl WriteGate for Gate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Err(self.0.into())
        }
    }
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    gh.open_pr(
        "alice",
        "isolation",
        &[("a".into(), "old".into())],
        false,
        &[],
    );
    let client = client(&gh, roomy());
    let scoped = client.with_execution_gate(Arc::new(Gate("execution expired")));
    let isolated = api(&gh, &scoped);
    let before = gh.requests_with_query().len();
    assert!(
        isolated
            .delete_ref("pr-1", Priority::Normal)
            .unwrap_err()
            .to_string()
            .contains("execution expired")
    );
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("DELETE "))
    );
    client.set_write_gate(Some(Arc::new(Gate("lease lost"))));
    assert!(
        api(&gh, &client)
            .delete_ref("pr-1", Priority::Normal)
            .unwrap_err()
            .to_string()
            .contains("lease lost")
    );
    let allowed = client.with_execution_gate(Arc::new(Allow));
    assert!(
        api(&gh, &allowed)
            .delete_ref("pr-1", Priority::Normal)
            .unwrap_err()
            .to_string()
            .contains("lease lost")
    );
    assert!(gh.branch_sha("pr-1").is_some());
    gh.shutdown();
}

#[test]
fn scoped_gate_cannot_outlive_shared_lease_check() {
    use schlange_github::client::WriteGate;
    use std::sync::atomic::{AtomicBool, Ordering};
    struct Lease(Arc<AtomicBool>);
    impl WriteGate for Lease {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            if self.0.load(Ordering::SeqCst) {
                Ok(())
            } else {
                Err("lease moved".into())
            }
        }
    }
    struct Invalidate(Arc<AtomicBool>);
    impl WriteGate for Invalidate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            self.0.store(false, Ordering::SeqCst);
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    gh.open_pr(
        "alice",
        "lease race",
        &[("a".into(), "old".into())],
        false,
        &[],
    );
    let c = client(&gh, roomy());
    let live = Arc::new(AtomicBool::new(true));
    c.set_write_gate(Some(Arc::new(Lease(live.clone()))));
    let scoped = c.with_execution_gate(Arc::new(Invalidate(live)));
    let before = gh.requests_with_query().len();
    assert!(
        api(&gh, &scoped)
            .delete_ref("pr-1", Priority::Normal)
            .unwrap_err()
            .to_string()
            .contains("lease moved")
    );
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("DELETE "))
    );
    assert!(gh.branch_sha("pr-1").is_some());
    gh.shutdown();
}
