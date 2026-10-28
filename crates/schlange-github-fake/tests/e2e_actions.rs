//! End-to-end, phase 2: the actions of `pull_request_rules` and the commands that run them,
//! with `schlange serve` against the fake GitHub over HTTP: labels, comments (once, also after
//! a restart), assignees, review requests, reviews and their dismissal, drafts, checks, closing,
//! branch deletion, updates, squashes, backports and copies, workflow dispatches and `extends`.

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, Tester};

const CONFIG: &str = r#"queue_rules:
  - name: default
    merge_method: squash
    queue_conditions:
      - check-success = quick
      - label = queue
    merge_conditions:
      - check-success = quick
pull_request_rules:
  - name: queue
    conditions:
      - label = queue
    actions:
      queue:
  - name: triage
    conditions:
      - files ~= ^api/
    actions:
      label:
        add: [api]
      comment:
        message: "Thanks @{{ author }} for #{{ number }}."
      assign:
        add_users: ["{{ author }}", lead]
      request_reviews:
        users: [r1, r2]
        teams: [core]
  - name: docs
    conditions:
      - files ~= ^docs/
    actions:
      review:
        type: APPROVE
        message: docs only
      dismiss_reviews:
        message: new commits
  - name: red
    conditions:
      - check-failure = quick
    actions:
      edit:
        draft: true
  - name: has ticket
    conditions:
      - label = ticket
    actions:
      post_check: {}
  - name: no vendor
    conditions:
      - files ~= ^vendor/
    actions:
      close:
        message: "no vendor changes, @{{ author }}"
  - name: tidy
    conditions:
      - closed
    actions:
      delete_head_branch:
  - name: up to date
    conditions:
      - label = keep-updated
    actions:
      update:
  - name: preview
    conditions:
      - label = preview
    actions:
      github_actions:
        workflow:
          dispatch:
            - workflow: preview.yml
              inputs:
                pr: "{{ number }}"
  - name: backport
    conditions:
      - base = main
      - label = backport
    actions:
      backport:
        branches: [stable]
        labels: [backport]
        report_mode: [check, comment]
"#;

fn ci(t: Tester) -> Tester {
    t.ci(CiRule::fails_on("quick", CiOn::Both, "BROKEN"))
}

fn count(t: &Running, n: u64, text: &str) -> usize {
    t.pr(n)
        .comments()
        .iter()
        .filter(|c| c.contains(text))
        .count()
}

#[test]
fn people_labels_comments_reviews_and_checks() {
    let t = ci(Tester::new(CONFIG)).start();
    let a = t.open_pr("alice").file("api/x.rs", "x\n").create();
    t.wait_until("labeled, assigned, reviews requested", |t| {
        t.gh.pr(a).is_some_and(|p| {
            p.labels.contains("api")
                && p.assignees == vec!["alice".to_string(), "lead".to_string()]
                && p.requested_reviewers == vec!["r1".to_string(), "r2".to_string()]
                && p.requested_teams == vec!["core".to_string()]
        })
    });
    t.pr(a).wait_comment("Thanks @alice for #");
    // Repeated reads and a reconciliation change nothing (MQ-12).
    t.pr(a).label("unrelated");
    t.advance("10m");
    t.settle();
    assert_eq!(count(&t, a, "Thanks @alice"), 1);
    // Someone removes the label: the level-triggered action puts it back.
    t.pr(a).unlabel("api");
    t.wait_until("label back", |t| {
        t.gh.pr(a).is_some_and(|p| p.labels.contains("api"))
    });
    assert_eq!(count(&t, a, "Thanks @alice"), 1);

    // Docs: the bot approves once; a push dismisses the approval and it approves again.
    let d = t.open_pr("bob").file("docs/a.md", "a\n").create();
    let approved = |t: &Running, k: usize| {
        t.gh.pr(d).is_some_and(|p| {
            p.reviews
                .iter()
                .filter(|r| r.user == "schlange[bot]" && r.state == "APPROVED")
                .count()
                == k
        })
    };
    t.wait_until("approved once", |t| approved(t, 1));
    t.settle();
    assert!(approved(&t, 1));
    t.pr(d).push("docs/a.md", "a2\n");
    t.wait_until("dismissed and approved again", |t| {
        t.gh.pr(d).is_some_and(|p| {
            p.reviews.iter().any(|r| r.state == "DISMISSED")
                && p.reviews.last().is_some_and(|r| r.state == "APPROVED")
        })
    });

    // A failing check: the pull request becomes a draft (GraphQL).
    let r = t.open_pr("carol").file("lib.rs", "BROKEN\n").create();
    t.wait_until("draft", |t| t.gh.pr(r).is_some_and(|p| p.draft));

    // post_check: a check run of the app, success with the label.
    let k = t
        .open_pr("dana")
        .file("k.rs", "k\n")
        .label("ticket")
        .create();
    t.wait_until("post_check", |t| {
        t.gh.runs().iter().any(|run| {
            run.name == "Rule: has ticket (post_check)"
                && run.conclusion.as_deref() == Some("success")
                && t.gh.pr(k).is_some_and(|p| p.head_sha == run.head_sha)
        })
    });
    t.assert_invariants();
}

#[test]
fn close_delete_update_dispatch_and_backport() {
    let t = ci(Tester::new(CONFIG)).start();
    t.gh.create_branch("stable");
    t.permission("maintainer", "write");
    // close + delete_head_branch.
    let v = t.open_pr("eve").file("vendor/lib.c", "patch\n").create();
    t.pr(v).wait_comment("no vendor changes, @eve");
    t.wait_until("closed and branch deleted", |t| {
        let p = t.gh.pr(v).expect("pr");
        !p.open && !t.gh.branches().contains(&p.head_ref)
    });
    // update: the base moves, the rule merges it in.
    let u = t
        .open_pr("frank")
        .file("u.rs", "u\n")
        .label("keep-updated")
        .create();
    t.settle();
    t.gh.push_base(&[("base.txt".into(), "moved\n".into())]);
    t.wait_until("updated", |t| {
        let p = t.gh.pr(u).expect("pr");
        t.gh.tree_of(&p.head_sha).contains_key("base.txt")
    });
    // github_actions: one dispatch.
    t.open_pr("gina")
        .file("g.rs", "g\n")
        .label("preview")
        .create();
    t.wait_until("dispatched", |t| t.gh.dispatches().len() == 1);
    // backport: after the queue merged, a pull request on stable, reported in a comment.
    let b = t
        .open_pr("hank")
        .file("fix.rs", "fix\n")
        .label("backport")
        .label("queue")
        .create();
    t.pr(b).wait_queued();
    t.advance("1m");
    t.queue_pr().check("quick").succeed();
    t.pr(b).wait_merged();
    t.wait_until("backport pull request", |t| {
        t.gh.prs().iter().any(|p| {
            p.head_ref == format!("mergify/bp/stable/pr-{b}")
                && p.base == "stable"
                && p.labels.contains("backport")
        })
    });
    t.pr(b).wait_comment("- `stable`: #");
    let bp =
        t.gh.prs()
            .into_iter()
            .find(|p| p.head_ref.starts_with("mergify/bp/stable/"))
            .expect("backport");
    assert_eq!(
        t.gh.tree_of(&bp.head_sha).get("fix.rs").map(String::as_str),
        Some("fix\n")
    );
    // Commands: copy to stable, squash.
    let c = t.open_pr("ivy").file("c.rs", "c\n").create();
    t.pr(c).push("c.rs", "c2\n");
    t.settle();
    t.pr(c).comment("maintainer", "@mergifyio copy stable");
    t.pr(c).wait_comment("`copy`: - `stable`: #");
    t.pr(c).comment("maintainer", "@mergifyio squash");
    t.pr(c).wait_comment("`squash`: done");
    t.settle();
    assert_eq!(t.gh.dispatches().len(), 1, "dispatched once");
    t.assert_invariants();
}

#[test]
fn a_restart_does_not_post_a_comment_twice() {
    let mut t = ci(Tester::new(CONFIG)).persistent().start();
    let a = t.open_pr("alice").file("api/x.rs", "x\n").create();
    t.pr(a).wait_comment("Thanks @alice");
    t.stop_service();
    t.start_service();
    t.advance("10m");
    t.settle();
    assert_eq!(count(&t, a, "Thanks @alice"), 1);
}

#[test]
fn extends_reads_the_other_repository() {
    let local = "extends: org\nqueue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = quick\npull_request_rules:\n  - name: local\n    conditions: []\n    actions:\n      label:\n        add: [local]\n";
    let t = ci(Tester::new(local))
        .fake(|c| {
            c.other_repos.insert(
                "org".into(),
                [(
                    ".mergify.yml".to_string(),
                    "pull_request_rules:\n  - name: org\n    conditions: []\n    actions:\n      comment:\n        message: from the org\n".to_string(),
                )]
                .into(),
            );
        })
        .start();
    let p = t.open_pr("alice").file("a.rs", "a\n").create();
    t.pr(p).wait_comment("from the org");
    t.wait_until("local label", |t| {
        t.gh.pr(p).is_some_and(|x| x.labels.contains("local"))
    });
}

/// The `merge` action lands outside the queue: only on a fresh read where its guard holds, at
/// the head of that read; a constitution change never passes the guard.
#[test]
fn merge_action_lands_only_what_its_guard_allows() {
    let cfg = r#"queue_rules:
  - name: default
    queue_conditions:
      - check-success = quick
pull_request_rules:
  - name: automerge
    conditions:
      - label = automerge
      - -label = halt
      - check-success = quick
      - -files ~= ^\.github/
    actions:
      merge:
        method: squash
"#;
    let t = ci(Tester::new(cfg)).start();
    let ok = t
        .open_pr("alice")
        .file("a.rs", "a\n")
        .label("automerge")
        .create();
    t.pr(ok).wait_merged();
    let evil = t
        .open_pr("agent")
        .file(".github/workflows/x.yml", "evil\n")
        .label("automerge")
        .create();
    // A clean pull request that gets a constitution file pushed, then the label.
    let late = t.open_pr("agent").file("b.rs", "b\n").create();
    t.pr(late).push(".github/workflows/y.yml", "evil\n");
    t.pr(late).label("automerge");
    t.advance("5m");
    t.settle();
    assert!(!t.gh.pr(evil).unwrap().merged, "constitution change merged");
    assert!(!t.gh.pr(late).unwrap().merged, "constitution change merged");
    assert!(
        t.decisions().iter().any(|d| d.contains("\"refresh\"")),
        "decided on a fresh read"
    );
}
