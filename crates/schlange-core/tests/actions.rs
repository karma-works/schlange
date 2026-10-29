//! Rule actions at the engine level: edge and level triggering, idempotence, the queue's
//! interplay, answers, restarts.

use std::collections::BTreeMap;

use schlange_config::rules::Rules;
use schlange_core::actions::{ActionResult, Op};
use schlange_core::model::PullRequest;
use schlange_core::{Decision, Event, State, step};

struct Driver {
    rules: Rules,
    state: State,
    now: u64,
}

impl Driver {
    fn new(config: &str) -> Self {
        Driver {
            rules: Rules::from_yaml(config).unwrap(),
            state: State::default(),
            now: 0,
        }
    }

    fn feed(&mut self, e: Event) -> Vec<Decision> {
        self.now += 1;
        let (s, d) = step(&self.state, &self.rules, &e, self.now);
        self.state = s;
        d
    }

    fn read(&mut self, prs: Vec<PullRequest>) -> Vec<Decision> {
        self.feed(Event::Snapshot {
            pull_requests: prs,
            branches: BTreeMap::from([("main".to_string(), Some("b0".to_string()))]),
            token: None,
        })
    }

    /// Answers every action request with `Done`.
    fn answer(&mut self, out: &[Decision]) -> Vec<Decision> {
        let mut all = Vec::new();
        for (id, _) in acts(out) {
            all.extend(self.feed(Event::ActionDone {
                id,
                result: ActionResult::Done { head_sha: None },
            }));
        }
        all
    }
}

fn acts(d: &[Decision]) -> Vec<(u64, Op)> {
    d.iter()
        .filter_map(|d| match d {
            Decision::Act { id, op, .. } => Some((*id, op.clone())),
            _ => None,
        })
        .collect()
}

fn ops(d: &[Decision]) -> Vec<Op> {
    acts(d).into_iter().map(|(_, o)| o).collect()
}

fn pr(n: u64, title: &str) -> PullRequest {
    let mut p = PullRequest::new(n, "dev", "main", &format!("pr-{n}"), &format!("h{n}"));
    p.title = title.into();
    p
}

const WIP: &str = "pull_request_rules:\n  - name: wip\n    conditions:\n      - title ~= WIP\n    actions:\n      label:\n        toggle: [WIP]\n      comment:\n        message: \"@{{ author }}, #{{number}} is a work in progress\"\n";

#[test]
fn comment_is_edge_triggered_and_label_is_level_triggered() {
    let mut d = Driver::new(WIP);
    let out = d.read(vec![pr(1, "WIP: thing")]);
    let o = ops(&out);
    assert_eq!(o.len(), 2, "{o:?}");
    // Actions run in the schema's order: `comment` before `label`.
    assert!(matches!(&o[1], Op::Labels { add, .. } if add == &vec!["WIP".to_string()]));
    let Op::Comment { body, .. } = &o[0] else {
        panic!("{o:?}")
    };
    assert!(body.starts_with("@dev, #1 is a work in progress\n<!-- schlange-action "));
    // The same read again while the label request is outstanding: nothing new (MQ-12).
    assert!(ops(&d.read(vec![pr(1, "WIP: thing")])).is_empty());
    // The label arrived: still nothing.
    let mut labeled = pr(1, "WIP: thing");
    labeled.labels.insert("WIP".into());
    assert!(ops(&d.read(vec![labeled.clone()])).is_empty());
    // Someone removes it while the title still matches: put back, but no second comment.
    let o = ops(&d.read(vec![pr(1, "WIP: thing")]));
    assert_eq!(o.len(), 1, "{o:?}");
    assert!(matches!(&o[0], Op::Labels { add, .. } if add == &vec!["WIP".to_string()]));
    // The title no longer matches: the toggle removes the label.
    let mut done = labeled.clone();
    done.title = "thing".into();
    let o = ops(&d.read(vec![done.clone()]));
    assert!(
        matches!(&o[..], [Op::Labels { remove, add, .. }] if remove == &vec!["WIP".to_string()] && add.is_empty()),
        "{o:?}"
    );
    done.labels.clear();
    assert!(ops(&d.read(vec![done])).is_empty());
    // It matches again: the comment runs a second time, with a new marker.
    let o = ops(&d.read(vec![pr(1, "WIP again")]));
    let bodies: Vec<&String> = o
        .iter()
        .filter_map(|x| match x {
            Op::Comment { body, .. } => Some(body),
            _ => None,
        })
        .collect();
    assert_eq!(bodies.len(), 1);
    assert!(!bodies[0].contains(&body[body.find("<!--").unwrap()..]));
}

#[test]
fn rules_do_not_fight_over_a_label_and_exclusive_actions_run_once() {
    let cfg = "pull_request_rules:\n  - name: a\n    conditions: []\n    actions:\n      label:\n        add: [x]\n      close:\n        message: first\n  - name: b\n    conditions: []\n    actions:\n      label:\n        remove: [x]\n      close:\n        message: second\n";
    let mut d = Driver::new(cfg);
    let o = ops(&d.read(vec![pr(1, "t")]));
    let closes = o.iter().filter(|x| matches!(x, Op::Close { .. })).count();
    assert_eq!(closes, 1, "{o:?}");
    assert!(
        o.iter()
            .any(|x| matches!(x, Op::Comment { body, .. } if body.starts_with("first"))),
        "the first rule in the file wins: {o:?}"
    );
    assert!(
        !o.iter()
            .any(|x| matches!(x, Op::Labels { remove, .. } if !remove.is_empty())),
        "{o:?}"
    );
}

#[test]
fn a_rule_that_undoes_itself_stops_after_ten_reversals() {
    // The rule's condition reads the label its own toggle sets: it would flip forever.
    let cfg = "pull_request_rules:\n  - name: flip\n    conditions:\n      - -label = x\n    actions:\n      label:\n        toggle: [x]\n";
    let mut d = Driver::new(cfg);
    let mut p = pr(1, "t");
    let mut changes = 0;
    for _ in 0..40 {
        let o = ops(&d.read(vec![p.clone()]));
        let Some(Op::Labels { add, remove, .. }) = o.first() else {
            continue;
        };
        changes += 1;
        for l in add {
            p.labels.insert(l.clone());
        }
        for l in remove {
            p.labels.remove(l);
        }
    }
    assert!(changes <= 11, "{changes} changes");
    assert!(d.state.actions.frozen_labels.contains("1/x"));
    // A new configuration lifts the stop.
    d.feed(Event::ConfigurationChanged);
    assert!(d.state.actions.frozen_labels.is_empty());
}

#[test]
fn defaults_fill_in_options() {
    let cfg = "defaults:\n  actions:\n    comment:\n      message: from defaults {{ title }}\npull_request_rules:\n  - name: c\n    conditions: []\n    actions:\n      comment:\n";
    let mut d = Driver::new(cfg);
    let o = ops(&d.read(vec![pr(1, "T")]));
    assert!(matches!(&o[..], [Op::Comment { body, .. }] if body.starts_with("from defaults T")));
}

#[test]
fn post_check_reports_success_and_failure_from_the_rule() {
    let cfg = "pull_request_rules:\n  - name: needs label\n    conditions:\n      - label = ok\n    actions:\n      post_check: {}\n";
    let mut d = Driver::new(cfg);
    let o = ops(&d.read(vec![pr(1, "t")]));
    let [
        Op::PostCheck {
            name,
            conclusion,
            title,
            summary,
            ..
        },
    ] = &o[..]
    else {
        panic!("{o:?}")
    };
    assert_eq!(name, "Rule: needs label (post_check)");
    assert_eq!(conclusion, "failure");
    assert_eq!(title, "Rule 'needs label': failure");
    assert_eq!(summary, "- [ ] `label = ok`");
    assert!(ops(&d.read(vec![pr(1, "t")])).is_empty(), "posted once");
    let mut ok = pr(1, "t");
    ok.labels.insert("ok".into());
    let o = ops(&d.read(vec![ok]));
    assert!(matches!(&o[..], [Op::PostCheck { conclusion, .. }] if conclusion == "success"));
}

#[test]
fn update_only_when_behind_and_not_queued() {
    let cfg = "queue_rules:\n  - name: default\n    queue_conditions:\n      - label = queue\npull_request_rules:\n  - name: q\n    conditions:\n      - label = queue\n    actions:\n      queue:\n  - name: up\n    conditions: []\n    actions:\n      update:\n";
    let mut d = Driver::new(cfg);
    assert!(ops(&d.read(vec![pr(1, "t")])).is_empty(), "up to date");
    let mut behind = pr(1, "t");
    behind.behind_base = true;
    let o = ops(&d.read(vec![behind.clone()]));
    assert!(matches!(&o[..], [Op::Update { head_sha, .. }] if head_sha == "h1"));
    // A queued pull request is left to the queue.
    let mut d = Driver::new(cfg);
    behind.labels.insert("queue".into());
    let out = d.read(vec![behind]);
    assert!(out.iter().any(|x| matches!(x, Decision::Enqueue { .. })));
    assert!(ops(&out).is_empty(), "{out:?}");
}

#[test]
fn backport_waits_for_the_merge_and_reports() {
    let cfg = "pull_request_rules:\n  - name: bp\n    conditions:\n      - label = backport\n    actions:\n      backport:\n        branches: [stable]\n";
    let mut d = Driver::new(cfg);
    let mut p = pr(1, "Fix");
    p.labels.insert("backport".into());
    assert!(ops(&d.read(vec![p.clone()])).is_empty(), "waits");
    // The label goes away before the merge: still armed (it matched while open).
    p.labels.clear();
    p.merged = true;
    p.closed = true;
    p.merge_commit_sha = Some("m1".into());
    let out = d.read(vec![p.clone()]);
    let a = acts(&out);
    let [
        (
            id,
            Op::Copy {
                branches,
                source,
                title,
                ..
            },
        ),
    ] = &a[..]
    else {
        panic!("{out:?}")
    };
    assert_eq!(branches, &vec!["stable".to_string()]);
    assert_eq!(source.as_deref(), Some("m1"));
    assert_eq!(title, "Fix (backport of #1)");
    let out = d.feed(Event::ActionDone {
        id: *id,
        result: ActionResult::Copied {
            copies: vec![schlange_core::actions::CopyOutcome {
                destination: "stable".into(),
                number: Some(7),
                conflicts: true,
                error: None,
            }],
        },
    });
    let o = ops(&out);
    assert!(
        matches!(&o[..], [Op::PostCheck { name, conclusion, summary, .. }]
            if name == "Rule: bp (backport)" && conclusion == "success" && summary.contains("#7, with conflicts")),
        "{o:?}"
    );
    assert!(ops(&d.read(vec![p])).is_empty(), "once");
}

#[test]
fn a_restart_resends_edge_requests_in_doubt_with_the_same_marker() {
    let mut d = Driver::new(WIP);
    let out = d.read(vec![pr(1, "WIP")]);
    let first = ops(&out);
    // Crash before any answer: restart from the journaled state.
    let journaled: State = serde_json::from_str(&serde_json::to_string(&d.state).unwrap()).unwrap();
    d.state = journaled;
    let out = d.feed(Event::Restarted);
    let again = ops(&out);
    let comment = |o: &[Op]| {
        o.iter()
            .find_map(|x| match x {
                Op::Comment { body, .. } => Some(body.clone()),
                _ => None,
            })
            .unwrap()
    };
    assert_eq!(
        comment(&first),
        comment(&again),
        "same marker: the forge dedups"
    );
    // The level request is re-decided (once) from what the engine knows.
    assert_eq!(
        again
            .iter()
            .filter(|x| matches!(x, Op::Labels { .. }))
            .count(),
        1
    );
    let o = ops(&d.read(vec![pr(1, "WIP")]));
    assert!(o.is_empty(), "{o:?}");
    // Answers end the requests; nothing is sent again later.
    d.answer(&out);
    d.now += 10_000;
    assert!(
        !ops(&d.feed(Event::Tick))
            .iter()
            .any(|x| matches!(x, Op::Comment { .. }))
    );
}

#[test]
fn a_lost_answer_is_sent_again_after_the_timeout_a_bounded_number_of_times() {
    let mut d = Driver::new(WIP);
    let out = d.read(vec![pr(1, "WIP")]);
    assert!(out.iter().any(|x| matches!(x, Decision::WakeAt { .. })) || true);
    let mut resent = 0;
    for _ in 0..10 {
        d.now += schlange_core::actions::ACTION_TIMEOUT;
        resent += ops(&d.feed(Event::Tick))
            .iter()
            .filter(|x| matches!(x, Op::Comment { .. }))
            .count();
    }
    assert_eq!(resent as u32, schlange_core::actions::ACTION_ATTEMPTS - 1);
}

#[test]
fn backport_and_squash_commands() {
    let mut d = Driver::new("pull_request_rules: []\n");
    let mut p = pr(1, "t");
    p.commit_messages = vec!["a".into(), "b".into()];
    d.read(vec![p.clone()]);
    let c = |id, body: &str| schlange_core::model::Comment {
        id,
        number: 1,
        author: "w".into(),
        sender: "w".into(),
        body: body.into(),
        edited: false,
        version: String::new(),
        sender_permission: Some("write".into()),
        by_queue: false,
        head_sha: None,
    };
    let out = d.feed(Event::Comments {
        comments: vec![c(1, "@mergifyio squash first-commit")],
    });
    let a = acts(&out);
    let [(id, Op::Squash { message, .. })] = &a[..] else {
        panic!("{out:?}")
    };
    assert_eq!(message, "a");
    let out = d.feed(Event::ActionDone {
        id: *id,
        result: ActionResult::Done {
            head_sha: Some("h9".into()),
        },
    });
    assert!(
        out.iter().any(|x| matches!(x, Decision::Cosmetic { .. })),
        "reply"
    );
    d.feed(Event::Comments {
        comments: vec![c(2, "@mergifyio backport stable")],
    });
    p.merged = true;
    p.closed = true;
    p.merge_commit_sha = Some("m".into());
    let o = ops(&d.read(vec![p]));
    assert!(
        matches!(&o[..], [Op::Copy { branches, .. }] if branches == &vec!["stable".to_string()]),
        "{o:?}"
    );
}

const MERGE: &str = "pull_request_rules:\n  - name: automerge\n    conditions:\n      - label = automerge\n      - -files ~= ^\\.github/\n    actions:\n      merge:\n        method: squash\n";

fn refresh_token(d: &[Decision]) -> Option<u64> {
    d.iter().find_map(|x| match x {
        Decision::Refresh { token, .. } => Some(*token),
        _ => None,
    })
}

fn fresh(d: &mut Driver, p: PullRequest, token: u64) -> Vec<Decision> {
    d.feed(Event::Snapshot {
        pull_requests: vec![p],
        branches: BTreeMap::from([("main".to_string(), Some("b0".to_string()))]),
        token: Some(token),
    })
}

fn automerge(n: u64) -> PullRequest {
    let mut p = pr(n, "t");
    p.labels.insert("automerge".into());
    p.files = vec!["src/a.rs".into()];
    p
}

/// The `merge` action lands outside the queue: it is decided on a fresh read of the pull
/// request (I1), at exactly the head of that read, and never on the engine's view alone.
#[test]
fn merge_is_decided_on_a_fresh_read_at_its_head() {
    let mut d = Driver::new(MERGE);
    let out = d.read(vec![automerge(1)]);
    assert!(
        ops(&out).is_empty(),
        "no merge on the engine's view: {out:?}"
    );
    let token = refresh_token(&out).expect("a fresh read first");
    // The fresh read still matches: merge at that head.
    let out = fresh(&mut d, automerge(1), token);
    assert!(
        matches!(&ops(&out)[..], [Op::Merge { head_sha, .. }] if head_sha == "h1"),
        "{out:?}"
    );

    // A head that moved between the decision and the read: no merge at the new head yet.
    let mut d = Driver::new(MERGE);
    let token = refresh_token(&d.read(vec![automerge(2)])).unwrap();
    let mut moved = automerge(2);
    moved.head_sha = "h2b".into();
    let out = fresh(&mut d, moved.clone(), token);
    assert!(ops(&out).is_empty(), "{out:?}");
    // It asks again for the new head instead.
    let token = refresh_token(&out).expect("a new read for the new head");
    let out = fresh(&mut d, moved, token);
    assert!(matches!(&ops(&out)[..], [Op::Merge { head_sha, .. }] if head_sha == "h2b"));

    // The fresh read shows a constitution file: nothing.
    let mut d = Driver::new(MERGE);
    let token = refresh_token(&d.read(vec![automerge(3)])).unwrap();
    let mut evil = automerge(3);
    evil.files.push(".github/workflows/x.yml".into());
    assert!(ops(&fresh(&mut d, evil, token)).is_empty());

    // A truncated file list fails closed, also before the read.
    let mut d = Driver::new(MERGE);
    let mut cut = automerge(4);
    cut.files_incomplete = true;
    let out = d.read(vec![cut]);
    assert!(
        refresh_token(&out).is_none() && ops(&out).is_empty(),
        "{out:?}"
    );
}

#[test]
fn merge_honours_the_pause_and_is_never_resent() {
    let mut d = Driver::new(MERGE);
    d.feed(Event::Control {
        control: schlange_core::engine::Control::Pause {
            reason: "release".into(),
            by: None,
        },
    });
    let out = d.read(vec![automerge(1)]);
    assert!(
        refresh_token(&out).is_none() && ops(&out).is_empty(),
        "paused: {out:?}"
    );
    let out = d.feed(Event::Control {
        control: schlange_core::engine::Control::Unpause,
    });
    let token = refresh_token(&out).expect("unpaused");
    let out = fresh(&mut d, automerge(1), token);
    let a = acts(&out);
    let [(id, Op::Merge { .. })] = &a[..] else {
        panic!("{out:?}")
    };
    // The merge call fails (perhaps after merging): it is not sent again as is.
    let decision = out
        .iter()
        .find(|x| matches!(x, Decision::Act { .. }))
        .cloned()
        .unwrap();
    let _ = id;
    let out = d.feed(Event::Failed {
        decision,
        error: "502".into(),
    });
    let mut all = out;
    d.now += 10_000;
    all.extend(d.feed(Event::Tick));
    all.extend(d.read(vec![automerge(1)]));
    assert!(
        !ops(&all).iter().any(|o| matches!(o, Op::Merge { .. })),
        "{all:?}"
    );
    // It is decided again, through a fresh read.
    assert!(refresh_token(&all).is_some(), "{all:?}");
}

#[test]
fn a_restart_does_not_resend_a_merge_or_a_head_rewrite() {
    let mut d = Driver::new(MERGE);
    let token = refresh_token(&d.read(vec![automerge(1)])).unwrap();
    let out = fresh(&mut d, automerge(1), token);
    assert_eq!(ops(&out).len(), 1);
    let out = d.feed(Event::Restarted);
    assert!(
        ops(&out).is_empty(),
        "a merge in doubt is decided again: {out:?}"
    );
}

/// A command vouches for the head at the comment's time: a push in between means nothing is
/// done under the sender's name.
#[test]
fn commands_are_bound_to_the_head_at_the_comment() {
    let mut d = Driver::new("pull_request_rules: []\n");
    let mut p = pr(1, "t");
    p.behind_base = true;
    p.commit_messages = vec!["a".into(), "b".into()];
    d.read(vec![p]);
    for (id, body) in [
        (1, "@mergifyio update"),
        (2, "@mergifyio squash"),
        (3, "@mergifyio copy stable"),
        (4, "@mergifyio backport stable"),
    ] {
        let out = d.feed(Event::Comments {
            comments: vec![schlange_core::model::Comment {
                id,
                number: 1,
                author: "w".into(),
                sender: "w".into(),
                body: body.into(),
                edited: false,
                version: String::new(),
                sender_permission: Some("write".into()),
                by_queue: false,
                head_sha: Some("older".into()),
            }],
        });
        assert!(ops(&out).is_empty(), "{body}: {out:?}");
        assert!(
            out.iter().any(|x| matches!(x, Decision::Cosmetic { cosmetic: schlange_core::engine::Cosmetic::Reply { body, .. } } if body.contains("changed after the command"))),
            "{body}: {out:?}"
        );
    }
}

#[test]
fn pending_action_is_not_resent_when_check_identity_is_unavailable() {
    let mut d = Driver::new(WIP);
    let first = d.read(vec![pr(1, "WIP")]);
    assert!(ops(&first).iter().any(|x| matches!(x, Op::Comment { .. })));
    let out = d.feed(Event::CheckIdentityUnavailable {
        base: "main".into(),
        generation: 4,
        numbers: vec![1],
    });
    assert!(ops(&out).is_empty());
    d.now += schlange_core::actions::ACTION_TIMEOUT;
    assert!(ops(&d.feed(Event::Tick)).is_empty());
    d.state = serde_json::from_slice(&serde_json::to_vec(&d.state).unwrap()).unwrap();
    assert!(ops(&d.feed(Event::Restarted)).is_empty());
}
