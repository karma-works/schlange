//! Golden files for `schlange explain` (S-EXPLAIN, W-13, MQ-43): the explanation of a pull
//! request in each place it can be: not queued (a failing sub-condition of an `or`/`and`/`not`
//! tree), waiting for a `queue` command's conditions, queued behind others, riding in a batch,
//! held by `Depends-On`, held by a pause, dequeued with its reason. `UPDATE_GOLDEN=1` rewrites
//! the files; review the diff.

use std::collections::BTreeMap;
use std::path::Path;

use schlange_config::rules::Rules;
use schlange_core::engine::Control;
use schlange_core::explain::explain;
use schlange_core::model::{CheckRun, Comment, Conclusion, PullRequest};
use schlange_core::{Event, State, step};

const CONFIG: &str = "queue_rules:
  - name: default
    batch_size: 1
    queue_conditions:
      - check-success = quick
      - or:
          - label = ready
          - and:
              - author = bot
              - -draft
      - not:
          label = halt
    merge_conditions:
      - check-success = full
pull_request_rules:
  - name: bots
    conditions:
      - author = bot
    actions:
      queue:
";

struct Driver {
    rules: Rules,
    state: State,
    now: u64,
}

fn run(name: &str, sha: &str, c: Option<Conclusion>, seq: u64) -> CheckRun {
    CheckRun {
        name: name.into(),
        app: None,
        head_sha: sha.into(),
        conclusion: c,
        seq,
        ..Default::default()
    }
}

fn pr(n: u64, author: &str, quick: bool) -> PullRequest {
    let sha = format!("h{n}");
    let mut p = PullRequest::new(n, author, "main", &format!("pr-{n}"), &sha);
    p.title = format!("Change {n}");
    if quick {
        p.checks = vec![run("quick", &sha, Some(Conclusion::Success), n * 10)];
    }
    p
}

impl Driver {
    fn new() -> Self {
        Driver {
            rules: Rules::from_yaml(CONFIG).unwrap(),
            state: State::default(),
            now: 0,
        }
    }

    fn feed(&mut self, e: Event) {
        self.now += 1;
        let (s, _) = step(&self.state, &self.rules, &e, self.now);
        self.state = s;
    }

    fn read(&mut self, prs: Vec<PullRequest>) {
        self.feed(Event::Snapshot {
            pull_requests: prs,
            branches: BTreeMap::from([("main".to_string(), Some("b0".to_string()))]),
            token: None,
        });
    }

    fn text(&self, n: u64) -> String {
        explain(&self.state, &self.rules, n).to_text()
    }
}

fn golden(name: &str, actual: &str) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/golden/explain");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(format!("{name}.txt"));
    if std::env::var_os("UPDATE_GOLDEN").is_some() || !path.exists() {
        std::fs::write(&path, actual).unwrap();
        return;
    }
    let want = std::fs::read_to_string(&path).unwrap();
    assert_eq!(
        want,
        actual,
        "{} differs (UPDATE_GOLDEN=1 rewrites it)",
        path.display()
    );
}

#[test]
fn not_queued_names_the_failing_sub_conditions() {
    let mut d = Driver::new();
    let mut p = pr(1, "alice", true);
    p.labels.insert("halt".into());
    d.read(vec![p]);
    let ex = explain(&d.state, &d.rules, 1);
    assert_eq!(ex.state, "not-queued");
    // `bots` does not match alice; the queue rule's `or` and `not` fail.
    let failing = ex.queue_rules[0].queue_conditions.failing();
    assert!(
        failing
            .iter()
            .any(|f| f.starts_with("one of [label = ready;")),
        "{failing:?}"
    );
    assert!(
        failing.iter().any(|f| f.starts_with("not (label = halt)")),
        "{failing:?}"
    );
    golden("not-queued", &d.text(1));
}

#[test]
fn a_queue_command_waiting_for_its_conditions() {
    let mut d = Driver::new();
    d.read(vec![pr(2, "alice", false)]);
    d.feed(Event::Comments {
        comments: vec![Comment {
            id: 5,
            number: 2,
            author: "carol".into(),
            sender: "carol".into(),
            body: "@mergifyio queue".into(),
            edited: false,
            version: String::new(),
            sender_permission: Some("write".into()),
            by_queue: false,
            head_sha: Some("h2".into()),
        }],
    });
    let ex = explain(&d.state, &d.rules, 2);
    assert_eq!(ex.state, "waiting-for-conditions", "{}", ex.to_text());
    assert_eq!(ex.waiting_command.as_deref(), Some("carol"));
    assert!(
        ex.summary.contains("check-success = quick"),
        "{}",
        ex.summary
    );
    golden("waiting-command", &d.text(2));
}

#[test]
fn queued_behind_others_and_riding_in_a_batch() {
    let mut d = Driver::new();
    d.read(vec![
        pr(3, "bot", true),
        pr(4, "bot", true),
        pr(5, "bot", true),
    ]);
    // batch_size 1 and one car at a time: #3 is checked, #4 and #5 wait.
    let first = explain(&d.state, &d.rules, 3);
    assert_eq!(first.state, "queued");
    let place = first.place.as_ref().unwrap();
    assert_eq!(place.status, "in-batch", "{}", first.to_text());
    assert_eq!(place.batch.as_ref().unwrap().own, vec![3]);
    let third = explain(&d.state, &d.rules, 5);
    let place = third.place.as_ref().unwrap();
    assert_eq!(place.ahead, vec![3, 4]);
    assert_eq!(place.position, 2);
    golden("in-batch", &d.text(3));
    golden("queued-behind", &d.text(5));
}

#[test]
fn held_by_depends_on() {
    let mut d = Driver::new();
    let mut p = pr(6, "bot", true);
    p.body = "Depends-On: #99\n".into();
    d.read(vec![p]);
    let ex = explain(&d.state, &d.rules, 6);
    assert_eq!(ex.state, "held", "{}", ex.to_text());
    assert_eq!(ex.holds[0].kind, "depends-on");
    golden("held-depends-on", &d.text(6));
}

#[test]
fn paused_queue_and_dequeue_reason() {
    let mut d = Driver::new();
    d.read(vec![pr(7, "bot", true)]);
    d.feed(Event::Control {
        control: Control::Pause {
            reason: "release".into(),
            by: Some("ops".into()),
        },
    });
    let ex = explain(&d.state, &d.rules, 7);
    assert_eq!(ex.state, "queued");
    assert!(
        ex.landing_holds
            .iter()
            .any(|h| h.contains("paused: release")),
        "{}",
        ex.to_text()
    );
    golden("paused", &d.text(7));
    // `halt` makes the queue conditions false: it leaves the queue with a reason.
    let mut p = pr(7, "bot", true);
    p.labels.insert("halt".into());
    d.read(vec![p]);
    let ex = explain(&d.state, &d.rules, 7);
    assert_eq!(ex.state, "dequeued", "{}", ex.to_text());
    assert_eq!(
        ex.last_dequeue.as_ref().and_then(|x| x.code.as_deref()),
        Some("PR_DEQUEUED")
    );
    golden("dequeued", &d.text(7));
}

#[test]
fn merged_and_unknown() {
    let d = Driver::new();
    let ex = explain(&d.state, &d.rules, 42);
    assert_eq!(ex.state, "unknown");
    golden("unknown", &d.text(42));
}
