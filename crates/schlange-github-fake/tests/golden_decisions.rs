//! Golden decision logs: the engine's decisions in three end-to-end runs against the fake
//! GitHub, compared with `tests/golden/*.txt` (in the spirit of bors' snapshot tests). Reads
//! and timers (`refresh`, `wake_at`) are left out because their number depends on how
//! webhooks coalesce; SHAs, car ids and queue branch names are numbered in order of
//! appearance. `UPDATE_GOLDEN=1` rewrites the files; review the diff.

use std::collections::BTreeMap;
use std::path::Path;

use schlange_github_fake::tester::Running;
use schlange_github_fake::{CiOn, CiRule, Tester};

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
}

/// Replaces SHAs (40 hex), queue branch suffixes (10 hex) and numeric car/token ids by
/// placeholders numbered in order of appearance.
fn normalise(lines: &[String]) -> String {
    let mut names: BTreeMap<String, String> = BTreeMap::new();
    let mut counters: BTreeMap<&'static str, usize> = BTreeMap::new();
    let mut name = |kind: &'static str, raw: &str| -> String {
        let key = format!("{kind}:{raw}");
        if let Some(n) = names.get(&key) {
            return n.clone();
        }
        let c = counters.entry(kind).or_insert(0);
        *c += 1;
        let n = format!("<{kind}{c}>");
        names.insert(key, n.clone());
        n
    };
    let mut out = String::new();
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("decision JSON");
        let kind = v["type"].as_str().unwrap_or("");
        if matches!(kind, "refresh" | "wake_at") {
            continue;
        }
        let mut v = v;
        fn walk(v: &mut serde_json::Value, name: &mut dyn FnMut(&'static str, &str) -> String) {
            match v {
                serde_json::Value::Object(o) => {
                    for (k, x) in o.iter_mut() {
                        if (k == "car" || k == "token") && x.is_u64() {
                            *x = serde_json::Value::String(name("car", &x.to_string()));
                        } else {
                            walk(x, name);
                        }
                    }
                }
                serde_json::Value::Array(a) => a.iter_mut().for_each(|x| walk(x, name)),
                serde_json::Value::String(s) => {
                    let hex = |t: &str| t.chars().all(|c| c.is_ascii_hexdigit());
                    if s.len() == 40 && hex(s) {
                        *s = name("sha", s);
                    } else if let Some(rest) = s.strip_prefix("mergify/merge-queue/")
                        && rest.len() == 10
                        && hex(rest)
                    {
                        *s = format!("mergify/merge-queue/{}", name("branch", rest));
                    } else {
                        // SHAs inside texts ("moved from 1a2b3c4 to …") are short.
                        let words: Vec<String> = s
                            .split(' ')
                            .map(|w| {
                                if w.len() == 7 && hex(w) && w.chars().any(|c| c.is_ascii_digit()) {
                                    name("short", w)
                                } else {
                                    w.to_string()
                                }
                            })
                            .collect();
                        *s = words.join(" ");
                    }
                }
                _ => {}
            }
        }
        walk(&mut v, &mut name);
        out.push_str(&v.to_string());
        out.push('\n');
    }
    out
}

fn check(t: &Running, file: &str) {
    t.settle();
    let actual = normalise(&t.decisions());
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(file);
    if std::env::var_os("UPDATE_GOLDEN").is_some() {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &actual).unwrap();
        return;
    }
    let expected = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("{} missing; run with UPDATE_GOLDEN=1", path.display()));
    assert_eq!(expected, actual, "golden file {} differs", path.display());
}

#[test]
fn golden_one_pull_request_lands() {
    let t = arztagent().start();
    let a = t
        .open_pr(BOT)
        .file("backend/a.py", "a = 1\n")
        .title("a")
        .create();
    t.pr(a).wait_queued();
    t.settle();
    t.advance("5m");
    t.pr(a).wait_merged();
    t.wait_until("closed", |t| t.gh.open_queue_drafts().is_empty());
    check(&t, "one-pull-request.txt");
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn golden_a_red_batch_is_bisected() {
    let t = arztagent().start();
    let mut prs = Vec::new();
    for (path, content) in [
        ("backend/a.py", "a = 1\n"),
        ("backend/b.py", "b = 1  # BREAKS_FULL_SUITE\n"),
    ] {
        let p = t.open_pr(BOT).file(path, content).title(path).create();
        t.pr(p).wait_queued();
        t.settle();
        prs.push(p);
    }
    t.advance("5m");
    t.pr(prs[0]).wait_merged();
    t.wait_until("culprit out", |t| {
        t.status()
            .is_some_and(|s| s.queued.is_empty() && s.cars == 0)
    });
    check(&t, "bisection.txt");
    t.assert_invariants();
    t.shutdown();
}

#[test]
fn golden_a_halt_label_dequeues() {
    let t = arztagent().start();
    let a = t
        .open_pr(BOT)
        .file("backend/a.py", "a = 1\n")
        .title("a")
        .create();
    t.pr(a).wait_queued();
    t.settle();
    t.pr(a).label("halt");
    t.wait_until("dequeued", |t| t.dequeue_code(a).is_some());
    check(&t, "halt.txt");
    t.assert_invariants();
    t.shutdown();
}
