//! Rule actions on GitHub: each [`Op`] as REST calls (GraphQL for the draft state), made
//! idempotent the way the engine expects (`schlange_core::actions`): a comment or review
//! whose marker is on the pull request already is not posted again, label and assignee
//! changes are set operations, head-bound operations name the head they expect, a copy whose
//! branch exists reuses its pull request.
//!
//! Without a git CLI (KONZEPT.md section 6) commits are made with the git data API:
//!
//! - `squash`: one commit with the head's tree on the merge base, then the head branch is moved
//!   (forced: the rewrite is the point of the action).
//! - `copy`/`backport`: a cherry-pick by merge. A temporary branch gets a commit with the
//!   destination's tree whose parent is the picked change's parent; merging the picked commit
//!   into it applies exactly that change; the result's tree becomes one commit on the
//!   destination, on the copy's branch. A conflict is reported (`ignore_conflicts` cannot
//!   commit conflict markers through the API, so the copy fails with the conflict instead).
//! - `rebase` is not available through the API without replaying commits one by one; Schlange
//!   refuses it on GitHub and says so (`update` merges the base in instead).

use std::collections::BTreeMap;

use schlange_core::actions::{ActionResult, CopyOutcome, Op};
use schlange_core::template;

use super::GithubForge;
use crate::api::{MergeOutcome, PullMergeOutcome};
use crate::client::{Error, Priority};

fn done() -> ActionResult {
    ActionResult::Done { head_sha: None }
}

fn skipped(m: impl Into<String>) -> ActionResult {
    ActionResult::Skipped { message: m.into() }
}

fn refused(m: impl Into<String>) -> ActionResult {
    ActionResult::Refused { message: m.into() }
}

/// The marker line of an action's body.
fn marker_of(body: &str) -> Option<&str> {
    body.lines()
        .find(|l| l.trim_start().starts_with("<!-- schlange-action "))
        .map(str::trim)
}

impl GithubForge {
    /// Carries out one rule action; API errors become `Refused` with the reason (the engine
    /// keeps an edge request pending and sends it again after a timeout).
    pub(crate) fn rule_action(&mut self, op: &Op) -> ActionResult {
        match self.try_rule_action(op) {
            Ok(r) => r,
            Err(e) => {
                self.note(format!("action failed: {}: {e}", op.describe()));
                refused(e.to_string())
            }
        }
    }

    fn try_rule_action(&mut self, op: &Op) -> Result<ActionResult, Error> {
        let prio = Priority::Normal;
        let n = op.number();
        Ok(match op {
            Op::Labels { add, remove, .. } => {
                if !add.is_empty() {
                    let add: Vec<&str> = add.iter().map(String::as_str).collect();
                    self.api.add_labels(n, &add, prio)?;
                }
                for l in remove {
                    self.api.remove_label(n, l, prio)?;
                }
                done()
            }
            Op::Comment { body, .. } => {
                if let Some(m) = marker_of(body)
                    && self
                        .api
                        .issue_comments(n, prio)?
                        .iter()
                        .any(|c| c.user.login == self.opts.bot_login && c.body.contains(m))
                {
                    return Ok(skipped("the comment is there already"));
                }
                self.api.comment(n, body, prio)?;
                done()
            }
            Op::Assign { add, remove, .. } => {
                if !add.is_empty() {
                    self.api.add_assignees(n, add, prio)?;
                }
                if !remove.is_empty() {
                    self.api.remove_assignees(n, remove, prio)?;
                }
                done()
            }
            Op::RequestReviews { users, teams, .. } => {
                self.api.request_reviewers(n, users, teams, prio)?;
                done()
            }
            Op::Review { event, body, .. } => {
                let reviews = self.api.reviews(n, prio)?;
                let latest = reviews.iter().rev().find(|r| {
                    r.user.as_ref().map(|u| u.login.as_str()) == Some(self.opts.bot_login.as_str())
                });
                if let Some(m) = marker_of(body)
                    && latest.is_some_and(|r| {
                        r.body.as_deref().is_some_and(|b| b.contains(m)) && r.state != "DISMISSED"
                    })
                {
                    return Ok(skipped("the review is there already"));
                }
                self.api.create_review(n, event, body, prio)?;
                done()
            }
            Op::DismissReviews {
                reviews, message, ..
            } => {
                for id in reviews {
                    match self.api.dismiss_review(n, *id, message, prio) {
                        // Dismissed already (or not dismissable any more).
                        Ok(()) | Err(Error::Status { status: 422, .. }) => {}
                        Err(e) => return Err(e),
                    }
                }
                done()
            }
            Op::Close { .. } => {
                self.api.close_pull(n, prio)?;
                done()
            }
            Op::SetDraft { draft, .. } => {
                let p = self.api.pull(n, prio)?;
                if p.draft == *draft {
                    return Ok(skipped("the draft state is already so"));
                }
                self.api.set_draft(&p.node_id, *draft, prio)?;
                done()
            }
            Op::DeleteBranch { branch, .. } => match self.api.delete_ref(branch, prio) {
                Ok(()) => done(),
                Err(Error::NotFound { .. }) => skipped(format!("branch {branch} does not exist")),
                Err(e) => return Err(e),
            },
            Op::PostCheck {
                head_sha,
                name,
                conclusion,
                title,
                summary,
                ..
            } => {
                self.api.create_check_run(
                    &serde_json::json!({
                        "name": name,
                        "head_sha": head_sha,
                        "status": "completed",
                        "conclusion": conclusion,
                        "output": { "title": title, "summary": summary }
                    }),
                    prio,
                )?;
                done()
            }
            Op::Update { head_sha, .. } => match self.api.update_branch(n, head_sha, prio) {
                Ok(()) => {
                    let head = self.api.pull(n, prio)?.head.sha;
                    ActionResult::Done {
                        head_sha: Some(head),
                    }
                }
                Err(Error::Status {
                    status: 422,
                    message,
                    ..
                }) => skipped(message),
                Err(e) => return Err(e),
            },
            Op::Rebase { .. } => refused(
                "rebasing needs commits replayed one by one, which the GitHub API does not offer; \
                 use `update` instead",
            ),
            Op::Squash {
                head_sha, message, ..
            } => {
                let p = self.api.pull(n, prio)?;
                if p.head.sha != *head_sha {
                    return Ok(skipped("the head moved"));
                }
                if p.head.repo.as_ref().map(|r| &r.full_name)
                    != p.base.repo.as_ref().map(|r| &r.full_name)
                {
                    return Ok(refused("the head branch is in a fork"));
                }
                let commits = self.api.pull_commits(n, prio)?;
                let Some(first) = commits.first() else {
                    return Ok(skipped("no commits"));
                };
                let parent = self
                    .api
                    .git_commit(&first.sha, prio)?
                    .parents
                    .first()
                    .map(|p| p.sha.clone())
                    .unwrap_or_default();
                let tree = self.api.git_commit(head_sha, prio)?.tree.sha;
                let c = self.api.create_commit(&tree, &[parent], message, prio)?;
                self.api.force_ref(&p.head.ref_name, &c, prio)?;
                ActionResult::Done { head_sha: Some(c) }
            }
            Op::Merge {
                head_sha, method, ..
            } => {
                let m = method.as_deref().unwrap_or("squash");
                if m == "fast-forward" {
                    return Ok(refused(
                        "fast-forward merges are not built for the merge action",
                    ));
                }
                match self.api.merge_pull(n, head_sha, m, prio)? {
                    PullMergeOutcome::Merged { .. } => done(),
                    PullMergeOutcome::HeadMoved => skipped("the head moved"),
                    PullMergeOutcome::NotMergeable(why) => refused(why),
                }
            }
            Op::Dispatch {
                workflow,
                reference,
                inputs,
                ..
            } => {
                let r = match reference {
                    Some(r) => r.clone(),
                    None => self.default_branch(prio)?,
                };
                self.api.dispatch_workflow(workflow, &r, inputs, prio)?;
                done()
            }
            Op::Copy { .. } => self.copy(op)?,
        })
    }

    /// A branch tip, `None` when the branch does not exist.
    fn branch_opt(&self, name: &str) -> Result<Option<String>, Error> {
        match self.api.branch_sha(name, Priority::Normal) {
            Ok(s) => Ok(Some(s)),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    fn copy(&mut self, op: &Op) -> Result<ActionResult, Error> {
        let prio = Priority::Normal;
        let Op::Copy {
            number,
            kind,
            branches,
            regexes,
            source,
            head_sha,
            title,
            body,
            labels,
            assignees,
            ignore_conflicts,
            label_conflicts,
        } = op
        else {
            return Ok(refused("not a copy"));
        };
        let n = *number;
        let pr = self.api.pull(n, prio)?;
        let mut destinations = branches.clone();
        if !regexes.is_empty() {
            let all = self.api.matching_branches("", prio)?;
            for r in regexes {
                let Ok(re) = fancy_regex::Regex::new(&format!("^(?:{r})")) else {
                    continue;
                };
                for b in &all {
                    let name = b.ref_name.trim_start_matches("refs/heads/").to_string();
                    if name.starts_with("mergify/") {
                        continue;
                    }
                    if re.is_match(&name).unwrap_or(false) && !destinations.contains(&name) {
                        destinations.push(name);
                    }
                }
            }
        }
        destinations.retain(|d| *d != pr.base.ref_name);
        // The change to apply: the merge commit against its parent, else the pull request's
        // head against its merge base (the first commit's parent).
        let (from, to) = match source {
            Some(c) => {
                let parent = self
                    .api
                    .git_commit(c, prio)?
                    .parents
                    .first()
                    .map(|p| p.sha.clone())
                    .unwrap_or_default();
                (parent, c.clone())
            }
            None => {
                let commits = self.api.pull_commits(n, prio)?;
                let first = commits.first().map(|c| c.sha.clone()).unwrap_or_default();
                let parent = self
                    .api
                    .git_commit(&first, prio)?
                    .parents
                    .first()
                    .map(|p| p.sha.clone())
                    .unwrap_or_default();
                (parent, head_sha.clone())
            }
        };
        let mut copies = Vec::new();
        for dest in destinations {
            let head_branch = kind.head_branch(&dest, n);
            let mut out = CopyOutcome {
                destination: dest.clone(),
                number: None,
                conflicts: false,
                error: None,
            };
            if self.branch_opt(&head_branch)?.is_some() {
                out.number = self
                    .api
                    .pulls_with_head(&head_branch, prio)?
                    .first()
                    .map(|p| p.number);
                copies.push(out);
                continue;
            }
            let Some(dest_tip) = self.branch_opt(&dest)? else {
                out.error = Some(format!("branch `{dest}` does not exist"));
                copies.push(out);
                continue;
            };
            match self.cherry_pick(&from, &to, &dest_tip, &head_branch, &pr.title, prio)? {
                Err(why) => {
                    out.error = Some(if *ignore_conflicts {
                        format!("{why} (conflict markers cannot be committed through the API)")
                    } else {
                        why
                    });
                    let _ = label_conflicts;
                }
                Ok(()) => {
                    let vars: BTreeMap<&str, String> = [
                        ("destination_branch", dest.clone()),
                        ("cherry_pick_error", String::new()),
                        ("cherry_picked_commits", to.clone()),
                    ]
                    .into();
                    let created = self.api.create_pull(
                        &template::simple(title, &vars),
                        &head_branch,
                        &dest,
                        &template::simple(body, &vars),
                        false,
                        prio,
                    )?;
                    if !labels.is_empty() {
                        let l: Vec<&str> = labels.iter().map(String::as_str).collect();
                        self.api.add_labels(created.number, &l, prio)?;
                    }
                    if !assignees.is_empty() {
                        self.api.add_assignees(created.number, assignees, prio)?;
                    }
                    out.number = Some(created.number);
                }
            }
            copies.push(out);
        }
        Ok(ActionResult::Copied { copies })
    }

    /// Applies the change `from → to` onto `onto` as one commit on a new branch `branch`;
    /// `Ok(Err(why))` on a conflict.
    fn cherry_pick(
        &mut self,
        from: &str,
        to: &str,
        onto: &str,
        branch: &str,
        title: &str,
        prio: Priority,
    ) -> Result<Result<(), String>, Error> {
        let onto_tree = self.api.git_commit(onto, prio)?.tree.sha;
        // A sibling of `to`: the destination's tree on `to`'s parent.
        let sibling =
            self.api
                .create_commit(&onto_tree, &[from.to_string()], "cherry-pick base", prio)?;
        let temp = format!("{branch}-tmp");
        let _ = self.api.delete_ref(&temp, prio);
        self.api.create_ref(&temp, &sibling, prio)?;
        let merged = self.api.merge_into(&temp, to, "cherry-pick", prio);
        let _ = self.api.delete_ref(&temp, prio);
        let tree = match merged? {
            MergeOutcome::Merged { tree, .. } => tree,
            MergeOutcome::NothingToMerge => onto_tree,
            MergeOutcome::Conflict => {
                return Ok(Err("the change does not apply cleanly (conflict)".into()));
            }
        };
        let message = format!("{title} (cherry picked from commit {to})");
        let c = self
            .api
            .create_commit(&tree, &[onto.to_string()], &message, prio)?;
        self.api.create_ref(branch, &c, prio)?;
        Ok(Ok(()))
    }
}
