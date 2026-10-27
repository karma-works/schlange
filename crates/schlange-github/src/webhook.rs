//! Webhooks: signature check (`X-Hub-Signature-256`, HMAC-SHA256, constant-time compare),
//! delivery-ID deduplication, and parsing of the payloads Schlange reacts to.
//!
//! Payloads are parsed with small structs that name only the fields used. A delivery becomes
//! a core [`Event`] (for the log; what the hint said) and one or more [`Hint`]s. The service
//! never feeds the webhook's own content to the engine: each hint schedules a read of the
//! named pull request or branch, and that read is what the engine sees (webhooks are hints,
//! the forge is the truth; reordered or stale deliveries cannot move the state backwards).

use std::collections::{HashSet, VecDeque};

use serde::Deserialize;

use schlange_core::Event;
use schlange_core::model::{CheckRun, Conclusion, PullRequest};

/// What a delivery asks the service to read.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Hint {
    PullRequest(u64),
    Branch(String),
    /// A commit (check run, status): the pull requests whose head it is.
    Sha(String),
    /// Something repository-wide changed (rulesets, installation): a full reconciliation.
    Full,
    /// A comment was created or edited (a command, or a ticked checkbox in the queue's own
    /// comment). The comment's content is taken from the signed delivery: GitHub's API does
    /// not say who edited a comment.
    Comment(Box<CommentHint>),
}

/// A comment as an `issue_comment` delivery names it.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CommentHint {
    pub id: u64,
    pub number: u64,
    /// Who wrote the comment.
    pub author: String,
    /// Who sent the event (for an edit: who edited it).
    pub sender: String,
    pub body: String,
    pub edited: bool,
    /// `updated_at`.
    pub version: String,
}

#[derive(Clone, Debug)]
pub struct Parsed {
    pub event_name: String,
    pub action: Option<String>,
    /// `owner/name` of the repository, when the payload has one.
    pub repo: Option<String>,
    pub installation: Option<u64>,
    pub event: Option<Event>,
    pub hints: Vec<Hint>,
    /// One line for the log.
    pub summary: String,
}

fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// `sha256=<hex>` of the body, as GitHub sends it.
pub fn sign(secret: &[u8], body: &[u8]) -> String {
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    format!("sha256={}", hex(ring::hmac::sign(&key, body).as_ref()))
}

/// Checks `X-Hub-Signature-256`. ring compares in constant time.
pub fn verify_signature(secret: &[u8], body: &[u8], header: Option<&str>) -> bool {
    let Some(tag) = header
        .and_then(|h| h.trim().strip_prefix("sha256="))
        .and_then(hex_decode)
    else {
        return false;
    };
    let key = ring::hmac::Key::new(ring::hmac::HMAC_SHA256, secret);
    ring::hmac::verify(&key, body, &tag).is_ok()
}

/// Remembers the last `cap` delivery ids (GitHub redelivers with the same id).
#[derive(Debug)]
pub struct Dedup {
    order: VecDeque<String>,
    seen: HashSet<String>,
    cap: usize,
}

impl Dedup {
    pub fn new(cap: usize) -> Self {
        Dedup {
            order: VecDeque::new(),
            seen: HashSet::new(),
            cap: cap.max(1),
        }
    }

    /// Records `id`; true when it was seen before.
    pub fn check(&mut self, id: &str) -> bool {
        if self.seen.contains(id) {
            return true;
        }
        if self.order.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        self.order.push_back(id.to_string());
        self.seen.insert(id.to_string());
        false
    }
}

/// Maps a check run's `status`/`conclusion` (GitHub's words) to the core's conclusion;
/// `None` while it is not completed.
pub fn conclusion(status: &str, conclusion: Option<&str>) -> Option<Conclusion> {
    if status != "completed" {
        return None;
    }
    Some(match conclusion? {
        "success" => Conclusion::Success,
        "failure" => Conclusion::Failure,
        "neutral" => Conclusion::Neutral,
        "cancelled" => Conclusion::Cancelled,
        "timed_out" => Conclusion::TimedOut,
        "action_required" => Conclusion::ActionRequired,
        "skipped" => Conclusion::Skipped,
        "stale" => Conclusion::Stale,
        _ => return None,
    })
}

/// A commit status `state` as a conclusion (`pending` → not completed).
pub fn status_conclusion(state: &str) -> Option<Conclusion> {
    match state {
        "success" => Some(Conclusion::Success),
        "failure" | "error" => Some(Conclusion::Failure),
        _ => None,
    }
}

#[derive(Deserialize)]
struct RepoJson {
    full_name: String,
}

#[derive(Deserialize)]
struct InstallationJson {
    id: u64,
}

#[derive(Deserialize)]
struct Common {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    repository: Option<RepoJson>,
    #[serde(default)]
    installation: Option<InstallationJson>,
}

#[derive(Deserialize)]
struct Side {
    #[serde(rename = "ref")]
    ref_name: String,
    sha: String,
}

#[derive(Deserialize)]
struct UserJson {
    login: String,
}

#[derive(Deserialize)]
struct LabelJson {
    name: String,
}

#[derive(Deserialize)]
struct PrJson {
    number: u64,
    #[serde(default)]
    title: String,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    state: String,
    #[serde(default)]
    merged: bool,
    user: UserJson,
    head: Side,
    base: Side,
    #[serde(default)]
    labels: Vec<LabelJson>,
}

#[derive(Deserialize)]
struct PullRequestEvent {
    number: u64,
    pull_request: PrJson,
    #[serde(default)]
    label: Option<LabelJson>,
}

#[derive(Deserialize)]
struct PrNumber {
    number: u64,
}

#[derive(Deserialize)]
struct AppJson {
    #[serde(default)]
    slug: Option<String>,
}

#[derive(Deserialize)]
struct CheckRunJson {
    id: u64,
    name: String,
    head_sha: String,
    status: String,
    #[serde(default)]
    conclusion: Option<String>,
    #[serde(default)]
    app: Option<AppJson>,
    #[serde(default)]
    pull_requests: Vec<PrNumber>,
}

#[derive(Deserialize)]
struct CheckRunEvent {
    check_run: CheckRunJson,
}

#[derive(Deserialize)]
struct CheckSuiteJson {
    head_sha: String,
    #[serde(default)]
    pull_requests: Vec<PrNumber>,
}

#[derive(Deserialize)]
struct CheckSuiteEvent {
    check_suite: CheckSuiteJson,
}

#[derive(Deserialize)]
struct StatusEvent {
    sha: String,
    context: String,
    state: String,
}

#[derive(Deserialize)]
struct IssueJson {
    number: u64,
    #[serde(default)]
    pull_request: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct CommentJson {
    id: u64,
    user: UserJson,
    #[serde(default)]
    body: String,
    #[serde(default)]
    updated_at: String,
}

#[derive(Deserialize)]
struct IssueCommentEvent {
    issue: IssueJson,
    #[serde(default)]
    comment: Option<serde_json::Value>,
    #[serde(default)]
    sender: Option<UserJson>,
}

#[derive(Deserialize)]
struct PushEvent {
    #[serde(rename = "ref")]
    ref_name: String,
    #[serde(default)]
    after: String,
    #[serde(default)]
    deleted: bool,
}

fn pull_request_of(p: &PrJson) -> PullRequest {
    let mut pr = PullRequest::new(
        p.number,
        &p.user.login,
        &p.base.ref_name,
        &p.head.ref_name,
        &p.head.sha,
    );
    pr.title = p.title.clone();
    pr.draft = p.draft;
    pr.closed = p.state == "closed";
    pr.merged = p.merged;
    pr.labels = p.labels.iter().map(|l| l.name.clone()).collect();
    pr
}

/// Parses a delivery. Unknown events parse to no hints (and are acknowledged).
pub fn parse(event_name: &str, body: &[u8]) -> Result<Parsed, String> {
    let common: Common = serde_json::from_slice(body).map_err(|e| format!("not JSON: {e}"))?;
    let action = common.action.clone();
    let mut parsed = Parsed {
        event_name: event_name.to_string(),
        action: action.clone(),
        repo: common.repository.map(|r| r.full_name),
        installation: common.installation.map(|i| i.id),
        event: None,
        hints: vec![],
        summary: format!(
            "{event_name}{}",
            action
                .as_deref()
                .map(|a| format!(".{a}"))
                .unwrap_or_default()
        ),
    };
    let de = |e: serde_json::Error| format!("{event_name}: {e}");
    match event_name {
        "pull_request" => {
            let ev: PullRequestEvent = serde_json::from_slice(body).map_err(de)?;
            let n = ev.number;
            let p = &ev.pull_request;
            parsed.summary = format!("{} #{n}", parsed.summary);
            parsed.event = match action.as_deref().unwrap_or("") {
                "opened" | "reopened" => Some(Event::PullRequestOpened {
                    pull_request: Box::new(pull_request_of(p)),
                }),
                "synchronize" => Some(Event::PullRequestSynchronized {
                    number: n,
                    head_sha: p.head.sha.clone(),
                    files: vec![],
                }),
                "closed" => Some(Event::Closed { number: n }),
                "labeled" => ev.label.as_ref().map(|l| Event::Labeled {
                    number: n,
                    label: l.name.clone(),
                }),
                "unlabeled" => ev.label.as_ref().map(|l| Event::Unlabeled {
                    number: n,
                    label: l.name.clone(),
                }),
                "ready_for_review" => Some(Event::ReadyForReview { number: n }),
                "converted_to_draft" => Some(Event::ConvertedToDraft { number: n }),
                // `edited` (title, body, base), assignments, reviews: only the read tells.
                _ => None,
            };
            parsed.hints.push(Hint::PullRequest(n));
            // A merge moves the base; read it too.
            if p.merged {
                parsed.hints.push(Hint::Branch(p.base.ref_name.clone()));
            }
        }
        "check_run" => {
            let ev: CheckRunEvent = serde_json::from_slice(body).map_err(de)?;
            let c = &ev.check_run;
            parsed.summary = format!("{} `{}` on {}", parsed.summary, c.name, short(&c.head_sha));
            if let Some(first) = c.pull_requests.first() {
                parsed.event = Some(Event::CheckRun {
                    number: first.number,
                    run: CheckRun {
                        name: c.name.clone(),
                        app: c.app.as_ref().and_then(|a| a.slug.clone()),
                        head_sha: c.head_sha.clone(),
                        conclusion: conclusion(&c.status, c.conclusion.as_deref()),
                        seq: c.id,
                        ..Default::default()
                    },
                });
            }
            parsed
                .hints
                .extend(c.pull_requests.iter().map(|p| Hint::PullRequest(p.number)));
            parsed.hints.push(Hint::Sha(c.head_sha.clone()));
        }
        "check_suite" => {
            let ev: CheckSuiteEvent = serde_json::from_slice(body).map_err(de)?;
            let s = &ev.check_suite;
            parsed
                .hints
                .extend(s.pull_requests.iter().map(|p| Hint::PullRequest(p.number)));
            parsed.hints.push(Hint::Sha(s.head_sha.clone()));
        }
        "status" => {
            let ev: StatusEvent = serde_json::from_slice(body).map_err(de)?;
            parsed.summary = format!("status `{}` {} on {}", ev.context, ev.state, short(&ev.sha));
            parsed.hints.push(Hint::Sha(ev.sha.clone()));
        }
        "issue_comment" => {
            let ev: IssueCommentEvent = serde_json::from_slice(body).map_err(de)?;
            // A comment on a pull request: the pull request is read, and a new or edited
            // comment goes to the engine (commands, ticked checkboxes).
            if ev.issue.pull_request.is_some() {
                let n = ev.issue.number;
                parsed.hints.push(Hint::PullRequest(n));
                parsed.summary = format!("{} #{n}", parsed.summary);
                let comment = ev
                    .comment
                    .clone()
                    .and_then(|c| serde_json::from_value::<CommentJson>(c).ok());
                if let (Some(c), Some("created" | "edited")) = (&comment, action.as_deref()) {
                    let sender = ev
                        .sender
                        .as_ref()
                        .map(|s| s.login.clone())
                        .unwrap_or_else(|| c.user.login.clone());
                    parsed.hints.push(Hint::Comment(Box::new(CommentHint {
                        id: c.id,
                        number: n,
                        author: c.user.login.clone(),
                        sender,
                        body: c.body.clone(),
                        edited: action.as_deref() == Some("edited"),
                        version: c.updated_at.clone(),
                    })));
                }
            }
        }
        "push" => {
            let ev: PushEvent = serde_json::from_slice(body).map_err(de)?;
            if let Some(branch) = ev.ref_name.strip_prefix("refs/heads/") {
                parsed.summary = format!(
                    "push to {branch} {}",
                    if ev.deleted {
                        "(deleted)".to_string()
                    } else {
                        short(&ev.after).to_string()
                    }
                );
                parsed.hints.push(Hint::Branch(branch.to_string()));
            }
        }
        "pull_request_review" => {
            // Approvals feed the `approved-by` trailer: read the pull request again.
            #[derive(Deserialize)]
            struct ReviewEvent {
                pull_request: PrNumber,
            }
            let ev: ReviewEvent = serde_json::from_slice(body).map_err(de)?;
            parsed.hints.push(Hint::PullRequest(ev.pull_request.number));
        }
        "branch_protection_rule"
        | "repository_ruleset"
        | "installation"
        | "installation_repositories" => parsed.hints.push(Hint::Full),
        _ => {}
    }
    Ok(parsed)
}

fn short(sha: &str) -> &str {
    &sha[..sha.len().min(7)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn signature_is_checked_in_constant_time_against_the_body() {
        let body = br#"{"zen":"Keep it logically awesome."}"#;
        let header = sign(b"s3cret", body);
        assert!(header.starts_with("sha256="));
        assert!(verify_signature(b"s3cret", body, Some(&header)));
        assert!(!verify_signature(b"other", body, Some(&header)));
        assert!(!verify_signature(b"s3cret", b"{}", Some(&header)));
        assert!(!verify_signature(b"s3cret", body, None));
        assert!(!verify_signature(b"s3cret", body, Some("sha1=abc")));
        assert!(!verify_signature(b"s3cret", body, Some("sha256=zz")));
        // GitHub's documented test vector.
        assert_eq!(
            sign(b"It's a Secret to Everybody", b"Hello, World!"),
            "sha256=757107ea0eb2509fc211221cce984b8a37570b6d7586c22c46f4379c8b043e17"
        );
    }

    #[test]
    fn dedup_remembers_a_bounded_window() {
        let mut d = Dedup::new(2);
        assert!(!d.check("a"));
        assert!(d.check("a"));
        assert!(!d.check("b"));
        assert!(!d.check("c"));
        // `a` fell out of the window.
        assert!(!d.check("a"));
    }

    fn pr_payload(action: &str, extra: serde_json::Value) -> Vec<u8> {
        let mut v = json!({
            "action": action,
            "number": 7,
            "repository": {"full_name": "karma-works/arzt-agent"},
            "installation": {"id": 42},
            "pull_request": {
                "number": 7, "title": "t", "draft": false, "state": "open", "merged": false,
                "user": {"login": "arztagent-bot"},
                "head": {"ref": "feature", "sha": "aaaaaaaaaa"},
                "base": {"ref": "main", "sha": "bbbbbbbbbb"},
                "labels": [{"name": "x"}]
            }
        });
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
            for (k, val) in e {
                o.insert(k.clone(), val.clone());
            }
        }
        serde_json::to_vec(&v).unwrap()
    }

    #[test]
    fn pull_request_actions_map_to_core_events_and_a_read() {
        for (action, want) in [
            ("opened", "pull_request_opened"),
            ("reopened", "pull_request_opened"),
            ("synchronize", "pull_request_synchronized"),
            ("closed", "closed"),
            ("ready_for_review", "ready_for_review"),
            ("converted_to_draft", "converted_to_draft"),
        ] {
            let p = parse("pull_request", &pr_payload(action, json!({}))).unwrap();
            let ev = serde_json::to_value(p.event.as_ref().unwrap()).unwrap();
            assert_eq!(ev["type"], want, "{action}");
            assert_eq!(p.hints, vec![Hint::PullRequest(7)]);
            assert_eq!(p.repo.as_deref(), Some("karma-works/arzt-agent"));
            assert_eq!(p.installation, Some(42));
        }
        let p = parse(
            "pull_request",
            &pr_payload("labeled", json!({"label": {"name": "halt"}})),
        )
        .unwrap();
        assert!(
            matches!(p.event, Some(Event::Labeled { number: 7, ref label }) if label == "halt")
        );
        let p = parse(
            "pull_request",
            &pr_payload("unlabeled", json!({"label": {"name": "halt"}})),
        )
        .unwrap();
        assert!(matches!(p.event, Some(Event::Unlabeled { .. })));
        let p = parse("pull_request", &pr_payload("edited", json!({}))).unwrap();
        assert!(p.event.is_none());
        assert_eq!(p.hints, vec![Hint::PullRequest(7)]);
    }

    #[test]
    fn checks_statuses_pushes_comments_and_rulesets() {
        let body = json!({
            "action": "completed",
            "repository": {"full_name": "o/r"},
            "check_run": {"id": 9, "name": "ci-ok", "head_sha": "cafe", "status": "completed",
                          "conclusion": "success", "app": {"slug": "github-actions"},
                          "pull_requests": [{"number": 3}]}
        });
        let p = parse("check_run", body.to_string().as_bytes()).unwrap();
        match p.event {
            Some(Event::CheckRun { number: 3, run }) => {
                assert_eq!(run.conclusion, Some(Conclusion::Success));
                assert_eq!(run.seq, 9);
                assert_eq!(run.app.as_deref(), Some("github-actions"));
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(
            p.hints,
            vec![Hint::PullRequest(3), Hint::Sha("cafe".into())]
        );

        let body = json!({"action": "completed", "check_suite": {"head_sha": "beef", "pull_requests": []}});
        let p = parse("check_suite", body.to_string().as_bytes()).unwrap();
        assert_eq!(p.hints, vec![Hint::Sha("beef".into())]);

        let body = json!({"sha": "f00d", "context": "ci/legacy", "state": "failure", "branches": [{"name": "x"}]});
        let p = parse("status", body.to_string().as_bytes()).unwrap();
        assert_eq!(p.hints, vec![Hint::Sha("f00d".into())]);

        let body = json!({"ref": "refs/heads/main", "after": "abc", "deleted": false});
        let p = parse("push", body.to_string().as_bytes()).unwrap();
        assert_eq!(p.hints, vec![Hint::Branch("main".into())]);
        let body = json!({"ref": "refs/tags/v1", "after": "abc"});
        assert!(
            parse("push", body.to_string().as_bytes())
                .unwrap()
                .hints
                .is_empty()
        );

        let body = json!({"action": "created", "issue": {"number": 5, "pull_request": {}}, "comment": {"body": "@schlange queue"}});
        let p = parse("issue_comment", body.to_string().as_bytes()).unwrap();
        assert_eq!(
            p.hints,
            vec![Hint::PullRequest(5)],
            "an incomplete comment is only a hint"
        );
        let body = json!({"action": "edited", "issue": {"number": 5, "pull_request": {}},
            "comment": {"id": 77, "user": {"login": "schlange[bot]"}, "body": "- [x] box", "updated_at": "2026-10-27T10:00:00Z"},
            "sender": {"login": "alice"}});
        let p = parse("issue_comment", body.to_string().as_bytes()).unwrap();
        assert_eq!(
            p.hints,
            vec![
                Hint::PullRequest(5),
                Hint::Comment(Box::new(CommentHint {
                    id: 77,
                    number: 5,
                    author: "schlange[bot]".into(),
                    sender: "alice".into(),
                    body: "- [x] box".into(),
                    edited: true,
                    version: "2026-10-27T10:00:00Z".into(),
                }))
            ]
        );
        let body = json!({"action": "created", "issue": {"number": 6}});
        assert!(
            parse("issue_comment", body.to_string().as_bytes())
                .unwrap()
                .hints
                .is_empty()
        );

        for e in ["branch_protection_rule", "repository_ruleset"] {
            let p = parse(e, br#"{"action":"edited"}"#).unwrap();
            assert_eq!(p.hints, vec![Hint::Full]);
        }
        assert!(parse("ping", br#"{"zen":"x"}"#).unwrap().hints.is_empty());
        assert!(parse("pull_request", b"not json").is_err());
    }

    #[test]
    fn conclusions() {
        assert_eq!(conclusion("in_progress", None), None);
        assert_eq!(
            conclusion("completed", Some("timed_out")),
            Some(Conclusion::TimedOut)
        );
        assert_eq!(conclusion("completed", Some("weird")), None);
        assert_eq!(status_conclusion("error"), Some(Conclusion::Failure));
        assert_eq!(status_conclusion("pending"), None);
    }
}
