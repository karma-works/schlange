//! P2 queue features end to end: `schlange serve` against the fake GitHub over HTTP. Merge
//! methods other than squash, required checks from rulesets, the queue API (status, pause,
//! scheduled freezes) behind the admin token.

use schlange_github_fake::{CiOn, CiRule, Tester};

const BOT: &str = "bot";

fn config(method: &str, batch: u32) -> String {
    format!(
        "merge_queue:\n  max_parallel_checks: 2\nqueue_rules:\n  - name: default\n    merge_method: {method}\n    batch_size: {batch}\n    batch_max_wait_time: 1 min\n    queue_conditions:\n      - base = main\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\npull_request_rules:\n  - name: q\n    conditions:\n      - author = bot\n    actions:\n      queue:\n"
    )
}

fn ci(t: Tester) -> Tester {
    t.ci(CiRule::success("quick", CiOn::PullRequest))
        .ci(CiRule::fails_on("full", CiOn::Queue, "BREAKS"))
}

/// Two pull requests (one with two commits) land with `method`; I0/I1 of the fake hold and the
/// base has the method's shape.
fn lands_with(method: &str, batch: u32) {
    let t = ci(Tester::new(&config(method, batch))).start();
    let a = t.open_pr(BOT).file("a.txt", "a\n").create();
    t.pr(a).push("a2.txt", "second commit\n");
    let b = t.open_pr(BOT).file("b.txt", "b\n").create();
    for n in [a, b] {
        t.pr(n).wait_queued();
    }
    t.advance("2m");
    for n in [a, b] {
        t.pr(n).wait_merged();
    }
    let tree = t.base_tree();
    for (path, content) in [
        ("a.txt", "a\n"),
        ("a2.txt", "second commit\n"),
        ("b.txt", "b\n"),
    ] {
        assert_eq!(
            tree.get(path).map(String::as_str),
            Some(content),
            "{method}"
        );
    }
    let history = t.gh.base_history();
    let parents: Vec<usize> = history.iter().map(|c| t.gh.parents_of(c).len()).collect();
    match method {
        "merge" => assert_eq!(parents, vec![2, 2], "{method}"),
        // #a has two commits, both replayed.
        "rebase" => assert_eq!(parents, vec![1, 1, 1], "{method}"),
        "fast-forward" => assert!(parents.iter().all(|p| *p == 2), "{method}: {parents:?}"),
        "merge-batch" => assert_eq!(parents, vec![2], "{method}: one merge commit per batch"),
        _ => unreachable!(),
    }
    if method != "rebase" {
        // The pull requests' own heads are on main (their SHAs are kept).
        let tip = t.gh.branch_sha("main").expect("main");
        for n in [a, b] {
            let head = t.gh.pr(n).unwrap().head_sha;
            assert!(t.gh.is_ancestor(&head, &tip), "{method}: #{n}");
        }
    }
    t.wait_until("drafts closed", |t| t.gh.open_queue_drafts().is_empty());
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn merge_method_merge_lands_merge_commits() {
    lands_with("merge", 1);
}

#[test]
fn merge_method_rebase_replays_the_commits() {
    lands_with("rebase", 1);
}

#[test]
fn merge_method_fast_forward_moves_main_onto_the_tested_branch() {
    lands_with("fast-forward", 2);
}

#[test]
fn merge_method_merge_batch_merges_the_batch_pull_request() {
    lands_with("merge-batch", 2);
}

#[test]
fn required_checks_of_a_ruleset_join_the_queue_conditions() {
    let t = ci(Tester::new(&config("squash", 1)))
        .ci(CiRule::fails_on("lint", CiOn::Both, "UNLINTED"))
        .start();
    t.gh.set_required_checks(&["lint", "Schlange merge queue"]);
    t.service.as_ref().expect("serve").reconcile_now();
    t.wait_until("required checks read", |t| {
        t.status()
            .is_some_and(|s| s.state["required_checks"]["main"] == serde_json::json!(["lint"]))
    });
    let bad = t.open_pr(BOT).file("a.txt", "UNLINTED\n").create();
    let good = t.open_pr(BOT).file("b.txt", "b\n").create();
    t.pr(good).wait_queued();
    t.advance("2m");
    t.pr(good).wait_merged();
    t.settle();
    let st = t.status().unwrap();
    assert!(!st.queued.contains(&bad), "lint failed: not queued");
    assert!(!t.gh.pr(bad).unwrap().merged);
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn the_queue_api_pauses_and_resumes_and_shows_the_status() {
    let t = ci(Tester::new(&config("squash", 1))).start();
    // Without the token: refused.
    let (code, _) = t.admin_as("GET", "merge-queue/status", None, Some("wrong"));
    assert_eq!(code, 401);
    let (code, body) = t.admin(
        "PUT",
        "merge-queue/pause",
        Some(r#"{"reason": "CI incident"}"#),
    );
    assert_eq!(code, 200, "{body}");
    let v: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(v["paused"], true);
    assert_eq!(v["reason"], "CI incident");
    let a = t.open_pr(BOT).file("a.txt", "a\n").create();
    t.pr(a).wait_queued();
    t.advance("2m");
    t.settle();
    // Paused: queued, but nothing is checked.
    assert!(t.gh.open_queue_drafts().is_empty());
    let (_, body) = t.admin("GET", "merge-queue/status", None);
    let st: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["pause"]["reason"], "CI incident", "{st}");
    assert_eq!(st["waiting_pull_requests"][0]["number"], a, "{st}");
    assert_eq!(st["mode"], "serial");
    let (code, body) = t.admin("GET", "merge-queue/pause", None);
    assert_eq!(
        (
            code,
            serde_json::from_str::<serde_json::Value>(&body).unwrap()["paused"].clone()
        ),
        (200, serde_json::json!(true))
    );
    // The pause is in the journaled engine state.
    assert!(t.status().unwrap().state["pause"].is_object());
    let (code, body) = t.admin("DELETE", "merge-queue/pause", None);
    assert_eq!(code, 200, "{body}");
    t.advance("10s");
    let draft = t.queue_pr();
    let (_, body) = t.admin("GET", "merge-queue/status", None);
    let st: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(st["batches"][0]["pull_requests"][0]["number"], a, "{st}");
    assert_eq!(
        st["batches"][0]["queue_pull_request_number"],
        draft.get().number,
        "{st}"
    );
    t.pr(a).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_scheduled_freeze_holds_matching_pull_requests_until_it_ends() {
    let t = ci(Tester::new(&config("squash", 1))).start();
    // The engine's clock starts at 1_800_000_000 (2027-01-15T08:00:00Z).
    let (code, body) = t.admin(
        "POST",
        "scheduled_freeze",
        Some(
            r#"{"reason": "release", "timezone": "UTC", "end": "2027-01-15T09:00:00Z",
                "matching_conditions": ["base = main"], "exclude_conditions": ["label = hotfix"]}"#,
        ),
    );
    assert_eq!(code, 200, "{body}");
    let freeze: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(freeze["active"], true, "{freeze}");
    let (code, _) = t.admin(
        "POST",
        "scheduled_freeze",
        Some(r#"{"reason": "x", "timezone": "Europe/Paris", "end": "2030-01-01T00:00:00"}"#),
    );
    assert_eq!(code, 422, "only UTC");
    // The hotfix comes first: in serial mode a pull request behind a frozen one waits too.
    let hot = t.open_pr(BOT).file("h.txt", "h\n").label("hotfix").create();
    t.pr(hot).wait_queued();
    let a = t.open_pr(BOT).file("a.txt", "a\n").create();
    t.pr(a).wait_queued();
    t.advance("2m");
    t.pr(hot).wait_merged();
    t.advance("5m");
    t.settle();
    assert!(!t.gh.pr(a).unwrap().merged, "frozen until 09:00");
    let (_, body) = t.admin("GET", "merge-queue/status", None);
    assert!(body.contains("frozen by `release`"), "{body}");
    t.advance("1h");
    t.pr(a).wait_merged();
    t.settle();
    let (code, body) = t.admin("GET", "scheduled_freeze", None);
    assert_eq!(code, 200);
    assert!(body.contains("\"active\":false"), "{body}");
    let (code, _) = t.admin("DELETE", "scheduled_freeze/1", None);
    assert_eq!(code, 200);
    t.assert_invariants();
    t.shutdown();
}

/// `allow_queue_branch_edit: true` with `fast-forward`: someone pushes to the queue branch, the
/// full suite runs on the pushed head, and the base moves onto that head (the edit lands). With
/// a red full suite on the pushed head, nothing lands. The fake's I0 checks that every commit
/// on the base is a tree the full suite passed.
fn edit_of_the_queue_branch(content: &str) -> (bool, Option<String>) {
    let cfg = config("fast-forward", 1).replace(
        "    merge_method: fast-forward\n",
        "    merge_method: fast-forward\n    allow_queue_branch_edit: true\n",
    );
    let t = ci(Tester::new(&cfg)).start();
    let a = t.open_pr(BOT).file("a.txt", "a\n").create();
    t.pr(a).wait_queued();
    t.advance("2m");
    // The fake's CI reports at once; the queue lands only after its fresh read, which
    // sees the pushed head.
    let draft = t.queue_pr().get();
    t.gh.push_pr_as(
        draft.number,
        "mallory",
        &[("edit.txt".to_string(), content.to_string())],
    );
    t.settle();
    t.advance("2m");
    t.settle();
    let merged = t.gh.pr(a).is_some_and(|p| p.merged);
    let edit = t.base_tree().get("edit.txt").cloned();
    t.assert_invariants();
    t.shutdown();
    (merged, edit)
}

#[test]
fn an_accepted_edit_of_the_queue_branch_lands_the_tested_pushed_head() {
    let (merged, edit) = edit_of_the_queue_branch("edited\n");
    assert!(merged);
    assert_eq!(edit.as_deref(), Some("edited\n"));
}

#[test]
fn an_accepted_edit_that_breaks_the_full_suite_never_lands() {
    let (merged, edit) = edit_of_the_queue_branch("BREAKS\n");
    assert!(!merged);
    assert_eq!(edit, None);
}
