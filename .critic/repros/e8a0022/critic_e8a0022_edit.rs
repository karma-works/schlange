//! Critic e8a0022 reproductions C5/C6 (apply fake-edit-helper.patch first) against the fake GitHub. Copy into
//! crates/schlange-github-fake/tests/ and run `cargo test -p schlange-github-fake --test critic_e8a0022`.

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, FaultKind, FaultRule, Tester, Then, guard_labels};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const BOT: &str = "arztagent-bot";

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
}

fn green_pr(t: &Running, path: &str, content: &str) -> u64 {
    t.open_pr(BOT).file(path, content).title(path).create()
}

/// C5 (needs a temporary `critic_edit_comment` helper on `FakeGithub`, see the report): the
/// agent has write access, which on GitHub lets it edit other people's comments. It edits an
/// old non-command comment of a listed human into `@mergifyio queue` while webhooks are lost.
/// Reconciliation (`unhandled_commands`) takes the comment as a fresh command by its author.
/// Expected: the constitution pull request is NOT queued (an edit never runs a command).
#[test]
fn c5_agent_edits_a_human_comment_into_a_queue_command() {
    use schlange_github_fake::HookFaults;
    let t = arztagent()
        .reconcile_every("60s")
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
        .start();
    t.permission(BOT, "write");
    t.permission("karma-works", "admin");
    let pr = t
        .open_pr(BOT)
        .file(".github/workflows/ci.yml", "on: [pull_request]\n")
        .title("Loosen the CI workflow")
        .create();
    t.pr(pr).wait_comment(schlange_core::commands::CONTROLS_MARKER);
    let human = t.pr(pr).comment("karma-works", "I will look at this tomorrow.");
    t.settle();
    t.gh.set_hook_faults(HookFaults {
        drop: 1.0,
        ..Default::default()
    });
    assert!(t.gh.critic_edit_comment(human, BOT, "@mergifyio queue"));
    for _ in 0..8 {
        t.advance("60s");
        t.settle();
    }
    let queued = t.status().is_some_and(|s| s.queued.contains(&pr));
    let merged = t.gh.pr(pr).unwrap().merged;
    assert!(!queued && !merged, "agent's edit queued the constitution PR: queued={queued} merged={merged}");
    t.shutdown();
}

/// C6: same edit with webhooks delivered, then a restart of the service (persistent store).
#[test]
fn c6_edit_then_restart() {
    let mut t = arztagent()
        .persistent()
        .reconcile_every("60s")
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
        .start();
    t.permission(BOT, "write");
    t.permission("karma-works", "admin");
    let pr = t
        .open_pr(BOT)
        .file(".github/workflows/ci.yml", "on: [pull_request]\n")
        .title("Loosen the CI workflow")
        .create();
    t.pr(pr).wait_comment(schlange_core::commands::CONTROLS_MARKER);
    let human = t.pr(pr).comment("karma-works", "I will look at this tomorrow.");
    t.settle();
    t.stop_service();
    assert!(t.gh.critic_edit_comment(human, BOT, "@mergifyio queue"));
    t.start_service();
    for _ in 0..8 {
        t.advance("60s");
        t.settle();
    }
    let queued = t.status().is_some_and(|s| s.queued.contains(&pr));
    let merged = t.gh.pr(pr).unwrap().merged;
    assert!(!queued && !merged, "edit during downtime queued the constitution PR: queued={queued} merged={merged}");
    t.shutdown();
}

