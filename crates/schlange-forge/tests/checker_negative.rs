//! The invariant checker against a misbehaving engine: these tests drive the simulated forge
//! with hand-made decisions (no engine) and require the checker to reject each unsafe landing
//! by name, and to accept the safe shapes it was taught. Each test pins one branch of the
//! landing checks in `invariants.rs`, so weakening that branch makes a test fail.

use schlange_config::rules::{MergeMethod, Rules};
use schlange_core::State;
use schlange_core::actions::Op;
use schlange_core::engine::{CarPr, Decision};
use schlange_core::model::Conclusion;
use schlange_forge::ci::CiScript;
use schlange_forge::invariants::Checker;
use schlange_forge::sim::{Action, PrDef, SimForge};

fn rules(method: &str, edit: bool, single_step: bool) -> Rules {
    let check = if single_step { "quick" } else { "full" };
    Rules::from_yaml(&format!(
        "queue_rules:\n  - name: default\n    merge_method: {method}\n    allow_queue_branch_edit: {edit}\n    merge_conditions:\n      - check-success = {check}\n"
    ))
    .unwrap()
}

/// A forge with `main` (file `base`), CI `full` on queue drafts and `quick` on pull requests,
/// and pull request #1 changing `a`, open and green.
fn forge() -> SimForge {
    let mut f = SimForge::new(
        "main",
        [("base".to_string(), "0\n".to_string())].into(),
        vec![CiScript::new("full", 60, Some(Conclusion::Success)).queue_only()],
        0,
    );
    let mut d = PrDef::new(1, "bot", vec![("a".into(), "1\n".into())]);
    d.scripts = vec![CiScript::new("quick", 30, Some(Conclusion::Success)).pr_only()];
    f.define(d);
    f.schedule(0, Action::Open(1));
    drain(&mut f);
    f
}

fn drain(f: &mut SimForge) {
    while f.next_item().is_some() {}
}

fn head(f: &SimForge, n: u64) -> String {
    f.read_pull_request(n).unwrap().head_sha
}

fn violations(f: &SimForge, rules: &Rules) -> Vec<String> {
    let mut c = Checker::new();
    c.after_step(f, &State::default(), rules);
    c.results
        .iter()
        .flat_map(|(id, r)| r.violations.iter().map(move |v| format!("{id}: {v}")))
        .collect()
}

fn has(v: &[String], needle: &str) -> bool {
    v.iter().any(|x| x.contains(needle))
}

const MOVED_BASE: &str = "landed on a base whose tree is not the one car 1 was tested on";
const UNTESTED_HEAD: &str = "landed although the draft's head was not the car's tested head";

fn build_car(f: &mut SimForge) -> String {
    let base = f.read_branch("main").unwrap();
    f.execute(&Decision::BuildCar {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        branch: "mergify/merge-queue/0000000001".into(),
        base_sha: base,
        prs: vec![CarPr {
            number: 1,
            head_sha: head(f, 1),
        }],
        own: vec![1],
        stack_on: None,
    });
    drain(f);
    f.read_branch("mergify/merge-queue/0000000001").unwrap()
}

fn land(f: &mut SimForge, method: MergeMethod, head_sha: &str) {
    let expected = f.read_branch("main").unwrap();
    f.execute(&Decision::Land {
        car: 1,
        queue: "default".into(),
        base_branch: "main".into(),
        expected_base_sha: expected,
        prs: vec![CarPr {
            number: 1,
            head_sha: head(f, 1),
        }],
        expected_tree: String::new(),
        messages: vec![],
        method,
        head: Some(head_sha.to_string()),
        draft: None,
    });
}

fn check_in_place(f: &mut SimForge) {
    let h = head(f, 1);
    f.execute(&Decision::CheckInPlace {
        car: 1,
        number: 1,
        head_sha: h,
        direct: false,
    });
}

fn update_with_base(f: &mut SimForge) {
    let h = head(f, 1);
    f.execute(&Decision::Act {
        id: 99,
        rule: "r".into(),
        op: Op::Update {
            number: 1,
            head_sha: h,
        },
    });
    drain(f);
}

fn push_base(f: &mut SimForge) {
    let at = f.now();
    f.schedule(
        at,
        Action::PushBase {
            branch: "main".into(),
            files: vec![("base".into(), "moved\n".into())],
        },
    );
    drain(f);
}

/// Safe: the base moved before the car, a rule's `update` merged it into the pull request, the
/// car is checked in place on that head and lands by fast-forward. Accepted.
#[test]
fn in_place_fast_forward_after_an_update_is_accepted() {
    let r = rules("fast-forward", false, true);
    let mut f = forge();
    push_base(&mut f);
    update_with_base(&mut f);
    check_in_place(&mut f);
    let h = head(&f, 1);
    land(&mut f, MergeMethod::FastForward, &h);
    let v = violations(&f, &r);
    assert!(!has(&v, MOVED_BASE), "{v:?}");
}

/// Unsafe: checked in place on a head that contains the moved base (tested base B1), but at
/// landing main is at another tree (B0, as after a revert) that the head also contains: the
/// fast-forward is possible, yet the tested change does not sit on the tested base. Rejected.
#[test]
fn in_place_fast_forward_on_a_moved_base_is_rejected() {
    let r = rules("fast-forward", false, true);
    let mut f = forge();
    let b0 = f.read_branch("main").unwrap();
    push_base(&mut f);
    update_with_base(&mut f);
    check_in_place(&mut f);
    let b1 = f.read_branch("main").unwrap();
    f.repo.update_ref("main", &b1, &b0).unwrap();
    let h = head(&f, 1);
    land(&mut f, MergeMethod::FastForward, &h);
    assert!(
        f.landings.last().unwrap().error.is_none(),
        "the forge landed it"
    );
    let v = violations(&f, &r);
    assert!(has(&v, MOVED_BASE), "{v:?}");
}

/// Unsafe: a foreign push to the queue branch without `allow_queue_branch_edit`, landed by
/// fast-forward. Rejected: the pushed head is not the tested one.
#[test]
fn a_foreign_push_without_the_option_landed_by_fast_forward_is_rejected() {
    let r = rules("fast-forward", false, false);
    let mut f = forge();
    build_car(&mut f);
    f.schedule(f.now(), Action::PushQueueBranch(1));
    drain(&mut f);
    let pushed = f.read_branch("mergify/merge-queue/0000000001").unwrap();
    land(&mut f, MergeMethod::FastForward, &pushed);
    let v = violations(&f, &r);
    assert!(has(&v, UNTESTED_HEAD), "{v:?}");
}

/// Safe with the option: the push sits on the tested head, the full suite ran on it, main did
/// not move. Accepted.
#[test]
fn an_accepted_edit_on_the_tested_head_retested_is_accepted() {
    let r = rules("fast-forward", true, false);
    let mut f = forge();
    build_car(&mut f);
    f.schedule(f.now(), Action::PushQueueBranch(1));
    drain(&mut f);
    let pushed = f.read_branch("mergify/merge-queue/0000000001").unwrap();
    land(&mut f, MergeMethod::FastForward, &pushed);
    let v = violations(&f, &r);
    assert!(!has(&v, UNTESTED_HEAD) && !has(&v, "I0"), "{v:?}");
}

/// Unsafe with the option: the pushed head does not descend from the tested head (a force
/// push of a sibling). Rejected.
#[test]
fn a_pushed_head_that_does_not_descend_from_the_tested_head_is_rejected() {
    let r = rules("fast-forward", true, false);
    let mut f = forge();
    build_car(&mut f);
    let base = f.read_branch("main").unwrap();
    f.schedule(
        f.now(),
        Action::ForcePushQueueBranch {
            number: 1,
            parents: vec![base],
            files: vec![("x".into(), "sibling\n".into())],
        },
    );
    drain(&mut f);
    let pushed = f.read_branch("mergify/merge-queue/0000000001").unwrap();
    land(&mut f, MergeMethod::FastForward, &pushed);
    let v = violations(&f, &r);
    assert!(has(&v, UNTESTED_HEAD), "{v:?}");
}

/// Unsafe with the option: main moved to another tree, the pushed head merges it on top of the
/// tested head and is landed by fast-forward. Rejected: the tested base is not main's tip.
#[test]
fn an_accepted_edit_on_a_moved_base_is_rejected() {
    let r = rules("fast-forward", true, false);
    let mut f = forge();
    let tested = build_car(&mut f);
    push_base(&mut f);
    let main = f.read_branch("main").unwrap();
    f.schedule(
        f.now(),
        Action::ForcePushQueueBranch {
            number: 1,
            parents: vec![tested, main.clone()],
            files: vec![("base".into(), "moved\n".into())],
        },
    );
    drain(&mut f);
    let pushed = f.read_branch("mergify/merge-queue/0000000001").unwrap();
    assert!(f.repo.is_ancestor(&main, &pushed));
    land(&mut f, MergeMethod::FastForward, &pushed);
    let v = violations(&f, &r);
    assert!(has(&v, MOVED_BASE), "{v:?}");
}

/// Unsafe with the option: the edit is landed before the full suite ran on the pushed head.
/// Rejected.
#[test]
fn an_accepted_edit_landed_untested_is_rejected() {
    let r = rules("fast-forward", true, false);
    let mut f = forge();
    build_car(&mut f);
    // Push and land at once: CI on the pushed head has not reported.
    f.act_now(Action::PushQueueBranch(1));
    let pushed = f.read_branch("mergify/merge-queue/0000000001").unwrap();
    land(&mut f, MergeMethod::FastForward, &pushed);
    let v = violations(&f, &r);
    assert!(
        has(&v, "merge conditions were false") || has(&v, "without green merge checks"),
        "{v:?}"
    );
}
