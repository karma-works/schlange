//! Commands, command restrictions, checkboxes, cosmetics and built-in protections, driven
//! event by event without a forge.

use std::collections::BTreeMap;

use schlange_config::rules::Rules;
use schlange_core::engine::{Cosmetic, Outcome};
use schlange_core::model::{CheckRun, Comment, Conclusion, PullRequest};
use schlange_core::reasons as code;
use schlange_core::{Decision, Event, State, step};

const CONFIG: &str = "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = quick\n      - -label = halt\n  - name: hotfix\n    queue_conditions:\n      - check-success = quick\n      - label = hotfix\n\
pull_request_rules:\n  - name: bots\n    conditions:\n      - author = bot\n    actions:\n      queue:\n\
commands_restrictions:\n  queue:\n    conditions:\n      - or:\n          - sender = alice\n          - sender = bob\n";

struct Driver {
    rules: Rules,
    state: State,
    now: u64,
    comment_id: u64,
}

fn green(n: u64, sha: &str) -> CheckRun {
    CheckRun {
        name: "quick".into(),
        app: None,
        head_sha: sha.into(),
        conclusion: Some(Conclusion::Success),
        seq: n * 100,
    }
}

fn pr(n: u64, author: &str, sha: &str, ok: bool) -> PullRequest {
    let mut p = PullRequest::new(n, author, "main", &format!("pr-{n}"), sha);
    if ok {
        p.checks = vec![green(n, sha)];
    }
    p
}

fn replies(d: &[Decision]) -> Vec<String> {
    d.iter()
        .filter_map(|d| match d {
            Decision::Cosmetic {
                cosmetic: Cosmetic::Reply { body, .. },
            } => Some(body.clone()),
            _ => None,
        })
        .collect()
}

fn has<F: Fn(&Decision) -> bool>(d: &[Decision], f: F) -> bool {
    d.iter().any(f)
}

impl Driver {
    fn new(config: &str) -> Self {
        Driver {
            rules: Rules::from_yaml(config).unwrap(),
            state: State::default(),
            now: 0,
            comment_id: 0,
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

    fn comment_as(&mut self, n: u64, sender: &str, perm: &str, body: &str) -> Vec<Decision> {
        self.comment_id += 1;
        let head = self.state.pull_requests.get(&n).map(|p| p.head_sha.clone());
        let c = Comment {
            id: self.comment_id,
            number: n,
            author: sender.into(),
            sender: sender.into(),
            body: body.into(),
            edited: false,
            version: String::new(),
            sender_permission: Some(perm.into()),
            by_queue: false,
            head_sha: head,
        };
        self.feed(Event::Comments { comments: vec![c] })
    }

    fn comment(&mut self, n: u64, sender: &str, body: &str) -> Vec<Decision> {
        self.comment_as(n, sender, "write", body)
    }
}

#[test]
fn queue_waits_for_the_conditions_then_enters_with_its_sender() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", false)]);
    let out = d.comment(1, "alice", "@mergifyio queue");
    assert!(has(&out, |x| matches!(
        x,
        Decision::Cosmetic {
            cosmetic: Cosmetic::React { content, .. }
        } if content == "+1"
    )));
    assert!(
        replies(&out).is_empty(),
        "an accepted queue reports in the status comment"
    );
    assert!(d.state.pending_commands.contains_key(&1));
    assert!(d.state.queued.is_empty());
    assert!(
        d.state.holds[&1].iter().any(|h| h.kind == "queue-command"),
        "{:?}",
        d.state.holds
    );
    let out = d.read(vec![pr(1, "human", "h1", true)]);
    assert!(has(
        &out,
        |x| matches!(x, Decision::Enqueue { number: 1, queue, .. } if queue == "default")
    ));
    assert_eq!(d.state.queued[&1].by.as_deref(), Some("alice"));
    assert!(d.state.pending_commands.is_empty());
    assert!(!d.state.holds.contains_key(&1));
    // Labels and the status comment follow.
    assert!(has(
        &out,
        |x| matches!(x, Decision::Cosmetic { cosmetic: Cosmetic::Labels { add, .. } } if add == &vec!["queued".to_string()])
    ));
}

#[test]
fn a_second_queue_command_is_ignored_and_another_name_replaces_it() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", false)]);
    d.comment(1, "alice", "@mergifyio queue");
    let out = d.comment(1, "bob", "@Mergifyio queue");
    assert!(replies(&out)[0].contains("already waiting"), "{out:?}");
    assert_eq!(d.state.pending_commands[&1].by, "alice");
    let out = d.comment(1, "bob", "@mergify queue hotfix");
    assert!(replies(&out)[0].contains("hotfix"));
    assert_eq!(
        d.state.pending_commands[&1].queue.as_deref(),
        Some("hotfix")
    );
    // Ready for `default` but not `hotfix`: the named queue only.
    let out = d.read(vec![pr(1, "human", "h1", true)]);
    assert!(!has(&out, |x| matches!(x, Decision::Enqueue { .. })));
    let mut p = pr(1, "human", "h1", true);
    p.labels.insert("hotfix".into());
    let out = d.read(vec![p]);
    assert!(has(
        &out,
        |x| matches!(x, Decision::Enqueue { queue, .. } if queue == "hotfix")
    ));
    // Once queued, the same name is ignored, another name moves it.
    let out = d.comment(1, "alice", "@mergifyio queue hotfix");
    assert!(replies(&out)[0].contains("already in queue"));
    let out = d.comment(1, "alice", "@mergifyio queue default");
    assert!(has(
        &out,
        |x| matches!(x, Decision::Dequeue { reason, .. } if reason.code == Some(code::PR_MANUALLY_DEQUEUED))
    ));
    assert!(has(
        &out,
        |x| matches!(x, Decision::Enqueue { queue, .. } if queue == "default")
    ));
}

#[test]
fn a_foreign_push_cancels_a_pending_command_and_names_the_pusher() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", false)]);
    d.comment(1, "alice", "@mergifyio queue");
    let mut p = pr(1, "human", "h2", true);
    p.head_pusher = Some("mallory".into());
    let out = d.read(vec![p]);
    let r = replies(&out);
    assert!(
        r[0].contains("@mallory") && r[0].contains("cancelled"),
        "{r:?}"
    );
    assert!(!has(&out, |x| matches!(x, Decision::Enqueue { .. })));
    assert!(d.state.pending_commands.is_empty());
    // The queue's own update does not cancel it.
    d.read(vec![pr(1, "human", "h2", false)]);
    d.comment(1, "alice", "@mergifyio queue");
    let mut p = pr(1, "human", "h3", false);
    p.head_by_queue = true;
    d.read(vec![p]);
    assert!(d.state.pending_commands.contains_key(&1));
}

#[test]
fn a_push_after_entering_dequeues_instead() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", true)]);
    d.comment(1, "alice", "@mergifyio queue");
    assert!(d.state.queued.contains_key(&1));
    let out = d.read(vec![pr(1, "human", "h2", true)]);
    assert!(has(
        &out,
        |x| matches!(x, Decision::Dequeue { reason, .. } if reason.code == Some(code::PULL_REQUEST_UPDATED))
    ));
    assert!(replies(&out).is_empty());
}

#[test]
fn dequeue_takes_it_out_for_good_until_a_new_queue_command() {
    let mut d = Driver::new(CONFIG);
    let out = d.read(vec![pr(1, "bot", "h1", true)]);
    assert!(has(&out, |x| matches!(x, Decision::Enqueue { .. })));
    let out = d.comment(1, "carol", "@mergifyio dequeue");
    assert!(has(
        &out,
        |x| matches!(x, Decision::Dequeue { reason, .. } if reason.code == Some(code::PR_MANUALLY_DEQUEUED))
    ));
    // The rule would queue it again; the manual dequeue keeps it out.
    let out = d.read(vec![pr(1, "bot", "h2", true)]);
    assert!(
        !has(&out, |x| matches!(x, Decision::Enqueue { .. })),
        "{out:?}"
    );
    assert!(
        d.state.holds[&1]
            .iter()
            .any(|h| h.kind == "manually-dequeued")
    );
    let out = d.comment(1, "carol", "@mergifyio dequeue");
    assert!(replies(&out)[0].contains("not queued"));
    // `requeue` (the deprecated alias) brings it back, by a sender the restriction allows.
    let out = d.comment(1, "alice", "@mergifyio requeue");
    assert!(
        has(&out, |x| matches!(x, Decision::Enqueue { .. })),
        "{out:?}"
    );
}

#[test]
fn dequeue_cancels_a_pending_command() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", false)]);
    d.comment(1, "alice", "@mergifyio queue");
    let out = d.comment(1, "carol", "@mergifyio dequeue");
    assert!(replies(&out)[0].contains("cancelled"));
    let out = d.read(vec![pr(1, "human", "h1", true)]);
    assert!(!has(&out, |x| matches!(x, Decision::Enqueue { .. })));
}

#[test]
fn restrictions_refuse_with_a_reply_and_defaults_need_write() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", true)]);
    // Arztagent's case: an agent may not queue, even with write permission.
    let out = d.comment_as(1, "arztagent-bot", "admin", "@mergifyio queue");
    assert!(replies(&out)[0].contains("may not run `queue`"), "{out:?}");
    assert!(d.state.queued.is_empty() && d.state.pending_commands.is_empty());
    // `dequeue` keeps the default: write permission.
    let out = d.comment_as(1, "reader", "read", "@mergifyio dequeue");
    assert!(replies(&out)[0].contains("may not run `dequeue`"));
    let out = d.comment_as(1, "writer", "write", "@mergifyio dequeue");
    assert!(replies(&out)[0].contains("not queued"));
    // `refresh`: write, or the author of a pull request from a fork.
    let out = d.comment_as(1, "human", "read", "@mergifyio refresh");
    assert!(replies(&out)[0].contains("may not run `refresh`"));
    let mut p = pr(2, "forker", "f1", false);
    p.from_fork = true;
    d.read(vec![p]);
    let out = d.comment_as(2, "forker", "read", "@mergifyio refresh");
    assert!(has(
        &out,
        |x| matches!(x, Decision::Refresh { numbers, .. } if numbers == &vec![2])
    ));
    let out = d.comment_as(2, "stranger", "read", "@mergifyio refresh");
    assert!(replies(&out)[0].contains("may not"));
}

#[test]
fn other_commands_are_recognised_but_not_supported_and_unknown_ones_answered() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "human", "h1", false)]);
    for c in ["backport stable", "copy x", "rebase", "squash", "update"] {
        let out = d.comment(1, "writer", &format!("@mergifyio {c}"));
        assert!(
            replies(&out)[0].contains("does not support it yet"),
            "{c}: {out:?}"
        );
    }
    let out = d.comment(1, "writer", "@mergifyio frobnicate");
    assert!(replies(&out)[0].contains("not a command"));
    let out = d.comment(1, "writer", "LGTM, @mergifyio queue later");
    assert!(out.is_empty(), "not a command: {out:?}");
    let out = d.comment(1, "alice", "@mergifyio queue nope");
    assert!(replies(&out)[0].contains("no queue named `nope`"));
}

#[test]
fn a_comment_is_handled_once() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "bot", "h1", true)]);
    let c = Comment {
        id: 7,
        number: 1,
        author: "carol".into(),
        sender: "carol".into(),
        body: "@mergifyio dequeue".into(),
        edited: false,
        version: String::new(),
        sender_permission: Some("write".into()),
        by_queue: false,
        head_sha: None,
    };
    let out = d.feed(Event::Comments {
        comments: vec![c.clone()],
    });
    assert!(has(&out, |x| matches!(x, Decision::Dequeue { .. })));
    let out = d.feed(Event::Comments {
        comments: vec![c.clone(), c],
    });
    assert!(out.is_empty(), "{out:?}");
}

#[test]
fn the_checkbox_queues_by_whoever_ticked_it_under_the_same_restrictions() {
    let mut d = Driver::new(CONFIG);
    let out = d.read(vec![pr(1, "human", "h1", true)]);
    let body = out
        .iter()
        .find_map(|x| match x {
            Decision::Cosmetic {
                cosmetic: Cosmetic::ControlsComment { body, .. },
            } => Some(body.clone()),
            _ => None,
        })
        .expect("controls comment");
    let tick = |sender: &str, version: &str| Comment {
        id: 50,
        number: 1,
        author: "schlange[bot]".into(),
        sender: sender.into(),
        body: body.replace("- [ ]", "- [x]"),
        edited: true,
        version: version.into(),
        sender_permission: Some("write".into()),
        by_queue: true,
        head_sha: None,
    };
    let out = d.feed(Event::Comments {
        comments: vec![tick("mallory", "1")],
    });
    assert!(replies(&out)[0].contains("did not queue"), "{out:?}");
    assert!(d.state.queued.is_empty());
    let out = d.feed(Event::Comments {
        comments: vec![tick("bob", "2")],
    });
    assert!(
        has(&out, |x| matches!(x, Decision::Enqueue { .. })),
        "{out:?}"
    );
    assert_eq!(d.state.queued[&1].by.as_deref(), Some("bob"));
    // The comment is rewritten without a box.
    assert!(has(
        &out,
        |x| matches!(x, Decision::Cosmetic { cosmetic: Cosmetic::ControlsComment { body, .. } } if body.contains("@bob") && !body.contains("- [ ]"))
    ));
}

#[test]
fn the_requeue_box_in_the_status_comment_after_a_dequeue() {
    let mut d = Driver::new(CONFIG);
    d.read(vec![pr(1, "bot", "h1", true)]);
    let mut p = pr(1, "bot", "h1", true);
    p.labels.insert("halt".into());
    let out = d.read(vec![p]);
    let status = out
        .iter()
        .find_map(|x| match x {
            Decision::Cosmetic {
                cosmetic: Cosmetic::StatusComment { body, .. },
            } => Some(body.clone()),
            _ => None,
        })
        .expect("status comment");
    assert!(
        status.contains("left the queue") && status.contains("- [ ]"),
        "{status}"
    );
    assert!(has(
        &out,
        |x| matches!(x, Decision::Cosmetic { cosmetic: Cosmetic::Labels { add, remove, .. } } if add == &vec!["dequeued".to_string()] && remove == &vec!["queued".to_string()])
    ));
    // Tick it; it waits for the conditions (the label is still on).
    let c = Comment {
        id: 60,
        number: 1,
        author: "schlange[bot]".into(),
        sender: "alice".into(),
        body: status.replace("- [ ]", "- [x]"),
        edited: true,
        version: "1".into(),
        sender_permission: Some("read".into()),
        by_queue: true,
        head_sha: None,
    };
    d.feed(Event::Comments { comments: vec![c] });
    assert!(d.state.pending_commands.contains_key(&1));
}

#[test]
fn status_comment_levels_and_label_options() {
    let cfg = |extra: &str| {
        format!(
            "merge_queue:\n{extra}queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = quick\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n"
        )
    };
    let count = |d: &[Decision]| {
        d.iter()
            .filter(|x| {
                matches!(
                    x,
                    Decision::Cosmetic {
                        cosmetic: Cosmetic::StatusComment { .. }
                    }
                )
            })
            .count()
    };
    let mut all = Driver::new(&cfg("  status_comments: all\n"));
    assert_eq!(count(&all.read(vec![pr(1, "x", "h1", true)])), 1);
    let mut outcomes = Driver::new(&cfg(
        "  status_comments: outcomes\n  checking_label: checking\n  queued_label: null\n",
    ));
    let out = outcomes.read(vec![pr(1, "x", "h1", true)]);
    assert_eq!(count(&out), 0, "entering is not an outcome");
    // In a car now: only the checking label (queued_label is off).
    assert!(
        has(
            &out,
            |x| matches!(x, Decision::Cosmetic { cosmetic: Cosmetic::Labels { add, .. } } if add == &vec!["checking".to_string()])
        ),
        "{out:?}"
    );
    let out = outcomes.read(vec![PullRequest {
        closed: true,
        ..pr(1, "x", "h1", true)
    }]);
    assert_eq!(count(&out), 1, "a dequeue is an outcome");
    let mut none = Driver::new(&cfg("  status_comments: none\n"));
    none.read(vec![pr(1, "x", "h1", true)]);
    let out = none.read(vec![PullRequest {
        closed: true,
        ..pr(1, "x", "h1", true)
    }]);
    assert_eq!(count(&out), 0);
}

#[test]
fn depends_on_holds_until_the_dependency_merges() {
    let mut d = Driver::new(CONFIG);
    d.state.host.repository = "acme/app".into();
    let mut a = pr(1, "bot", "h1", true);
    a.body = "Needs the schema first.\n\nDepends-On: #2\n".into();
    let out = d.read(vec![a, pr(2, "human", "h2", false)]);
    assert!(!has(&out, |x| matches!(
        x,
        Decision::Enqueue { number: 1, .. }
    )));
    assert!(d.state.holds[&1][0].text.contains("acme/app#2"));
    let mut b = pr(2, "human", "h2", false);
    b.merged = true;
    b.closed = true;
    let out = d.read(vec![b]);
    assert!(
        has(&out, |x| matches!(x, Decision::Enqueue { number: 1, .. })),
        "{out:?}"
    );
}

#[test]
fn merge_after_waits_on_the_clock_and_an_invalid_value_blocks() {
    let mut d = Driver::new(CONFIG);
    let mut a = pr(1, "bot", "h1", true);
    a.body = "Merge-After: 1970-01-01T00:10:00Z".into();
    let out = d.read(vec![a.clone()]);
    assert!(
        has(&out, |x| matches!(x, Decision::WakeAt { at: 600 })),
        "{out:?}"
    );
    assert!(d.state.queued.is_empty());
    d.now = 599;
    let out = d.feed(Event::Tick);
    assert!(has(&out, |x| matches!(x, Decision::Enqueue { .. })));
    let mut b = pr(2, "bot", "h2", true);
    b.body = "Merge-After: whenever".into();
    d.read(vec![b]);
    assert!(d.state.holds[&2][0].failed);
    assert!(!d.state.queued.contains_key(&2));
}

#[test]
fn a_changed_configuration_that_does_not_validate_holds_and_is_reported() {
    use schlange_config::config_file::{CHECK_CHANGED, ConfigChange};
    let mut d = Driver::new(CONFIG);
    let mut a = pr(1, "bot", "h1", true);
    a.files = vec![".mergify.yml".into()];
    a.config_change = Some(ConfigChange {
        paths: vec![".mergify.yml".into()],
        deleted: false,
        error: Some("bad".into()),
        summary: "bad".into(),
    });
    let out = d.read(vec![a.clone()]);
    assert!(d.state.queued.is_empty());
    assert!(has(
        &out,
        |x| matches!(x, Decision::Cosmetic { cosmetic: Cosmetic::ProtectionCheck { name, ok: false, .. } } if name == CHECK_CHANGED)
    ));
    a.config_change.as_mut().unwrap().error = None;
    let out = d.read(vec![a]);
    assert!(has(&out, |x| matches!(x, Decision::Enqueue { .. })));
    assert!(has(&out, |x| matches!(
        x,
        Decision::Cosmetic {
            cosmetic: Cosmetic::ProtectionCheck { ok: true, .. }
        }
    )));
}

#[test]
fn queue_dequeue_reason_is_an_attribute() {
    let cfg = "queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = quick\n      - queue-dequeue-reason != pr-manually-dequeued\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n";
    let mut d = Driver::new(cfg);
    d.read(vec![pr(1, "x", "h1", true)]);
    assert!(d.state.queued.contains_key(&1));
    d.comment(1, "w", "@mergifyio dequeue");
    assert_eq!(d.state.checks_ended[&1], code::PR_MANUALLY_DEQUEUED);
    assert!(matches!(d.state.outcomes[&1], Outcome::Dequeued { .. }));
}
