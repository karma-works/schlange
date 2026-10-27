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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRun {
    pub name: String,
    /// Slug of the app that published the run (`github-actions`); `None` if unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<String>,
    pub head_sha: String,
    /// `None` while queued or in progress.
    pub conclusion: Option<Conclusion>,
    /// Creation order on the forge; the newest run per name wins.
    pub seq: u64,
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
}

impl PullRequest {
    /// The newest run per check name on the current head commit.
    pub fn current_checks(&self) -> Vec<&CheckRun> {
        let mut newest: Vec<&CheckRun> = Vec::new();
        for run in self.checks.iter().filter(|c| c.head_sha == self.head_sha) {
            match newest.iter_mut().find(|r| r.name == run.name) {
                Some(slot) if slot.seq < run.seq => *slot = run,
                Some(_) => {}
                None => newest.push(run),
            }
        }
        newest
    }
}
