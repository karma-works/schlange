//! End-to-end: `schlange serve` against the fake GitHub, everything over HTTP (REST calls,
//! signed webhooks), with Arztagent's real `.mergify.yml` read from the repository.

use schlange_github::time::Clock;
use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, HookFaults, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const BOT: &str = "arztagent-bot";
const QUEUE_PREFIX: &str = "mergify/merge-queue/";

/// Arztagent's CI: quick checks on the pull request, the full suite (`ci-ok`) on the queue
/// draft, which fails when the tested tree contains `BREAKS_FULL_SUITE`.
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

#[test]
fn arztagent_one_pull_request_through_the_draft_to_a_squash_landing() {
    // No CI responder: every check is reported by hand, like bors' tests do.
    let t = Tester::new(ARZTAGENT).start();
    let pr = t
        .open_pr(BOT)
        .file("backend/a.py", "a = 1\n")
        .title("Add a")
        .create();
    // Not queued without its checks.
    t.settle();
    assert!(t.status().unwrap().queued.is_empty());
    for check in ["ci-ok", "Risk class and data protection", "KI-Review"] {
        t.pr(pr).check(check).succeed();
    }
    t.pr(pr).wait_queued();
    // batch_size 5, batch_max_wait_time 5 minutes: nothing is built before the wait ends.
    t.settle();
    assert!(t.gh.open_queue_drafts().is_empty());
    t.advance("5m");
    let draft = t.queue_pr();
    let d = draft.get();
    assert!(d.draft, "the queue pull request is a draft");
    assert!(d.head_ref.starts_with(QUEUE_PREFIX), "{}", d.head_ref);
    assert_eq!(d.head_ref.len(), QUEUE_PREFIX.len() + 10);
    assert_eq!(d.user, "schlange[bot]");
    assert_eq!(d.title, format!("merge queue: embarking #{pr}"));
    // Arztagent's PR gate applies its normal data-protection heading check to Schlange drafts.
    assert!(
        d.body.lines().any(|line| line == "## Datenschutz"),
        "{}",
        d.body
    );
    assert!(d.body.contains(&format!("- #{pr} at ")), "{}", d.body);
    assert_eq!(
        t.gh.tree_of(&d.head_sha)
            .get("backend/a.py")
            .map(String::as_str),
        Some("a = 1\n")
    );
    // merge_conditions (`check-success = ci-ok`) are judged on the draft: while the full
    // suite runs there, the pull request's own green ci-ok does not count.
    let full_suite = draft.check("ci-ok").start();
    // Model the heading part of pr-gate's non-Mergify path. The classifier still runs in
    // the real workflow; this assertion does not simulate or bypass it.
    let heading = d.body.lines().any(|line| {
        let hashes = line.bytes().take_while(|b| *b == b'#').count();
        (1..=6).contains(&hashes)
            && line[hashes..]
                .trim_start()
                .to_lowercase()
                .starts_with("datenschutz")
    });
    if heading {
        draft.check("Risk class and data protection").succeed();
    } else {
        draft.check("Risk class and data protection").fail();
    }
    t.settle();
    assert!(!t.gh.pr(pr).unwrap().merged);
    assert!(t.status().unwrap().queued.contains(&pr));
    t.gh.complete_check(full_suite, "success");
    t.pr(pr).wait_merged();
    let merged = t.gh.pr(pr).unwrap();
    assert!(!merged.open, "a merged pull request is closed");
    // One squash commit with one parent; its tree is the tree the draft tested.
    let history = t.gh.base_history();
    assert_eq!(history.len(), 1);
    assert_eq!(t.gh.parents_of(&history[0]).len(), 1);
    assert_eq!(t.gh.tree_id_of(&history[0]), t.gh.tree_id_of(&d.head_sha));
    assert_eq!(
        merged.merge_commit_sha.as_deref(),
        Some(history[0].as_str())
    );
    // The draft is closed and the queue branch gone.
    t.wait_until("draft closed", |t| t.gh.open_queue_drafts().is_empty());
    t.wait_until("queue branch deleted", |t| {
        !t.gh.branches().iter().any(|b| b.starts_with(QUEUE_PREFIX))
    });
    // The queue's own check run on the pull request ended green.
    t.wait_until("queue check completed", |t| {
        t.pr(pr)
            .queue_check()
            .is_some_and(|c| c.conclusion.as_deref() == Some("success"))
    });
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_batch_of_three_is_tested_once_and_lands_as_three_squash_commits() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    let c = t.open_pr("karma-works").file("docs/c.md", "c\n").create();
    for n in [a, b, c] {
        t.pr(n).wait_queued();
    }
    t.advance("5m");
    for n in [a, b, c] {
        t.pr(n).wait_merged();
    }
    // One draft for the batch of three (it is closed by now).
    let drafts: Vec<_> =
        t.gh.prs()
            .into_iter()
            .filter(|p| p.user == "schlange[bot]")
            .collect();
    assert_eq!(drafts.len(), 1, "{drafts:?}");
    assert_eq!(
        drafts[0].title,
        format!("merge queue: embarking #{a}, #{b}, #{c}")
    );
    let squashes = t.gh.squashes();
    assert_eq!(
        squashes.iter().map(|s| s.pr).collect::<Vec<_>>(),
        vec![a, b, c]
    );
    let tree = t.base_tree();
    assert_eq!(
        tree.get("backend/a.py").map(String::as_str),
        Some("a = 1\n")
    );
    assert_eq!(
        tree.get("backend/b.py").map(String::as_str),
        Some("b = 1\n")
    );
    assert_eq!(tree.get("docs/c.md").map(String::as_str), Some("c\n"));
    // The last landed tree is the draft's tested tree.
    assert_eq!(
        t.gh.tree_id_of(&t.gh.base_history()[0]),
        t.gh.tree_id_of(&drafts[0].head_sha)
    );
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_red_batch_is_bisected_and_the_culprit_leaves_the_queue() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let bad = green_pr(&t, "backend/b.py", "b = 1  # BREAKS_FULL_SUITE\n");
    let c = green_pr(&t, "backend/c.py", "c = 1\n");
    for n in [a, bad, c] {
        t.pr(n).wait_queued();
    }
    t.advance("5m");
    t.pr(a).wait_merged();
    t.pr(c).wait_merged();
    t.wait_until("the culprit left the queue", |t| {
        t.status().is_some_and(|s| s.queued.is_empty())
    });
    assert!(!t.gh.pr(bad).unwrap().merged);
    let q = t.pr(bad).queue_check();
    t.wait_until("culprit's queue check says why", |t| {
        t.pr(bad)
            .queue_check()
            .is_some_and(|c| c.output_title.contains("CHECKS_FAILED"))
    });
    let _ = q;
    assert!(!t.base_tree().contains_key("backend/b.py"));
    // More than one draft: the batch, then the split tests.
    let drafts =
        t.gh.prs()
            .into_iter()
            .filter(|p| p.user == "schlange[bot]")
            .count();
    assert!(drafts >= 2, "bisection built {drafts} drafts");
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn lost_webhooks_are_recovered_by_reconciliation() {
    // Every webhook is lost; only the periodic reconciliation (every minute) sees anything.
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .reconcile_every("60s")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.settle();
    assert!(t.status().unwrap().queued.is_empty(), "no webhook, no news");
    // Minute by minute: reconcile, queue, wait out the batch time, build, test, land.
    for _ in 0..15 {
        if t.gh.pr(a).unwrap().merged && t.gh.pr(b).unwrap().merged {
            break;
        }
        t.advance("60s");
        let advanced_to = t.clock.now();
        t.wait_until("reconciliation after clock advance", |t| {
            t.status()
                .is_some_and(|s| s.last_reconcile.is_some_and(|at| at >= advanced_to))
        });
        t.settle();
    }
    assert!(
        t.gh.pr(a).unwrap().merged && t.gh.pr(b).unwrap().merged,
        "lost webhooks must be recovered within 15 reconciliation intervals"
    );
    t.pr(a).wait_merged();
    t.pr(b).wait_merged();
    assert_eq!(t.status().unwrap().webhooks, 0, "no webhook arrived");
    let (dropped, _) = t.gh.hook_fault_counts();
    assert!(dropped > 10, "dropped {dropped}");
    // The draft closure is seen at the next pass too.
    t.advance("60s");
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn duplicated_and_reordered_webhooks_change_nothing() {
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .hook_faults(HookFaults {
            drop: 0.0,
            duplicate: 0.5,
            max_delay: std::time::Duration::from_millis(40),
            seed: 7,
        })
        .start();
    let prs: Vec<u64> = (0..3)
        .map(|i| green_pr(&t, &format!("backend/m{i}.py"), "x\n"))
        .collect();
    for n in &prs {
        t.pr(*n).wait_queued();
    }
    t.advance("5m");
    for n in &prs {
        t.pr(*n).wait_merged();
    }
    let (_, duplicated) = t.gh.hook_fault_counts();
    assert!(duplicated > 0);
    assert_eq!(t.gh.squashes().len(), 3);
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_5xx_after_the_merge_happened_does_not_land_twice() {
    // MQ-35: GitHub merges #1 but answers 502. Schlange reads, sees it merged, and goes on.
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.gh.add_fault(FaultRule::new(
        "PUT",
        &format!("/pulls/{a}/merge"),
        FaultKind::Lie(502),
        1,
    ));
    for n in [a, b] {
        t.pr(n).wait_queued();
    }
    t.advance("5m");
    t.pr(a).wait_merged();
    t.pr(b).wait_merged();
    let squashes = t.gh.squashes();
    assert_eq!(
        squashes.iter().map(|s| s.pr).collect::<Vec<_>>(),
        vec![a, b],
        "each pull request squashed exactly once"
    );
    assert!(
        t.gh.requests()
            .iter()
            .any(|r| r == &format!("PUT /repos/karma-works/arzt-agent/pulls/{a}/merge 502"))
    );
    let s = t.status().unwrap();
    assert!(s.halted.is_empty(), "no halt: {:?}", s.halted);
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_lying_5xx_on_draft_creation_and_queue_branch_merges_is_absorbed() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    t.gh.add_fault(FaultRule::new("POST", "/pulls", FaultKind::Lie(502), 1));
    t.gh.add_fault(FaultRule::new("POST", "/merges", FaultKind::Lie(504), 1));
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.advance("5m");
    t.pr(a).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn storms_of_404_and_5xx_never_dequeue() {
    // W-24: while #1 is queued, GitHub answers its reads and the base branch with 404 and
    // 5xx for a while. Nothing leaves the queue; it lands once GitHub answers again.
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{a}"),
        FaultKind::Status(404),
        40,
    ));
    t.gh.add_fault(FaultRule::new(
        "GET",
        "/git/ref/heads/main",
        FaultKind::Status(404),
        40,
    ));
    t.gh.add_fault(FaultRule::new(
        "GET",
        "/git/ref/heads/",
        FaultKind::Status(502),
        20,
    ));
    t.gh.add_fault(FaultRule::new(
        "GET",
        "/commits/",
        FaultKind::Status(503),
        20,
    ));
    // A webhook during the storm, and the batch timer.
    t.pr(a).label("unrelated");
    t.advance("5m");
    t.advance("1h");
    t.pr(a).wait_merged();
    let s = t.status().unwrap();
    assert!(s.halted.is_empty());
    assert!(
        t.gh.requests().iter().any(|r| r.ends_with(" 404")),
        "the storm happened"
    );
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn rate_limits_are_waited_out_and_landing_still_happens() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(FaultRule::new(
        "*",
        "/",
        FaultKind::RateLimit { retry_after: 1 },
        1,
    ));
    t.gh.add_fault(FaultRule::new(
        "PUT",
        "/merge",
        FaultKind::SecondaryLimit { retry_after: 1 },
        1,
    ));
    t.advance("5m");
    t.pr(a).wait_merged();
    let stats = t.client.stats();
    assert!(stats.rate_limited >= 2, "{stats:?}");
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_restart_recovers_from_github_and_sweeps_orphans() {
    // The service dies while a car runs (no journal yet): the new process reconciles, deletes
    // the orphaned queue branch and draft, rebuilds the car and lands.
    let mut t = Tester::new(ARZTAGENT)
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::PullRequest))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.advance("5m");
    let old_draft = t.queue_pr().number;
    t.stop_service();
    t.start_service();
    t.wait_until("old draft closed", |t| !t.gh.pr(old_draft).unwrap().open);
    // The rebuilt car waits for its batch time again, then gets a new draft.
    t.advance("5m");
    t.wait_until("a new draft", |t| {
        t.gh.open_queue_drafts()
            .iter()
            .any(|d| d.number != old_draft)
    });
    let draft = t.queue_pr();
    assert_ne!(draft.number, old_draft);
    draft.check("ci-ok").succeed();
    draft.check("Risk class and data protection").succeed();
    t.pr(a).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_push_to_a_queued_pull_request_dequeues_it_and_a_halt_label_too() {
    let t = arztagent_ci(Tester::new(ARZTAGENT)).start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.pr(a).push("backend/a.py", "a = 2\n");
    t.pr(b).label("halt");
    t.wait_until("both left the queue", |t| {
        t.status().is_some_and(|s| !s.queued.contains(&b))
    });
    // #1 is green again on its new head and queued again; #2 stays out while labelled.
    t.pr(a).wait_queued();
    t.advance("5m");
    t.pr(a).wait_merged();
    assert!(!t.gh.pr(b).unwrap().merged);
    assert_eq!(
        t.base_tree().get("backend/a.py").map(String::as_str),
        Some("a = 2\n")
    );
    t.assert_invariants();
    t.shutdown();
}
