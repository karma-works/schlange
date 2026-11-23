//! S-SHADOW: `schlange serve --shadow` next to another merge queue on the same repository.
//!
//! - The e2e flows (one pull request through a draft, a batch of three, a red batch, a halt
//!   label, commands, a push to a queued pull request, a paused queue) run in shadow mode
//!   against the fake GitHub while a scripted other queue ("Mergify", `mergify.rs` in the fake)
//!   does the real work: GitHub sees **no write request** from Schlange, and its write gate saw
//!   no attempt (it panics on one in tests).
//! - The shadow report agrees where both queues did the same (queued, landed, landing order,
//!   batch composition, dequeue code) and names every difference.

use schlange_github_fake::mergify::{MERGIFY_APP, MERGIFY_LOGIN, MERGIFY_QUEUE_CHECK};
use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const BOT: &str = "arztagent-bot";

const CHECK_FAILURE_QUEUE: &str = r##"
queue_rules:
  - name: default
    batch_size: 1
    queue_conditions:
      - base = main
      - "#check-failure = 0"
    merge_conditions:
      - "#check-failure = 0"
      - check-success = ci-ok
pull_request_rules:
  - name: queue test pull requests
    conditions:
      - author = arztagent-bot
    actions:
      queue:
"##;

fn arztagent_ci(t: Tester) -> Tester {
    t.ci(CiRule::success(
        "Risk class and data protection",
        CiOn::Both,
    ))
    .ci(CiRule::success("ci-ok", CiOn::PullRequest))
    .ci(CiRule::fails_on("ci-ok", CiOn::Queue, "BREAKS_FULL_SUITE"))
    .ci(CiRule::success("KI-Review", CiOn::PullRequest))
}

fn green_pr(t: &Running, path: &str, content: &str) -> u64 {
    t.open_pr(BOT).file(path, content).title(path).create()
}

/// Every request Schlange sent that is not a read (installation tokens are not writes to the
/// repository and are allowed).
fn writes(t: &Running) -> Vec<String> {
    t.gh.requests()
        .into_iter()
        .filter(|r| !r.starts_with("GET "))
        .filter(|r| !r.contains("/access_tokens "))
        .collect()
}

fn assert_no_writes(t: &Running) {
    t.settle();
    let w = writes(t);
    assert!(
        w.is_empty(),
        "shadow mode wrote to GitHub:\n{}",
        w.join("\n")
    );
    let s = t.service.as_ref().expect("serve");
    let attempts = s.shadow_gate.as_ref().expect("shadow gate").attempts();
    assert!(attempts.is_empty(), "write attempts: {attempts:?}");
    assert!(s.dead_workers().is_empty(), "{:?}", s.dead_workers());
    assert_eq!(t.status().unwrap().role, "shadow");
}

fn reconcile(t: &Running) {
    t.service.as_ref().unwrap().reconcile_now();
    t.settle();
}

fn report(t: &Running) -> serde_json::Value {
    let (code, body) = t.admin("GET", "shadow/report", None);
    assert_eq!(code, 200, "{body}");
    serde_json::from_str(&body).unwrap()
}

fn verdict(r: &serde_json::Value, n: u64) -> String {
    r["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["number"] == n)
        .map(|p| p["verdict"].as_str().unwrap().to_string())
        .unwrap_or_else(|| "absent".into())
}

#[test]
fn the_e2e_flows_in_shadow_send_no_write_request() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).shadow().start();
    // One pull request, then a batch of three, the other queue doing the work.
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.mergify_queue(a);
    t.advance("5m");
    t.settle();
    // No draft, no branch, no label, no comment, no check run of Schlange's on GitHub.
    assert!(t.gh.open_queue_drafts().is_empty());
    assert!(t.pr(a).queue_check().is_none());
    assert!(t.pr(a).comments().is_empty());
    let d = t.gh.mergify_embark(&[a]);
    t.settle();
    t.gh.mergify_land(d);
    t.advance("3m");
    reconcile(&t);
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    let c = green_pr(&t, "backend/c.py", "c = 1\n");
    let e = t.open_pr("karma-works").file("docs/e.md", "e\n").create();
    for n in [b, c, e] {
        t.pr(n).wait_queued();
    }
    t.advance("5m");
    t.settle();
    let d = t.gh.mergify_embark(&[b, c, e]);
    t.settle();
    t.gh.mergify_land(d);
    t.advance("3m");
    reconcile(&t);
    // A red batch: the other queue drops the culprit.
    let bad = green_pr(&t, "backend/x.py", "x = 1  # BREAKS_FULL_SUITE\n");
    t.pr(bad).wait_queued();
    t.advance("5m");
    t.settle();
    let d = t.gh.mergify_embark(&[bad]);
    t.settle();
    t.gh.mergify_dequeue(bad, "CHECKS_FAILED");
    t.advance("3m");
    reconcile(&t);
    // A halt label, commands, a push to a queued pull request, a pause.
    let h = green_pr(&t, "backend/h.py", "h = 1\n");
    t.pr(h).wait_queued();
    t.pr(h).label("halt");
    let cons = t
        .open_pr(BOT)
        .file(".github/workflows/x.yml", "on: push\n")
        .create();
    t.pr(cons).comment("karma-works", "@mergifyio queue");
    t.pr(cons).comment("arztagent-bot", "@mergifyio dequeue");
    let p = green_pr(&t, "backend/p.py", "p = 1\n");
    t.pr(p).wait_queued();
    t.pr(p).push("backend/p.py", "p = 2\n");
    let (code, _) = t.admin("PUT", "merge-queue/pause", Some(r#"{"reason":"release"}"#));
    assert_eq!(code, 200);
    let (code, _) = t.admin("DELETE", "merge-queue/pause", None);
    assert_eq!(code, 200);
    t.advance("10m");
    reconcile(&t);
    let _ = d;
    assert_no_writes(&t);
    // The engine decided all along: it would have landed a, b, c, e.
    let r = report(&t);
    eprintln!("{r:#}");
    for n in [a, b, c, e] {
        assert_eq!(verdict(&r, n), "agree", "#{n}: {r:#}");
    }
    let s = t.status().unwrap();
    assert!(s.shadow_suppressed > 0, "decisions were answered in shadow");
    t.shutdown();
}

#[test]
fn watchdog_does_not_observe_shadow_queues() {
    use schlange_github::service::PauseHook;
    use std::sync::atomic::Ordering;
    let hook = PauseHook::new();
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .pause_before_land(hook)
        .start();
    let pr = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(pr).wait_queued();
    let cycles = t.serve_cfg.watchdog_test_cycles.as_ref().unwrap();
    let samples = t.serve_cfg.watchdog_test_samples.as_ref().unwrap();
    t.wait_until("initial watchdog cycle", |_| {
        cycles.load(Ordering::SeqCst) >= 1
    });
    let before = cycles.load(Ordering::SeqCst);
    t.serve_cfg
        .heartbeat_test_offset
        .as_ref()
        .unwrap()
        .store(61, Ordering::SeqCst);
    t.wait_until("shadow watchdog cycle", |_| {
        cycles.load(Ordering::SeqCst) > before
    });
    assert_eq!(
        samples.load(Ordering::SeqCst),
        0,
        "shadow state must never be observed"
    );
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn the_report_agrees_with_the_other_queue_and_names_each_difference() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).shadow().start();
    // a, b: both queues queue them, test them together and land them in this order.
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    for n in [a, b] {
        t.pr(n).wait_queued();
        t.gh.mergify_queue(n);
    }
    t.advance("5m");
    t.settle();
    let d = t.gh.mergify_embark(&[a, b]);
    t.settle();
    // Schlange borrowed the draft's green checks and decided to land a, b.
    t.wait_until("Schlange would land a and b", |t| {
        t.decisions()
            .iter()
            .any(|d| d.contains("\"type\":\"land\""))
    });
    let merges = t.gh.mergify_land(d);
    assert_eq!(merges.len(), 2);
    assert!(t.gh.pr(a).unwrap().merged);
    // c: the other queue drops it as CHECKS_TIMEOUT; Schlange's own reading of the same red
    // draft is CHECKS_FAILED.
    let c = green_pr(&t, "backend/c.py", "c = 1  # BREAKS_FULL_SUITE\n");
    t.pr(c).wait_queued();
    t.gh.mergify_queue(c);
    t.advance("5m");
    t.settle();
    let _dc = t.gh.mergify_embark(&[c]);
    t.settle();
    t.gh.mergify_dequeue(c, "CHECKS_TIMEOUT");
    // f: only the other queue takes it (a human queued it there; Schlange's rules keep the
    // constitution pull request out, and nobody commanded Schlange).
    let f = t
        .open_pr(BOT)
        .file(".github/workflows/y.yml", "on: push\n")
        .create();
    t.gh.mergify_queue(f);
    let df = t.gh.mergify_embark(&[f]);
    t.settle();
    t.gh.mergify_land(df);
    t.advance("3m");
    reconcile(&t);
    t.advance("3m");
    reconcile(&t);
    assert_no_writes(&t);
    let r = report(&t);
    eprintln!("{r:#}");
    assert_eq!(verdict(&r, a), "agree", "{r:#}");
    assert_eq!(verdict(&r, b), "agree", "{r:#}");
    let pa = r["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["number"] == b)
        .unwrap()
        .clone();
    assert_eq!(pa["order"]["schlange"], 2);
    assert_eq!(pa["order"]["mergify"], 2);
    assert_eq!(pa["batch"]["schlange"], serde_json::json!([a, b]));
    assert_eq!(verdict(&r, c), "disagree", "{r:#}");
    assert_eq!(verdict(&r, f), "disagree", "{r:#}");
    let c_report = r["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|pr| pr["number"] == c)
        .unwrap();
    assert_eq!(
        c_report["differences"],
        serde_json::json!([
            "dequeue history: Schlange [\"CHECKS_FAILED\"], Mergify [\"CHECKS_TIMEOUT\"]"
        ])
    );
    let text = serde_json::to_string(&r).unwrap();
    assert!(text.contains("queued: Schlange no, Mergify yes"), "{r:#}");
    assert_eq!(r["agreements"], 2);
    assert_eq!(r["disagreements"], 2);
    // Mergify merged as itself; Schlange's engine counted a and b as its own landings.
    assert_eq!(t.gh.prs().iter().filter(|p| p.merged).count(), 3);
    let _ = MERGIFY_LOGIN;
    t.shutdown();
}

#[test]
fn the_report_survives_a_restart_from_the_database() {
    let mut t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .persistent()
        .start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.mergify_queue(a);
    t.advance("5m");
    t.settle();
    let d = t.gh.mergify_embark(&[a]);
    t.settle();
    t.wait_until("Schlange would land a", |t| {
        t.decisions()
            .iter()
            .any(|d| d.contains("\"type\":\"land\""))
    });
    t.gh.mergify_land(d);
    t.advance("3m");
    reconcile(&t);
    let before = report(&t);
    assert_eq!(verdict(&before, a), "agree", "{before:#}");
    t.stop_service();
    t.start_service();
    let after = report(&t);
    assert_eq!(after["pull_requests"], before["pull_requests"]);
    assert_no_writes(&t);
    t.shutdown();
}

/// The shadow app's permissions: everything Schlange reads, nothing it could write with.
const READ_ONLY: [(&str, &str); 7] = [
    ("metadata", "read"),
    ("contents", "read"),
    ("pull_requests", "read"),
    ("issues", "read"),
    ("checks", "read"),
    ("actions", "read"),
    ("statuses", "read"),
];

#[test]
fn a_read_only_app_is_enough_for_shadow_mode() {
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .fake(|c| {
            c.permissions = READ_ONLY
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect();
        })
        .shadow()
        .start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.advance("5m");
    t.settle();
    let d = t.gh.mergify_embark(&[a]);
    t.settle();
    t.gh.mergify_land(d);
    t.advance("3m");
    reconcile(&t);
    assert_no_writes(&t);
    // Tokens ask for read access to everything Schlange reads, so no read is refused.
    let tokens = t.gh.token_requests();
    let last = tokens.last().expect("a token was requested");
    for (name, level) in READ_ONLY {
        assert_eq!(last["permissions"][name], level, "token scope: {last}");
    }
    let refused: Vec<_> =
        t.gh.requests()
            .into_iter()
            .filter(|r| r.ends_with(" 403"))
            .collect();
    assert!(
        refused.is_empty(),
        "refused requests:\n{}",
        refused.join("\n")
    );
    assert_eq!(verdict(&report(&t), a), "agree");
    t.shutdown();
}

/// A pull request Mergify merged while shadow mode was not watching (before its start, or
/// while it was stopped) is not a disagreement: Schlange never saw it open and could not
/// decide. It is `inconclusive`; one Schlange saw open stays comparable.
#[test]
fn pull_requests_merged_before_shadow_mode_saw_them_are_inconclusive() {
    let mut t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .persistent()
        .start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.stop_service();
    // While nobody watches: a new pull request goes through Mergify and lands.
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.gh.mergify_queue(b);
    let d = t.gh.mergify_embark(&[b]);
    t.gh.mergify_land(d);
    t.start_service();
    t.advance("3m");
    reconcile(&t);
    let r = report(&t);
    assert_eq!(verdict(&r, b), "inconclusive", "{r:#}");
    assert_eq!(r["disagreements"], 0, "{r:#}");
    // `a` was seen open and is still compared.
    assert_ne!(verdict(&r, a), "absent", "{r:#}");
    assert_ne!(verdict(&r, a), "inconclusive", "{r:#}");
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn generic_mergify_checks_on_unqueued_prs_are_not_queue_evidence() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).shadow().start();
    let n = t
        .open_pr(BOT)
        .file(".github/workflows/x.yml", "on: push\n")
        .create();
    for (name, conclusion) in [
        ("Summary", "success"),
        ("Rule: ready (queue)", "neutral"),
        ("Rule: checks (queue)", "failure"),
    ] {
        t.gh.mergify_check(n, name, Some(conclusion));
    }
    reconcile(&t);
    let r = report(&t);
    assert_eq!(verdict(&r, n), "agree", "{r:#}");
    let row = r["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(row["queue_entries"]["schlange"], 0);
    assert_eq!(row["queue_entries"]["mergify"], 0);
    assert_eq!(row["dequeue_reasons"]["schlange"], serde_json::json!([]));
    assert_eq!(row["dequeue_reasons"]["mergify"], serde_json::json!([]));
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn shadow_queue_conditions_ignore_the_competing_queues_cancelled_check() {
    let mut t = Tester::new(CHECK_FAILURE_QUEUE)
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .shadow()
        .persistent()
        .start();
    let own = green_pr(&t, "backend/own.py", "own = 1\n");
    let unknown = green_pr(&t, "backend/unknown.py", "unknown = 1\n");
    let competing = green_pr(&t, "backend/competing.py", "competing = 1\n");
    for n in [own, unknown, competing] {
        t.pr(n).wait_queued();
    }

    // A cancellation from an ordinary CI app is part of the counterfactual input and must
    // still make the exact fixture-style queue condition false.
    t.gh.report_check_as(
        own,
        "cancelled ordinary CI",
        Some("cancelled"),
        "github-actions",
    );
    t.wait_until("ordinary cancellation dequeues", |t| {
        t.dequeue_code(own).as_deref() == Some("PR_DEQUEUED")
    });
    let own_decision = t
        .decisions()
        .into_iter()
        .find(|d| d.contains(&format!(r#""type":"dequeue","number":{own}"#)))
        .expect("ordinary cancellation has a dequeue decision");
    assert!(
        own_decision.contains("queue_conditions no longer hold (condition 2 is false)"),
        "{own_decision}"
    );

    // A legacy commit status has no verified app slug and must not be projected out either.
    t.gh.report_status(unknown, "unknown publisher", "failure");
    t.wait_until("unknown publisher failure dequeues", |t| {
        t.dequeue_code(unknown).as_deref() == Some("PR_DEQUEUED")
    });

    // The configured competing queue would not exist in Schlange's counterfactual. Its own
    // cancelled status must remain observer evidence, not an input that dequeues the PR.
    t.gh.mergify_check(competing, "cancelled competing queue", Some("cancelled"));
    reconcile(&t);
    let status = t.status().expect("fresh shadow worker state");
    assert!(
        status.state["queued"].get(competing.to_string()).is_some(),
        "configured Mergify cancellation contaminated the core: decisions={:?}",
        t.decisions()
    );
    assert_eq!(t.dequeue_code(competing), None);

    // Projection changes only the counterfactual input. The raw observer still records the
    // configured app's structured queue membership and recognized dequeue output durably.
    t.gh.mergify_queue(competing);
    let _draft = t.gh.mergify_embark(&[competing]);
    t.settle();
    t.gh.mergify_dequeue(competing, "CHECKS_FAILED");
    reconcile(&t);
    let observed = report(&t);
    let row = observed["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == competing)
        .unwrap();
    assert_eq!(
        row["dequeue_reasons"]["mergify"],
        serde_json::json!(["CHECKS_FAILED"]),
        "{observed:#}"
    );
    t.stop_service();
    t.start_service();
    reconcile(&t);
    let restarted = report(&t);
    let row = restarted["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == competing)
        .unwrap();
    assert_eq!(
        row["dequeue_reasons"]["mergify"],
        serde_json::json!(["CHECKS_FAILED"]),
        "{restarted:#}"
    );
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn a_virtual_draft_does_not_borrow_the_reference_queues_own_checks() {
    let mut t = Tester::new(CHECK_FAILURE_QUEUE)
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .shadow()
        .persistent()
        .start();
    let n = green_pr(&t, "backend/virtual.py", "virtual = 1\n");
    t.pr(n).wait_queued();

    // Let the matching draft and all its checks first appear while the observer is stopped.
    // Restart reconciliation must project the configured app's cancellation both from the
    // full snapshot and from a source fetched separately for the virtual draft.
    t.stop_service();
    let draft = t.gh.mergify_embark(&[n]);
    t.gh.mergify_check(draft, "cancelled reference bookkeeping", Some("cancelled"));
    t.start_service();
    reconcile(&t);
    t.wait_until(
        "Schlange would land from the projected virtual draft",
        |t| {
            t.decisions().iter().any(|decision| {
                decision.contains(r#""type":"land""#)
                    && decision.contains(&format!(r#""number":{n}"#))
            })
        },
    );
    assert_eq!(t.dequeue_code(n), None);
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn a_completed_run_is_reexamined_when_the_same_id_gains_a_dequeue_code() {
    use schlange_github::store::Store;

    let mut t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .persistent()
        .start();
    let n = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(n).wait_queued();
    t.gh.mergify_queue(n);
    t.advance("5m");
    t.settle();
    let _draft = t.gh.mergify_embark(&[n]);
    t.settle();

    let head = t.gh.pr(n).unwrap().head_sha;
    let run = t
        .gh
        .runs()
        .into_iter()
        .find(|r| r.head_sha == head && r.app_slug == MERGIFY_APP && r.name == MERGIFY_QUEUE_CHECK)
        .unwrap();
    t.gh.complete_check(run.id, "failure");
    reconcile(&t);
    let first = report(&t);
    let row = first["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(row["dequeue_reasons"]["mergify"], serde_json::json!([]));

    t.stop_service();
    t.start_service();
    t.gh.update_check_output(run.id, "Observed PR_MANUALLY_DEQUEUED");
    reconcile(&t);
    let recognized = report(&t);
    let row = recognized["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(
        row["dequeue_reasons"]["mergify"],
        serde_json::json!(["PR_MANUALLY_DEQUEUED"])
    );
    let facts = Store::open(t.serve_cfg.data_dir.as_ref().unwrap())
        .unwrap()
        .shadow_facts(&t.gh.config().full_name())
        .unwrap();
    let observed = facts
        .iter()
        .find(|fact| fact.pr == n && fact.side == "mergify" && fact.kind == "dequeued")
        .unwrap();
    assert_eq!(observed.data["run_id"], run.id);
    assert!(observed.data.get("source_at").is_none());

    // The first recognized durable fact for a GitHub run ID stays authoritative.
    t.stop_service();
    t.start_service();
    t.gh.update_check_output(run.id, "Observed CHECKS_TIMEOUT");
    reconcile(&t);
    let retained = report(&t);
    let row = retained["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(
        row["dequeue_reasons"]["mergify"],
        serde_json::json!(["PR_MANUALLY_DEQUEUED"])
    );
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn a_failed_code_free_marker_never_reuses_the_old_completion_time_after_recovery() {
    use schlange_github::store::Store;

    let mut t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .persistent()
        .start();
    let n = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(n).wait_queued();
    t.gh.mergify_queue(n);
    t.advance("5m");
    t.settle();
    let _draft = t.gh.mergify_embark(&[n]);
    t.settle();

    let head = t.gh.pr(n).unwrap().head_sha;
    let run = t
        .gh
        .runs()
        .into_iter()
        .find(|r| r.head_sha == head && r.app_slug == MERGIFY_APP && r.name == MERGIFY_QUEUE_CHECK)
        .unwrap();
    let db = rusqlite::Connection::open(t.serve_cfg.data_dir.as_ref().unwrap().join("schlange.db"))
        .unwrap();
    db.execute_batch(
        "CREATE TRIGGER reject_pending BEFORE INSERT ON shadow_events \
         BEGIN SELECT RAISE(FAIL, 'pending storage failure'); END; \
         CREATE TRIGGER reject_incomplete BEFORE INSERT ON shadow \
         BEGIN SELECT RAISE(FAIL, 'marker storage failure'); END;",
    )
    .unwrap();

    t.gh.complete_check(run.id, "failure");
    reconcile(&t);
    let failed = report(&t);
    let row = failed["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(row["history_incomplete"], true, "{failed:#}");

    db.execute_batch("DROP TRIGGER reject_pending; DROP TRIGGER reject_incomplete;")
        .unwrap();
    t.gh.update_check_output(run.id, "Observed PR_MANUALLY_DEQUEUED");
    reconcile(&t);
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let observed = store
        .shadow_facts(&t.gh.config().full_name())
        .unwrap()
        .into_iter()
        .find(|fact| fact.pr == n && fact.side == "mergify" && fact.kind == "dequeued")
        .unwrap();
    assert!(observed.data.get("source_at").is_none());

    t.stop_service();
    t.start_service();
    let restarted = report(&t);
    let row = restarted["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(row["history_incomplete"], true, "{restarted:#}");
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn structured_embarkment_ignores_issue_mentions_and_borrows_checks() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).shadow().start();
    let n = t
        .open_pr(BOT)
        .file("backend/a.py", "a = 1\n")
        .title("Fixes #12 (#500)")
        .create();
    t.pr(n).wait_queued();
    t.advance("5m");
    t.settle();
    let draft = t.gh.mergify_embark(&[n]);
    // The member's title has unrelated references, as does prose outside the list.
    t.gh.edit_body(
        draft,
        &format!("Embarked pull requests:\n#{n}: Fixes #12 (#500)\n\nPull requests ahead:\n#42\n"),
    );
    reconcile(&t);
    t.wait_until("Schlange borrowed the exact draft's checks", |t| {
        t.decisions()
            .iter()
            .any(|d| d.contains("\"type\":\"land\""))
    });
    t.gh.mergify_land(draft);
    reconcile(&t);
    let r = report(&t);
    assert_eq!(verdict(&r, 12), "absent");
    assert_eq!(verdict(&r, 500), "absent");
    assert_eq!(verdict(&r, 42), "absent");
    assert_eq!(
        r["draft_observer"]["matching_queue_prefix"],
        serde_json::json!([draft])
    );
    assert_eq!(
        r["draft_observer"]["matching_configured_author"],
        serde_json::json!([draft])
    );
    assert_eq!(
        r["draft_observer"]["structured_membership_parsed"],
        serde_json::json!([draft])
    );
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn a_short_lived_queue_draft_is_captured_from_its_durable_webhook() {
    use schlange_github::store::Store;

    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .persistent()
        .start();
    let n = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(n).wait_queued();
    t.advance("5m");
    t.settle();
    let draft = t.gh.mergify_embark(&[n]);
    // Close before allowing the service to settle/read the draft. The signed opened delivery
    // is already durable in the inbox and must retain the structured membership.
    t.gh.close_pr(draft);
    t.settle();

    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    let queued = store
        .shadow_facts(&t.gh.config().full_name())
        .unwrap()
        .into_iter()
        .find(|f| f.side == "mergify" && f.pr == n && f.kind == "queued")
        .expect("signed queue-draft delivery was observed");
    assert_eq!(queued.data["draft"], draft);
    assert_eq!(queued.data["source"], "webhook");
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn shadow_storage_failure_leaves_webhook_pending_until_restart_recovers_it() {
    use schlange_github::store::Store;

    let mut t = arztagent_ci(Tester::new(ARZTAGENT))
        .shadow()
        .persistent()
        .start();
    let n = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(n).wait_queued();
    t.advance("5m");
    t.settle();
    let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
    assert!(store.inbox_pending().unwrap().is_empty());
    rusqlite::Connection::open(t.serve_cfg.data_dir.as_ref().unwrap().join("schlange.db"))
        .unwrap()
        .execute_batch("DROP TABLE shadow_events; DROP TABLE shadow")
        .unwrap();

    t.gh.mergify_embark(&[n]);
    t.wait_until("shadow observer exposes its non-durable failure", |t| {
        report(t)["pull_requests"].as_array().is_some_and(|rows| {
            rows.iter()
                .any(|row| row["number"] == n && row["history_incomplete"] == true)
        })
    });
    assert!(
        store
            .inbox_pending()
            .unwrap()
            .iter()
            .any(|row| row.2 == "pull_request")
    );
    let failed = report(&t);
    let row = failed["pull_requests"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["number"] == n)
        .unwrap();
    assert_eq!(row["verdict"], "inconclusive", "{failed:#}");
    assert_eq!(row["history_incomplete"], true, "{failed:#}");

    t.stop_service();
    t.start_service(); // Store::open recreates both tables before inbox replay.
    t.wait_until("pending shadow webhook is durably replayed", |_| {
        store.inbox_pending().unwrap().is_empty()
    });
    let facts = Store::open(t.serve_cfg.data_dir.as_ref().unwrap())
        .unwrap()
        .shadow_facts(&t.gh.config().full_name())
        .unwrap();
    assert!(facts.iter().any(|f| {
        f.pr == n && f.side == "mergify" && (f.kind == "queued" || f.kind == "history_incomplete")
    }));
    assert_no_writes(&t);
    t.shutdown();
}

#[test]
fn a_closed_queued_pr_keeps_its_dequeue_reason_and_completed_runs_are_deduplicated() {
    use schlange_github::store::Store;
    // The engine currently calls a closed PR PR_DEQUEUED. Preserve any actual
    // PR_CLOSED observation as a disagreement until sandbox evidence settles parity.
    for (code, expected) in [("PR_DEQUEUED", "agree"), ("PR_CLOSED", "disagree")] {
        let t = arztagent_ci(Tester::new(ARZTAGENT))
            .shadow()
            .persistent()
            .start();
        let n = green_pr(&t, "backend/a.py", "a = 1\n");
        t.pr(n).wait_queued();
        t.gh.mergify_embark(&[n]);
        reconcile(&t);
        t.gh.close_pr(n);
        t.gh.mergify_dequeue(n, code);
        reconcile(&t);
        let r = report(&t);
        assert_eq!(verdict(&r, n), expected, "{r:#}");
        let store = Store::open(t.serve_cfg.data_dir.as_ref().unwrap()).unwrap();
        let reason = || {
            store
                .shadow_facts(&t.gh.config().full_name())
                .unwrap()
                .into_iter()
                .find(|f| f.pr == n && f.side == "mergify" && f.kind == "dequeued")
                .unwrap()
        };
        let before = reason();
        assert_eq!(before.data["code"], code);
        assert!(before.data["run_id"].is_u64());
        assert!(before.data.get("text").is_none());
        t.advance("3m");
        reconcile(&t);
        let after = reason();
        assert_eq!(
            before.seq, after.seq,
            "the same check-run ID was observed twice"
        );
        assert_eq!(before.at, after.at);
        assert_no_writes(&t);
        t.shutdown();
    }
}
