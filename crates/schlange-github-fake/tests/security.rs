//! Arztagent's threat model against `schlange serve` on the fake GitHub: an AI agent with
//! write access may push, comment, label and edit, but a constitution change (the `-files`
//! gate of `.mergify.yml`) lands only when a listed human queued it, at the head that lands.
//!
//! The critic's reproductions at e8a0022 (`.critic/repros/e8a0022/`), made permanent:
//! N1 (an edited comment run as its author's command), N2 (a forged commit author keeps a
//! human's waiting command), N3 (a `halt` label during a transient 405/409 lands anyway), C3
//! (a read failure between batch landings), M1 (truncated file lists) and M2 (commands among
//! many comments); plus the race of a push against a human's command.

use schlange_core::commands::CONTROLS_MARKER;
use schlange_github_fake::tester::Running;
use schlange_github_fake::{
    CiOn, CiRule, FaultKind, FaultRule, HookFaults, Tester, Then, guard_labels,
};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const AGENT: &str = "arztagent-bot";
const HUMAN: &str = "karma-works";

fn arztagent() -> Tester {
    Tester::new(ARZTAGENT)
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::PullRequest))
        .ci(CiRule::fails_on("ci-ok", CiOn::Queue, "BREAKS_FULL_SUITE"))
        .ci(CiRule::success("KI-Review", CiOn::PullRequest))
        .merge_guard(guard_labels(&["halt"]))
        // Constitution pull requests change workflows; GitHub refuses those merges without it.
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
}

/// The agent and a listed human with their real permissions.
fn people(t: &Running) {
    t.permission(AGENT, "write");
    t.permission(HUMAN, "admin");
}

fn green_pr(t: &Running, path: &str, content: &str) -> u64 {
    t.open_pr(AGENT).file(path, content).title(path).create()
}

fn constitution_pr(t: &Running) -> u64 {
    t.open_pr(AGENT)
        .file(".github/workflows/ci.yml", "on: [pull_request]\n")
        .title("Loosen the CI workflow")
        .create()
}

fn queued(t: &Running, n: u64) -> bool {
    t.status().is_some_and(|s| s.queued.contains(&n))
}

/// Drives time on, approving whatever queue draft is open.
fn run_queue(t: &Running, rounds: usize) {
    for _ in 0..rounds {
        t.advance("60s");
        t.settle();
        for d in t.gh.open_queue_drafts() {
            t.gh.report_check(d.number, "ci-ok", Some("success"));
        }
    }
}

// ---- N3: the merge call answers 405/409 while someone sets `halt` ---------------------

fn halt_during_transient(status: u16, message: &str) {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    t.pr(a).wait_queued();
    t.gh.add_fault(
        FaultRule::new(
            "PUT",
            &format!("/pulls/{a}/merge"),
            FaultKind::StatusMessage(status, message.into()),
            1,
        )
        .then(Then::Label(a, "halt".into())),
    );
    t.advance("5m");
    for _ in 0..6 {
        t.advance("1m");
        t.settle();
    }
    let merged = t.gh.pr(a).unwrap().merged;
    let v = t.gh.invariant_violations(true);
    assert!(!merged && v.is_empty(), "merged={merged} violations={v:?}");
    // Not stuck either: without the label it lands.
    t.pr(a).unlabel("halt");
    for _ in 0..10 {
        if t.gh.pr(a).unwrap().merged {
            break;
        }
        t.advance("5m");
        t.settle();
    }
    t.pr(a).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

/// Critic e8a0022 C1 (N3): a transient 405 goes back to the engine, which sees `halt`.
#[test]
fn a_halt_label_during_a_transient_405_never_lands() {
    halt_during_transient(
        405,
        "Base branch was modified. Review and try the merge again.",
    );
}

/// Critic e8a0022 C2 (N3): the same with a transient 409.
#[test]
fn a_halt_label_during_a_transient_409_never_lands() {
    halt_during_transient(409, "Head branch was modified.");
}

/// Critic e8a0022 C3: a batch of two; the merge reply of the first is lost (it merged, 502),
/// then reading the second fails for a while. Both land exactly once.
#[test]
fn a_read_failure_between_batch_landings_lands_each_once() {
    let t = arztagent().start();
    let a = green_pr(&t, "backend/a.py", "a = 1\n");
    let b = green_pr(&t, "backend/b.py", "b = 1\n");
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.gh.add_fault(FaultRule::new(
        "GET",
        &format!("/pulls/{b}"),
        FaultKind::Status(502),
        8,
    ));
    t.gh.add_fault(FaultRule::new(
        "PUT",
        &format!("/pulls/{a}/merge"),
        FaultKind::Lie(502),
        1,
    ));
    t.advance("5m");
    for _ in 0..20 {
        if t.gh.pr(a).unwrap().merged && t.gh.pr(b).unwrap().merged {
            break;
        }
        t.advance("2m");
        t.settle();
        for d in t.gh.open_queue_drafts() {
            t.gh.report_check(d.number, "ci-ok", Some("success"));
        }
    }
    assert!(t.gh.pr(a).unwrap().merged && t.gh.pr(b).unwrap().merged);
    t.assert_invariants();
    t.shutdown();
}

// ---- N1: edited comments --------------------------------------------------------------

/// The fake models GitHub: the author and anyone with write access may edit a comment.
#[test]
fn the_fake_lets_writers_edit_other_peoples_comments() {
    let t = arztagent().start();
    t.permission(AGENT, "write");
    t.permission("reader", "read");
    let pr = green_pr(&t, "backend/a.py", "a = 1\n");
    let c = t.pr(pr).comment(HUMAN, "Looks fine.");
    assert!(t.gh.edit_comment(c, "reader", "@mergifyio queue").is_err());
    assert!(t.gh.edit_comment(c, AGENT, "Edited by the agent.").is_ok());
    assert!(t.gh.edit_comment(c, HUMAN, "Edited back.").is_ok());
    let body =
        t.gh.comments_on(pr)
            .into_iter()
            .find(|x| x.id == c)
            .unwrap();
    assert_eq!(body.body, "Edited back.");
    assert!(body.updated_at > body.created_at);
    t.shutdown();
}

/// Critic e8a0022 C5 (N1): the agent edits an old non-command comment of a listed human into
/// `@mergifyio queue` while webhooks are lost. Reconciliation must not run it as the human's.
#[test]
fn an_agent_editing_a_human_comment_into_a_command_queues_nothing() {
    let t = arztagent().reconcile_every("60s").start();
    people(&t);
    let pr = constitution_pr(&t);
    t.pr(pr).wait_comment(CONTROLS_MARKER);
    let human = t.pr(pr).comment(HUMAN, "I will look at this tomorrow.");
    t.settle();
    t.gh.set_hook_faults(HookFaults {
        drop: 1.0,
        ..Default::default()
    });
    t.gh.edit_comment(human, AGENT, "@mergifyio queue").unwrap();
    run_queue(&t, 8);
    let merged = t.gh.pr(pr).unwrap().merged;
    assert!(
        !queued(&t, pr) && !merged,
        "the agent's edit queued the constitution PR (merged={merged})"
    );
    t.shutdown();
}

/// The same edit with webhooks delivered: the edit's webhook names the agent as sender and
/// is not a command (only a ticked checkbox counts as an edit).
#[test]
fn an_edit_delivered_by_webhook_queues_nothing() {
    let t = arztagent().reconcile_every("60s").start();
    people(&t);
    let pr = constitution_pr(&t);
    t.pr(pr).wait_comment(CONTROLS_MARKER);
    let human = t.pr(pr).comment(HUMAN, "I will look at this tomorrow.");
    t.settle();
    t.gh.edit_comment(human, AGENT, "@mergifyio queue").unwrap();
    run_queue(&t, 4);
    assert!(!queued(&t, pr) && !t.gh.pr(pr).unwrap().merged);
    t.shutdown();
}

/// Critic e8a0022 C6 (N1): the edit happens while `serve` is down (persistent store); the
/// restart's reconciliation must not run it.
#[test]
fn an_edit_during_a_restart_queues_nothing() {
    let mut t = arztagent().persistent().reconcile_every("60s").start();
    people(&t);
    let pr = constitution_pr(&t);
    t.pr(pr).wait_comment(CONTROLS_MARKER);
    let human = t.pr(pr).comment(HUMAN, "I will look at this tomorrow.");
    t.settle();
    t.stop_service();
    t.gh.edit_comment(human, AGENT, "@mergifyio queue").unwrap();
    t.start_service();
    run_queue(&t, 8);
    let merged = t.gh.pr(pr).unwrap().merged;
    assert!(
        !queued(&t, pr) && !merged,
        "the edit during downtime queued the constitution PR (merged={merged})"
    );
    t.shutdown();
}

// ---- N2: forged commit authors --------------------------------------------------------

/// Critic e8a0022 C7 (N2): a listed human queues a constitution pull request while
/// `KI-Review` is pending; the agent pushes a commit whose author maps to the app's bot. The
/// waiting command is cancelled; nothing is queued or merged.
#[test]
fn a_push_with_a_forged_bot_author_cancels_a_waiting_command() {
    let t = Tester::new(ARZTAGENT)
        .ci(CiRule::success(
            "Risk class and data protection",
            CiOn::Both,
        ))
        .ci(CiRule::success("ci-ok", CiOn::Both))
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
        .start();
    people(&t);
    let pr = constitution_pr(&t);
    t.settle();
    t.pr(pr).comment(HUMAN, "@mergifyio queue");
    t.settle();
    assert!(!queued(&t, pr), "waits for KI-Review");
    t.pr(pr).push_as(
        "schlange[bot]",
        ".github/workflows/evil.yml",
        "on: [push]\n",
    );
    t.settle();
    t.pr(pr).check("KI-Review").succeed();
    run_queue(&t, 6);
    let merged = t.gh.pr(pr).unwrap().merged;
    assert!(
        !queued(&t, pr) && !merged,
        "the forged push kept the human's command (merged={merged})"
    );
    assert!(
        t.pr(pr)
            .comments()
            .iter()
            .any(|c| c.contains("cancelled") && c.contains("pushed new commits")),
        "{:#?}",
        t.pr(pr).comments()
    );
    t.shutdown();
}

// ---- Races of a push against a human's command ---------------------------------------

/// The human comments `queue` on the head they reviewed; the agent pushes right after. The
/// webhooks are lost, so reconciliation finds the command only after the push. GitHub's
/// activity log says which head was current at the comment: the command vouches for that
/// one, and the push cancels it.
#[test]
fn a_push_after_a_lost_command_is_not_attributed_to_the_human() {
    let t = arztagent()
        .reconcile_every("60s")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    people(&t);
    let pr = constitution_pr(&t);
    t.pr(pr).comment(HUMAN, "@mergifyio queue");
    t.pr(pr)
        .push_as(AGENT, ".github/workflows/evil.yml", "on: [push]\n");
    run_queue(&t, 8);
    let merged = t.gh.pr(pr).unwrap().merged;
    assert!(
        !queued(&t, pr) && !merged,
        "a head pushed after the command was queued under the human's name (merged={merged})"
    );
    t.shutdown();
}

/// The same race with webhooks delivered: the agent pushes before the service handled the
/// comment. Whatever order the service sees, the new head is never queued.
#[test]
fn a_push_racing_a_command_webhook_is_not_attributed_to_the_human() {
    let t = arztagent().reconcile_every("60s").start();
    people(&t);
    let pr = constitution_pr(&t);
    t.pr(pr).wait_comment(CONTROLS_MARKER);
    t.settle();
    t.pr(pr).comment(HUMAN, "@mergifyio queue");
    t.pr(pr)
        .push_as(AGENT, ".github/workflows/evil.yml", "on: [push]\n");
    run_queue(&t, 6);
    let pushed = t.gh.pr(pr).unwrap().head_sha;
    let landed =
        t.gh.squashes()
            .iter()
            .any(|q| q.pr == pr && q.head == pushed);
    assert!(!landed, "the pushed head landed under the human's command");
    t.shutdown();
}

/// A recovered command on an unchanged head still works (recovery is not switched off).
#[test]
fn a_lost_command_on_an_unchanged_head_is_recovered() {
    let t = arztagent()
        .reconcile_every("60s")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    people(&t);
    let pr = constitution_pr(&t);
    t.pr(pr).comment(HUMAN, "@mergifyio queue");
    for _ in 0..24 {
        if t.gh.pr(pr).unwrap().merged {
            break;
        }
        run_queue(&t, 1);
    }
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

// ---- M2: commands among many comments -------------------------------------------------

/// Reconciliation reads every page of each open pull request's comments, not only the
/// repository's 100 newest: a command buried under 150 newer comments elsewhere is found.
#[test]
fn a_lost_command_behind_many_newer_comments_is_recovered() {
    let t = arztagent()
        .reconcile_every("60s")
        .hook_faults(HookFaults {
            drop: 1.0,
            ..Default::default()
        })
        .start();
    people(&t);
    let pr = constitution_pr(&t);
    let chatty = green_pr(&t, "docs/notes.md", "notes\n");
    t.pr(pr).comment(HUMAN, "@mergifyio queue");
    for i in 0..150 {
        t.gh.comment(chatty, "someone", &format!("note {i}"));
    }
    for _ in 0..24 {
        if t.gh.pr(pr).unwrap().merged {
            break;
        }
        run_queue(&t, 1);
    }
    t.pr(pr).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

// ---- M1: truncated file lists ---------------------------------------------------------

/// GitHub lists at most 3000 files (here: 2). A pull request whose constitution path is
/// beyond the cap is not queued: `files` conditions fail closed on an incomplete list.
#[test]
fn a_constitution_path_beyond_the_file_list_cap_is_not_queued() {
    let t = arztagent().start();
    people(&t);
    t.gh.set_files_cap(2);
    let pr = t
        .open_pr(AGENT)
        .file("backend/a.py", "a = 1\n")
        .file("backend/b.py", "b = 1\n")
        .file("zzz/quality/gate.json", "{}\n")
        .title("Three files, the last one constitution")
        .create();
    // A small pull request within the cap is queued as usual.
    let small = green_pr(&t, "backend/c.py", "c = 1\n");
    t.pr(small).wait_queued();
    t.settle();
    assert!(!queued(&t, pr), "queued on a truncated file list");
    run_queue(&t, 6);
    assert!(!t.gh.pr(pr).unwrap().merged);
    t.assert_invariants();
    t.shutdown();
}
