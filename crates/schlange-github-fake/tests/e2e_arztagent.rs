//! Arztagent's `.mergify.yml` (the golden copy), the parts the other end-to-end tests do not
//! pin down, over HTTP against the fake GitHub: Dependabot update types, the
//! `KI-Review or dependabot` condition, `batch_max_wait_time: 5 minutes`, and
//! `commands_restrictions` with the real logins `karma-works` and `julianhaegele-create`.

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, Tester};

const ARZTAGENT: &str = include_str!("../../schlange-config/tests/golden/arztagent.yml");
const DEPENDABOT: &str = "dependabot[bot]";
const BOT: &str = "arztagent-bot";

fn start() -> Running {
    Tester::new(ARZTAGENT)
        // Checks are reported by hand so that each test decides which ones exist.
        .ci(CiRule::fails_on("ci-ok", CiOn::Queue, "BREAKS_FULL_SUITE"))
        .fake(|c| {
            c.permissions.insert("workflows".into(), "write".into());
        })
        .start()
}

fn green(t: &Running, n: u64, ki_review: bool) {
    t.pr(n).check("ci-ok").succeed();
    t.pr(n).check("Risk class and data protection").succeed();
    if ki_review {
        t.pr(n).check("KI-Review").succeed();
    }
}

fn dependabot(t: &Running, update_type: &str, path: &str) -> u64 {
    // The update type comes from the commit message, as Dependabot writes it.
    t.open_pr(DEPENDABOT)
        .file(path, "bumped\n")
        .title(&format!(
            "Bump a dependency\n\n---\nupdated-dependencies:\n- dependency-name: a\n  update-type: {update_type}\n..."
        ))
        .create()
}

fn queued(t: &Running, n: u64) -> bool {
    t.status().is_some_and(|s| s.queued.contains(&n))
}

#[test]
fn dependabot_patch_and_minor_are_queued_without_ki_review() {
    let t = start();
    let patch = dependabot(&t, "version-update:semver-patch", "package.json");
    let minor = dependabot(&t, "version-update:semver-minor", "lock.json");
    for n in [patch, minor] {
        green(&t, n, false);
    }
    t.pr(patch).wait_queued();
    t.pr(minor).wait_queued();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn dependabot_major_and_constitution_updates_are_not_queued() {
    let t = start();
    t.permission("karma-works", "admin");
    let major = dependabot(&t, "version-update:semver-major", "package.json");
    let workflow = dependabot(
        &t,
        "version-update:semver-patch",
        ".github/workflows/ci.yml",
    );
    // A patch update with a failing check on the side: `#check-failure = 0` holds it back.
    let red = dependabot(&t, "version-update:semver-patch", "other.json");
    for n in [major, workflow, red] {
        green(&t, n, false);
    }
    t.pr(red).check("lint").fail();
    t.settle();
    for (n, why) in [
        (major, "major update: a human queues it"),
        (workflow, "workflow update is constitution"),
        (red, "a failing check"),
    ] {
        assert!(!queued(&t, n), "#{n} must not be queued: {why}");
    }
    // The major update is queued by a listed human, with the command.
    t.pr(major).comment("karma-works", "@mergifyio queue");
    t.pr(major).wait_queued();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn ki_review_is_required_from_everyone_but_dependabot() {
    let t = start();
    let agent = t.open_pr(BOT).file("a.py", "a\n").create();
    let human = t.open_pr("karma-works").file("b.py", "b\n").create();
    let bump = dependabot(&t, "version-update:semver-patch", "c.json");
    for n in [agent, human, bump] {
        green(&t, n, false);
    }
    t.pr(bump).wait_queued();
    t.settle();
    assert!(!queued(&t, agent) && !queued(&t, human), "no KI-Review yet");
    t.pr(agent).check("KI-Review").succeed();
    t.pr(agent).wait_queued();
    // A failing KI-Review keeps the human's pull request out.
    t.pr(human).check("KI-Review").fail();
    t.settle();
    assert!(!queued(&t, human));
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn a_partial_batch_waits_exactly_five_minutes() {
    let t = start();
    let a = t.open_pr(BOT).file("a.py", "a\n").create();
    let b = t.open_pr(BOT).file("b.py", "b\n").create();
    for n in [a, b] {
        green(&t, n, true);
    }
    t.pr(a).wait_queued();
    t.pr(b).wait_queued();
    t.settle();
    t.advance("4m");
    t.settle();
    assert!(
        t.gh.open_queue_drafts().is_empty(),
        "nothing is built before batch_max_wait_time (5 minutes) is over"
    );
    t.advance("1m");
    let draft = t.queue_pr().get();
    let tree = t.gh.tree_of(&draft.head_sha);
    assert!(
        tree.contains_key("a.py") && tree.contains_key("b.py"),
        "one batch holds both pull requests"
    );
    t.queue_pr().check("ci-ok").succeed();
    t.pr(a).wait_merged();
    t.pr(b).wait_merged();
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn only_the_two_listed_humans_may_queue_by_command() {
    let t = start();
    for (login, perm) in [
        ("karma-works", "admin"),
        ("julianhaegele-create", "write"),
        (BOT, "write"),
        ("mallory", "write"),
    ] {
        t.permission(login, perm);
    }
    let prs: Vec<u64> = (0..4)
        .map(|i| {
            t.open_pr(BOT)
                .file(&format!(".github/workflows/w{i}.yml"), "on: [push]\n")
                .create()
        })
        .collect();
    for &n in &prs {
        green(&t, n, true);
    }
    t.settle();
    for (&n, user, allowed) in [
        (&prs[0], "karma-works", true),
        (&prs[1], "julianhaegele-create", true),
        (&prs[2], BOT, false),
        (&prs[3], "mallory", false),
    ] {
        t.pr(n).comment(user, "@mergifyio queue");
        if allowed {
            t.pr(n).wait_queued();
        } else {
            t.pr(n)
                .wait_comment(&format!("@{user}, you may not run `queue`"));
            t.settle();
            assert!(!queued(&t, n), "#{n}: {user} must not queue");
        }
    }
    t.assert_invariants();
    t.shutdown();
}
