//! The actions of `pull_request_rules` (docs: workflow, workflow/rule-syntax, workflow/actions),
//! decided without I/O. The forge adapter carries out each [`Op`] and answers with
//! [`crate::Event::ActionDone`].
//!
//! Evaluation, after every event, for every pull request that is not one of the queue's own:
//!
//! - **Edge-triggered** actions (`comment`, `close`, `delete_head_branch`, `merge`, `copy`,
//!   `backport`, `github_actions`) run once when their rule starts to match and again only
//!   after it stopped matching (`fired`). `backport` and `delete_head_branch` that match on an
//!   open pull request wait (`armed`) for the merge or the close.
//! - **Level-triggered** actions (`assign`, `dismiss_reviews`, `edit`, `label`, `post_check`,
//!   `rebase`, `request_reviews`, `review`, `squash`, `update`) compare what the rule wants with
//!   what the forge shows and ask only for the difference. A request is not repeated while the
//!   same difference is outstanding (`asked`), so repeated events never duplicate labels or
//!   reviews; when someone undoes the change, the difference comes back and is asked again
//!   (the docs' "puts it back").
//! - `close`, `delete_head_branch`, `merge`, `rebase`, `squash` and `update` run at most once
//!   per evaluation of a pull request: the first matching rule in the file wins.
//! - Labels: within one evaluation the first rule that adds or removes a label owns it; a later
//!   rule's opposite change of the same label is dropped (no add/remove loop between rules).
//! - Rules are not evaluated on the queue's draft pull requests, and not on a pull request
//!   that the engine never saw open (a first read of an old closed pull request runs nothing).
//!
//! Crash safety (journal): every request gets an id and, for edge-triggered actions and
//! commands, stays in `pending` until the forge answers. `fired` and `pending` are part of the
//! state that is journaled before the side effect, so a crash never decides an edge action
//! twice; after a restart the pending requests are sent again, and every operation is
//! idempotent at the forge (comments and reviews carry a marker the adapter looks for, label
//! and assignee changes are set operations, updates carry the head they expect, a copy reuses
//! its branch). Workflow dispatches cannot be made idempotent and are not sent again (at most
//! once). Level-triggered requests are simply re-decided from the fresh read after a restart.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use schlange_config::actions::{CopyKind, CopySpec, Dismiss, RuleAction, Trigger};
use schlange_config::conditions::{Atom, Condition};
use schlange_config::rules::{PullRequestRule, Rules};

use crate::commands::Name;
use crate::cosmetics::{self, Cosmetic};
use crate::engine::{Decision, State, context, is_queue_pull_request};
use crate::eval;
use crate::model::{Comment, PullRequest, Time};
use crate::template::{self, TValue, Vars};

/// How long the engine waits for the answer to an edge-triggered request before it sends it
/// again (lost replies), and how often.
pub const ACTION_TIMEOUT: Time = 600;
pub const ACTION_ATTEMPTS: u32 = 3;

/// Marker in the body of the comments and reviews the actions post: the adapter skips a post
/// whose marker is already on the pull request.
pub fn marker(key: &str) -> String {
    format!("<!-- schlange-action {key} -->")
}

/// One operation on the forge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    Labels {
        number: u64,
        add: Vec<String>,
        remove: Vec<String>,
    },
    /// A comment; `body` ends with a [`marker`].
    Comment {
        number: u64,
        body: String,
    },
    Assign {
        number: u64,
        add: Vec<String>,
        remove: Vec<String>,
    },
    RequestReviews {
        number: u64,
        users: Vec<String>,
        teams: Vec<String>,
    },
    /// A review by the app; `event` is `APPROVE`, `REQUEST_CHANGES` or `COMMENT`.
    Review {
        number: u64,
        event: String,
        body: String,
    },
    DismissReviews {
        number: u64,
        reviews: Vec<u64>,
        message: String,
    },
    Close {
        number: u64,
    },
    SetDraft {
        number: u64,
        draft: bool,
    },
    /// Delete the head branch of a closed pull request.
    DeleteBranch {
        number: u64,
        branch: String,
    },
    /// A check run on the head (`post_check`, copy reports). `conclusion`: `success`,
    /// `failure` or `neutral`.
    PostCheck {
        number: u64,
        head_sha: String,
        name: String,
        conclusion: String,
        title: String,
        summary: String,
    },
    /// Merge the base into the head, if the head is still `head_sha`.
    Update {
        number: u64,
        head_sha: String,
    },
    /// Rebase the head onto the base, if the head is still `head_sha`.
    Rebase {
        number: u64,
        head_sha: String,
    },
    /// Replace the commits with one, if the head is still `head_sha`.
    Squash {
        number: u64,
        head_sha: String,
        message: String,
    },
    /// Cherry-pick the pull request onto other branches and open a pull request for each.
    /// `source`: the merge commit (backport of a merged pull request), else the pull request's
    /// changes against its base. `title` and `body` still hold `{{ destination_branch }}`,
    /// `{{ cherry_pick_error }}` and `{{ cherry_picked_commits }}`, which the adapter fills in
    /// per destination with [`template::simple`].
    Copy {
        number: u64,
        kind: CopyKind,
        branches: Vec<String>,
        regexes: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        source: Option<String>,
        head_sha: String,
        title: String,
        body: String,
        labels: Vec<String>,
        assignees: Vec<String>,
        ignore_conflicts: bool,
        label_conflicts: String,
    },
    /// Merge the pull request directly (action `merge`), if the head is still `head_sha`.
    Merge {
        number: u64,
        head_sha: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        method: Option<String>,
    },
    /// Start a GitHub Actions workflow (`workflow_dispatch`).
    Dispatch {
        number: u64,
        workflow: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reference: Option<String>,
        inputs: BTreeMap<String, String>,
    },
}

impl Op {
    pub fn number(&self) -> u64 {
        match self {
            Op::Labels { number, .. }
            | Op::Comment { number, .. }
            | Op::Assign { number, .. }
            | Op::RequestReviews { number, .. }
            | Op::Review { number, .. }
            | Op::DismissReviews { number, .. }
            | Op::Close { number }
            | Op::SetDraft { number, .. }
            | Op::DeleteBranch { number, .. }
            | Op::PostCheck { number, .. }
            | Op::Update { number, .. }
            | Op::Rebase { number, .. }
            | Op::Squash { number, .. }
            | Op::Copy { number, .. }
            | Op::Merge { number, .. }
            | Op::Dispatch { number, .. } => *number,
        }
    }

    /// One line for logs.
    pub fn describe(&self) -> String {
        let list =
            |v: &[String], sign: &str| v.iter().map(|x| format!("{sign}{x}")).collect::<Vec<_>>();
        match self {
            Op::Labels {
                number,
                add,
                remove,
            } => {
                let mut p = list(add, "+");
                p.extend(list(remove, "-"));
                format!("label #{number} {}", p.join(" "))
            }
            Op::Comment { number, body } => {
                format!("comment on #{number}: {}", first_line(body))
            }
            Op::Assign {
                number,
                add,
                remove,
            } => {
                let mut p = list(add, "+");
                p.extend(list(remove, "-"));
                format!("assign #{number} {}", p.join(" "))
            }
            Op::RequestReviews {
                number,
                users,
                teams,
            } => {
                let mut p = users.clone();
                p.extend(teams.iter().map(|t| format!("team {t}")));
                format!("request reviews on #{number} from {}", p.join(", "))
            }
            Op::Review {
                number,
                event,
                body,
            } => {
                format!("review #{number}: {event} {}", first_line(body))
            }
            Op::DismissReviews {
                number, reviews, ..
            } => {
                format!("dismiss {} review(s) on #{number}", reviews.len())
            }
            Op::Close { number } => format!("close #{number}"),
            Op::SetDraft { number, draft } => {
                format!(
                    "mark #{number} {}",
                    if *draft {
                        "as draft"
                    } else {
                        "ready for review"
                    }
                )
            }
            Op::DeleteBranch { number, branch } => {
                format!("delete head branch {branch} of #{number}")
            }
            Op::PostCheck {
                number,
                name,
                conclusion,
                title,
                ..
            } => {
                format!("check `{name}` on #{number}: {conclusion} ({title})")
            }
            Op::Update { number, .. } => format!("update #{number} with its base"),
            Op::Rebase { number, .. } => format!("rebase #{number} onto its base"),
            Op::Squash { number, .. } => format!("squash the commits of #{number}"),
            Op::Copy {
                number,
                kind,
                branches,
                regexes,
                ..
            } => {
                let mut to = branches.clone();
                to.extend(regexes.iter().map(|r| format!("/{r}/")));
                format!("{} #{number} to {}", kind.as_str(), to.join(", "))
            }
            Op::Merge { number, method, .. } => format!(
                "merge #{number} directly{}",
                method
                    .as_ref()
                    .map(|m| format!(" ({m})"))
                    .unwrap_or_default()
            ),
            Op::Dispatch {
                number, workflow, ..
            } => {
                format!("dispatch workflow {workflow} for #{number}")
            }
        }
    }
}

fn first_line(body: &str) -> String {
    body.lines()
        .map(|l| match l.find("<!--") {
            Some(i) => l[..i].trim(),
            None => l.trim(),
        })
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_string()
}

/// One destination of a copy, as the forge carried it out.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyOutcome {
    pub destination: String,
    /// The pull request created (or found from an earlier attempt).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub conflicts: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The forge's answer to an [`Op`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum ActionResult {
    /// Done; `head_sha`: the new head after an update, rebase or squash.
    Done {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        head_sha: Option<String>,
    },
    /// Nothing to do (already in that state, the head moved, …).
    Skipped {
        message: String,
    },
    /// The forge could not do it (conflict, permission, …).
    Refused {
        message: String,
    },
    Copied {
        copies: Vec<CopyOutcome>,
    },
}

/// Where a command's answer goes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandReply {
    pub comment: u64,
    pub sender: String,
    pub command: String,
}

/// How a copy reports its result on the original pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CopyReport {
    pub check: bool,
    pub comment: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingAction {
    pub rule: String,
    pub op: Op,
    /// The `fired`/`asked` key, for level-triggered requests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply: Option<CommandReply>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub report: Option<CopyReport>,
    pub sent: Time,
    pub attempts: u32,
    /// The `fired` key of an edge-triggered request that is decided again (not resent) when
    /// it fails or is in doubt (`merge`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub edge: Option<String>,
}

/// Whether a request in doubt may simply be sent again. Writes that change code (a merge onto
/// the base, a rewritten head) never are: the engine decides them again on a fresh read. A
/// dispatch cannot be deduplicated.
pub fn resendable(op: &Op) -> bool {
    !matches!(
        op,
        Op::Merge { .. }
            | Op::Update { .. }
            | Op::Rebase { .. }
            | Op::Squash { .. }
            | Op::Dispatch { .. }
    )
}

/// A fresh read before a `merge` action: the engine merges only if the rule, the protections
/// and the head still hold on it (I1).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRead {
    pub number: u64,
    pub rule: String,
    pub head_sha: String,
    pub since: Time,
}

/// A `backport` command on an open pull request, waiting for the merge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitingBackport {
    pub branches: Vec<String>,
    /// The head the command vouched for: a pull request merged at another head is not
    /// backported.
    #[serde(default)]
    pub head_sha: String,
    pub reply: CommandReply,
}

/// What the actions remember between events.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct ActionsState {
    /// Edge-triggered actions that ran while their rule kept matching (`n/rule/action`).
    #[serde(default)]
    pub fired: BTreeSet<String>,
    /// `backport` and `delete_head_branch` that matched and wait for the merge or the close.
    #[serde(default)]
    pub armed: BTreeSet<String>,
    /// How often each edge action fired (part of the comment marker).
    #[serde(default)]
    pub generation: BTreeMap<String, u64>,
    /// Level-triggered requests outstanding: key → the request.
    #[serde(default)]
    pub asked: BTreeMap<String, String>,
    /// Failed level requests per key (retried up to [`ACTION_ATTEMPTS`] times).
    #[serde(default)]
    pub failures: BTreeMap<String, u32>,
    /// Requests sent and not answered, by id.
    #[serde(default)]
    pub pending: BTreeMap<u64, PendingAction>,
    /// Pull requests seen open.
    #[serde(default)]
    pub open_seen: BTreeSet<u64>,
    /// Last head seen per pull request (`dismiss_reviews` on `synchronize`).
    #[serde(default)]
    pub heads: BTreeMap<u64, String>,
    /// Pull requests whose reviews from before head `…` are to be dismissed.
    #[serde(default)]
    pub dismiss_due: BTreeMap<u64, String>,
    /// Heads the actions pushed themselves (update, rebase, squash).
    #[serde(default)]
    pub own_heads: BTreeSet<String>,
    /// `backport` commands waiting for the merge.
    #[serde(default)]
    pub waiting_backports: BTreeMap<u64, Vec<WaitingBackport>>,
    /// The last refusal per action key, for reports.
    #[serde(default)]
    pub last_error: BTreeMap<String, String>,
    /// The queue's own labels under the current rules: left out of `{{ labels }}` and never
    /// removed by `remove_all` (cosmetics are never read back).
    #[serde(default)]
    pub own_labels: BTreeSet<String>,
    /// Label changes by the actions per `n/label`: the last direction and when it reversed.
    #[serde(default)]
    pub label_flips: BTreeMap<String, LabelFlips>,
    /// Labels the actions stopped changing on a pull request (`n/label`): rules undo each
    /// other's change. Cleared when the configuration changes.
    #[serde(default)]
    pub frozen_labels: BTreeSet<String>,
    /// Fresh reads before `merge` actions, by refresh token.
    #[serde(default)]
    pub merge_reads: BTreeMap<u64, MergeRead>,
}

/// How the actions last changed one label on one pull request.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct LabelFlips {
    /// `true`: added.
    pub last: Option<bool>,
    /// When the actions reversed their own last change, within the last hour.
    pub reversals: Vec<Time>,
}

/// Reversals within an hour after which the actions stop changing a label (docs: label
/// action, conflicting rules: on the tenth reversal within an hour).
pub const LABEL_REVERSALS: usize = 10;

/// Whether the actions may change label `l` on `n` in direction `add`; records reversals and
/// freezes the label on the tenth within an hour.
fn label_change_allowed(a: &mut ActionsState, n: u64, l: &str, add: bool, now: Time) -> bool {
    let k = format!("{n}/{l}");
    if a.frozen_labels.contains(&k) {
        return false;
    }
    let f = a.label_flips.entry(k.clone()).or_default();
    if f.last == Some(!add) {
        f.reversals.retain(|t| now.saturating_sub(*t) < 3600);
        f.reversals.push(now);
        if f.reversals.len() >= LABEL_REVERSALS {
            a.frozen_labels.insert(k);
            return false;
        }
    }
    f.last = Some(add);
    true
}

/// Whether a pull request is behind its base: as the forge read it, or the base moved since
/// that read (a push to the base names only the branch).
pub fn behind(state: &State, pr: &PullRequest) -> bool {
    let base_now = state.branches.get(&pr.base);
    pr.behind_base
        || (pr.base_tip.is_some() && base_now.is_some() && pr.base_tip.as_ref() != base_now)
}

fn key(n: u64, rule: &str, action: &str) -> String {
    format!("{n}/{rule}/{action}")
}

fn short_hash(text: &str) -> String {
    let d = Sha256::digest(text.as_bytes());
    d.iter().take(6).map(|b| format!("{b:02x}")).collect()
}

/// A number in [0, 1) from text (deterministic "random" choices).
fn unit(text: &str) -> f64 {
    let d = Sha256::digest(text.as_bytes());
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[..8]);
    (u64::from_be_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

// ---- templates -------------------------------------------------------------------------

fn mentions(v: &[String]) -> String {
    v.iter()
        .map(|l| format!("@{l}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn approvers(pr: &PullRequest) -> Vec<String> {
    if !pr.approved_by.is_empty() {
        return pr.approved_by.clone();
    }
    let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
    for r in &pr.reviews {
        latest.insert(&r.author, &r.state);
    }
    latest
        .into_iter()
        .filter(|(_, s)| *s == "APPROVED")
        .map(|(a, _)| a.to_string())
        .collect()
}

/// The variables of the message templates (docs: data types, "Message template").
pub fn simple_vars(state: &State, pr: &PullRequest) -> BTreeMap<&'static str, String> {
    let reason = state
        .checks_ended
        .get(&pr.number)
        .cloned()
        .unwrap_or_default();
    [
        ("author", pr.author.clone()),
        ("number", pr.number.to_string()),
        ("title", pr.title.clone()),
        ("body", pr.body.clone()),
        ("base", pr.base.clone()),
        ("head", pr.head_ref.clone()),
        (
            "labels",
            pr.labels
                .iter()
                .filter(|l| !state.actions.own_labels.contains(*l))
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        ),
        ("assignees", mentions(&pr.assignees)),
        ("approvers", mentions(&approvers(pr))),
        ("queue_dequeue_reason", reason),
        ("merged_by", pr.merged_by.clone().unwrap_or_default()),
        ("repository_full_name", state.host.repository.clone()),
    ]
    .into()
}

/// The variables of legacy templates: the message variables and pull request attributes.
pub fn legacy_vars(state: &State, pr: &PullRequest) -> Vars {
    let mut v: Vars = simple_vars(state, pr)
        .into_iter()
        .map(|(k, s)| (k.to_string(), TValue::Str(s)))
        .collect();
    v.insert("number".into(), TValue::Num(pr.number as i64));
    v.insert(
        "label".into(),
        TValue::list(
            pr.labels
                .iter()
                .filter(|l| !state.actions.own_labels.contains(*l))
                .cloned(),
        ),
    );
    v.insert("files".into(), TValue::list(pr.files.iter().cloned()));
    v.insert(
        "assignee".into(),
        TValue::list(pr.assignees.iter().cloned()),
    );
    v.insert("approved_reviews_by".into(), TValue::list(approvers(pr)));
    v.insert("draft".into(), TValue::Bool(pr.draft));
    v.insert("closed".into(), TValue::Bool(pr.closed));
    v.insert("merged".into(), TValue::Bool(pr.merged));
    v
}

/// A condition as text, for `check_conditions`.
pub fn condition_text(c: &Condition) -> String {
    fn atom(a: &Atom) -> String {
        let mut s = String::new();
        if a.negate {
            s.push('-');
        }
        if a.length {
            s.push('#');
        }
        s.push_str(&a.attribute);
        if let (Some(op), Some(v)) = (a.operator, &a.value) {
            s.push_str(&format!(" {} {v}", op.as_str()));
        }
        s
    }
    let group = |word: &str, cs: &[Condition]| {
        format!(
            "{word}({})",
            cs.iter().map(condition_text).collect::<Vec<_>>().join(", ")
        )
    };
    match c {
        Condition::Atom(a) => atom(a),
        Condition::And(cs) => group("all of", cs),
        Condition::Or(cs) => group("any of", cs),
        Condition::Not(c) => format!("not({})", condition_text(c)),
        Condition::RequiredCheck { context, app_id } => {
            format!("required check {context} (app {app_id})")
        }
        Condition::Always => "true".into(),
    }
}

// ---- evaluation ------------------------------------------------------------------------

/// What one evaluation of one pull request has decided so far.
struct Eval<'a> {
    rules: &'a Rules,
    pr: &'a PullRequest,
    open: bool,
    /// Behind its base: as the forge read it, or the base moved since that read.
    behind: bool,
    queued: bool,
    exclusive: BTreeSet<&'static str>,
    /// Label → (added?) by the first rule that changed it.
    labels: BTreeMap<String, bool>,
}

fn send(
    state: &mut State,
    out: &mut Vec<Decision>,
    rule: &str,
    op: Op,
    level: Option<String>,
    reply: Option<CommandReply>,
    report: Option<CopyReport>,
) {
    // A request that changes what the queue's conditions read must not be overtaken by a landing
    // decided on a read that was sent before it (I1): the car verifies again afterwards.
    if matches!(
        op,
        Op::Labels { .. }
            | Op::Assign { .. }
            | Op::RequestReviews { .. }
            | Op::Review { .. }
            | Op::DismissReviews { .. }
            | Op::Close { .. }
            | Op::SetDraft { .. }
    ) {
        crate::queue::invalidate_verification(state, op.number());
    }
    let id = state.next_id();
    let now = state.now;
    state.actions.pending.insert(
        id,
        PendingAction {
            rule: rule.to_string(),
            op: op.clone(),
            level,
            reply,
            report,
            sent: now,
            attempts: 1,
            edge: None,
        },
    );
    out.push(Decision::Act {
        id,
        rule: rule.to_string(),
        op,
    });
}

/// Sends a level-triggered request unless the same one is outstanding; with `None` the rule's
/// wish is met and the memo is cleared.
fn level(state: &mut State, out: &mut Vec<Decision>, k: &str, rule: &str, op: Option<Op>) {
    let Some(op) = op else {
        state.actions.asked.remove(k);
        state.actions.failures.remove(k);
        return;
    };
    let memo = serde_json::to_string(&op).unwrap_or_default();
    if state.actions.asked.get(k) == Some(&memo) {
        return;
    }
    state.actions.asked.insert(k.to_string(), memo);
    send(state, out, rule, op, Some(k.to_string()), None, None);
}

/// Runs the actions of every rule on every pull request (after the queue's own decisions).
pub(crate) fn run(state: &mut State, rules: &Rules, token: Option<u64>, out: &mut Vec<Decision>) {
    state.actions.own_labels = cosmetics::queue_labels(rules);
    resend_overdue(state, out);
    merge_after_read(state, rules, token, out);
    if rules
        .pull_request_rules
        .iter()
        .all(|r| r.actions.is_empty())
        && state.actions.waiting_backports.is_empty()
    {
        return;
    }
    let numbers: Vec<u64> = state.pull_requests.keys().copied().collect();
    for n in numbers {
        if state.check_identity_unavailable.contains(&n) {
            continue;
        }
        let pr = state.pull_requests[&n].clone();
        if is_queue_pull_request(rules, &pr) || crate::queue::is_car_draft(state, n) {
            continue;
        }
        let open = !pr.closed && !pr.merged;
        if open {
            state.actions.open_seen.insert(n);
        } else if !state.actions.open_seen.contains(&n) {
            continue;
        }
        // `dismiss_reviews` on `synchronize`: a new head not pushed by the actions.
        match state.actions.heads.insert(n, pr.head_sha.clone()) {
            Some(old) if old != pr.head_sha && !state.actions.own_heads.contains(&pr.head_sha) => {
                state.actions.dismiss_due.insert(n, pr.head_sha.clone());
            }
            _ => {}
        }
        let behind = behind(state, &pr);
        let mut ev = Eval {
            rules,
            pr: &pr,
            open,
            behind,
            queued: state.queued.contains_key(&n),
            exclusive: BTreeSet::new(),
            labels: BTreeMap::new(),
        };
        for rule in &rules.pull_request_rules {
            if rule.actions.is_empty() {
                continue;
            }
            let ctx = context(state, n);
            let mut errors = Vec::new();
            let (matches, results) = eval::eval_list(&rule.conditions, &pr, &ctx, &mut errors);
            for action in &rule.actions {
                decide(state, rule, action, matches, &results, &mut ev, out);
            }
        }
        waiting_backport_commands(state, &pr, out);
        if state.actions.dismiss_due.get(&n) == Some(&pr.head_sha)
            && !rules.pull_request_rules.iter().any(|r| {
                r.actions
                    .iter()
                    .any(|a| matches!(a, RuleAction::DismissReviews { .. }))
            })
        {
            state.actions.dismiss_due.remove(&n);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn decide(
    state: &mut State,
    rule: &PullRequestRule,
    action: &RuleAction,
    matches: bool,
    results: &[bool],
    ev: &mut Eval,
    out: &mut Vec<Decision>,
) {
    let pr = ev.pr;
    let n = pr.number;
    let k = key(n, &rule.name, action.name());
    if action.trigger() == Trigger::Edge {
        edge(state, rule, action, matches, &k, ev, out);
        return;
    }
    let vars = simple_vars(state, pr);
    let render = |t: &str| template::simple(t, &vars);
    let op = match action {
        RuleAction::Label {
            add,
            remove,
            remove_all,
            toggle,
        } => {
            let mut want_add: Vec<String> = Vec::new();
            let mut want_remove: Vec<String> = Vec::new();
            if matches {
                want_add.extend(add.iter().chain(toggle).map(|l| render(l)));
                want_remove.extend(remove.iter().map(|l| render(l)));
                if *remove_all {
                    want_remove.extend(
                        pr.labels
                            .iter()
                            .filter(|l| !state.actions.own_labels.contains(*l))
                            .cloned(),
                    );
                }
            } else {
                want_remove.extend(toggle.iter().map(|l| render(l)));
            }
            let mut a = Vec::new();
            let mut r = Vec::new();
            for l in want_add {
                if ev.labels.get(&l) == Some(&false) || a.contains(&l) {
                    continue;
                }
                ev.labels.insert(l.clone(), true);
                if !pr.labels.contains(&l) {
                    a.push(l);
                }
            }
            for l in want_remove {
                if ev.labels.get(&l) == Some(&true) || r.contains(&l) {
                    continue;
                }
                ev.labels.insert(l.clone(), false);
                if pr.labels.contains(&l) {
                    r.push(l);
                }
            }
            let now = state.now;
            let frozen_before = state.actions.frozen_labels.len();
            let wanted: Vec<String> = a.iter().chain(r.iter()).cloned().collect();
            a.retain(|l| label_change_allowed(&mut state.actions, n, l, true, now));
            r.retain(|l| label_change_allowed(&mut state.actions, n, l, false, now));
            if state.actions.frozen_labels.len() > frozen_before {
                // Make the stop visible on the pull request (and in `/status`, which shows
                // `actions.frozen_labels`).
                for l in wanted
                    .iter()
                    .filter(|l| state.actions.frozen_labels.contains(&format!("{n}/{l}")))
                    .filter(|l| !a.contains(l) && !r.contains(l))
                {
                    cosmetics::emit(
                        out,
                        Cosmetic::Reply {
                            number: n,
                            to: None,
                            body: format!(
                                "The rule actions stopped changing the label `{l}` on this pull \
                                 request: rules kept undoing each other's change of it ({} \
                                 times within an hour). Make their conditions exclusive; the \
                                 stop is lifted when the configuration changes.",
                                LABEL_REVERSALS
                            ),
                        },
                    );
                }
            }
            (!a.is_empty() || !r.is_empty()).then_some(Op::Labels {
                number: n,
                add: a,
                remove: r,
            })
        }
        _ if !ev.open => None,
        RuleAction::PostCheck {
            title,
            summary,
            success_conditions,
            neutral_conditions,
        } => post_check(
            state,
            rule,
            pr,
            matches,
            results,
            title,
            summary,
            success_conditions.as_deref(),
            neutral_conditions.as_deref(),
        ),
        _ if !matches => None,
        RuleAction::Assign { add, remove } => {
            let add: Vec<String> = add.iter().map(|u| render(u)).collect();
            let remove: Vec<String> = remove.iter().map(|u| render(u)).collect();
            let a: Vec<String> = add
                .iter()
                .filter(|u| !u.ends_with("[bot]") && !remove.contains(u) && !u.is_empty())
                .filter(|u| !pr.assignees.contains(u))
                .cloned()
                .collect();
            let r: Vec<String> = remove
                .iter()
                .filter(|u| !add.contains(u) && pr.assignees.contains(u))
                .cloned()
                .collect();
            (!a.is_empty() || !r.is_empty()).then_some(Op::Assign {
                number: n,
                add: a,
                remove: r,
            })
        }
        RuleAction::RequestReviews {
            users,
            teams,
            random_count,
            ..
        } => request_reviews(pr, &rule.name, users, teams, *random_count),
        RuleAction::Review { kind, message } => {
            let state_name = match kind.as_str() {
                "REQUEST_CHANGES" => "CHANGES_REQUESTED",
                "COMMENT" => "COMMENTED",
                _ => "APPROVED",
            };
            let text = message.as_deref().map(&render).unwrap_or_default();
            let bot = &state.host.bot;
            let latest = pr.reviews.iter().rev().find(|r| &r.author == bot);
            let same = latest
                .is_some_and(|r| r.state == state_name && first_line(&r.body) == first_line(&text));
            (!same).then(|| {
                let text = if text.is_empty() && kind != "APPROVE" {
                    format!("Review by the rule \"{}\".", rule.name)
                } else {
                    text
                };
                Op::Review {
                    number: n,
                    event: kind.clone(),
                    body: format!("{text}\n{}", marker(&short_hash(&k))),
                }
            })
        }
        RuleAction::DismissReviews {
            approved,
            changes_requested,
            message,
            always,
        } => {
            let due = *always || state.actions.dismiss_due.get(&n) == Some(&pr.head_sha);
            let pick = |sel: &Dismiss, author: &str| match sel {
                Dismiss::None => false,
                Dismiss::All => true,
                Dismiss::Users(u) => u.iter().any(|x| x == author),
                Dismiss::FromRequestedReviewers => {
                    pr.requested_reviewers.iter().any(|x| x == author)
                }
            };
            let ids: Vec<u64> = pr
                .reviews
                .iter()
                .filter(|r| *always || r.commit.as_deref() != Some(pr.head_sha.as_str()))
                .filter(|r| match r.state.as_str() {
                    "APPROVED" => pick(approved, &r.author),
                    "CHANGES_REQUESTED" => pick(changes_requested, &r.author),
                    _ => false,
                })
                .map(|r| r.id)
                .collect();
            if !*always && due && ids.is_empty() {
                state.actions.dismiss_due.remove(&n);
            }
            (due && !ids.is_empty()).then(|| Op::DismissReviews {
                number: n,
                reviews: ids,
                message: message
                    .as_deref()
                    .map(render)
                    .unwrap_or_else(|| "Dismissed after new commits.".into()),
            })
        }
        RuleAction::Edit { draft } => draft.filter(|d| *d != pr.draft).map(|d| Op::SetDraft {
            number: n,
            draft: d,
        }),
        RuleAction::Update | RuleAction::Rebase | RuleAction::Squash { .. } => {
            // A queued pull request is the queue's; a waiting `queue` command vouches for the
            // head as it is (a rewrite would cancel it).
            if ev.queued
                || state.pending_commands.contains_key(&n)
                || ev.exclusive.contains(action.name())
            {
                None
            } else {
                let op = match action {
                    RuleAction::Update => ev.behind.then(|| Op::Update {
                        number: n,
                        head_sha: pr.head_sha.clone(),
                    }),
                    RuleAction::Rebase => (ev.behind && !pr.from_fork).then(|| Op::Rebase {
                        number: n,
                        head_sha: pr.head_sha.clone(),
                    }),
                    RuleAction::Squash { commit_message } => {
                        (pr.commit_messages.len() > 1).then(|| Op::Squash {
                            number: n,
                            head_sha: pr.head_sha.clone(),
                            message: squash_message(pr, commit_message),
                        })
                    }
                    _ => None,
                };
                if op.is_some() {
                    ev.exclusive.insert(action.name());
                }
                op
            }
        }
        _ => None,
    };
    level(state, out, &k, &rule.name, op);
}

/// The squash commit message for `commit_message` (docs: squash action).
pub fn squash_message(pr: &PullRequest, mode: &str) -> String {
    let title = format!("{} (#{})", pr.title, pr.number);
    match mode {
        "first-commit" => pr.commit_messages.first().cloned().unwrap_or(title),
        "title+body" if pr.body.trim().is_empty() => title,
        "title+body" => format!("{title}\n\n{}", pr.body.trim()),
        _ => {
            let lines: Vec<String> = pr
                .commit_messages
                .iter()
                .map(|m| format!("* {m}"))
                .collect();
            format!("{title}\n\n{}", lines.join("\n"))
        }
    }
}

fn request_reviews(
    pr: &PullRequest,
    rule: &str,
    users: &[(String, u32)],
    teams: &[(String, u32)],
    random_count: Option<u32>,
) -> Option<Op> {
    let reviewed: BTreeSet<&str> = pr.reviews.iter().map(|r| r.author.as_str()).collect();
    let involved = |u: &str| pr.requested_reviewers.iter().any(|x| x == u) || reviewed.contains(u);
    let team_involved = |t: &str| pr.requested_teams.iter().any(|x| x == t);
    // Candidates: (is team, name, weight).
    let mut pool: Vec<(bool, String, u32)> = Vec::new();
    let mut already = 0u32;
    for (u, w) in users {
        if *u == pr.author {
            continue;
        }
        if involved(u) {
            already += 1;
        } else {
            pool.push((false, u.clone(), *w));
        }
    }
    for (t, w) in teams {
        if team_involved(t) {
            already += 1;
        } else {
            pool.push((true, t.clone(), *w));
        }
    }
    if let Some(count) = random_count {
        let need = count.saturating_sub(already) as usize;
        // Weighted choice without replacement, deterministic per pull request and rule.
        pool.sort_by(|a, b| {
            let score = |c: &(bool, String, u32)| {
                unit(&format!("{}/{rule}/{}", pr.number, c.1)).powf(1.0 / c.2 as f64)
            };
            score(b)
                .partial_cmp(&score(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        pool.truncate(need);
    }
    // GitHub takes at most 15 requests per pull request, users first.
    let room = 15usize.saturating_sub(pr.requested_reviewers.len() + pr.requested_teams.len());
    pool.sort_by_key(|c| c.0);
    pool.truncate(room);
    let users: Vec<String> = pool.iter().filter(|c| !c.0).map(|c| c.1.clone()).collect();
    let teams: Vec<String> = pool.iter().filter(|c| c.0).map(|c| c.1.clone()).collect();
    (!users.is_empty() || !teams.is_empty()).then_some(Op::RequestReviews {
        number: pr.number,
        users,
        teams,
    })
}

/// The check name of a rule's `post_check` and of copy reports.
pub fn check_name(rule: &str, action: &str) -> String {
    format!("Rule: {rule} ({action})")
}

#[allow(clippy::too_many_arguments)]
fn post_check(
    state: &State,
    rule: &PullRequestRule,
    pr: &PullRequest,
    matches: bool,
    results: &[bool],
    title: &str,
    summary: &str,
    success: Option<&[Condition]>,
    neutral: Option<&[Condition]>,
) -> Option<Op> {
    // Our reading of the docs: without `success_conditions` and `neutral_conditions` the
    // rule's own conditions decide success or failure, so the check is posted on every pull
    // request; with either set, the rule's conditions choose the pull requests.
    if (success.is_some() || neutral.is_some()) && !matches {
        return None;
    }
    let ctx = context(state, pr.number);
    let mut errors = Vec::new();
    let (shown, shown_results, status) = match (success, neutral) {
        (None, None) => (
            rule.conditions.clone(),
            results.to_vec(),
            if matches { "success" } else { "failure" },
        ),
        _ => {
            let s = success.map(|c| eval::eval_list(c, pr, &ctx, &mut errors));
            let ne = neutral.map(|c| eval::eval_list(c, pr, &ctx, &mut errors));
            let status = if s.as_ref().is_some_and(|x| x.0) {
                "success"
            } else if ne.as_ref().is_some_and(|x| x.0) {
                "neutral"
            } else {
                "failure"
            };
            match (success, s) {
                (Some(c), Some((_, r))) => (c.to_vec(), r, status),
                _ => (vec![], vec![], status),
            }
        }
    };
    let conditions = shown
        .iter()
        .zip(shown_results.iter())
        .map(|(c, ok)| {
            format!(
                "- [{}] `{}`",
                if *ok { "x" } else { " " },
                condition_text(c)
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    let mut vars = legacy_vars(state, pr);
    vars.insert("check_rule_name".into(), TValue::Str(rule.name.clone()));
    vars.insert("check_status".into(), TValue::Str(status.into()));
    vars.insert("check_succeeded".into(), TValue::Bool(status == "success"));
    vars.insert("check_succeed".into(), TValue::Bool(status == "success"));
    vars.insert("check_conditions".into(), TValue::Str(conditions));
    let (title, summary) = match (
        template::legacy(title, &vars),
        template::legacy(summary, &vars),
    ) {
        (Ok(t), Ok(s)) => (t, s),
        (Err(e), _) | (_, Err(e)) => (
            format!("Rule '{}': the check's template does not render", rule.name),
            e,
        ),
    };
    Some(Op::PostCheck {
        number: pr.number,
        head_sha: pr.head_sha.clone(),
        name: check_name(&rule.name, "post_check"),
        conclusion: status.into(),
        title,
        summary,
    })
}

fn edge(
    state: &mut State,
    rule: &PullRequestRule,
    action: &RuleAction,
    matches: bool,
    k: &str,
    ev: &mut Eval,
    out: &mut Vec<Decision>,
) {
    let pr = ev.pr;
    let n = pr.number;
    let waits = matches!(
        action,
        RuleAction::Copy(CopySpec {
            kind: CopyKind::Backport,
            ..
        }) | RuleAction::DeleteHeadBranch { .. }
    );
    if !matches {
        state.actions.fired.remove(k);
        // A waiting backport or branch deletion stays armed until the close.
        if !waits || (pr.closed && !pr.merged && action.name() == "backport") {
            state.actions.armed.remove(k);
        }
        if !(waits && state.actions.armed.contains(k)) {
            return;
        }
    }
    if state.actions.fired.contains(k) {
        return;
    }
    if waits {
        let ready = match action {
            RuleAction::DeleteHeadBranch { .. } => pr.closed,
            _ => pr.merged,
        };
        if !ready {
            if ev.open && matches {
                state.actions.armed.insert(k.to_string());
            }
            return;
        }
    }
    if action.exclusive() {
        if ev.exclusive.contains(action.name()) {
            return;
        }
        ev.exclusive.insert(action.name());
    }
    let generation = {
        let g = state.actions.generation.entry(k.to_string()).or_default();
        *g += 1;
        *g
    };
    let mark = marker(&format!("{}-{generation}", short_hash(k)));
    let vars = simple_vars(state, pr);
    let render = |t: &str| template::simple(t, &vars);
    let mut ops = Vec::new();
    let mut report = None;
    match action {
        RuleAction::Comment { message } => {
            let body = render(message);
            if !body.trim().is_empty() {
                ops.push(Op::Comment {
                    number: n,
                    body: format!("{body}\n{mark}"),
                });
            }
        }
        RuleAction::Close { message } => {
            if ev.open {
                ops.push(Op::Close { number: n });
                let body = render(message);
                if !body.trim().is_empty() {
                    ops.push(Op::Comment {
                        number: n,
                        body: format!("{body}\n{mark}"),
                    });
                }
            }
        }
        RuleAction::DeleteHeadBranch { force } => {
            let used = state
                .pull_requests
                .values()
                .any(|o| o.number != n && !o.closed && o.base == pr.head_ref);
            if !pr.from_fork && (*force || !used) {
                ops.push(Op::DeleteBranch {
                    number: n,
                    branch: pr.head_ref.clone(),
                });
            }
        }
        RuleAction::Copy(spec) => {
            if let Some(op) = copy_op(state, pr, spec, &spec.branches) {
                ops.push(op);
                report = Some(CopyReport {
                    check: spec.report_check,
                    comment: spec.report_comment,
                });
            }
        }
        RuleAction::Merge { .. } => {
            // Never on the engine's view alone: read the pull request again first; the merge
            // is decided on that fresh read (`merge_after_read`).
            state
                .actions
                .generation
                .entry(k.to_string())
                .and_modify(|g| *g -= 1);
            let waiting = state.actions.merge_reads.values().any(|r| r.number == n);
            if !waiting && merge_ready(state, ev.rules, rule, pr) {
                let token = state.next_id();
                state.actions.merge_reads.insert(
                    token,
                    MergeRead {
                        number: n,
                        rule: rule.name.clone(),
                        head_sha: pr.head_sha.clone(),
                        since: state.now,
                    },
                );
                out.push(Decision::Refresh {
                    token,
                    numbers: vec![n],
                    branches: vec![pr.base.clone()],
                });
            } else {
                ev.exclusive.remove("merge");
            }
            return;
        }
        RuleAction::GithubActions { dispatch } => {
            for d in dispatch {
                ops.push(Op::Dispatch {
                    number: n,
                    workflow: d.workflow.clone(),
                    reference: d.reference.clone(),
                    inputs: d
                        .inputs
                        .iter()
                        .map(|(k, v)| {
                            let text = match v {
                                serde_json::Value::String(s) => render(s),
                                o => o.to_string(),
                            };
                            (k.clone(), text)
                        })
                        .collect(),
                });
            }
        }
        _ => {}
    }
    state.actions.fired.insert(k.to_string());
    state.actions.armed.remove(k);
    for op in ops {
        send(state, out, &rule.name, op, None, None, report.clone());
    }
}

/// The copy operation for a pull request, or `None` without destinations.
fn copy_op(state: &State, pr: &PullRequest, spec: &CopySpec, branches: &[String]) -> Option<Op> {
    let mut vars = simple_vars(state, pr);
    let branches: Vec<String> = branches
        .iter()
        .filter(|b| **b != pr.base)
        .cloned()
        .collect();
    if branches.is_empty() && spec.regexes.is_empty() {
        return None;
    }
    let assignees = spec
        .assignees
        .iter()
        .map(|a| template::simple(a, &vars))
        .filter(|a| !a.is_empty() && !a.ends_with("[bot]"))
        .collect();
    vars.remove("merged_by");
    Some(Op::Copy {
        number: pr.number,
        kind: spec.kind,
        branches,
        regexes: spec.regexes.clone(),
        source: if pr.merged {
            pr.merge_commit_sha.clone()
        } else {
            None
        },
        head_sha: pr.head_sha.clone(),
        title: template::simple(&spec.title, &vars),
        body: template::simple(&spec.body, &vars),
        labels: spec.labels.clone(),
        assignees,
        ignore_conflicts: spec.ignore_conflicts,
        label_conflicts: spec.label_conflicts.clone(),
    })
}

/// Whether the `merge` action of `rule` may merge `pr` now: the rule matches without an
/// evaluation error (a truncated file list fails closed), the pull request is open, ready,
/// not queued, no protection holds it, and neither the pause, a freeze nor a merge protection
/// holds it (stricter than the docs for the freeze, which they leave to the queue).
fn merge_ready(state: &State, rules: &Rules, rule: &PullRequestRule, pr: &PullRequest) -> bool {
    if pr.closed || pr.merged || pr.draft || pr.files_incomplete {
        return false;
    }
    if state.queued.contains_key(&pr.number)
        || state.pending_commands.contains_key(&pr.number)
        || state.holds.get(&pr.number).is_some_and(|h| !h.is_empty())
    {
        return false;
    }
    let ctx = context(state, pr.number);
    let mut errors = Vec::new();
    let (ok, _) = eval::eval_list(&rule.conditions, pr, &ctx, &mut errors);
    ok && errors.is_empty() && crate::gates::landing_hold(state, rules, pr, state.now).is_none()
}

/// The fresh read for a `merge` action arrived (`token`): merge if everything still holds on
/// it, at exactly the head the decision saw. Reads that never came back expire.
fn merge_after_read(state: &mut State, rules: &Rules, token: Option<u64>, out: &mut Vec<Decision>) {
    let now = state.now;
    state
        .actions
        .merge_reads
        .retain(|_, r| now < r.since.saturating_add(ACTION_TIMEOUT));
    let Some(read) = token.and_then(|t| state.actions.merge_reads.remove(&t)) else {
        return;
    };
    let Some(pr) = state.pull_requests.get(&read.number).cloned() else {
        return;
    };
    let Some(rule) = rules
        .pull_request_rules
        .iter()
        .find(|r| r.name == read.rule)
    else {
        return;
    };
    let Some(RuleAction::Merge { method }) = rule
        .actions
        .iter()
        .find(|a| matches!(a, RuleAction::Merge { .. }))
    else {
        return;
    };
    if pr.head_sha != read.head_sha || !merge_ready(state, rules, rule, &pr) {
        return;
    }
    let k = key(pr.number, &rule.name, "merge");
    if state.actions.fired.contains(&k) {
        return;
    }
    state.actions.fired.insert(k.clone());
    *state.actions.generation.entry(k.clone()).or_default() += 1;
    send(
        state,
        out,
        &rule.name,
        Op::Merge {
            number: pr.number,
            head_sha: pr.head_sha.clone(),
            method: method.clone(),
        },
        None,
        None,
        None,
    );
    if let Some(p) = state.actions.pending.values_mut().last() {
        p.edge = Some(k);
    }
}

/// Drops a request in doubt that is never resent: an edge request becomes undecided again, a
/// command is answered.
fn abandon(state: &mut State, p: &PendingAction, why: &str, out: &mut Vec<Decision>) {
    if let Some(k) = &p.edge {
        state.actions.fired.remove(k);
    }
    if let Some(k) = &p.level {
        state.actions.asked.remove(k);
    }
    if let Some(r) = &p.reply {
        cosmetics::emit(
            out,
            Cosmetic::Reply {
                number: p.op.number(),
                to: Some(r.comment),
                body: format!(
                    "@{}, `{}` is not confirmed ({why}); nothing is retried, comment again if \
                     it is still needed.",
                    r.sender, r.command
                ),
            },
        );
    }
}

fn waiting_backport_commands(state: &mut State, pr: &PullRequest, out: &mut Vec<Decision>) {
    if !pr.closed {
        return;
    }
    let Some(list) = state.actions.waiting_backports.remove(&pr.number) else {
        return;
    };
    for w in list {
        if pr.merged && !w.head_sha.is_empty() && pr.head_sha != w.head_sha {
            cosmetics::emit(
                out,
                Cosmetic::Reply {
                    number: pr.number,
                    to: Some(w.reply.comment),
                    body: format!(
                        "@{}, the backport is cancelled: the pull request was merged at a head \
                         ({}) the command did not vouch for.",
                        w.reply.sender,
                        crate::engine::short(&pr.head_sha)
                    ),
                },
            );
            continue;
        }
        if !pr.merged {
            cosmetics::emit(
                out,
                Cosmetic::Reply {
                    number: pr.number,
                    to: Some(w.reply.comment),
                    body: format!(
                        "@{}, the backport is cancelled: the pull request was closed without \
                         being merged.",
                        w.reply.sender
                    ),
                },
            );
            continue;
        }
        let spec = CopySpec::new(CopyKind::Backport);
        if let Some(op) = copy_op(state, pr, &spec, &w.branches) {
            let rule = format!("command `backport` by @{}", w.reply.sender);
            send(state, out, &rule, op, None, Some(w.reply), None);
        }
    }
}

// ---- answers ---------------------------------------------------------------------------

/// Requests sent long ago without an answer are sent again (lost replies); idempotent.
fn resend_overdue(state: &mut State, out: &mut Vec<Decision>) {
    let now = state.now;
    let mut wake = None;
    for (id, p) in state.actions.pending.iter_mut() {
        if state.check_identity_unavailable.contains(&p.op.number()) {
            continue;
        }
        if p.level.is_some() || !resendable(&p.op) {
            continue;
        }
        let due = p.sent.saturating_add(ACTION_TIMEOUT);
        if now >= due && p.attempts < ACTION_ATTEMPTS {
            p.attempts += 1;
            p.sent = now;
            out.push(Decision::Act {
                id: *id,
                rule: p.rule.clone(),
                op: p.op.clone(),
            });
        }
        if p.attempts < ACTION_ATTEMPTS {
            let at = p.sent.saturating_add(ACTION_TIMEOUT);
            wake = Some(wake.map_or(at, |w: Time| w.min(at)));
        }
    }
    // Give up on requests that never got an answer; a level request is decided again.
    let stale: Vec<u64> = state
        .actions
        .pending
        .iter()
        .filter(|(_, p)| {
            !state.check_identity_unavailable.contains(&p.op.number())
                && now >= p.sent.saturating_add(ACTION_TIMEOUT)
                && (p.level.is_some() || !resendable(&p.op) || p.attempts >= ACTION_ATTEMPTS)
        })
        .map(|(id, _)| *id)
        .collect();
    for id in stale {
        if let Some(p) = state.actions.pending.remove(&id) {
            abandon(state, &p, "no answer", out);
        }
    }
    if let Some(at) = wake {
        crate::queue::wake(state, at, out);
    }
}

/// The forge answered request `id`.
pub(crate) fn done(state: &mut State, id: u64, result: &ActionResult, out: &mut Vec<Decision>) {
    let Some(p) = state.actions.pending.remove(&id) else {
        return; // a duplicate or late answer
    };
    let n = p.op.number();
    match result {
        ActionResult::Done { head_sha: Some(h) } => {
            state.actions.own_heads.insert(h.clone());
        }
        ActionResult::Refused { message } => {
            if let Some(k) = &p.level {
                state.actions.last_error.insert(k.clone(), message.clone());
            }
        }
        _ => {}
    }
    if let Some(k) = &p.level
        && matches!(result, ActionResult::Done { .. })
    {
        state.actions.last_error.remove(k);
    }
    let summary = outcome_text(&p.op, result);
    if let Some(r) = &p.reply {
        cosmetics::emit(
            out,
            Cosmetic::Reply {
                number: n,
                to: Some(r.comment),
                body: format!("@{}, `{}`: {summary}", r.sender, r.command),
            },
        );
    }
    if let (Some(report), Op::Copy { kind, head_sha, .. }) = (&p.report, &p.op) {
        let failed = match result {
            ActionResult::Copied { copies } => copies.iter().any(|c| c.error.is_some()),
            ActionResult::Done { .. } => false,
            _ => true,
        };
        if report.check {
            let op = Op::PostCheck {
                number: n,
                head_sha: head_sha.clone(),
                name: check_name(&p.rule, kind.as_str()),
                conclusion: if failed { "failure" } else { "success" }.into(),
                title: format!(
                    "{} {}",
                    kind.as_str(),
                    if failed { "failed" } else { "done" }
                ),
                summary: summary.clone(),
            };
            send(state, out, &p.rule, op, None, None, None);
        }
        if report.comment {
            let op = Op::Comment {
                number: n,
                body: format!(
                    "{summary}\n{}",
                    marker(&format!(
                        "{}-report",
                        short_hash(&format!(
                            "{}/{}",
                            p.rule,
                            serde_json::to_string(&p.op).unwrap_or_default()
                        ))
                    ))
                ),
            };
            send(state, out, &p.rule, op, None, None, None);
        }
    }
}

/// A one-paragraph account of what an operation did.
pub fn outcome_text(op: &Op, result: &ActionResult) -> String {
    match result {
        ActionResult::Done { .. } => format!("done: {}.", op.describe()),
        ActionResult::Skipped { message } => format!("nothing to do: {message}."),
        ActionResult::Refused { message } => format!("not done: {message}."),
        ActionResult::Copied { copies } => {
            let kind = match op {
                Op::Copy { kind, .. } => kind.as_str(),
                _ => "copy",
            };
            if copies.is_empty() {
                return format!("no {kind}: no destination branch matched.");
            }
            copies
                .iter()
                .map(|c| match (&c.error, c.number) {
                    (Some(e), _) => format!("- `{}`: failed: {e}", c.destination),
                    (None, Some(x)) if c.conflicts => format!(
                        "- `{}`: #{x}, with conflicts to resolve by hand",
                        c.destination
                    ),
                    (None, Some(x)) => format!("- `{}`: #{x}", c.destination),
                    (None, None) => format!("- `{}`: done", c.destination),
                })
                .collect::<Vec<_>>()
                .join("\n")
        }
    }
}

/// The forge answered request `id` with an error; the effect may still have happened.
pub(crate) fn failed(state: &mut State, id: u64, error: &str, out: &mut Vec<Decision>) {
    let Some(p) = state.actions.pending.get(&id).cloned() else {
        return;
    };
    match &p.level {
        Some(k) => {
            state.actions.pending.remove(&id);
            let f = state.actions.failures.entry(k.clone()).or_default();
            *f += 1;
            if *f < ACTION_ATTEMPTS {
                state.actions.asked.remove(k);
            }
            state
                .actions
                .last_error
                .insert(k.clone(), error.to_string());
        }
        // A code-changing write (merge, head rewrite by command) is decided again, never
        // resent as is.
        None if !resendable(&p.op) => {
            state.actions.pending.remove(&id);
            abandon(state, &p, error, out);
        }
        // Other edge requests stay pending and are sent again after the timeout (idempotent).
        None => {
            let at = p.sent.saturating_add(ACTION_TIMEOUT);
            crate::queue::wake(state, at, out);
        }
    }
}

/// After a restart: level requests are re-decided from the fresh read; edge requests and
/// commands in doubt are sent again when that is idempotent; code-changing writes (merge,
/// head rewrites) are decided again, workflow dispatches are not repeated (at most once).
pub(crate) fn restarted(state: &mut State, out: &mut Vec<Decision>) {
    state.actions.asked.clear();
    state.actions.merge_reads.clear();
    let doubt: Vec<PendingAction> = state
        .actions
        .pending
        .values()
        .filter(|p| p.level.is_none() && !resendable(&p.op))
        .cloned()
        .collect();
    for p in &doubt {
        if !matches!(p.op, Op::Dispatch { .. }) {
            abandon(state, p, "the engine restarted", out);
        }
    }
    state
        .actions
        .pending
        .retain(|_, p| p.level.is_none() && resendable(&p.op));
    let now = state.now;
    let ids: Vec<u64> = state.actions.pending.keys().copied().collect();
    for id in ids {
        let p = state.actions.pending.get_mut(&id).expect("pending");
        if state.check_identity_unavailable.contains(&p.op.number()) {
            continue;
        }
        p.sent = now;
        out.push(Decision::Act {
            id,
            rule: p.rule.clone(),
            op: p.op.clone(),
        });
    }
}

// ---- commands --------------------------------------------------------------------------

/// Runs `backport`, `copy`, `rebase`, `squash` or `update` from a comment (restrictions already
/// passed). Docs: commands; the answer is a reply once the forge answered.
pub(crate) fn command(
    state: &mut State,
    c: &Comment,
    name: Name,
    args: Vec<String>,
    out: &mut Vec<Decision>,
) {
    let n = c.number;
    let pr = state.pull_requests[&n].clone();
    let reply = CommandReply {
        comment: c.id,
        sender: c.sender.clone(),
        command: name.as_str().to_string(),
    };
    let say = |out: &mut Vec<Decision>, body: String| {
        cosmetics::emit(
            out,
            Cosmetic::Reply {
                number: n,
                to: Some(c.id),
                body: format!("@{}, {body}", c.sender),
            },
        );
    };
    let rule = format!("command `{}` by @{}", name.as_str(), c.sender);
    let open = !pr.closed && !pr.merged;
    // The command vouches for the head at the comment's time (as the adapter read it); a
    // pull request that moved on since is not touched under the sender's name.
    let vouched = c.head_sha.clone().unwrap_or_else(|| pr.head_sha.clone());
    if open && vouched != pr.head_sha {
        return say(
            out,
            format!(
                "the pull request changed after the command (head {} instead of {}); nothing \
                 was done, comment again to vouch for the new head.",
                crate::engine::short(&pr.head_sha),
                crate::engine::short(&vouched)
            ),
        );
    }
    match name {
        Name::Update | Name::Rebase | Name::Squash => {
            if !open {
                return say(out, "this pull request is closed; nothing was done.".into());
            }
            if state.queued.contains_key(&n) {
                return say(
                    out,
                    "this pull request is queued; the queue tests it on top of its base, \
                     nothing was done."
                        .into(),
                );
            }
            let op = match name {
                Name::Update if !behind(state, &pr) => {
                    return say(out, "this pull request is up to date with its base.".into());
                }
                Name::Update => Op::Update {
                    number: n,
                    head_sha: pr.head_sha.clone(),
                },
                Name::Rebase if pr.from_fork => {
                    return say(
                        out,
                        "a pull request from a fork cannot be rebased; use `update`.".into(),
                    );
                }
                Name::Rebase if !behind(state, &pr) => {
                    return say(out, "this pull request is up to date with its base.".into());
                }
                Name::Rebase => Op::Rebase {
                    number: n,
                    head_sha: pr.head_sha.clone(),
                },
                _ => {
                    let mode = args
                        .first()
                        .cloned()
                        .unwrap_or_else(|| "all-commits".into());
                    if !["all-commits", "first-commit", "title+body"].contains(&mode.as_str()) {
                        return say(
                            out,
                            format!(
                                "`{mode}` is not a commit message mode (all-commits, \
                                 first-commit, title+body)."
                            ),
                        );
                    }
                    if pr.commit_messages.len() < 2 {
                        return say(
                            out,
                            "this pull request has one commit; nothing to squash.".into(),
                        );
                    }
                    Op::Squash {
                        number: n,
                        head_sha: pr.head_sha.clone(),
                        message: squash_message(&pr, &mode),
                    }
                }
            };
            send(state, out, &rule, op, None, Some(reply), None);
        }
        Name::Backport | Name::Copy => {
            if args.is_empty() {
                return say(
                    out,
                    format!("`{}` needs the branches to copy to.", name.as_str()),
                );
            }
            if name == Name::Backport && open {
                state
                    .actions
                    .waiting_backports
                    .entry(n)
                    .or_default()
                    .push(WaitingBackport {
                        branches: args.clone(),
                        head_sha: vouched.clone(),
                        reply,
                    });
                return say(
                    out,
                    format!(
                        "the pull request will be backported to {} once it is merged.",
                        args.iter()
                            .map(|b| format!("`{b}`"))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                );
            }
            if name == Name::Backport && !pr.merged {
                return say(
                    out,
                    "the pull request was closed without being merged; nothing to backport.".into(),
                );
            }
            let kind = if name == Name::Backport {
                CopyKind::Backport
            } else {
                CopyKind::Copy
            };
            let spec = CopySpec::new(kind);
            match copy_op(state, &pr, &spec, &args) {
                Some(op) => send(state, out, &rule, op, None, Some(reply), None),
                None => say(
                    out,
                    "no destination other than the base branch; nothing was done.".into(),
                ),
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn squash_messages() {
        let mut p = PullRequest::new(4, "a", "main", "f", "h");
        p.title = "Add x".into();
        p.body = "Why.\n".into();
        p.commit_messages = vec!["one".into(), "two".into()];
        assert_eq!(
            squash_message(&p, "all-commits"),
            "Add x (#4)\n\n* one\n* two"
        );
        assert_eq!(squash_message(&p, "first-commit"), "one");
        assert_eq!(squash_message(&p, "title+body"), "Add x (#4)\n\nWhy.");
    }

    #[test]
    fn random_reviewers_are_deterministic_and_count_the_involved() {
        let mut p = PullRequest::new(9, "author", "main", "f", "h");
        p.requested_reviewers = vec!["b".into()];
        let users: Vec<(String, u32)> = ["author", "a", "b", "c", "d"]
            .iter()
            .map(|u| (u.to_string(), 1))
            .collect();
        let one = request_reviews(&p, "r", &users, &[], Some(2));
        let two = request_reviews(&p, "r", &users, &[], Some(2));
        assert_eq!(one, two);
        let Some(Op::RequestReviews { users: picked, .. }) = one else {
            panic!()
        };
        assert_eq!(picked.len(), 1, "b is requested already and counts");
        assert!(!picked.contains(&"author".to_string()));
        assert_eq!(request_reviews(&p, "r", &users[..3], &[], Some(1)), None);
    }

    #[test]
    fn condition_texts() {
        let mut e = Vec::new();
        let list = schlange_config::conditions::parse_list(
            &serde_json::json!(["-label = wip", { "or": ["base = main", "#files > 2"] }]),
            "",
            &mut e,
        );
        let t: Vec<String> = list.iter().map(condition_text).collect();
        assert_eq!(t, vec!["-label = wip", "any of(base = main, #files > 2)"]);
    }
}
