//! A scripted other merge queue on the fake GitHub: "Mergify" as Schlange's shadow mode sees it
//! (S-SHADOW). Tests drive it step by step, so a shadow report can be checked against what
//! this queue did. Nothing here is Mergify's code or text; it is the behaviour observable on
//! GitHub, in our words and with our own titles:
//!
//! - **queue** ([`FakeGithub::mergify_queue`]): its check run (app `mergify`, name
//!   [`MERGIFY_QUEUE_CHECK`]) appears on the pull request's head, in progress;
//! - **embark** ([`FakeGithub::mergify_embark`]): a branch under the queue prefix with the
//!   pull requests merged onto the base tip, and a draft pull request by `mergify[bot]` whose
//!   body lists the embarked pull requests (`#n`, one per line); the CI rules run on it;
//! - **land** ([`FakeGithub::mergify_land`]): every embarked pull request is squash-merged in
//!   order by `mergify[bot]`, its check run concludes `success`, the draft is closed and its
//!   branch deleted;
//! - **dequeue** ([`FakeGithub::mergify_dequeue`]): its check run concludes `failure` with the
//!   dequeue code in the title (`Left the queue: CODE`); a draft that carried it is closed.
//!
//! Its merges count as external commits for the fake's invariant checker (they are not
//! Schlange's landings).

use serde_json::json;

use crate::FakeGithub;
use schlange_forge::git::Sha;

/// The other queue's bot login.
pub const MERGIFY_LOGIN: &str = "mergify[bot]";
/// The other queue's app slug (its check runs).
pub const MERGIFY_APP: &str = "mergify";
/// The name of its check run on queued pull requests (ours, not Mergify's text).
pub const MERGIFY_QUEUE_CHECK: &str = "Queue: merge queue (other)";

impl FakeGithub {
    /// The other queue takes pull request `n` into its queue (its check run appears).
    pub fn mergify_queue(&self, n: u64) {
        self.with(|s| {
            let Some(pr) = s.prs.get(&n).cloned() else {
                return;
            };
            s.add_run(&pr.head_sha, MERGIFY_QUEUE_CHECK, None, MERGIFY_APP);
        });
    }

    /// The other queue embarks these pull requests, in order, in one draft pull request on a
    /// queue branch; returns the draft's number.
    pub fn mergify_embark(&self, prs: &[u64]) -> u64 {
        self.with(|s| {
            let base = s.cfg.default_branch.clone();
            let tip = s.tip(&base).expect("base");
            let mut head: Sha = tip.clone();
            for n in prs {
                let pr = s.prs.get(n).cloned().expect("pr");
                if !s
                    .runs
                    .iter()
                    .any(|r| r.head_sha == pr.head_sha && r.app_slug == MERGIFY_APP)
                {
                    s.add_run(&pr.head_sha, MERGIFY_QUEUE_CHECK, None, MERGIFY_APP);
                }
                head = s
                    .repo
                    .merge(&head, &pr.head_sha, &format!("Merge #{n} into the queue"))
                    .expect("the other queue embarks pull requests that merge");
            }
            s.next_id += 1;
            let branch = format!("{}{:010x}", s.cfg.queue_prefix, s.next_id);
            s.repo.create_ref(&branch, &head).expect("new queue branch");
            s.external.insert(head.clone());
            s.emit_push(
                &branch,
                "0000000000000000000000000000000000000000",
                Some(&head),
            );
            s.next_issue += 1;
            let d = s.next_issue;
            let list: Vec<String> = prs.iter().map(|n| format!("#{n}")).collect();
            let created_at = s.server_clock;
            s.prs.insert(
                d,
                crate::FakePr {
                    number: d,
                    created_at,
                    title: format!("other queue: {}", list.join(", ")),
                    body: format!("Embarked pull requests:\n{}\n", list.join("\n")),
                    user: MERGIFY_LOGIN.into(),
                    head_ref: branch,
                    head_sha: head,
                    base,
                    draft: true,
                    open: true,
                    merged: false,
                    merge_commit_sha: None,
                    labels: Default::default(),
                    fork: false,
                    reviews: vec![],
                    assignees: vec![],
                    requested_reviewers: vec![],
                    requested_teams: vec![],
                },
            );
            s.emit_pr("opened", d, json!({}));
            s.run_ci(d);
            d
        })
    }

    /// The other queue lands its draft `draft`: each embarked pull request is squash-merged in
    /// order; the draft is closed and its branch deleted. Returns the squash commits.
    pub fn mergify_land(&self, draft: u64) -> Vec<Sha> {
        self.with(|s| {
            let d = s.prs.get(&draft).cloned().expect("draft");
            let numbers: Vec<u64> = d
                .body
                .lines()
                .filter_map(|l| l.trim().strip_prefix('#'))
                .filter_map(|n| n.parse().ok())
                .collect();
            let mut out = Vec::new();
            for n in numbers {
                let pr = s.prs.get(&n).cloned().expect("pr");
                let tip = s.tip(&pr.base).expect("base");
                let c = s
                    .repo
                    .squash(&tip, &pr.head_sha, &format!("{} (#{n})", pr.title))
                    .expect("squash");
                s.repo.update_ref(&pr.base, &tip, &c).expect("cas");
                s.external.insert(c.clone());
                s.comment_clock += 1;
                let at = s.comment_clock;
                s.merged_info.insert(n, (MERGIFY_LOGIN.to_string(), at));
                let p = s.prs.get_mut(&n).expect("pr");
                p.merged = true;
                p.open = false;
                p.merge_commit_sha = Some(c.clone());
                s.emit_push(&pr.base, &tip, Some(&c));
                s.emit_pr("closed", n, json!({}));
                conclude(s, &pr.head_sha, "success", "Merged by the other queue");
                out.push(c);
            }
            close_draft(s, draft);
            out
        })
    }

    /// The other queue drops pull request `n` with this dequeue code.
    pub fn mergify_dequeue(&self, n: u64, code: &str) {
        self.with(|s| {
            let Some(pr) = s.prs.get(&n).cloned() else {
                return;
            };
            conclude(
                s,
                &pr.head_sha,
                "failure",
                &format!("Left the queue: {code}"),
            );
            let drafts: Vec<u64> = s
                .prs
                .values()
                .filter(|d| {
                    d.open
                        && d.user == MERGIFY_LOGIN
                        && d.body.lines().any(|l| l.trim() == format!("#{n}"))
                })
                .map(|d| d.number)
                .collect();
            for d in drafts {
                close_draft(s, d);
            }
        });
    }
}

fn conclude(s: &mut crate::St, sha: &str, conclusion: &str, title: &str) {
    let ids: Vec<u64> = s
        .runs
        .iter()
        .filter(|r| r.head_sha == sha && r.app_slug == MERGIFY_APP && r.status != "completed")
        .map(|r| r.id)
        .collect();
    for id in ids {
        if let Some(r) = s.runs.iter_mut().find(|r| r.id == id) {
            r.status = "completed".into();
            r.conclusion = Some(conclusion.into());
            r.output_title = title.into();
        }
        s.emit_run("completed", id);
    }
}

fn close_draft(s: &mut crate::St, draft: u64) {
    let Some(d) = s.prs.get(&draft).cloned() else {
        return;
    };
    if d.open {
        s.prs.get_mut(&draft).expect("draft").open = false;
        s.emit_pr("closed", draft, json!({}));
    }
    if let Some(tip) = s.tip(&d.head_ref) {
        s.repo.delete_ref(&d.head_ref);
        s.emit_push(&d.head_ref, &tip, None);
    }
}
