//! `schlange serve` as a service: persistence across crashes (journal, snapshot, audit),
//! the configuration validator, webhook delivery ids, token scopes.

use std::time::Duration;

use schlange_github::service::CrashPoint;
use schlange_github::webhook::sign;
use schlange_github_fake::{CiOn, CiRule, Tester};

/// One pull request per car, so a second one waits in the queue while the first lands.
const ONE_BY_ONE: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - base = main\n      - check-success = ci\n    merge_conditions:\n      - check-success = ci\n\
                          pull_request_rules:\n  - name: all\n    conditions:\n      - base = main\n    actions:\n      queue:\n";

fn post(url: &str, event: &str, id: &str, body: &str, secret: &[u8]) -> (u16, String) {
    let agent = ureq::Agent::new_with_config(
        ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(Duration::from_secs(5)))
            .build(),
    );
    let mut r = agent
        .post(url)
        .header("x-github-event", event)
        .header("x-github-delivery", id)
        .header("x-hub-signature-256", &sign(secret, body.as_bytes()))
        .send(body.as_bytes())
        .unwrap();
    (
        r.status().as_u16(),
        r.body_mut().read_to_string().unwrap_or_default(),
    )
}

/// MQ-31 on GitHub: `serve` dies right after GitHub merged a pull request, before the reply
/// reached the engine. The new process restarts from the journal (the queue as it was,
/// including the pull request waiting behind), resolves the landing from GitHub's state
/// without merging again, and goes on.
#[test]
fn a_crash_mid_landing_recovers_the_queue_and_never_lands_twice() {
    // CI runs on pull requests by itself; the drafts' checks are reported by hand.
    let mut t = Tester::new(ONE_BY_ONE)
        .ci(CiRule::success("ci", CiOn::PullRequest))
        .crash_at(CrashPoint::AfterLand)
        .start();
    let a = t.open_pr("someone").file("a.txt", "a\n").create();
    t.pr(a).wait_queued();
    let b = t.open_pr("someone").file("b.txt", "b\n").create();
    t.pr(b).wait_queued();
    // #a's car is the first draft (the car of #b, behind it, tests #a and #b together).
    t.wait_until("two cars", |t| t.gh.open_queue_drafts().len() == 2);
    let first = t.gh.open_queue_drafts()[0].number;
    t.pr(first).check("ci").succeed();
    t.wait_dead();
    assert!(
        t.gh.pr(a).unwrap().merged,
        "GitHub merged #a before the crash"
    );
    let before = t.status().unwrap();
    let since_b = before.state["queued"][b.to_string()]["since"].clone();
    assert!(since_b.is_u64(), "{since_b}");
    t.stop_service();
    // Time passes while the service is down: a fresh enqueue would get a new `since`.
    t.advance("90s");
    t.start_service();
    let s = t.status().unwrap();
    let recovered = s.recovered.clone().expect("restarted from the journal");
    assert!(
        recovered.iter().any(|d| d.contains("\"land\"")),
        "the landing was in doubt: {recovered:?}"
    );
    // Whatever cars the queue has now (kept from the journal or rebuilt): CI succeeds.
    for _ in 0..200 {
        if t.gh.pr(b).unwrap().merged {
            break;
        }
        for d in t.gh.open_queue_drafts() {
            if !t
                .gh
                .runs()
                .iter()
                .any(|r| r.head_sha == d.head_sha && r.name == "ci")
            {
                t.pr(d.number).check("ci").succeed();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    t.pr(b).wait_merged();
    t.wait_until("both recorded as merged", |t| {
        t.status()
            .is_some_and(|s| s.state["outcomes"][b.to_string()]["outcome"] == "merged")
    });
    let s = t.status().unwrap();
    assert_eq!(s.state["outcomes"][a.to_string()]["outcome"], "merged");
    assert_eq!(s.state["outcomes"][b.to_string()]["outcome"], "merged");
    assert_eq!(t.gh.squashes().len(), 2, "each pull request landed once");
    // The queue entry of #b came from the journal, not from a new enqueue.
    let log = t.decisions();
    assert!(
        !log.iter()
            .any(|d| d.contains("\"enqueue\"") && d.contains(&format!("\"number\":{b},"))),
        "#b was not enqueued again after the restart: {log:?}"
    );
    let _ = since_b;
    // The audit has one row per landing (the one before the crash too).
    let store = t.service.as_ref().unwrap().store.clone().unwrap();
    let audit = store.audits(&t.gh.config().full_name()).unwrap();
    assert_eq!(
        audit.iter().map(|r| r.number).collect::<Vec<_>>(),
        vec![a, b],
        "{audit:?}"
    );
    for r in &audit {
        assert_eq!(r.tested_tree.as_deref(), Some(r.landed_tree.as_str()));
        assert_eq!(r.base_before, r.landed_parent);
    }
    // `checks_timeout: auto` history survives too.
    assert!(
        !store
            .history(&t.gh.config().full_name())
            .unwrap()
            .is_empty()
    );
    t.assert_invariants();
    t.shutdown();
}

/// The configuration is validated like `config validate`; an invalid one is not applied and
/// the last valid one stays in force. Rules without `conditions` never match everything.
#[test]
fn serve_validates_the_configuration_and_keeps_the_last_valid_one() {
    let no_conditions = "queue_rules:\n  - name: default\n    queue_conditions:\n      - base = main\n\
                         pull_request_rules:\n  - name: everything\n    actions:\n      queue:\n";
    let t = Tester::new(no_conditions)
        .ci(CiRule::success("ci", CiOn::Both))
        .reconcile_every("60s")
        .start();
    let s = t.status().unwrap();
    let err = s.config_error.clone().expect("rejected");
    assert!(err.contains("conditions"), "{err}");
    let a = t.open_pr("someone").file("a.txt", "a\n").create();
    t.settle();
    assert!(t.status().unwrap().queued.is_empty(), "nothing matches");
    // A valid configuration on the default branch: picked up at the next reconciliation.
    t.gh.push_base(&[(".mergify.yml".into(), ONE_BY_ONE.into())]);
    t.advance("61s");
    t.pr(a).wait_merged();
    assert!(t.status().unwrap().config_error.is_none());
    // Broken again (a condition that does not parse): the valid one stays in force.
    t.gh.push_base(&[(
        ".mergify.yml".into(),
        ONE_BY_ONE.replace("check-success = ci\n    merge", "nosuch = 1\n    merge"),
    )]);
    t.advance("61s");
    t.wait_until("rejected", |t| {
        t.status().is_some_and(|s| s.config_error.is_some())
    });
    let b = t.open_pr("someone").file("b.txt", "b\n").create();
    t.pr(b).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

/// A delivery id is recorded only once the delivery was handled: a delivery answered 400 is
/// handled when GitHub redelivers it. With a data directory the ids survive a restart.
#[test]
fn delivery_ids_are_recorded_only_after_success_and_survive_restarts() {
    let mut t = Tester::new(ONE_BY_ONE)
        .ci(CiRule::success("ci", CiOn::Both))
        .persistent()
        .start();
    let secret = t.gh.config().webhook_secret;
    let url = t.service.as_ref().unwrap().webhook_url();
    let (s, _) = post(&url, "pull_request", "d-1", "{not json", &secret);
    assert_eq!(s, 400);
    let good = format!(
        r#"{{"action":"labeled","number":1,"label":{{"name":"x"}},"pull_request":{{"number":1,"state":"open","title":"t","draft":false,"merged":false,"user":{{"login":"someone"}},"head":{{"ref":"pr-1","sha":"0000000000000000000000000000000000000001"}},"base":{{"ref":"main","sha":"0000000000000000000000000000000000000002"}},"labels":[]}},"repository":{{"full_name":"{}"}}}}"#,
        t.gh.config().full_name()
    );
    let (s, text) = post(&url, "pull_request", "d-1", &good, &secret);
    assert_eq!(s, 202, "the redelivery is handled: {text}");
    let (s, text) = post(&url, "pull_request", "d-1", &good, &secret);
    assert_eq!((s, text.as_str()), (200, "duplicate delivery"));
    t.stop_service();
    t.start_service();
    let url = t.service.as_ref().unwrap().webhook_url();
    let (s, text) = post(&url, "pull_request", "d-1", &good, &secret);
    assert_eq!((s, text.as_str()), (200, "duplicate delivery"));
    // Bad signature: 401, not recorded.
    let (s, _) = post(&url, "pull_request", "d-2", &good, b"wrong-secret-wrong");
    assert_eq!(s, 401);
    let (s, _) = post(&url, "pull_request", "d-2", &good, &secret);
    assert_eq!(s, 202);
    t.shutdown();
}

/// Installation tokens are downscoped to the served repository and Schlange's permissions.
#[test]
fn installation_tokens_are_downscoped() {
    let t = Tester::new(ONE_BY_ONE)
        .fake(|c| {
            c.permissions.insert("administration".into(), "read".into());
        })
        .start();
    let reqs = t.gh.token_requests();
    let last = reqs.last().expect("a token was issued");
    assert_eq!(last["repositories"], serde_json::json!(["arzt-agent"]));
    let perms = last["permissions"].as_object().unwrap();
    assert!(!perms.contains_key("administration"), "{last}");
    assert_eq!(perms["contents"], "write");
    t.shutdown();
}

/// Commands are deduplicated by comment id and version in the engine state, which the journal
/// persists: a command comment delivered again after a restart (a redelivery with a new
/// delivery id) is not run a second time.
#[test]
fn command_dedup_survives_a_restart() {
    const BY_COMMAND: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
                              pull_request_rules:\n  - name: bots\n    conditions:\n      - author = bot\n    actions:\n      queue:\n";
    let mut t = Tester::new(BY_COMMAND)
        .ci(CiRule::success("quick", CiOn::PullRequest))
        .persistent()
        .start();
    t.permission("alice", "write");
    let pr = t.open_pr("carol").file("c.txt", "c\n").create();
    let cmd = t.pr(pr).comment("alice", "@mergifyio queue");
    t.pr(pr).wait_queued();
    let acks = |t: &schlange_github_fake::tester::Running| {
        t.gh.reactions()
            .iter()
            .filter(|(c, u, _)| *c == cmd && u == "schlange[bot]")
            .count()
    };
    t.wait_until("acknowledged", |t| acks(t) == 1);
    t.stop_service();
    t.start_service();
    t.gh.resend_comment(cmd);
    t.settle();
    assert_eq!(acks(&t), 1, "the command ran again after the restart");
    let log = t.decisions();
    assert!(
        !log.iter().any(|d| d.contains("\"enqueue\"")),
        "queued again by the redelivered command: {log:?}"
    );
    t.shutdown();
}
