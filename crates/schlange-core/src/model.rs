//! Domain types: pull requests and check runs as the forge reports them.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// Virtual or real time in seconds since the start of a run.
pub type Time = u64;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Conclusion {
    Success,
    Failure,
    Neutral,
    Cancelled,
    TimedOut,
    ActionRequired,
    Skipped,
    Stale,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    /// Slug of the app that published the run (`github-actions`); `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub head_sha: String,
    /// `None` while queued or in progress.
    pub conclusion: Option<Conclusion>,
    /// Creation order on the forge among runs of the same kind (check runs and commit
    /// statuses are numbered apart, so their orders are never compared).
    pub seq: u64,
    /// A commit status (one per context), not a check run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub status: bool,
    /// The GitHub Actions workflow file that ran it, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workflow: Option<String>,
    /// The event that triggered that workflow run, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub event: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullRequest {
    pub number: u64,
    pub author: String,
    pub base: String,
    pub head_ref: String,
    pub head_sha: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub closed: bool,
    #[serde(default)]
    pub labels: BTreeSet<String>,
    #[serde(default)]
    pub files: Vec<String>,
    /// From Dependabot's commit metadata, e.g. `version-update:semver-patch`.
    #[serde(default)]
    pub dependabot_update_types: Vec<String>,
    #[serde(default)]
    pub checks: Vec<CheckRun>,
    /// Merged (by the queue or by hand). A merged pull request is also closed.
    #[serde(default)]
    pub merged: bool,
    /// The base branch tip is not an ancestor of the head (the pull request is behind).
    #[serde(default)]
    pub behind_base: bool,
    /// The base branch tip when `behind_base` was computed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_tip: Option<String>,
    /// Tree id of the head commit, when the forge reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_tree: Option<String>,
    /// The commit that merged the pull request, once merged (`merge_commit_sha` on GitHub).
    /// The engine needs it to recognise its own landing after a lost reply (MQ-35).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_commit_sha: Option<String>,
    /// The description; `Depends-On` and `Merge-After` lines live here.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
    /// The head branch lives in another repository than the base (`from-fork`).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_fork: bool,
    /// Authors of the pull request's commits as `Name <email>`, in commit order (for the
    /// `co-authored-by` trailer).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commit_authors: Vec<String>,
    /// Logins whose latest review approves the pull request (for `approved-by`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub approved_by: Vec<String>,
    /// Login of whoever pushed the head commit, when the forge knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_pusher: Option<String>,
    /// The head commit is the queue's own update of the pull request (Schlange does not update
    /// pull requests yet, so the adapters never set it).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub head_by_queue: bool,
    /// The pull requests named in `Depends-On` lines, as the forge resolved them when it read
    /// this pull request. The engine prefers its own, fresher view of pull requests it knows.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<Dependency>,
    /// The check of the configuration this pull request changes (only for pull requests
    /// against the default branch that touch a configuration file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config_change: Option<schlange_config::config_file::ConfigChange>,
    /// Assigned logins (`assign` action).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub assignees: Vec<String>,
    /// Logins and teams whose review is requested and not given yet (`request_reviews`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_reviewers: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_teams: Vec<String>,
    /// Reviews, oldest first (`review`, `dismiss_reviews`), when the forge read them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reviews: Vec<Review>,
    /// Messages of the pull request's commits, in order (`squash`), when the forge read them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub commit_messages: Vec<String>,
    /// Who merged it, once merged (`{{ merged_by }}` in copies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merged_by: Option<String>,
}

/// One review of a pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Review {
    pub id: u64,
    pub author: String,
    /// `APPROVED`, `CHANGES_REQUESTED`, `COMMENTED` or `DISMISSED`.
    pub state: String,
    /// The head the review was given on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub body: String,
}

/// A pull request named in a `Depends-On` line, as the forge resolved it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Dependency {
    /// `owner/repo#number`.
    pub reference: String,
    pub state: DependencyState,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyState {
    Open,
    Merged,
    /// Closed without being merged: can never be satisfied.
    Closed,
    /// The pull request does not exist or cannot be read.
    Missing,
    /// It belongs to another owner than the repository declaring the dependency.
    OtherOwner,
}

/// A comment on a pull request, or an edit of one.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub id: u64,
    /// The pull request it is on.
    pub number: u64,
    /// Who wrote the comment.
    pub author: String,
    /// Who created it or, for an edit, who edited it (a ticked checkbox in the queue's own
    /// comment is an edit by the person who ticked it).
    pub sender: String,
    pub body: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub edited: bool,
    /// Distinguishes edits of the same comment (GitHub: `updated_at`).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub version: String,
    /// The sender's permission on the repository (`admin`, `maintain`, `write`, `triage`,
    /// `read`, `none`), read by the adapter when it handed the comment over.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sender_permission: Option<String>,
    /// The comment is the queue's own (written by its app).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub by_queue: bool,
    /// The pull request's head when the comment was written, when the forge knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_sha: Option<String>,
}

impl PullRequest {
    /// An open, ready pull request without labels, files or checks.
    pub fn new(number: u64, author: &str, base: &str, head_ref: &str, head_sha: &str) -> Self {
        PullRequest {
            number,
            author: author.to_string(),
            base: base.to_string(),
            head_ref: head_ref.to_string(),
            head_sha: head_sha.to_string(),
            title: String::new(),
            draft: false,
            closed: false,
            labels: Default::default(),
            files: vec![],
            dependabot_update_types: vec![],
            checks: vec![],
            merged: false,
            behind_base: false,
            base_tip: None,
            head_tree: None,
            merge_commit_sha: None,
            body: String::new(),
            from_fork: false,
            commit_authors: vec![],
            approved_by: vec![],
            head_pusher: None,
            head_by_queue: false,
            dependencies: vec![],
            config_change: None,
            assignees: vec![],
            requested_reviewers: vec![],
            requested_teams: vec![],
            reviews: vec![],
            commit_messages: vec![],
            merged_by: None,
        }
    }

    /// The run that counts per check on the current head commit (docs: conditions, "Which Check
    /// Run a Condition Reads"), one per kind (check run or commit status), name and app:
    ///
    /// - commit statuses: the latest one posted per context;
    /// - check runs: the newest run of each workflow file and triggering event; when one check
    ///   spans several of those, a run that has not passed is kept even if another one passed
    ///   later. The docs read it that way for checks the base branch requires; the engine does
    ///   not know which checks are required, so it reads every check that way (fail closed).
    pub fn current_checks(&self) -> Vec<&CheckRun> {
        type Key<'a> = (bool, &'a str, Option<&'a str>);
        let mut groups: Vec<(Key, Vec<&CheckRun>)> = Vec::new();
        for run in self.checks.iter().filter(|c| c.head_sha == self.head_sha) {
            let key = (run.status, run.name.as_str(), run.app.as_deref());
            let suite = (run.workflow.as_deref(), run.event.as_deref());
            let Some((_, runs)) = groups.iter_mut().find(|(k, _)| *k == key) else {
                groups.push((key, vec![run]));
                continue;
            };
            let same =
                |r: &&CheckRun| run.status || (r.workflow.as_deref(), r.event.as_deref()) == suite;
            match runs.iter_mut().find(|r| same(r)) {
                Some(slot) if slot.seq < run.seq => *slot = run,
                Some(_) => {}
                None => runs.push(run),
            }
        }
        groups
            .into_iter()
            .map(|(_, runs)| {
                runs.iter()
                    .copied()
                    .filter(|r| r.conclusion != Some(Conclusion::Success))
                    .max_by_key(|r| r.seq)
                    .unwrap_or_else(|| runs.iter().copied().max_by_key(|r| r.seq).expect("a run"))
            })
            .collect()
    }
}
