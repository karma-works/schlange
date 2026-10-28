//! S-SHADOW: `schlange serve --shadow` next to another merge queue on the same repository.
//!
//! - The e2e flows (one pull request through a draft, a batch of three, a red batch, a halt
//!   label, commands, a push to a queued pull request, a paused queue) run in shadow mode
//!   against the fake GitHub while a scripted other queue ("Mergify", `mergify.rs` in the fake)
//!   does the real work: GitHub sees **no write request** from Schlange, and its write gate saw
//!   no attempt (it panics on one in tests).
//! - The shadow report agrees where both queues did the same (queued, landed, landing order,
//!   batch composition, dequeue code) and names every difference.

use schlange_github_fake::mergify::MERGIFY_LOGIN;
use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const BOT: &str = "arztagent-bot";

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
    let text = serde_json::to_string(&r).unwrap();
    assert!(
        text.contains("dequeue reason: Schlange CHECKS_FAILED, Mergify CHECKS_TIMEOUT"),
        "{r:#}"
    );
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
const READ_ONLY: [(&str, &str); 6] = [
    ("metadata", "read"),
    ("contents", "read"),
    ("pull_requests", "read"),
    ("issues", "read"),
    ("checks", "read"),
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
