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
fn refresh_rejects_a_label_change_during_fake_http_read() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use schlange_github_fake::Then;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "read race",
        &[("a".into(), "old".into())],
        false,
        &[],
    );
    gh.set_classic_required_checks(Some(&[]));
    let forge = GithubForge::new(
        api(&gh, &client(&gh, roomy())),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    gh.add_fault(
        FaultRule::new("GET", "/check-suites", FaultKind::Status(500), 1)
            .then(Then::Label(pr, "halt".into())),
    );
    let error = forge
        .consistent_refresh(&[pr], &["main".into()], 9, Priority::Landing, || true)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("refresh inputs changed between reads"),
        "unexpected error: {error}"
    );
    let requests = gh.requests_with_query();
    assert!(
        requests
            .iter()
            .any(|r| r.contains("/check-suites") && r.ends_with(" 500"))
    );
    assert!(
        requests
            .iter()
            .any(|r| r.contains("/check-suites") && r.ends_with(" 200"))
    );
    let stable = forge
        .consistent_refresh(&[pr], &["main".into()], 9, Priority::Landing, || true)
        .unwrap();
    assert!(
        matches!(stable, schlange_core::Event::Snapshot { pull_requests, .. } if pull_requests[0].labels.contains("halt"))
    );
    gh.shutdown();
}

#[test]
fn captured_refresh_rejects_push_after_real_http_read() {
    use schlange_core::{Decision, Event, State, journal::Journal};
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "read race",
        &[("a".into(), "old".into())],
        false,
        &[],
    );
    gh.set_classic_required_checks(Some(&[]));
    let api = api(&gh, &client(&gh, roomy()));
    let forge = GithubForge::new(
        api,
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let dir = temp_dir("captured-refresh-http");
    let store = Store::open(&dir).unwrap();
    let repo = gh.config().full_name();
    assert!(store.lease_acquire(&repo, "a", 100, 130).unwrap());
    let mut journal = store.journal(&repo).fenced("a");
    journal.checkpoint(&State::default()).unwrap();
    let revision = store.inbox_revision(&repo).unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES (?1, 'session-a', 'a', 7)", [&repo]).unwrap();
    let request = Decision::Refresh {
        token: 9,
        numbers: vec![pr],
        branches: vec!["main".into()],
    };
    let assigned = journal
        .commit_session_step(
            "session-a",
            revision,
            &State::default(),
            &[request.clone()],
            || 101,
        )
        .unwrap();
    let ticket = journal
        .capture_session_refresh(
            "session-a",
            revision + 1,
            assigned[0].0,
            &request,
            17,
            || 101,
        )
        .unwrap();
    let old = forge
        .consistent_refresh(&[pr], &["main".into()], 9, Priority::Landing, || true)
        .unwrap();
    let old_head = match &old {
        Event::Snapshot { pull_requests, .. } => pull_requests[0].head_sha.clone(),
        _ => panic!("snapshot"),
    };
    let new_head = gh.push_pr(pr, &[("a".into(), "new".into())]);
    assert_ne!(old_head, new_head);
    let rules = schlange_config::rules::Rules::default();
    assert!(
        journal
            .commit_captured_refresh(&ticket, 18, &rules, &old, 5, || 101)
            .is_err()
    );
    assert_eq!(journal.recover().unwrap().unwrap().in_doubt, assigned);
    let _failed_ticket = journal
        .capture_session_refresh(
            "session-a",
            revision + 1,
            assigned[0].0,
            &request,
            18,
            || 101,
        )
        .unwrap();
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{pr}"),
        FaultKind::Status(403),
        1,
    ));
    assert!(
        forge
            .consistent_refresh(&[pr], &["main".into()], 9, Priority::Landing, || true)
            .is_err()
    );
    assert_eq!(journal.recover().unwrap().unwrap().in_doubt, assigned);
    let ticket = journal
        .capture_session_refresh(
            "session-a",
            revision + 1,
            assigned[0].0,
            &request,
            18,
            || 101,
        )
        .unwrap();
    let fresh = forge
        .consistent_refresh(&[pr], &["main".into()], 9, Priority::Landing, || true)
        .unwrap();
    let committed = journal
        .commit_captured_refresh(&ticket, 18, &rules, &fresh, 6, || 101)
        .unwrap();
    assert_eq!(committed.state.pull_requests[&pr].head_sha, new_head);
    assert!(committed.intents.is_empty());
    gh.shutdown();
    let _ = std::fs::remove_dir_all(dir);
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
fn service_build_stage_evidence_failure_blocks_later_writes() {
    use schlange_core::Decision;
    use schlange_core::engine::{CarPr, short};
    use schlange_github::service::TestBuildAttempts;
    for case in [
        "read-denied",
        "source-number",
        "wrong-host",
        "merge-tree",
        "receipt-swap",
        "draft-changed",
        "wrong-bot",
    ] {
        let gh = FakeGithub::start(FakeConfig::default());
        let pr = gh.open_pr(
            "alice",
            "staged evidence",
            &[("a".into(), "new".into())],
            false,
            &[],
        );
        let head = gh.pr(pr).unwrap().head_sha;
        let base = gh.branch_sha("main").unwrap();
        let branch = "queue/evidence";
        let decision = Decision::BuildCar {
            car: 1,
            queue: "default".into(),
            base_branch: "main".into(),
            branch: branch.into(),
            base_sha: base.clone(),
            prs: vec![CarPr {
                number: pr,
                head_sha: head.clone(),
            }],
            own: vec![pr],
            stack_on: None,
        };
        let plan = TestBuildAttempts::new(
            &gh.config().full_name(),
            &decision,
            &gh.url(),
            gh.config().installation_id,
            "schlange[bot]",
        )
        .unwrap();
        let c = client(&gh, roomy()).with_execution_gate(plan.gate());
        let repo = api(&gh, &c);
        repo.create_ref(branch, &base, Priority::Normal).unwrap();
        if case == "read-denied" || case == "source-number" || case == "wrong-host" {
            if case == "read-denied" {
                gh.add_fault(FaultRule::new(
                    "GET",
                    "/git/ref/heads/queue/evidence",
                    FaultKind::Status(403),
                    1,
                ));
            } else if case == "source-number" {
                gh.add_fault(FaultRule::new("GET", &format!("/pulls/{pr}"), FaultKind::Json(json!({"number":pr+1,"state":"open","user":{"login":"alice"},"head":{"ref":"pr-1","sha":head,"repo":{"full_name":gh.config().full_name()}},"base":{"ref":"main","sha":base,"repo":{"full_name":gh.config().full_name()}}})), 1));
            }
            if case == "wrong-host" {
                let other = FakeGithub::start(FakeConfig::default());
                let other_client = client(&other, roomy());
                let other_repo = api(&other, &other_client);
                let before = other.requests_with_query().len();
                assert!(plan.confirm_create(&other_repo).is_err());
                assert_eq!(
                    other.requests_with_query().len(),
                    before,
                    "a substituted host must be rejected before reading"
                );
                other.shutdown();
            } else {
                assert!(plan.confirm_create(&repo).is_err());
            }
            assert!(
                repo.merge_into(
                    branch,
                    &head,
                    &format!("Merge #{pr} into {branch}"),
                    Priority::Normal
                )
                .is_err()
            );
        } else {
            plan.confirm_create(&repo).unwrap();
            let merged = repo
                .merge_into(
                    branch,
                    &head,
                    &format!("Merge #{pr} into {branch}"),
                    Priority::Normal,
                )
                .unwrap();
            if case == "receipt-swap" {
                plan.test_replace_response(br#"{"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","commit":{"tree":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}}"#.to_vec());
                assert!(
                    plan.confirm_merge(&repo, &merged).is_err(),
                    "the claimed merge must match the authenticated response"
                );
            } else if case == "merge-tree" {
                let MergeOutcome::Merged { sha, .. } = &merged else {
                    panic!("expected merge")
                };
                let forged = MergeOutcome::Merged {
                    sha: sha.clone(),
                    tree: "1111111111111111111111111111111111111111".into(),
                };
                assert!(plan.confirm_merge(&repo, &forged).is_err());
                assert!(
                    plan.confirm_merge(&repo, &merged).is_err(),
                    "failed evidence remains held"
                );
            } else {
                plan.confirm_merge(&repo, &merged).unwrap();
                let body = format!(
                    "Schlange tests these pull requests together with `main` at {}:\n\n- #{pr} at {}\n\n## Datenschutz\n\nSee the constituent pull requests for their data protection details.\n\nDo not edit this pull request; the queue closes it.",
                    short(&base),
                    short(&head)
                );
                let draft = repo
                    .create_pull(
                        &format!("merge queue: embarking #{pr}"),
                        branch,
                        "main",
                        &body,
                        true,
                        Priority::Normal,
                    )
                    .unwrap();
                if case == "wrong-bot" {
                    assert!(plan.confirm_draft(&repo, &draft, "attacker").is_err());
                } else {
                    gh.set_draft(draft.number, false);
                    assert!(plan.confirm_draft(&repo, &draft, "schlange[bot]").is_err());
                    gh.set_draft(draft.number, true);
                }
                assert!(
                    plan.confirm_draft(&repo, &draft, "schlange[bot]").is_err(),
                    "failed evidence remains held"
                );
            }
        }
        gh.shutdown();
    }
}

#[test]
fn service_build_plan_refuses_retry_after_non_created_response() {
    use schlange_core::Decision;
    use schlange_core::engine::CarPr;
    use schlange_github::service::TestBuildAttempts;
    for status in [401, 500, 307, 308] {
        let gh = FakeGithub::start(FakeConfig::default());
        let pr = gh.open_pr(
            "alice",
            "staged failure",
            &[("a".into(), "new".into())],
            false,
            &[],
        );
        let head = gh.pr(pr).unwrap().head_sha;
        let base = gh.branch_sha("main").unwrap();
        let decision = Decision::BuildCar {
            car: 1,
            queue: "default".into(),
            base_branch: "main".into(),
            branch: "queue/staged-fail".into(),
            base_sha: base.clone(),
            prs: vec![CarPr {
                number: pr,
                head_sha: head,
            }],
            own: vec![pr],
            stack_on: None,
        };
        let plan = TestBuildAttempts::new(
            &gh.config().full_name(),
            &decision,
            &gh.url(),
            gh.config().installation_id,
            "schlange[bot]",
        )
        .unwrap();
        let c = client(&gh, roomy()).with_execution_gate(plan.gate());
        let repo = api(&gh, &c);
        let kind = if status == 307 || status == 308 {
            FaultKind::Redirect {
                status,
                location: format!("/repos/{}/git/refs", gh.config().full_name()),
            }
        } else {
            FaultKind::Status(status)
        };
        gh.add_fault(FaultRule::new("POST", "/git/refs", kind, 1));
        let before = gh.requests_with_query().len();
        assert!(
            repo.create_ref("queue/staged-fail", &base, Priority::Normal)
                .is_err()
        );
        assert!(plan.confirm().is_err());
        assert!(
            repo.create_ref("queue/staged-fail", &base, Priority::Normal)
                .is_err()
        );
        assert_eq!(
            gh.requests_with_query()[before..]
                .iter()
                .filter(|r| r.starts_with("POST ") && r.contains("/git/refs"))
                .count(),
            1,
            "status {status}"
        );
        assert!(gh.branch_sha("queue/staged-fail").is_none());
        gh.shutdown();
    }
}

#[test]
fn service_build_plan_matches_real_http_serialization_and_confirms_each_stage() {
    use schlange_core::Decision;
    use schlange_core::engine::{CarPr, short};
    use schlange_github::service::TestBuildAttempts;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("alice", "staged", &[("a".into(), "new".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    let branch = "queue/staged";
    let decision = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: branch.into(),
        base_sha: base.clone(),
        prs: vec![CarPr {
            number: pr,
            head_sha: head.clone(),
        }],
        own: vec![pr],
        stack_on: None,
    };
    let plan = TestBuildAttempts::new(
        &gh.config().full_name(),
        &decision,
        &gh.url(),
        gh.config().installation_id,
        "schlange[bot]",
    )
    .unwrap();
    let c = client(&gh, roomy()).with_execution_gate(plan.gate());
    let repo = api(&gh, &c);
    assert!(plan.confirm().is_err());
    repo.create_ref(branch, &base, Priority::Normal).unwrap();
    plan.confirm_create(&repo).unwrap();
    let merged = repo
        .merge_into(
            branch,
            &head,
            &format!("Merge #{pr} into {branch}"),
            Priority::Normal,
        )
        .unwrap();
    plan.confirm_merge(&repo, &merged).unwrap();
    let body = format!(
        "Schlange tests these pull requests together with `main` at {}:\n\n- #{pr} at {}\n\n## Datenschutz\n\nSee the constituent pull requests for their data protection details.\n\nDo not edit this pull request; the queue closes it.",
        short(&base),
        short(&head)
    );
    let draft = repo
        .create_pull(
            &format!("merge queue: embarking #{pr}"),
            branch,
            "main",
            &body,
            true,
            Priority::Normal,
        )
        .unwrap();
    plan.confirm_draft(&repo, &draft, "schlange[bot]").unwrap();
    assert!(
        repo.create_ref(branch, &base, Priority::Normal).is_err(),
        "completed plan cannot restart"
    );
    gh.shutdown();
}

#[test]
fn scoped_gate_observes_raw_status_before_retry() {
    use schlange_github::client::WriteGate;
    struct Gate;
    impl WriteGate for Gate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn response(&self, _: &str, _: &str, status: u16, _: &[u8]) -> Result<(), String> {
            if status == 201 {
                Ok(())
            } else {
                Err("outcome unknown".into())
            }
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    gh.open_pr(
        "alice",
        "response",
        &[("a".into(), "old".into())],
        false,
        &[],
    );
    gh.add_fault(FaultRule::new(
        "DELETE",
        "/git/refs/heads/pr-1",
        FaultKind::Status(401),
        1,
    ));
    let c = client(&gh, roomy()).with_execution_gate(Arc::new(Gate));
    let before = gh.requests_with_query().len();
    let error = api(&gh, &c)
        .delete_ref("pr-1", Priority::Normal)
        .unwrap_err();
    assert!(error.to_string().contains("outcome unknown"));
    assert_eq!(
        gh.requests_with_query()[before..]
            .iter()
            .filter(|r| r.starts_with("DELETE "))
            .count(),
        1
    );
    assert!(gh.branch_sha("pr-1").is_some());
    gh.shutdown();
}

#[test]
fn scoped_gate_checks_the_exact_write_body() {
    use schlange_github::client::WriteGate;
    struct Gate;
    impl WriteGate for Gate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn permit_body(&self, _: &str, _: &str, body: Option<&[u8]>) -> Result<(), String> {
            let value: serde_json::Value =
                serde_json::from_slice(body.ok_or("missing body")?).map_err(|_| "invalid body")?;
            if value["ref"] == "refs/heads/queue/allowed" {
                Ok(())
            } else {
                Err("wrong branch".into())
            }
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy()).with_execution_gate(Arc::new(Gate));
    let repo = api(&gh, &c);
    let sha = gh.branch_sha("main").unwrap();
    let before = gh.requests_with_query().len();
    assert!(
        repo.create_ref("queue/other", &sha, Priority::Normal)
            .unwrap_err()
            .to_string()
            .contains("wrong branch")
    );
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("POST ") && r.contains("/git/refs"))
    );
    repo.create_ref("queue/allowed", &sha, Priority::Normal)
        .unwrap();
    assert_eq!(gh.branch_sha("queue/allowed"), Some(sha));
    gh.shutdown();
}

#[test]
fn scoped_client_cannot_issue_unfenced_write() {
    use schlange_github::client::{Auth, WriteGate};
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    gh.open_pr(
        "alice",
        "unfenced",
        &[("a".into(), "old".into())],
        false,
        &[],
    );
    let c = client(&gh, roomy()).with_execution_gate(Arc::new(Allow));
    let before = gh.requests_with_query().len();
    let path = format!("/repos/{}/git/refs/heads/pr-1", gh.config().full_name());
    assert!(
        c.send_unfenced(
            "DELETE",
            &path,
            Auth::Installation(gh.config().installation_id),
            Priority::Normal,
            None
        )
        .is_err()
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

#[test]
fn session_build_returns_prefix_without_standalone_car_persistence() {
    use schlange_core::Decision;
    use schlange_core::engine::CarPr;
    use schlange_github::client::WriteGate;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    struct Allow;
    impl WriteGate for Allow {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("alice", "prefix", &[("a".into(), "new".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    let dir = std::env::temp_dir().join(format!("schlange-build-prefix-{}", std::process::id()));
    let store = Store::open(&dir).unwrap();
    let original_gate: Arc<dyn WriteGate> = Arc::new(Allow);
    let other_gate: Arc<dyn WriteGate> = Arc::new(Allow);
    let scoped_client = client(&gh, roomy()).with_execution_gate(original_gate.clone());
    let mut forge = GithubForge::new(
        api(&gh, &scoped_client),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    forge.store = Some(store.clone());
    let decision = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "queue/prefix".into(),
        base_sha: base,
        prs: vec![CarPr {
            number: pr,
            head_sha: head,
        }],
        own: vec![pr],
        stack_on: None,
    };
    let mut result = forge.execute_session_build_once(&decision).unwrap();
    assert!(schlange_github::service::test_receipt_gate_matches(
        &original_gate,
        &original_gate,
        &result
    ));
    assert!(
        !schlange_github::service::test_receipt_gate_matches(&other_gate, &original_gate, &result),
        "a receipt cannot be substituted into another gate"
    );
    assert_eq!(result.prefix().len(), 1);
    assert_eq!(result.prefix()[0].0, pr);
    let schlange_core::Event::CarBuilt {
        car,
        branch,
        tree,
        draft,
        ..
    } = result.event()
    else {
        panic!("missing build reply")
    };
    assert_eq!(*car, 1);
    assert_eq!(branch, "queue/prefix");
    assert_eq!(result.prefix()[0].1, *tree);
    let draft = *draft;
    forge.verify_session_build_once(&decision, &result).unwrap();
    let other_gh = FakeGithub::start(FakeConfig::default());
    let other = client(&other_gh, roomy()).with_execution_gate(Arc::new(Allow));
    let other_forge = GithubForge::new(
        api(&other_gh, &other),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    assert!(
        other_forge
            .verify_session_build_once(&decision, &result)
            .unwrap_err()
            .to_string()
            .contains("receipt context changed")
    );
    other_gh.shutdown();
    let mut fresh_forge = GithubForge::new(
        api(&gh, &scoped_client),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let before_retry = gh.requests_with_query().len();
    assert!(
        fresh_forge.execute_session_build_once(&decision).is_err(),
        "a fresh adapter must not adopt an existing artifact"
    );
    assert!(
        !gh.requests_with_query()[before_retry..]
            .iter()
            .any(|r| r.starts_with("DELETE "))
    );
    let mut substituted = decision.clone();
    if let Decision::BuildCar { queue, .. } = &mut substituted {
        *queue = "attacker-queue".into();
    }
    assert!(
        forge
            .verify_session_build_once(&substituted, &result)
            .is_err(),
        "receipt must bind the entire build decision"
    );
    let prefix = result.prefix().to_vec();
    result.test_replace_prefix(vec![(pr, "invalid".into())]);
    assert!(
        forge.verify_session_build_once(&decision, &result).is_err(),
        "mismatched prefix must not verify"
    );
    result.test_replace_prefix(prefix);
    gh.set_draft(draft, false);
    assert!(
        forge.verify_session_build_once(&decision, &result).is_err(),
        "draft becoming ready must not verify"
    );
    gh.set_draft(draft, true);
    gh.add_fault(FaultRule::new("GET", "/git/ref/heads/queue/prefix", FaultKind::Json(json!({"ref":"refs/heads/queue/prefix", "object":{"sha":"0000000000000000000000000000000000000000"}})), 1));
    assert!(
        forge.verify_session_build_once(&decision, &result).is_err(),
        "moved branch must not verify"
    );
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{draft}"),
        FaultKind::Json(json!({"number": draft})),
        1,
    ));
    assert!(
        forge.verify_session_build_once(&decision, &result).is_err(),
        "incomplete draft must not verify"
    );
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{pr}"),
        FaultKind::Status(403),
        1,
    ));
    assert!(
        forge.verify_session_build_once(&decision, &result).is_err(),
        "source read failure must not verify"
    );
    gh.push_pr(pr, &[("later".into(), "changed".into())]);
    assert!(
        forge.verify_session_build_once(&decision, &result).is_err(),
        "changed source head must not verify"
    );
    assert!(
        store
            .load_car(&gh.config().full_name(), 1)
            .unwrap()
            .is_none()
    );
    gh.shutdown();
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn session_build_keeps_partial_branch_on_ambiguous_merge() {
    use schlange_core::Decision;
    use schlange_core::engine::CarPr;
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
        "ambiguous merge",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    gh.add_fault(FaultRule::new("POST", "/merges", FaultKind::Lie(500), 1));
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
    let decision = Decision::BuildCar {
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
    assert!(forge.execute_session_build_once(&decision).is_err());
    forge.hurry_pending();
    forge.retry_pending();
    let log = gh.requests_with_query();
    let writes: Vec<_> = log[before..]
        .iter()
        .filter(|r| {
            (r.starts_with("POST ") || r.starts_with("DELETE ")) && !r.contains("/access_tokens")
        })
        .collect();
    assert_eq!(writes.len(), 2, "{writes:?}");
    assert!(
        writes[0].contains("/git/refs") && writes[1].contains("/merges"),
        "{writes:?}"
    );
    assert_ne!(gh.branch_sha("queue/ambiguous"), Some(base));
    assert_eq!(gh.open_queue_drafts().len(), 0);
    let mut noop = decision.clone();
    if let Decision::BuildCar { branch, .. } = &mut noop {
        *branch = "queue/noop".into();
    }
    gh.add_fault(FaultRule::new("POST", "/merges", FaultKind::Status(204), 1));
    assert!(
        forge.execute_session_build_once(&noop).is_err(),
        "204 provides no merge receipt"
    );
    assert_eq!(gh.open_queue_drafts().len(), 0);
    if let Decision::BuildCar { branch, .. } = &mut noop {
        *branch = "queue/wrong-status".into();
    }
    gh.add_fault(FaultRule::new("POST", "/merges", FaultKind::Json(json!({"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa", "commit":{"tree":{"sha":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"}}})), 1));
    assert!(
        forge.execute_session_build_once(&noop).is_err(),
        "a 200 response is not a merge creation receipt"
    );
    assert_eq!(gh.open_queue_drafts().len(), 0);
    gh.shutdown();
}

#[test]
fn session_build_refuses_existing_branch_without_deleting_it() {
    use schlange_core::Decision;
    use schlange_core::engine::CarPr;
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
        "existing branch",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let original = gh.branch_sha("pr-1").unwrap();
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
    let decision = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "pr-1".into(),
        base_sha: gh.branch_sha("main").unwrap(),
        prs: vec![CarPr {
            number: pr,
            head_sha: original.clone(),
        }],
        own: vec![pr],
        stack_on: None,
    };
    let before = gh.requests_with_query().len();
    assert!(forge.execute_session_build_once(&decision).is_err());
    assert_eq!(gh.branch_sha("pr-1"), Some(original));
    assert!(
        !gh.requests_with_query()[before..]
            .iter()
            .any(|r| r.starts_with("DELETE "))
    );
    gh.shutdown();
}

#[test]
fn scoped_build_stops_after_first_write_when_execution_is_invalidated() {
    use schlange_core::Decision;
    use schlange_core::engine::CarPr;
    use schlange_github::client::WriteGate;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Gate(AtomicUsize);
    impl WriteGate for Gate {
        fn permit(&self, _: &str, _: &str) -> Result<(), String> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                Ok(())
            } else {
                Err("execution invalidated".into())
            }
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "partial build",
        &[("a".into(), "new".into())],
        false,
        &[],
    );
    let head = gh.pr(pr).unwrap().head_sha;
    let base = gh.branch_sha("main").unwrap();
    let gate = Arc::new(Gate(AtomicUsize::new(0)));
    let c = client(&gh, roomy()).with_execution_gate(gate.clone());
    let mut forge = GithubForge::new(
        api(&gh, &c),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let decision = Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "queue/partial".into(),
        base_sha: base.clone(),
        prs: vec![CarPr {
            number: pr,
            head_sha: head,
        }],
        own: vec![pr],
        stack_on: None,
    };
    let mut unscoped = GithubForge::new(
        api(&gh, &client(&gh, roomy())),
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let before = gh.requests_with_query().len();
    assert!(unscoped.execute_session_build_once(&decision).is_err());
    assert_eq!(gh.requests_with_query().len(), before);
    assert!(forge.execute_session_build_once(&decision).is_err());
    let attempts = gate.0.load(Ordering::SeqCst);
    forge.hurry_pending();
    forge.retry_pending();
    assert_eq!(
        gate.0.load(Ordering::SeqCst),
        attempts,
        "failed session builds must not be queued for retry"
    );
    let writes: Vec<_> = gh.requests_with_query()[before..]
        .iter()
        .filter(|r| {
            (r.starts_with("POST ") || r.starts_with("PATCH ") || r.starts_with("DELETE "))
                && !r.contains("/access_tokens")
        })
        .cloned()
        .collect();
    assert_eq!(writes.len(), 1, "{writes:?}");
    assert!(writes[0].contains("/git/refs"), "{writes:?}");
    assert_eq!(gh.branch_sha("queue/partial"), Some(base));
    assert_eq!(gh.open_queue_drafts().len(), 0);
    gh.shutdown();
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
        let c = client(&gh, roomy()).with_execution_gate(Arc::new(Gate(AtomicUsize::new(0))));
        let repo = api(&gh, &c);
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
    assert_eq!(
        a.branch_protection("main", Priority::Normal)
            .unwrap_err()
            .status(),
        Some(404)
    );
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
    perms.remove("administration");
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

#[test]
fn suite_run_and_actions_workflow_identity_are_read_over_http() {
    let mut cfg = FakeConfig::default();
    cfg.permissions.insert("actions".into(), "read".into());
    let gh = FakeGithub::start(cfg);
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let (suite, run) =
        gh.report_check_with_identity(pr, "ci-ok", "failure", "ci.yml", "pull_request");
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let head = gh.pr(pr).unwrap().head_sha;
    let suites = api.check_suites(&head, Priority::Normal).unwrap();
    let found = suites.iter().find(|s| s.id == suite).expect("suite");
    assert!(found.created_at.is_some());
    let runs = api.check_suite_runs(suite, Priority::Normal).unwrap();
    assert_eq!(runs.len(), 1);
    assert_eq!(runs[0].id, run);
    assert_eq!(runs[0].check_suite.as_ref().unwrap().id, suite);
    let workflows = api
        .workflow_runs_for_suite(suite, Priority::Normal)
        .unwrap();
    assert_eq!(workflows.len(), 1);
    assert_eq!(workflows[0].check_suite_id, suite);
    assert_eq!(workflows[0].path, ".github/workflows/ci.yml");
    assert_eq!(workflows[0].event, "pull_request");
    assert_eq!(workflows[0].head_sha, head);
}

#[test]
fn suite_and_workflow_identity_reads_require_their_permissions() {
    let mut cfg = FakeConfig::default();
    cfg.permissions.remove("checks");
    cfg.permissions.remove("actions");
    let gh = FakeGithub::start(cfg);
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    assert_eq!(
        api.check_suite_runs(5, Priority::Normal)
            .unwrap_err()
            .status(),
        Some(403)
    );
    assert_eq!(
        api.workflow_runs_for_suite(5, Priority::Normal)
            .unwrap_err()
            .status(),
        Some(403)
    );
}

#[test]
fn malformed_check_suite_list_does_not_become_an_empty_list() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    gh.add_fault(FaultRule::new(
        "GET",
        "/commits/abc/check-suites",
        FaultKind::Json(json!({"total_count": 1})),
        1,
    ));
    assert!(api.check_suites("abc", Priority::Normal).is_err());
    gh.add_fault(FaultRule::new(
        "GET",
        "/commits/abc/check-suites",
        FaultKind::Json(json!({"total_count": 2, "check_suites": []})),
        1,
    ));
    assert!(api.check_suites("abc", Priority::Normal).is_err());
}

#[test]
fn suite_history_paginates_and_preserves_filter_query() {
    let mut cfg = FakeConfig::default();
    cfg.permissions.insert("actions".into(), "read".into());
    let gh = FakeGithub::start(cfg);
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let (suite, _) =
        gh.report_check_with_identity(pr, "ci-ok", "failure", "ci.yml", "pull_request");
    for _ in 0..100 {
        gh.report_check_in_suite(pr, suite, "ci-ok", "success");
    }
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let runs = api.check_suite_runs(suite, Priority::Normal).unwrap();
    assert_eq!(runs.len(), 101);
    let requests = gh.requests_with_query();
    assert!(
        requests
            .iter()
            .any(|r| r.contains("filter=all") && r.contains("page=2")),
        "{requests:?}"
    );
    let path = format!(
        "/repos/{}/check-suites/{suite}/check-runs?filter=all&per_page=100",
        gh.config().full_name()
    );
    let short = c.get_paginated_checked::<serde_json::Value>(
        &path,
        schlange_github::client::Auth::Installation(gh.config().installation_id),
        Priority::Normal,
        1,
        "check_runs",
    );
    assert!(
        short.is_err(),
        "a page limit must not silently return a partial suite"
    );
    gh.add_fault(
        FaultRule::new(
            "GET",
            &format!("/check-suites/{suite}/check-runs"),
            FaultKind::Json(json!({"total_count": 101, "check_runs": []})),
            1,
        )
        .query("page=2"),
    );
    assert!(
        api.check_suite_runs(suite, Priority::Normal).is_err(),
        "missing second page items must fail closed"
    );
}

#[test]
fn required_contexts_include_rulesets_and_classic_protection() {
    let gh = FakeGithub::start(FakeConfig::default());
    gh.set_required_checks(&["ruleset-ci"]);
    gh.set_classic_required_checks(Some(&["classic-ci"]));
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let required = api
        .required_check_contexts("main", Priority::Normal)
        .unwrap();
    assert!(required.iter().any(|r| r.context == "ruleset-ci"));
    assert!(required.iter().any(|r| r.context == "classic-ci"));
    gh.set_required_checks(&[]);
    gh.set_classic_required_checks(None);
    let required = api
        .required_check_contexts("main", Priority::Normal)
        .unwrap();
    assert!(!required.iter().any(|r| r.context == "classic-ci"));
    gh.set_required_checks(&["ruleset-ci"]);
    assert!(
        api.required_check_contexts("main", Priority::Normal)
            .is_err(),
        "a ruleset-protected branch and a classic 404 is ambiguous"
    );
}

#[test]
fn classic_protection_permission_denial_is_not_treated_as_no_checks() {
    let mut cfg = FakeConfig::default();
    cfg.permissions.remove("administration");
    let gh = FakeGithub::start(cfg);
    gh.set_classic_required_checks(Some(&["critical-ci"]));
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    assert!(
        api.required_check_contexts("main", Priority::Normal)
            .is_err()
    );
    gh.set_classic_required_checks(None);
    assert!(
        api.required_check_contexts("main", Priority::Normal)
            .is_err(),
        "an unprotected-looking branch still requires known administration permission"
    );
}

#[test]
fn downscoped_token_without_admin_cannot_assert_no_classic_protection() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let cfg = gh.config();
    let permissions = [
        ("metadata", "read"),
        ("contents", "read"),
        ("checks", "read"),
        ("actions", "read"),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    c.set_token_scope(cfg.installation_id, &[cfg.name.clone()], &permissions);
    assert!(
        api(&gh, &c)
            .required_check_contexts("main", Priority::Normal)
            .is_err()
    );
}

#[test]
fn workflow_jobs_include_all_attempts_and_fail_on_incomplete_later_page() {
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let (suite, _) =
        gh.report_check_with_identity(pr, "ci-ok", "failure", "ci.yml", "pull_request");
    for _ in 0..100 {
        gh.report_check_in_suite(pr, suite, "ci-ok", "success");
    }
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    assert_eq!(
        api.workflow_jobs(suite, Priority::Normal).unwrap().len(),
        101
    );
    let requests = gh.requests_with_query();
    assert!(
        requests
            .iter()
            .any(|r| r.contains(&format!("/actions/runs/{suite}/jobs"))
                && r.contains("filter=all")
                && r.contains("page=2")),
        "{requests:?}"
    );
    gh.add_fault(
        FaultRule::new(
            "GET",
            &format!("/actions/runs/{suite}/jobs"),
            FaultKind::Json(json!({"total_count": 101, "jobs": []})),
            1,
        )
        .query("page=2"),
    );
    assert!(api.workflow_jobs(suite, Priority::Normal).is_err());
}

#[test]
fn required_actions_same_name_distinct_workflows_do_not_hide_failure() {
    use schlange_core::model::Conclusion;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.set_required_checks(&["ci-ok"]);
    gh.set_classic_required_checks(Some(&[]));
    gh.report_check_with_identity(pr, "ci-ok", "failure", "a.yml", "pull_request");
    gh.report_check_with_identity(pr, "ci-ok", "success", "b.yml", "pull_request");
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
    let read = forge.read_pr(pr, Priority::Normal).unwrap();
    assert!(read.required_check_names.contains("ci-ok"));
    assert!(
        read.checks
            .iter()
            .filter(|c| c.name == "ci-ok")
            .all(|c| c.app_id == Some(15368))
    );
    assert!(
        read.current_checks()
            .iter()
            .any(|c| c.name == "ci-ok" && c.conclusion == Some(Conclusion::Failure))
    );
}

#[test]
fn required_actions_same_workflow_rerun_replaces_only_same_event() {
    use schlange_core::model::Conclusion;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.set_required_checks(&["ci-ok"]);
    gh.set_classic_required_checks(Some(&[]));
    gh.report_check_with_identity(pr, "ci-ok", "failure", "a.yml", "pull_request");
    gh.report_check_with_identity(pr, "ci-ok", "success", "a.yml", "pull_request");
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
    let read = forge.read_pr(pr, Priority::Normal).unwrap();
    assert_eq!(
        read.current_checks()
            .iter()
            .filter(|c| c.name == "ci-ok")
            .count(),
        1
    );
    assert!(
        read.current_checks()
            .iter()
            .any(|c| c.name == "ci-ok" && c.conclusion == Some(Conclusion::Success))
    );
    gh.report_check_with_identity(pr, "ci-ok", "failure", "a.yml", "workflow_dispatch");
    let read = forge.read_pr(pr, Priority::Normal).unwrap();
    assert!(
        read.current_checks()
            .iter()
            .any(|c| c.name == "ci-ok" && c.conclusion == Some(Conclusion::Failure))
    );
}

#[test]
fn required_actions_check_without_matching_workflow_job_fails_closed() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.set_required_checks(&["ci-ok"]);
    gh.set_classic_required_checks(Some(&[]));
    let (suite, _) = gh.report_check_with_identity(pr, "ci-ok", "failure", "a.yml", "pull_request");
    gh.report_check_with_identity(pr, "ci-ok", "success", "b.yml", "pull_request");
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/actions/runs/{suite}/jobs"),
        FaultKind::Json(json!({"total_count": 0, "jobs": []})),
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
    assert!(forge.read_pr(pr, Priority::Normal).is_err());
}

#[test]
fn required_actions_rejects_forged_and_ambiguous_job_associations() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.set_required_checks(&["ci-ok"]);
    gh.set_classic_required_checks(Some(&[]));
    let (suite, check) =
        gh.report_check_with_identity(pr, "ci-ok", "failure", "a.yml", "pull_request");
    gh.report_check_with_identity(pr, "ci-ok", "success", "b.yml", "pull_request");
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let head = api.pull(pr, Priority::Normal).unwrap().head.sha;
    let good = json!({"run_id": suite, "head_sha": head,
        "check_run_url": format!("{}/repos/{}/check-runs/{check}", gh.url(), gh.config().full_name())});
    let forge = GithubForge::new(
        api,
        Arc::new(ManualClock::new(1_800_000_000)),
        ForgeOptions {
            bot_login: "schlange[bot]".into(),
            app_slug: "schlange".into(),
            check_name: "queue".into(),
        },
    );
    let path = format!("/actions/runs/{suite}/jobs");
    for jobs in [
        vec![
            json!({"run_id": suite, "head_sha": head, "check_run_url": format!("https://evil.example/repos/{}/check-runs/{check}", gh.config().full_name())}),
        ],
        vec![good.clone(), good.clone()],
        vec![
            json!({"run_id": suite + 1, "head_sha": head, "check_run_url": good["check_run_url"]}),
        ],
    ] {
        gh.add_fault(FaultRule::new(
            "GET",
            &path,
            FaultKind::Json(json!({"total_count": jobs.len(), "jobs": jobs})),
            1,
        ));
        assert!(forge.read_pr(pr, Priority::Normal).is_err());
    }
}

#[test]
fn required_actions_missing_workflow_identity_fails_closed() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.set_required_checks(&["ci-ok"]);
    gh.set_classic_required_checks(Some(&[]));
    gh.report_check(pr, "ci-ok", Some("failure"));
    gh.report_check_with_identity(pr, "ci-ok", "success", "b.yml", "pull_request");
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
    assert!(forge.read_pr(pr, Priority::Normal).is_err());
}

#[test]
fn nonrequired_custom_review_uses_newest_suite_without_workflow_mapping() {
    use schlange_core::model::Conclusion;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.report_check(pr, "KI-Review", Some("failure"));
    gh.report_check(pr, "KI-Review", Some("success"));
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
    let read = forge.read_pr(pr, Priority::Normal).unwrap();
    assert!(
        read.current_checks()
            .iter()
            .any(|c| c.name == "KI-Review" && c.conclusion == Some(Conclusion::Success))
    );
    assert!(
        !read
            .current_checks()
            .iter()
            .any(|c| c.name == "KI-Review" && c.conclusion == Some(Conclusion::Failure))
    );
}

#[test]
fn suite_pagination_rejects_same_host_link_that_drops_filter() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let path = format!(
        "/repos/{}/check-suites/5/check-runs",
        gh.config().full_name()
    );
    let next = format!("{}{path}?per_page=100&page=2", gh.url());
    gh.add_fault(FaultRule::new(
        "GET",
        "/check-suites/5/check-runs",
        FaultKind::JsonLink(json!({"total_count": 2, "check_runs": []}), next),
        1,
    ));
    assert!(api.check_suite_runs(5, Priority::Normal).is_err());
    assert!(
        !gh.requests_with_query()
            .iter()
            .any(|r| r.contains("check-suites/5/check-runs") && r.contains("page=2")),
        "the redirected query must not be sent"
    );
}

#[test]
fn branch_rules_reject_same_host_link_to_another_branch() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let next = format!(
        "{}/repos/{}/rules/branches/other?per_page=100&page=2",
        gh.url(),
        gh.config().full_name()
    );
    gh.add_fault(FaultRule::new(
        "GET",
        "/rules/branches/main",
        FaultKind::JsonLink(json!([]), next),
        1,
    ));
    assert!(api.branch_rules("main", Priority::Normal).is_err());
    assert!(
        !gh.requests_with_query()
            .iter()
            .any(|r| r.contains("rules/branches/other"))
    );
}

#[test]
fn identity_batch_rejects_label_change_between_reads() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use schlange_github_fake::Then;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
    gh.add_fault(
        FaultRule::new(
            "GET",
            &format!("/commits/{head}/status"),
            FaultKind::Status(500),
            1,
        )
        .then(Then::Label(pr, "halt".into())),
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
    let error = forge
        .read_identity_batch("main", &[pr], Priority::Normal, || true)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("identity batch changed between reads"),
        "{error}"
    );
}

#[test]
fn identity_batch_requires_stable_membership_base_and_live_fence() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use schlange_github_fake::Then;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let head = gh.pr(pr).unwrap().head_sha;
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
            .read_identity_batch("main", &[pr], Priority::Normal, || true)
            .is_ok()
    );
    assert!(
        forge
            .read_identity_batch("main", &[pr], Priority::Normal, || false)
            .is_err()
    );
    let second = gh.open_pr(
        "someone",
        "another",
        &[("b".into(), "b".into())],
        false,
        &[],
    );
    assert!(
        forge
            .read_identity_batch("main", &[pr], Priority::Normal, || true)
            .is_err()
    );
    gh.add_fault(
        FaultRule::new(
            "GET",
            &format!("/commits/{head}/status"),
            FaultKind::Status(500),
            1,
        )
        .then(Then::PushBase(vec![("base-move".into(), "new\n".into())])),
    );
    assert!(
        forge
            .read_identity_batch("main", &[pr, second], Priority::Normal, || true)
            .is_err()
    );
}

#[test]
fn identity_batch_rechecks_membership_after_last_pull_read() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use std::sync::atomic::{AtomicBool, Ordering};
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
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
    let fired = AtomicBool::new(false);
    let guard = || {
        let path = format!("/pulls/{pr}");
        let count = gh
            .requests_with_query()
            .iter()
            .filter(|r| {
                r.split_whitespace()
                    .nth(1)
                    .and_then(|p| p.split('?').next())
                    .is_some_and(|p| p.ends_with(&path))
            })
            .count();
        if count >= 2 && !fired.swap(true, Ordering::SeqCst) {
            gh.open_pr("someone", "new", &[("b".into(), "b".into())], false, &[]);
        }
        true
    };
    let result = forge.read_identity_batch("main", &[pr], Priority::Normal, guard);
    assert!(
        fired.load(Ordering::SeqCst),
        "{:?}; {result:?}",
        gh.requests_with_query()
    );
    assert!(
        result.is_err(),
        "new pull request must invalidate the batch"
    );
}

#[test]
fn combined_status_rejects_truncated_or_malformed_count() {
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let a = api(&gh, &c);
    gh.add_fault(FaultRule::new(
        "GET",
        "/commits/abc/status",
        FaultKind::Json(json!({"state": "success", "total_count": 1, "statuses": []})),
        1,
    ));
    assert!(a.combined_status("abc", Priority::Normal).is_err());
    gh.add_fault(FaultRule::new(
        "GET",
        "/commits/abc/status",
        FaultKind::Json(json!({"state": "success", "total_count": 0})),
        1,
    ));
    assert!(a.combined_status("abc", Priority::Normal).is_err());
}

#[test]
fn identity_batch_rechecks_final_pull_fields() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    use std::sync::atomic::{AtomicBool, Ordering};
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
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
    let fired = AtomicBool::new(false);
    let guard = || {
        let path = format!("/pulls/{pr}");
        let count = gh
            .requests_with_query()
            .iter()
            .filter(|r| {
                r.split_whitespace()
                    .nth(1)
                    .and_then(|p| p.split('?').next())
                    .is_some_and(|p| p.ends_with(&path))
            })
            .count();
        if count >= 2 && !fired.swap(true, Ordering::SeqCst) {
            gh.label(pr, "halt", true);
        }
        true
    };
    let result = forge.read_identity_batch("main", &[pr], Priority::Normal, guard);
    assert!(fired.load(Ordering::SeqCst));
    assert!(result.is_err(), "a late label must invalidate the batch");
}

#[test]
fn identity_batch_can_prove_an_empty_base_without_guessing() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
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
    let batch = forge
        .read_identity_batch("main", &[], Priority::Normal, || true)
        .unwrap();
    assert!(batch.pull_requests.is_empty());
    gh.open_pr("someone", "new", &[("a".into(), "a".into())], false, &[]);
    assert!(
        forge
            .read_identity_batch("main", &[], Priority::Normal, || true)
            .is_err()
    );
}

#[test]
fn identity_batch_requires_direct_terminal_evidence_for_a_closed_target() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    gh.close_pr(pr);
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
    let batch = forge
        .read_identity_batch("main", &[pr], Priority::Normal, || true)
        .unwrap();
    assert_eq!(batch.pull_requests.len(), 1);
    assert!(batch.pull_requests[0].closed);
    assert!(!batch.pull_requests[0].merged);
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{pr}"),
        FaultKind::Status(404),
        100,
    ));
    assert!(
        forge
            .read_identity_batch("main", &[pr], Priority::Normal, || true)
            .is_err()
    );
}

#[test]
fn identity_batch_rejects_duplicate_open_pull_records() {
    use schlange_github::client::Auth;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
    let c = client(&gh, roomy());
    let cfg = gh.config();
    let path = format!("/repos/{}/pulls/{pr}", cfg.full_name());
    let raw: serde_json::Value = c
        .get_json(
            &path,
            Auth::Installation(cfg.installation_id),
            Priority::Normal,
        )
        .unwrap();
    gh.add_fault(
        FaultRule::new(
            "GET",
            "/pulls",
            FaultKind::Json(json!([raw.clone(), raw])),
            1,
        )
        .query("state=open"),
    );
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
            .read_identity_batch("main", &[pr], Priority::Normal, || true)
            .is_err()
    );
}

#[test]
fn identity_batch_cannot_ignore_adapter_confirmed_merge() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
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
    forge.remember_merge_for_test(pr, "known-merge");
    assert!(
        forge
            .read_identity_batch("main", &[pr], Priority::Normal, || true)
            .is_err()
    );
}

#[test]
fn lease_check_rejects_expiry_after_first_clock_sample() {
    use schlange_github::lease::{Lease, LeaseConfig};
    use schlange_github::time::Clock;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    struct JumpClock {
        now: AtomicU64,
        jump_after_read: AtomicBool,
    }
    impl Clock for JumpClock {
        fn now(&self) -> u64 {
            let now = self.now.load(Ordering::SeqCst);
            if self.jump_after_read.swap(false, Ordering::SeqCst) {
                self.now.fetch_add(61, Ordering::SeqCst);
            }
            now
        }
    }
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let clock = Arc::new(JumpClock {
        now: AtomicU64::new(1_800_000_000),
        jump_after_read: AtomicBool::new(false),
    });
    let lease = Lease::new(
        api(&gh, &c),
        None,
        LeaseConfig {
            holder: "a".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(lease.ensure());
    clock.jump_after_read.store(true, Ordering::SeqCst);
    assert!(
        lease.check().is_err(),
        "the lease expires after the first clock sample"
    );
}

#[test]
fn initial_missing_lease_fence_is_not_proof_of_journal_continuity() {
    use schlange_github::lease::{Lease, LeaseConfig};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let dir = temp_dir("lease-lineage");
    let store = Store::open(&dir).unwrap();
    let clock = Arc::new(ManualClock::new(1_800_000_000));
    let lease = Lease::new(
        api(&gh, &c),
        Some(store),
        LeaseConfig {
            holder: "a".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "absence of both fence and local metadata is not lineage proof"
    );
    clock.advance(31);
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "renewal must not launder an unknown lineage into a trusted one"
    );
}

#[test]
fn clean_identity_command_scan_rejects_ambiguous_bot_checkbox_comments() {
    use schlange_core::State;
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
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
    let mut state = State::default();
    state
        .pull_requests
        .insert(pr, forge.read_pr(pr, Priority::Normal).unwrap());
    assert!(
        forge
            .identity_commands_clear(&state, &Default::default())
            .unwrap()
    );
    gh.add_fault(FaultRule::new("GET", &format!("/issues/{pr}/comments"), FaultKind::Json(json!([{ "id": 99, "user": {"login": "someone"}, "body": "text", "created_at": "", "updated_at": "" }])), 1));
    assert!(
        !forge
            .identity_commands_clear(&state, &Default::default())
            .unwrap(),
        "missing timestamps cannot prove a clean history"
    );
    gh.set_collaborator("someone", "write");
    let comment = gh.comment(pr, "schlange[bot]", "<!-- schlange:status -->\n- [ ] queue");
    gh.edit_comment(comment, "someone", "<!-- schlange:status -->\n- [x] queue")
        .unwrap();
    assert!(
        !forge
            .identity_commands_clear(&state, &Default::default())
            .unwrap(),
        "an edited bot comment has unknown sender"
    );
}

#[test]
fn clean_identity_command_scan_rejects_changed_comment_snapshot() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
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
    let mut state = schlange_core::State::default();
    state
        .pull_requests
        .insert(pr, forge.read_pr(pr, Priority::Normal).unwrap());
    gh.comment(pr, "schlange[bot]", "<!-- schlange:status -->\n- [x] queue");
    gh.add_fault(FaultRule::new(
        "GET",
        &format!("/issues/{pr}/comments"),
        FaultKind::Json(json!([])),
        1,
    ));
    assert!(
        !forge
            .identity_commands_clear(&state, &Default::default())
            .unwrap(),
        "a bot checkbox appearing between comment scans must not prove a clean history"
    );
}

#[test]
fn clean_identity_scan_refuses_even_acknowledged_human_commands() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::ManualClock;
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr("someone", "test", &[("a".into(), "a".into())], false, &[]);
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
    let mut state = schlange_core::State::default();
    state
        .pull_requests
        .insert(pr, forge.read_pr(pr, Priority::Normal).unwrap());
    let comment = gh.comment(pr, "someone", "@mergifyio queue");
    // Treat even previously seen commands as uncertain until recovery has been proven.
    let item = api(&gh, &c)
        .issue_comments(pr, Priority::Normal)
        .unwrap()
        .pop()
        .unwrap();
    state.seen_comments.insert((comment, item.updated_at));
    assert!(
        !forge
            .identity_commands_clear(&state, &Default::default())
            .unwrap()
    );
}

#[test]
fn broken_trusted_lineage_cannot_be_laundered_by_renewal() {
    use schlange_github::lease::{Lease, LeaseConfig};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let dir = temp_dir("broken-trusted-lineage");
    let store = Store::open(&dir).unwrap();
    let repo = gh.config().full_name();
    let clock = Arc::new(ManualClock::new(1_800_000_000));
    let lease = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "a".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(lease.ensure());
    // Test-only setup simulates a previously trusted lineage. Break continuity before renewal.
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    store
        .set_meta(&format!("fence:{repo}"), "not-the-remote-parent")
        .unwrap();
    clock.advance(31);
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "mismatched predecessor invalidates trust"
    );
    clock.advance(31);
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "renewal cannot restore invalidated trust"
    );
}

#[test]
fn lineage_write_failure_remains_untrusted_after_retry_and_restart() {
    use schlange_github::lease::{Lease, LeaseConfig};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let dir = temp_dir("lineage-write-failure");
    let store = Store::open(&dir).unwrap();
    let repo = gh.config().full_name();
    let clock = Arc::new(ManualClock::new(1_800_000_000));
    let lease = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "a".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(lease.ensure());
    let previous = store.meta(&format!("fence:{repo}")).unwrap().unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_fence_write BEFORE INSERT ON meta WHEN NEW.key LIKE 'fence:%' BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;").unwrap();
    clock.advance(31);
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "failed local transaction cannot establish continuity"
    );
    assert_eq!(
        store.meta(&format!("fence:{repo}")).unwrap().as_deref(),
        Some(previous.as_str())
    );
    assert_eq!(
        store
            .meta(&format!("fence-trust:{repo}"))
            .unwrap()
            .as_deref(),
        Some("verified"),
        "failed transaction rolled back all local writes"
    );
    db.execute_batch("DROP TRIGGER reject_fence_write").unwrap();
    clock.advance(31);
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "remote fence advanced while metadata write failed"
    );
    assert_eq!(
        store
            .meta(&format!("fence-trust:{repo}"))
            .unwrap()
            .as_deref(),
        Some("untrusted")
    );
    lease.release();
    clock.advance(61);
    let successor = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "b".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(successor.ensure());
    assert!(
        !successor.journal_is_current(),
        "restart cannot restore broken lineage"
    );
}

#[test]
fn lineage_write_failure_then_crash_cannot_restore_trust() {
    use schlange_github::lease::{Lease, LeaseConfig};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let dir = temp_dir("lineage-write-failure-crash");
    let store = Store::open(&dir).unwrap();
    let repo = gh.config().full_name();
    let clock = Arc::new(ManualClock::new(1_800_000_000));
    let lease = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "a".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(lease.ensure());
    let previous = store.meta(&format!("fence:{repo}")).unwrap().unwrap();
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    let db = rusqlite::Connection::open(dir.join("schlange.db")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_fence_write BEFORE INSERT ON meta WHEN NEW.key LIKE 'fence:%' BEGIN SELECT RAISE(ABORT, 'injected write failure'); END;").unwrap();
    clock.advance(31);
    assert!(lease.ensure());
    assert!(
        !lease.journal_is_current(),
        "failed local transaction cannot establish continuity"
    );
    assert_eq!(
        store.meta(&format!("fence:{repo}")).unwrap().as_deref(),
        Some(previous.as_str())
    );
    assert_eq!(
        store
            .meta(&format!("fence-trust:{repo}"))
            .unwrap()
            .as_deref(),
        Some("verified"),
        "failed transaction rolled back all local writes"
    );
    db.execute_batch("DROP TRIGGER reject_fence_write").unwrap();
    // The old process disappears before retrying its failed metadata write.
    drop(lease);
    clock.advance(61);
    let successor = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "b".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(successor.ensure());
    assert!(
        !successor.journal_is_current(),
        "restart cannot restore broken lineage"
    );
    assert_eq!(
        store
            .meta(&format!("fence-trust:{repo}"))
            .unwrap()
            .as_deref(),
        Some("untrusted")
    );
}

#[test]
fn seeded_cooperating_lineage_continues_across_different_holders() {
    use schlange_github::lease::{Lease, LeaseConfig};
    use schlange_github::store::Store;
    use schlange_github::time::ManualClock;
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let dir = temp_dir("cooperating-lineage");
    let store = Store::open(&dir).unwrap();
    let repo = gh.config().full_name();
    let clock = Arc::new(ManualClock::new(1_800_000_000));
    let first = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "a".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(first.ensure());
    assert!(
        !first.journal_is_current(),
        "initial absence is not a trusted baseline"
    );
    // Test-only seed represents an already established trust root, not an adoption method.
    store
        .set_meta(&format!("fence-trust:{repo}"), "verified")
        .unwrap();
    clock.advance(31);
    assert!(first.ensure());
    assert!(
        first.journal_is_current(),
        "matching predecessor on seeded lineage remains current"
    );
    drop(first);
    clock.advance(61);
    let successor = Lease::new(
        api(&gh, &c),
        Some(store.clone()),
        LeaseConfig {
            holder: "b".into(),
            ttl: 60,
        },
        clock.clone(),
    );
    assert!(successor.ensure());
    assert!(
        successor.journal_is_current(),
        "successor building on matching fence retains cooperating lineage"
    );
}

#[test]
fn pull_creation_timestamp_comes_from_independent_fake_server_clock() {
    use schlange_github::time::{Clock, ManualClock};
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let worker_clock = ManualClock::new(1_800_000_000);
    let first = gh.open_pr("alice", "old", &[("a".into(), "a".into())], false, &[]);
    let old = api
        .pull(first, Priority::Normal)
        .unwrap()
        .created_at
        .unwrap();
    worker_clock.advance(1000);
    assert_eq!(worker_clock.now(), 1_800_001_000);
    assert_eq!(
        api.pull(first, Priority::Normal)
            .unwrap()
            .created_at
            .as_deref(),
        Some(old.as_str())
    );
    gh.advance_server_time(10);
    let second = gh.open_pr("alice", "new", &[("b".into(), "b".into())], false, &[]);
    let newer = api
        .pull(second, Priority::Normal)
        .unwrap()
        .created_at
        .unwrap();
    assert_eq!(old, "2027-01-15T08:00:00Z");
    assert_eq!(newer, "2027-01-15T08:00:10Z");
    let listed = api.open_pulls(Priority::Normal).unwrap();
    assert_eq!(
        listed
            .iter()
            .find(|p| p.number == first)
            .unwrap()
            .created_at
            .as_deref(),
        Some(old.as_str())
    );
    assert_eq!(
        listed
            .iter()
            .find(|p| p.number == second)
            .unwrap()
            .created_at
            .as_deref(),
        Some(newer.as_str())
    );
}

#[test]
fn provisional_boundary_does_not_make_late_discovered_old_pull_new() {
    use schlange_github::store::{AdoptionAge, AdoptionBoundary, Store};
    use schlange_github_fake::tester::temp_dir;
    let gh = FakeGithub::start(FakeConfig::default());
    let c = client(&gh, roomy());
    let api = api(&gh, &c);
    let old = gh.open_pr(
        "alice",
        "before boundary",
        &[("a".into(), "a".into())],
        false,
        &[],
    );
    // No PR list was read before the boundary: the old PR is intentionally discovered late.
    gh.advance_server_time(10);
    let dir = temp_dir("adoption-late-discovery");
    let store = Store::open(&dir).unwrap();
    let repo = gh.config().full_name();
    assert!(store.lease_acquire(&repo, "a", 100, 130).unwrap());
    let boundary = AdoptionBoundary {
        epoch: "session-a".into(),
        fence: "test-fence".into(),
        cutoff: 1_800_000_010,
    };
    store
        .create_provisional_adoption(&repo, "a", || 101, &boundary)
        .unwrap();
    let equal = gh.open_pr(
        "alice",
        "equal boundary",
        &[("b".into(), "b".into())],
        false,
        &[],
    );
    gh.advance_server_time(1);
    let new = gh.open_pr(
        "alice",
        "after boundary",
        &[("c".into(), "c".into())],
        false,
        &[],
    );
    drop(store);
    let stored = Store::open(&dir)
        .unwrap()
        .provisional_adoption(&repo)
        .unwrap()
        .unwrap();
    let pulls = api.open_pulls(Priority::Normal).unwrap();
    let age = |n| {
        stored.classify_created_at(
            pulls
                .iter()
                .find(|p| p.number == n)
                .unwrap()
                .created_at
                .as_deref(),
        )
    };
    assert_eq!(age(old), AdoptionAge::BeforeOrEqual);
    assert_eq!(age(equal), AdoptionAge::BeforeOrEqual);
    assert_eq!(age(new), AdoptionAge::TimestampAfter);
}

#[test]
fn replay_head_requires_authoritative_activity_and_never_uses_current_fallback() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::{ManualClock, format_rfc3339};
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "replay provenance",
        &[("a".into(), "a".into())],
        false,
        &[],
    );
    let old_head = gh.pr(pr).unwrap().head_sha;
    let comment_id = gh.comment(pr, "alice", "@mergifyio queue");
    let comment_time = gh
        .comments_on(pr)
        .into_iter()
        .find(|c| c.id == comment_id)
        .unwrap()
        .created_at;
    gh.push_pr(pr, &[("a".into(), "new".into())]);
    let new_head = gh.pr(pr).unwrap().head_sha;
    assert_ne!(old_head, new_head);
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
    let proved = forge
        .replay_activity_head_candidate(&state, pr, &format_rfc3339(comment_time))
        .unwrap();
    assert_eq!(
        proved, old_head,
        "late replay must retain the head seen at comment creation"
    );
    assert!(
        forge
            .replay_activity_head_candidate(&state, pr, "malformed")
            .is_err(),
        "invalid time cannot use current head"
    );
    let tie = format_rfc3339(comment_time);
    let before_time = format_rfc3339(comment_time - 1);
    gh.add_fault(FaultRule::new("GET", "/activity", FaultKind::Json(json!([
        {"ref": "refs/heads/pr-1", "before": old_head, "after": new_head, "timestamp": tie, "activity_type": "push"},
        {"ref": "refs/heads/pr-1", "before": "0".repeat(40), "after": old_head, "timestamp": before_time, "activity_type": "branch_creation"}
    ])), 1));
    assert!(
        forge
            .replay_activity_head_candidate(&state, pr, &format_rfc3339(comment_time))
            .is_err(),
        "same-second move is ambiguous even if an older head exists"
    );
    gh.add_fault(FaultRule::new(
        "GET",
        "/activity",
        FaultKind::Json(json!([])),
        1,
    ));
    assert!(
        forge
            .replay_activity_head_candidate(&state, pr, &format_rfc3339(comment_time))
            .is_err(),
        "empty activity cannot use current head"
    );
    gh.add_fault(FaultRule::new(
        "GET",
        "/activity",
        FaultKind::Status(404),
        100,
    ));
    assert!(
        forge
            .replay_activity_head_candidate(&state, pr, &format_rfc3339(comment_time))
            .is_err(),
        "unavailable activity cannot use current head"
    );
    gh.clear_faults();
    state.pull_requests.get_mut(&pr).unwrap().from_fork = true;
    assert!(
        forge
            .replay_activity_head_candidate(&state, pr, &format_rfc3339(comment_time))
            .is_err(),
        "fork activity without proof must remain held"
    );
}

#[test]
fn replay_activity_candidate_reads_all_pages_and_refuses_later_page_failure() {
    use schlange_github::forge::{ForgeOptions, GithubForge};
    use schlange_github::time::{ManualClock, format_rfc3339};
    let gh = FakeGithub::start(FakeConfig::default());
    let pr = gh.open_pr(
        "alice",
        "many pushes",
        &[("a".into(), "0".into())],
        false,
        &[],
    );
    for i in 0..101 {
        gh.push_pr(pr, &[("a".into(), format!("{i}"))]);
    }
    let old_head = gh.pr(pr).unwrap().head_sha;
    let id = gh.comment(pr, "alice", "@mergifyio queue");
    let at = gh
        .comments_on(pr)
        .into_iter()
        .find(|c| c.id == id)
        .unwrap()
        .created_at;
    gh.push_pr(pr, &[("a".into(), "last".into())]);
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
    assert_eq!(
        forge
            .replay_activity_head_candidate(&state, pr, &format_rfc3339(at))
            .unwrap(),
        old_head
    );
    assert!(
        gh.requests_with_query()
            .iter()
            .any(|r| r.contains("/activity?") && r.contains("page=2")),
        "second page must be read"
    );
    gh.add_fault(FaultRule::new("GET", "/activity", FaultKind::Status(403), 1).query("page=2"));
    assert!(
        forge
            .replay_activity_head_candidate(&state, pr, &format_rfc3339(at))
            .is_err(),
        "failed later page cannot be used as complete evidence"
    );
}
