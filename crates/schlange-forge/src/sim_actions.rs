//! The simulated forge carries out rule actions ([`Op`]): labels, comments, assignees, review
//! requests and reviews, closing, drafts, branch deletion, checks, updates, rebases, squashes,
//! copies onto other branches (cherry-picks with conflict markers), direct merges and workflow
//! dispatches. Every operation is idempotent the way the GitHub adapter's is: a comment or
//! review whose marker is already there is not posted again, a copy whose branch exists reuses
//! its pull request, a head-bound operation does nothing once the head moved.

use std::collections::BTreeMap;

use schlange_core::actions::{ActionResult, CopyOutcome, Op};
use schlange_core::engine::short;
use schlange_core::model::{PullRequest, Review};
use schlange_core::template;

use super::{BOT, Hint, SimForge, sim_identity};
use crate::git::{GitStore, Repo, Sha, Tree};

/// A check run an action reported (`post_check`, copy reports).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PostedCheck {
    pub number: u64,
    pub head_sha: String,
    pub name: String,
    pub conclusion: String,
    pub title: String,
    pub summary: String,
}

/// Head branch prefixes of the pull requests copies create.
pub const COPY_PREFIXES: [&str; 2] = ["mergify/bp/", "mergify/copy/"];

/// Whether a pull request is one an action created (`backport`, `copy`).
pub fn is_copy(pr: &PullRequest) -> bool {
    COPY_PREFIXES.iter().any(|p| pr.head_ref.starts_with(p))
}

fn done() -> ActionResult {
    ActionResult::Done { head_sha: None }
}

fn skipped(m: impl Into<String>) -> ActionResult {
    ActionResult::Skipped { message: m.into() }
}

fn refused(m: impl Into<String>) -> ActionResult {
    ActionResult::Refused { message: m.into() }
}

/// The marker line of an action's body, if any.
fn marker_of(body: &str) -> Option<&str> {
    body.lines()
        .find(|l| l.trim_start().starts_with("<!-- schlange-action "))
        .map(str::trim)
}

impl<G: GitStore> SimForge<G> {
    fn changed(&mut self, n: u64, what: &str) {
        self.emit(Hint::PullRequest(n), format!("#{n} {what}"));
    }

    /// Carries out one rule action.
    pub fn rule_action(&mut self, op: &Op) -> ActionResult {
        let n = op.number();
        if !self.prs.contains_key(&n) {
            return refused(format!("#{n} does not exist"));
        }
        match op {
            Op::Labels { add, remove, .. } => {
                let p = self.prs.get_mut(&n).expect("pr");
                let before = p.labels.clone();
                p.labels.extend(add.iter().cloned());
                for l in remove {
                    p.labels.remove(l);
                }
                if p.labels == before {
                    return skipped("the labels are already so");
                }
                self.changed(n, "labeled");
                done()
            }
            Op::Comment { body, .. } => {
                if let Some(m) = marker_of(body)
                    && self
                        .comments
                        .values()
                        .any(|c| c.number == n && c.author == BOT && c.body.contains(m))
                {
                    return skipped("the comment is there already");
                }
                let id = self.add_comment(n, BOT, body.clone());
                self.emit(Hint::Comment(id), format!("#{n} comment {id} by the queue"));
                done()
            }
            Op::Assign { add, remove, .. } => {
                let p = self.prs.get_mut(&n).expect("pr");
                let before = p.assignees.clone();
                for a in add {
                    if !p.assignees.contains(a) {
                        p.assignees.push(a.clone());
                    }
                }
                p.assignees.retain(|a| !remove.contains(a));
                if p.assignees == before {
                    return skipped("the assignees are already so");
                }
                self.changed(n, "assigned");
                done()
            }
            Op::RequestReviews { users, teams, .. } => {
                let p = self.prs.get_mut(&n).expect("pr");
                let mut any = false;
                for u in users {
                    if !p.requested_reviewers.contains(u) {
                        p.requested_reviewers.push(u.clone());
                        any = true;
                    }
                }
                for t in teams {
                    if !p.requested_teams.contains(t) {
                        p.requested_teams.push(t.clone());
                        any = true;
                    }
                }
                if !any {
                    return skipped("the reviews are requested already");
                }
                self.changed(n, "review_requested");
                done()
            }
            Op::Review { event, body, .. } => {
                let p = self.prs.get(&n).expect("pr");
                let latest = p.reviews.iter().rev().find(|r| r.author == BOT);
                if let Some(m) = marker_of(body)
                    && latest.is_some_and(|r| r.body.contains(m) && r.state != "DISMISSED")
                {
                    return skipped("the review is there already");
                }
                self.next_review += 1;
                let id = self.next_review;
                let p = self.prs.get_mut(&n).expect("pr");
                p.reviews.push(Review {
                    id,
                    author: BOT.into(),
                    state: match event.as_str() {
                        "REQUEST_CHANGES" => "CHANGES_REQUESTED",
                        "COMMENT" => "COMMENTED",
                        _ => "APPROVED",
                    }
                    .into(),
                    commit: Some(p.head_sha.clone()),
                    body: body.clone(),
                });
                self.changed(n, "reviewed");
                done()
            }
            Op::DismissReviews { reviews, .. } => {
                let p = self.prs.get_mut(&n).expect("pr");
                let mut any = false;
                for r in p.reviews.iter_mut().filter(|r| reviews.contains(&r.id)) {
                    if r.state != "DISMISSED" {
                        r.state = "DISMISSED".into();
                        any = true;
                    }
                }
                if !any {
                    return skipped("the reviews are dismissed already");
                }
                self.changed(n, "review dismissed");
                done()
            }
            Op::Close { .. } => {
                let p = self.prs.get_mut(&n).expect("pr");
                if p.closed {
                    return skipped("closed already");
                }
                p.closed = true;
                self.changed(n, "closed");
                done()
            }
            Op::SetDraft { draft, .. } => {
                let p = self.prs.get_mut(&n).expect("pr");
                if p.draft == *draft || p.closed {
                    return skipped("the draft state is already so");
                }
                p.draft = *draft;
                self.changed(
                    n,
                    if *draft {
                        "converted_to_draft"
                    } else {
                        "ready_for_review"
                    },
                );
                done()
            }
            Op::DeleteBranch { branch, .. } => {
                if self.repo.delete_ref(branch) {
                    self.deleted_branches.push(branch.clone());
                    self.emit(Hint::Branch(branch.clone()), format!("delete {branch}"));
                    done()
                } else {
                    skipped(format!("branch {branch} does not exist"))
                }
            }
            Op::PostCheck {
                head_sha,
                name,
                conclusion,
                title,
                summary,
                ..
            } => {
                let c = PostedCheck {
                    number: n,
                    head_sha: head_sha.clone(),
                    name: name.clone(),
                    conclusion: conclusion.clone(),
                    title: title.clone(),
                    summary: summary.clone(),
                };
                // The latest report per head and name counts; reposting the same is a no-op.
                if self.posted_checks.last_matching(&c) {
                    return skipped("the check is reported already");
                }
                self.posted_checks.push(c);
                done()
            }
            Op::Update { head_sha, .. } => self.rewrite(n, head_sha, Rewrite::Update),
            Op::Rebase { head_sha, .. } => self.rewrite(n, head_sha, Rewrite::Rebase),
            Op::Squash {
                head_sha, message, ..
            } => self.rewrite(n, head_sha, Rewrite::Squash(message.clone())),
            Op::Merge {
                head_sha, method, ..
            } => {
                let pr = self.prs[&n].clone();
                if pr.merged {
                    return skipped("merged already");
                }
                if pr.closed || pr.head_sha != *head_sha {
                    return refused("the pull request changed (closed or new head)");
                }
                let Some(tip) = self.repo.ref_sha(&pr.base) else {
                    return refused("the base branch is gone");
                };
                let message = format!("{} (#{n})", pr.title);
                let made = match method.as_deref() {
                    Some("merge") => self.repo.merge(&tip, &pr.head_sha, &message),
                    Some("fast-forward") if self.repo.is_ancestor(&tip, &pr.head_sha) => {
                        Ok(pr.head_sha.clone())
                    }
                    Some("fast-forward") => {
                        return refused("fast-forward needs the pull request up to date");
                    }
                    _ => self.repo.squash(&tip, &pr.head_sha, &message),
                };
                let Ok(c) = made else {
                    return refused("the pull request conflicts with its base");
                };
                if self.repo.update_ref(&pr.base, &tip, &c).is_err() {
                    return refused("the base moved");
                }
                self.external_commits.insert(c.clone());
                let p = self.prs.get_mut(&n).expect("pr");
                p.merged = true;
                p.closed = true;
                p.merge_commit_sha = Some(c.clone());
                p.merged_by = Some(BOT.into());
                self.emit(
                    Hint::Branch(pr.base.clone()),
                    format!("push to {}", pr.base),
                );
                self.changed(n, "closed (merged by a rule)");
                done()
            }
            Op::Dispatch {
                workflow,
                reference,
                inputs,
                ..
            } => {
                self.dispatches.push((
                    n,
                    workflow.clone(),
                    reference
                        .clone()
                        .unwrap_or_else(|| self.default_branch.clone()),
                    inputs.clone(),
                ));
                done()
            }
            Op::Copy { .. } => self.copy(op),
        }
    }

    /// Update, rebase or squash the head of `n`, if it is still `head_sha`.
    fn rewrite(&mut self, n: u64, head_sha: &str, how: Rewrite) -> ActionResult {
        let pr = self.prs[&n].clone();
        if pr.closed {
            return skipped("closed");
        }
        if pr.head_sha != head_sha {
            return skipped(format!("the head moved to {}", short(&pr.head_sha)));
        }
        let Some(tip) = self.repo.ref_sha(&pr.base) else {
            return refused("the base branch is gone");
        };
        let mb = self.repo.merge_base(&tip, &pr.head_sha).unwrap_or_default();
        let (new, log): (Sha, Vec<(String, String)>) = match &how {
            Rewrite::Update => {
                if self.repo.is_ancestor(&tip, &pr.head_sha) {
                    return skipped("up to date with its base");
                }
                let message = format!("Merge branch '{}' into {}", pr.base, pr.head_ref);
                match self.repo.merge(&pr.head_sha, &tip, &message) {
                    Ok(c) => {
                        let mut log = self.commit_log.get(&n).cloned().unwrap_or_default();
                        log.push((sim_identity(BOT), message));
                        (c, log)
                    }
                    Err(e) => return refused(format!("conflict in {}", e.0.join(", "))),
                }
            }
            Rewrite::Rebase => {
                if self.repo.is_ancestor(&tip, &pr.head_sha) {
                    return skipped("up to date with its base");
                }
                let mut commits: Vec<Sha> = self
                    .repo
                    .first_parent_history(&pr.head_sha)
                    .into_iter()
                    .take_while(|c| *c != mb && !self.repo.is_ancestor(c, &tip))
                    .collect();
                commits.reverse();
                let log = self.commit_log.get(&n).cloned().unwrap_or_default();
                let mut cur = tip.clone();
                for (i, c) in commits.iter().enumerate() {
                    let parent = self.repo.parents(c).first().cloned().unwrap_or_default();
                    let base_tree = self.repo.tree(&parent);
                    let merged =
                        Repo::merge_trees(&base_tree, &self.repo.tree(&cur), &self.repo.tree(c));
                    let Ok(tree) = merged else {
                        return refused(format!("conflict while replaying commit {}", i + 1));
                    };
                    let message = log
                        .get(log.len().saturating_sub(commits.len()) + i)
                        .map(|(_, m)| m.clone())
                        .unwrap_or_else(|| format!("rebased commit {}", i + 1));
                    cur = self.repo.commit(tree, vec![cur.clone()], &message);
                }
                (cur, log)
            }
            Rewrite::Squash(message) => {
                let tree = self.repo.tree(&pr.head_sha);
                let c = self.repo.commit(tree, vec![mb], message);
                (c, vec![(sim_identity(&pr.author), message.clone())])
            }
        };
        if self
            .repo
            .update_ref(&pr.head_ref, &pr.head_sha, &new)
            .is_err()
        {
            return skipped("the head moved");
        }
        self.prs.get_mut(&n).expect("pr").head_sha = new.clone();
        self.commit_log.insert(n, log);
        self.pushers.insert(n, BOT.into());
        self.emit(
            Hint::PullRequest(n),
            format!("#{n} synchronize (by the queue)"),
        );
        self.start_ci(n, false);
        ActionResult::Done {
            head_sha: Some(new),
        }
    }

    fn copy(&mut self, op: &Op) -> ActionResult {
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
            return refused("not a copy");
        };
        let n = *number;
        let pr = self.prs[&n].clone();
        let mut destinations: Vec<String> = branches.clone();
        for r in regexes {
            let Ok(re) = fancy_regex::Regex::new(&format!("^(?:{r})")) else {
                continue;
            };
            for b in self.repo.ref_names() {
                if b.starts_with("pr-")
                    || COPY_PREFIXES.iter().any(|p| b.starts_with(p))
                    || b.starts_with("mergify/merge-queue/")
                {
                    continue;
                }
                if re.is_match(&b).unwrap_or(false) && !destinations.contains(&b) {
                    destinations.push(b);
                }
            }
        }
        destinations.retain(|d| *d != pr.base);
        // The changes to apply: the merge commit against its parent, else the pull request's
        // changes against its merge base.
        let (from, to) = match source {
            Some(c) => (
                self.repo.parents(c).first().cloned().unwrap_or_default(),
                c.clone(),
            ),
            None => {
                let tip = self.repo.ref_sha(&pr.base).unwrap_or_default();
                (
                    self.repo.merge_base(&tip, head_sha).unwrap_or_default(),
                    head_sha.clone(),
                )
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
            if self.repo.ref_sha(&head_branch).is_some() {
                out.number = self
                    .prs
                    .values()
                    .find(|p| p.head_ref == head_branch)
                    .map(|p| p.number);
                out.conflicts = out
                    .number
                    .is_some_and(|x| self.prs[&x].labels.contains(label_conflicts));
                copies.push(out);
                continue;
            }
            let Some(dest_tip) = self.repo.ref_sha(&dest) else {
                out.error = Some(format!("branch `{dest}` does not exist"));
                copies.push(out);
                continue;
            };
            let base_tree = self.repo.tree(&from);
            let ours = self.repo.tree(&dest_tip);
            let theirs = self.repo.tree(&to);
            let (tree, conflicts) = match Repo::merge_trees(&base_tree, &ours, &theirs) {
                Ok(t) => (t, vec![]),
                Err(c) if !ignore_conflicts => {
                    out.error = Some(format!("cherry-pick conflicts in {}", c.0.join(", ")));
                    copies.push(out);
                    continue;
                }
                Err(c) => (with_markers(&base_tree, &ours, &theirs, &dest), c.0),
            };
            let vars: BTreeMap<&str, String> = [
                ("destination_branch", dest.clone()),
                (
                    "cherry_pick_error",
                    if conflicts.is_empty() {
                        String::new()
                    } else {
                        format!("conflicts in {}", conflicts.join(", "))
                    },
                ),
                ("cherry_picked_commits", to.clone()),
            ]
            .into();
            let message = format!("{} (cherry picked from {})", pr.title, short(&to));
            let c = self.repo.commit(tree, vec![dest_tip.clone()], &message);
            if self.repo.create_ref(&head_branch, &c).is_err() {
                out.error = Some(format!("branch {head_branch} could not be created"));
                copies.push(out);
                continue;
            }
            self.next_copy += 1;
            let m = self.next_copy;
            let mut p = PullRequest::new(m, BOT, &dest, &head_branch, &c);
            p.title = template::simple(title, &vars);
            p.body = template::simple(body, &vars);
            p.labels = labels.iter().cloned().collect();
            if !conflicts.is_empty() {
                p.labels.insert(label_conflicts.clone());
            }
            p.assignees = assignees.clone();
            self.prs.insert(m, p);
            self.commit_log
                .insert(m, vec![(sim_identity(BOT), message)]);
            self.pushers.insert(m, BOT.into());
            self.emit(
                Hint::PullRequest(m),
                format!("#{m} opened ({} of #{n})", kind.as_str()),
            );
            out.number = Some(m);
            out.conflicts = !conflicts.is_empty();
            copies.push(out);
        }
        ActionResult::Copied { copies }
    }
}

enum Rewrite {
    Update,
    Rebase,
    Squash(String),
}

/// The merged tree with conflict markers in the files both sides changed.
fn with_markers(base: &Tree, ours: &Tree, theirs: &Tree, dest: &str) -> Tree {
    let mut out = ours.clone();
    let paths: Vec<&String> = base
        .keys()
        .chain(ours.keys())
        .chain(theirs.keys())
        .collect();
    for p in paths {
        let (b, o, t) = (base.get(p), ours.get(p), theirs.get(p));
        if o == t || t == b {
            continue;
        }
        if o == b {
            match t {
                Some(t) => out.insert(p.clone(), t.clone()),
                None => out.remove(p),
            };
            continue;
        }
        out.insert(
            p.clone(),
            format!(
                "<<<<<<< {dest}\n{}=======\n{}>>>>>>> cherry-pick\n",
                o.cloned().unwrap_or_default(),
                t.cloned().unwrap_or_default()
            ),
        );
    }
    out
}

/// The checks reported by actions.
#[derive(Clone, Debug, Default)]
pub struct PostedChecks(pub Vec<PostedCheck>);

impl PostedChecks {
    fn last_matching(&self, c: &PostedCheck) -> bool {
        self.0
            .iter()
            .rev()
            .find(|x| x.number == c.number && x.head_sha == c.head_sha && x.name == c.name)
            == Some(c)
    }

    fn push(&mut self, c: PostedCheck) {
        self.0.push(c);
    }

    /// The latest report of `name` on pull request `n` (any head).
    pub fn latest(&self, n: u64, name: &str) -> Option<&PostedCheck> {
        self.0
            .iter()
            .rev()
            .find(|x| x.number == n && x.name == name)
    }

    pub fn iter(&self) -> impl Iterator<Item = &PostedCheck> {
        self.0.iter()
    }
}
