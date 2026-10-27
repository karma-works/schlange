//! End-to-end, phase 2a: comment commands, command restrictions, checkboxes, labels and
//! status comments, built-in protections, commit messages and the configuration file, with
//! `schlange serve` against the fake GitHub over HTTP (signed webhooks, REST calls).

use schlange_core::commands::{CONTROLS_MARKER, STATUS_MARKER};
use schlange_github::client::BudgetConfig;
use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, HookFaults, Tester};

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

/// Two-step CI with a `quick` check on pull requests and `full` on queue drafts; `bot` pull
/// requests are queued by a rule, everyone else by command (alice and bob only).
const SIMPLE: &str = "queue_rules:\n  - name: default\n    merge_method: squash\n    queue_conditions:\n      - check-success = quick\n    merge_conditions:\n      - check-success = full\n\
pull_request_rules:\n  - name: bots\n    conditions:\n      - author = bot\n    actions:\n      queue:\n\
commands_restrictions:\n  queue:\n    conditions:\n      - or:\n          - sender = alice\n          - sender = bob\n";

fn simple_ci(t: Tester) -> Tester {
    t.ci(CiRule::success("quick", CiOn::PullRequest))
        .ci(CiRule::fails_on("full", CiOn::Queue, "BROKEN"))
}

fn queued(t: &Running, n: u64) -> bool {
    t.status().is_some_and(|s| s.queued.contains(&n))
}

fn holds(t: &Running, n: u64) -> String {
    t.status()
        .map(|s| s.state["holds"][n.to_string()].to_string())
        .unwrap_or_default()
}

#[test]
fn arztagent_constitution_pull_request_waits_for_a_listed_human() {
    // The pull request changes a workflow file: GitHub refuses such merges from an app without
    // `workflows: write` (the fake does too), so this installation grants it.
    let t = arztagent_ci(Tester::new(ARZTAGENT))
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
        .start();
    t.permission(BOT, "write");
    t.permission("karma-works", "admin");
    let pr = t
        .open_pr(BOT)
        .file(".github/workflows/ci.yml", "on: [pull_request]\n")
        .title("Tighten the CI workflow")
        .create();
    // Green, but constitution: no rule queues it; the checkbox comment says it is ready.
    t.pr(pr).wait_comment(CONTROLS_MARKER);
    t.settle();
    assert!(!queued(&t, pr));
    // The agent may not queue it, although it can write to the repository.
    let agent = t.pr(pr).comment(BOT, "@mergifyio queue");
    t.pr(pr)
        .wait_comment("@arztagent-bot, you may not run `queue`");
    t.settle();
    assert!(!queued(&t, pr));
    assert!(
        t.gh.reactions()
            .iter()
            .any(|(c, u, r)| *c == agent && u == "schlange[bot]" && r == "+1"),
        "acknowledged with a reaction"
    );
    // A listed human can.
    t.pr(pr).comment("karma-works", "@Mergifyio queue");
    t.pr(pr).wait_queued();
    t.advance("5m");
    let draft = t.queue_pr();
    draft.check("ci-ok").succeed();
    t.pr(pr).wait_merged();
    // The queued label came and went; the status comment reports the merge.
    t.pr(pr).wait_comment("merged");
    t.wait_until("queued label removed", |t| {
        t.gh.pr(pr).is_some_and(|p| !p.labels.contains("queued"))
    });
    // The app itself merged and opened the draft (no bot accounts: queue-bot-accounts).
    let sq = t.gh.squashes();
    assert_eq!(sq.len(), 1);
    assert_eq!(sq[0].merged_by, "schlange[bot]");
    assert_eq!(draft.get().user, "schlange[bot]");
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn queue_dequeue_refresh_and_unsupported_commands() {
    // No CI responder: the test reports the checks, so the queue waits where it should.
    let t = Tester::new(SIMPLE).start();
    for (u, p) in [("alice", "write"), ("bob", "write"), ("carol", "write")] {
        t.permission(u, p);
    }
    let a = t.open_pr("human").file("a.txt", "a\n").create();
    let b = t.open_pr("human").file("b.txt", "b\n").create();
    // A pending command is cancelled by mallory's push, with her name in the reply.
    t.pr(b).comment("alice", "@mergifyio queue");
    t.wait_until("pending", |t| holds(t, b).contains("queue-command"));
    t.pr(b).push_as("mallory", "b.txt", "b2\n");
    t.pr(b).wait_comment("@mallory pushed new commits");
    // #a: queued once its quick check is green; a second command is ignored with a reply.
    t.pr(a).comment("alice", "@mergifyio queue");
    t.pr(a).check("quick").succeed();
    t.pr(a).wait_queued();
    t.pr(a).comment("bob", "@mergifyio queue");
    t.pr(a).wait_comment("already in queue `default`");
    // Dequeue: out for good, until a new queue command.
    t.pr(a).comment("carol", "@mergifyio dequeue");
    t.wait_until("dequeued", |t| !queued(t, a));
    t.wait_until("dequeued label", |t| {
        t.gh.pr(a).is_some_and(|p| p.labels.contains("dequeued"))
    });
    t.pr(a).push("a.txt", "a2\n");
    t.pr(a).check("quick").succeed();
    t.settle();
    assert!(
        !queued(&t, a),
        "a dequeued pull request never rejoins on its own"
    );
    assert!(
        holds(&t, a).contains("manually-dequeued"),
        "{}",
        holds(&t, a)
    );
    t.pr(a).comment("carol", "@mergifyio dequeue");
    t.pr(a).wait_comment("not queued; nothing to do");
    t.pr(a).comment("carol", "@mergifyio refresh");
    t.pr(a).wait_comment("read again and re-evaluated");
    t.pr(a).comment("carol", "@mergifyio squash");
    t.pr(a).wait_comment("does not support it yet");
    t.pr(a).comment("mallory", "@mergifyio queue");
    t.pr(a).wait_comment("@mallory, you may not run `queue`");
    t.pr(a).comment("bob", "@mergifyio requeue");
    t.pr(a).wait_queued();
    t.queue_pr().check("full").succeed();
    t.pr(a).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn ticking_the_checkboxes_queues_and_requeues() {
    let t = simple_ci(Tester::new(SIMPLE)).start();
    t.permission("alice", "write");
    t.permission("mallory", "write");
    let pr = t.open_pr("human").file("x.txt", "BROKEN\n").create();
    t.pr(pr).tick("mallory", CONTROLS_MARKER);
    t.pr(pr)
        .wait_comment("@mallory, the checkbox did not queue");
    // The box is unticked again; alice ticks it.
    t.pr(pr).tick("alice", CONTROLS_MARKER);
    t.pr(pr).wait_queued();
    t.advance("1m");
    // The full suite fails on the draft: the status comment offers the requeue box.
    t.wait_until("dequeued", |t| {
        t.gh.comments_on(pr)
            .iter()
            .any(|c| c.body.contains(STATUS_MARKER) && c.body.contains("CHECKS_FAILED"))
    });
    t.pr(pr).push("x.txt", "fixed\n");
    t.settle();
    t.pr(pr).tick("alice", STATUS_MARKER);
    t.pr(pr).wait_queued();
    t.advance("1m");
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_lost_command_webhook_is_recovered_by_reconciliation() {
    let t = simple_ci(Tester::new(SIMPLE))
        .reconcile_every("60s")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    t.permission("alice", "write");
    let pr = t.open_pr("human").file("y.txt", "y\n").create();
    let c = t.pr(pr).comment("alice", "@mergifyio queue");
    for _ in 0..10 {
        if t.gh.pr(pr).unwrap().merged {
            break;
        }
        t.advance("60s");
        t.settle();
    }
    t.pr(pr).wait_merged();
    // Handled once: one reaction.
    let n = t.gh.reactions().iter().filter(|(id, ..)| *id == c).count();
    assert_eq!(n, 1);
    t.shutdown();
}

#[test]
fn depends_on_and_merge_after_hold_with_visible_reasons() {
    let t = Tester::new(SIMPLE)
        .ci(CiRule::success("quick", CiOn::PullRequest))
        .start();
    let schema = t
        .open_pr("bot")
        .file("schema.txt", "s\n")
        .title("schema")
        .create();
    let impl_ = t
        .open_pr("bot")
        .file("impl.txt", "i\n")
        .body(&format!("Needs the schema.\n\nDepends-On: #{schema}\n"))
        .create();
    let late = t
        .open_pr("bot")
        .file("late.txt", "l\n")
        .body("Merge-After: 2099-01-01T00:00:00Z")
        .create();
    let bad = t
        .open_pr("bot")
        .file("bad.txt", "b\n")
        .body("Merge-After: whenever")
        .create();
    t.pr(schema).wait_queued();
    t.settle();
    assert!(!queued(&t, impl_));
    t.wait_until("held by Depends-On", |t| {
        holds(t, impl_).contains("depends-on")
    });
    assert!(holds(&t, late).contains("merge-after"));
    assert!(holds(&t, bad).contains("not a valid timestamp"));
    t.queue_pr().check("full").succeed();
    t.pr(schema).wait_merged();
    t.pr(impl_).wait_queued();
    t.queue_pr().check("full").succeed();
    t.pr(impl_).wait_merged();
    assert!(!queued(&t, late) && !queued(&t, bad));
    t.shutdown();
}

#[test]
fn a_changed_configuration_is_checked_and_applies_only_once_merged() {
    // `.mergify.yml` on the default branch; #1 breaks it, #2 adds a second file, #3 loosens
    // it (drops the need for `quick`) and has no quick check at first.
    let t = Tester::new(SIMPLE)
        .ci(CiRule::success("full", CiOn::Queue))
        .start();
    t.permission("alice", "write");
    let broken = t
        .open_pr("bot")
        .file(".mergify.yml", "queue_rules:\n  - name: a\n  - name: a\n")
        .create();
    let second = t
        .open_pr("bot")
        .file(".github/mergify.yml", "queue_rules:\n  - name: a\n")
        .create();
    let loosen = SIMPLE.replace("      - check-success = quick\n", "      - base = main\n");
    let pr3 = t.open_pr("bot").file(".mergify.yml", &loosen).create();
    let check = |n: u64, name: &str, ok: &str| {
        let n2 = n;
        let (name, ok) = (name.to_string(), ok.to_string());
        t.wait_until(&format!("check {name} {ok} on #{n2}"), move |t| {
            let head = t.gh.pr(n2).unwrap().head_sha;
            t.gh.runs().iter().any(|r| {
                r.head_sha == head
                    && r.name == name
                    && r.app_slug == "schlange"
                    && r.conclusion.as_deref() == Some(ok.as_str())
            })
        });
    };
    check(broken, "Configuration changed", "failure");
    check(second, "Configuration changed", "failure");
    check(pr3, "Configuration changed", "success");
    t.settle();
    assert!(holds(&t, broken).contains("configuration-changed"));
    // #3's own copy never evaluates it (MQ-52): it has no quick check, so it waits.
    assert!(!queued(&t, pr3));
    // Under the configuration of the default branch it needs `quick`: once green, it lands.
    t.pr(pr3).check("quick").succeed();
    t.pr(pr3).wait_queued();
    t.pr(pr3).wait_merged();
    // The merge's push to the default branch loads the new configuration (MQ-19), without a
    // reconciliation: a bot pull request without any quick check is queued now.
    t.wait_until("configuration reloaded", |t| {
        t.status().is_some_and(|s| {
            s.log_tail
                .iter()
                .any(|l| l.contains("configuration changed"))
        })
    });
    let free = t.open_pr("bot").file("free.txt", "f\n").create();
    t.pr(free).wait_queued();
    t.pr(free).wait_merged();
    assert!(
        t.status()
            .unwrap()
            .log_tail
            .iter()
            .any(|l| l.contains("configuration changed"))
    );
    t.shutdown();
}

#[test]
fn two_configuration_files_are_an_error_and_the_last_good_one_stays() {
    let t = simple_ci(Tester::new(SIMPLE)).start();
    t.gh.push_base(&[(".github/mergify.yml".into(), "queue_rules: []\n".into())]);
    t.wait_until("configuration error", |t| {
        t.status()
            .and_then(|s| s.config_error)
            .is_some_and(|e| e.contains("more than one configuration file"))
    });
    // The last good configuration still queues and lands.
    let pr = t.open_pr("bot").file("x.txt", "x\n").create();
    t.pr(pr).wait_queued();
    t.advance("1m");
    t.pr(pr).wait_merged();
    t.shutdown();
}

#[test]
fn commit_message_format_and_the_inherited_default() {
    let cfg = SIMPLE.replace(
        "    merge_method: squash\n",
        "    merge_method: squash\n    commit_message_format:\n      title: pr-title\n      body: pr-body\n      trailers: [co-authored-by, approved-by, merged-by]\n",
    );
    let t = simple_ci(Tester::new(&cfg)).start();
    t.permission("alice", "write");
    let pr = t
        .open_pr("human")
        .file("login.txt", "fixed\n")
        .title("Fix the login redirect")
        .body("The redirect lost the query string.")
        .create();
    t.pr(pr)
        .push_as("carol", "login.txt", "fixed, with a test\n");
    t.pr(pr).approve("dave");
    t.settle();
    t.pr(pr).comment("alice", "@mergifyio queue");
    t.pr(pr).wait_queued();
    t.advance("1m");
    t.pr(pr).wait_merged();
    let m = &t.gh.squashes()[0].message;
    assert_eq!(
        m,
        &format!(
            "Fix the login redirect (#{pr})\n\nThe redirect lost the query string.\n\n\
             Co-authored-by: carol <carol@users.noreply.github.com>\nApproved-by: dave\n\
             Merged-by: alice"
        )
    );
    t.shutdown();
    // Without `commit_message_format`: GitHub's own message.
    let t = simple_ci(Tester::new(SIMPLE)).start();
    let pr = t.open_pr("bot").file("b.txt", "b\n").title("Bump").create();
    t.pr(pr).wait_queued();
    t.advance("1m");
    t.pr(pr).wait_merged();
    assert_eq!(
        t.gh.squashes()[0].message,
        format!("Bump (#{pr})\n\n* Bump (#{pr})")
    );
    t.shutdown();
}

#[test]
fn cosmetics_wait_for_the_budget_and_never_delay_landing() {
    // A budget that leaves nothing for cosmetics: labels and comments wait, landing goes on.
    let t = simple_ci(Tester::new(SIMPLE))
        .budget(BudgetConfig {
            capacity: 10_000.0,
            refill_per_sec: 10_000.0,
            cosmetic_reserve: 1_000_000.0,
        })
        .start();
    let pr = t.open_pr("bot").file("c.txt", "c\n").create();
    t.pr(pr).wait_queued();
    t.advance("1m");
    t.pr(pr).wait_merged();
    t.settle();
    assert!(t.gh.comments_on(pr).is_empty(), "no status comment yet");
    assert!(!t.gh.pr(pr).unwrap().labels.contains("queued"));
    assert!(t.status().unwrap().cosmetics_waiting > 0);
    t.shutdown();
}
