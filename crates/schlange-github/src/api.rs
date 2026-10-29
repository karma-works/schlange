//! The REST calls the queue needs, with small hand-written response types (only the fields
//! Schlange reads, not GitHub's whole schema).
//!
//! **No compare-and-swap on GitHub's merge.** The merge endpoint (`PUT /pulls/{n}/merge`)
//! guards only the pull request's head (`sha`), not the base: GitHub squashes onto whatever the
//! base is when it executes the call.
//!
//! A real compare-and-swap exists for refs: build the squash commit with the Git Data API
//! (`POST /git/commits`, the tested tree, the expected base as parent) and move the base with
//! `PATCH /git/refs/heads/<base>` and `force: false`, which GitHub refuses unless it is a
//! fast-forward, so it fails when anyone pushed in between. That path has two costs, which is
//! why Schlange does not take it (yet):
//!
//! - GitHub does not mark the pull request as **merged**: its head is not in the base after a
//!   squash. The queue would have to close it with a comment. Workflows on
//!   `pull_request: closed` with `merged == true`, Dependabot's bookkeeping, release notes and
//!   the PR timeline would all see "closed", not "merged". Arztagent's flows may rely on it.
//! - A direct ref update on a protected base needs the app on the bypass list of every
//!   ruleset (or classic protection) that requires pull requests; the merge API also needs
//!   that for batches, but a bypassing ref update skips **all** rules for that push, required
//!   checks included, so a bug in the queue would not be stopped by GitHub.
//!
//! So landing keeps the merge API with the guards in [`crate::forge`]: one writer per
//! repository, the base read immediately before every merge call (never a call after a wait
//! without that read; the client hands rate-limited writes back instead of repeating them),
//! the head guarded by `sha`, and afterwards a check that the landed commit's parent is the
//! expected base and its tree the tested tree; a mismatch halts the queue (MQ-02). The window
//! left is one request long: a push that lands between the base read and GitHub executing the
//! merge call is detected afterwards, not prevented. Only a sandbox on github.com can measure
//! how often that happens and confirm GitHub's exact answers (the list is in the
//! `github-integration` feature file, Gotchas).
//!
//! [`RepoApi::update_ref_cas`] reads the ref, compares it with the expected SHA and then
//! patches with `force: false`; a racing writer between the read and the patch is not
//! detected by GitHub when its commit is an ancestor of ours.

use std::collections::BTreeMap;

use serde::Deserialize;
use serde_json::{Value, json};

use crate::client::{Auth, Client, Error, Priority, enc_path, enc_query};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepoName {
    pub owner: String,
    pub name: String,
}

impl RepoName {
    pub fn parse(full: &str) -> Option<Self> {
        let (o, n) = full.split_once('/')?;
        (!o.is_empty() && !n.is_empty() && !n.contains('/')).then(|| RepoName {
            owner: o.to_string(),
            name: n.to_string(),
        })
    }

    pub fn full(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct User {
    pub login: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RepoRef {
    pub full_name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitRefSide {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub sha: String,
    /// The repository of the branch; `null` when the head repository was deleted.
    #[serde(default)]
    pub repo: Option<RepoRef>,
}

/// A review of a pull request.
#[derive(Clone, Debug, Deserialize)]
pub struct Review {
    #[serde(default)]
    pub id: u64,
    #[serde(default)]
    pub user: Option<User>,
    pub state: String,
    #[serde(default)]
    pub commit_id: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

/// A team in a pull request's requested reviewers.
#[derive(Clone, Debug, Deserialize)]
pub struct TeamRef {
    pub slug: String,
}

/// A comment on an issue or pull request.
#[derive(Clone, Debug, Deserialize)]
pub struct IssueComment {
    pub id: u64,
    pub user: User,
    #[serde(default)]
    pub body: String,
    /// Equal to `updated_at` until someone edits the comment. The REST API does not say who
    /// edited it: anyone with write access may edit anyone's comment.
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// `…/issues/<n>`.
    #[serde(default)]
    pub issue_url: String,
}

/// One entry of the repository's activity log (`GET /repos/{o}/{r}/activity`): GitHub's own
/// record of a branch moving, with its time and actor. Unlike commit metadata, the pusher
/// cannot write it.
#[derive(Clone, Debug, Deserialize)]
pub struct Activity {
    #[serde(default)]
    pub before: String,
    #[serde(default)]
    pub after: String,
    #[serde(default)]
    pub timestamp: String,
    #[serde(default)]
    pub activity_type: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Reaction {
    pub user: User,
    pub content: String,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CommitPerson {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub email: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Label {
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Pull {
    pub number: u64,
    pub state: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub body: Option<String>,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub merged: bool,
    #[serde(default)]
    pub merge_commit_sha: Option<String>,
    #[serde(default)]
    pub mergeable: Option<bool>,
    pub user: User,
    pub head: GitRefSide,
    pub base: GitRefSide,
    #[serde(default)]
    pub labels: Vec<Label>,
    #[serde(default)]
    pub node_id: String,
    #[serde(default)]
    pub assignees: Vec<User>,
    #[serde(default)]
    pub requested_reviewers: Vec<User>,
    #[serde(default)]
    pub requested_teams: Vec<TeamRef>,
    /// How many files the pull request changes; `GET /pulls/{n}/files` lists at most 3000.
    #[serde(default)]
    pub changed_files: Option<u64>,
    /// When it was merged (also in list responses, which lack `merged`).
    #[serde(default)]
    pub merged_at: Option<String>,
    /// Who merged it (single reads only).
    #[serde(default)]
    pub merged_by: Option<User>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct PullFile {
    pub filename: String,
    /// Set on a renamed file: its path before the rename.
    #[serde(default)]
    pub previous_filename: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct AppRef {
    #[serde(default)]
    pub slug: Option<String>,
    #[serde(default)]
    pub id: Option<u64>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CheckSuiteRef {
    pub id: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CheckRunJson {
    pub id: u64,
    pub name: String,
    pub head_sha: String,
    pub status: String,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub app: Option<AppRef>,
    #[serde(default)]
    pub check_suite: Option<CheckSuiteRef>,
    #[serde(default)]
    pub output: Option<CheckOutput>,
}

#[derive(Clone, Debug, Default, Deserialize)]
pub struct CheckOutput {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub summary: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CheckSuiteJson {
    pub id: u64,
    pub head_sha: String,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub conclusion: Option<String>,
    #[serde(default)]
    pub app: Option<AppRef>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct StatusJson {
    pub id: u64,
    pub context: String,
    pub state: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CombinedStatus {
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub statuses: Vec<StatusJson>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct ShaOnly {
    pub sha: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitRef {
    #[serde(rename = "ref")]
    pub ref_name: String,
    pub object: ShaOnly,
}

/// Evidence of one matching authenticated create-ref response. The fields are private so a
/// caller cannot construct a receipt from a later read or an arbitrary response.
#[derive(Debug)]
pub struct RefCreatedReceipt {
    repo: String,
    installation: u64,
    api: String,
    branch: String,
    sha: String,
}

impl RefCreatedReceipt {
    pub fn repo(&self) -> &str {
        &self.repo
    }
    pub fn installation(&self) -> u64 {
        self.installation
    }
    pub fn api(&self) -> &str {
        &self.api
    }
    pub fn branch(&self) -> &str {
        &self.branch
    }
    pub fn sha(&self) -> &str {
        &self.sha
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct GitCommit {
    pub sha: String,
    pub tree: ShaOnly,
    #[serde(default)]
    pub parents: Vec<ShaOnly>,
    #[serde(default)]
    pub message: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct CommitInner {
    pub tree: ShaOnly,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub author: Option<CommitPerson>,
}

/// A commit as `POST /merges` and `GET /pulls/{n}/commits` return it.
#[derive(Clone, Debug, Deserialize)]
pub struct RepoCommit {
    pub sha: String,
    pub commit: CommitInner,
    #[serde(default)]
    pub parents: Vec<ShaOnly>,
    /// The GitHub account of the commit's author, when GitHub matched one.
    #[serde(default)]
    pub author: Option<User>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Compare {
    pub status: String,
    #[serde(default)]
    pub ahead_by: u64,
    #[serde(default)]
    pub behind_by: u64,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Repository {
    pub full_name: String,
    pub default_branch: String,
    /// `PR_BODY`, `COMMIT_MESSAGES` or `BLANK`: GitHub's squash body setting.
    #[serde(default)]
    pub squash_merge_commit_message: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Installation {
    pub id: u64,
    #[serde(default)]
    pub permissions: BTreeMap<String, String>,
    #[serde(default)]
    pub events: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct App {
    pub id: u64,
    pub slug: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BypassActor {
    #[serde(default)]
    pub actor_id: Option<u64>,
    pub actor_type: String,
    #[serde(default)]
    pub bypass_mode: Option<String>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Ruleset {
    pub id: u64,
    pub name: String,
    #[serde(default)]
    pub target: Option<String>,
    #[serde(default)]
    pub enforcement: String,
    #[serde(default)]
    pub bypass_actors: Option<Vec<BypassActor>>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct BranchRule {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub ruleset_id: Option<u64>,
    /// The rule's parameters (`required_status_checks` lists the required contexts).
    #[serde(default)]
    pub parameters: Option<Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RequiredContext {
    pub context: String,
    pub app_id: Option<u64>,
}

fn context_from_value(value: &Value, id_key: &str) -> Option<RequiredContext> {
    let context = value.get("context")?.as_str()?.to_string();
    if context.is_empty() {
        return None;
    }
    let app = value.get(id_key);
    let app_id = match app {
        None | Some(Value::Null) => None,
        Some(v) if v.as_i64() == Some(-1) => None,
        Some(v) => Some(v.as_u64().filter(|id| *id != 0)?),
    };
    Some(RequiredContext { context, app_id })
}

impl BranchRule {
    /// The check contexts a `required_status_checks` rule requires.
    pub fn required_checks(&self) -> Vec<String> {
        if self.kind != "required_status_checks" {
            return vec![];
        }
        self.parameters
            .as_ref()
            .and_then(|p| p["required_status_checks"].as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|c| c["context"].as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[derive(Clone, Debug, Deserialize)]
pub struct HookDelivery {
    pub id: u64,
    #[serde(default)]
    pub guid: String,
    pub delivered_at: String,
    #[serde(default)]
    pub status_code: u16,
    #[serde(default)]
    pub event: String,
}

#[derive(Clone, Debug, Deserialize)]
pub struct RateResource {
    pub limit: u64,
    pub remaining: u64,
    pub reset: u64,
}

/// Outcome of `POST /merges`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MergeOutcome {
    /// A merge commit: its SHA and tree.
    Merged { sha: String, tree: String },
    /// 204: the head is already in the branch.
    NothingToMerge,
    /// 409: merge conflict.
    Conflict,
}

/// Outcome of `PUT /pulls/{n}/merge`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PullMergeOutcome {
    Merged {
        sha: String,
    },
    /// 409: the head is not the given SHA.
    HeadMoved,
    /// 405: not mergeable (conflict, protection, already merged).
    NotMergeable(String),
}

/// Calls on one repository with its installation token.
#[derive(Clone)]
pub struct RepoApi {
    pub client: Client,
    pub repo: RepoName,
    pub installation: u64,
}

fn auth_inst(i: u64) -> Auth {
    Auth::Installation(i)
}

impl RepoApi {
    fn p(&self, rest: &str) -> String {
        format!(
            "/repos/{}/{}{rest}",
            enc_path(&self.repo.owner),
            enc_path(&self.repo.name)
        )
    }

    fn get<T: serde::de::DeserializeOwned>(&self, rest: &str, prio: Priority) -> Result<T, Error> {
        self.client
            .get_json(&self.p(rest), auth_inst(self.installation), prio)
    }

    fn send(
        &self,
        method: &str,
        rest: &str,
        prio: Priority,
        body: Option<&Value>,
    ) -> Result<crate::client::Response, Error> {
        self.client.send(
            method,
            &self.p(rest),
            auth_inst(self.installation),
            prio,
            body,
        )
    }

    fn list<T: serde::de::DeserializeOwned>(
        &self,
        rest: &str,
        prio: Priority,
        key: Option<&'static str>,
    ) -> Result<Vec<T>, Error> {
        let sep = if rest.contains('?') { '&' } else { '?' };
        self.client.get_paginated(
            &self.p(&format!("{rest}{sep}per_page=100")),
            auth_inst(self.installation),
            prio,
            30,
            move |v| match key {
                Some(k) => v.get(k).cloned(),
                None => Some(v),
            },
        )
    }

    fn checked_list<T: serde::de::DeserializeOwned>(
        &self,
        rest: &str,
        prio: Priority,
        key: &str,
    ) -> Result<Vec<T>, Error> {
        let sep = if rest.contains('?') { '&' } else { '?' };
        self.client.get_paginated_checked(
            &self.p(&format!("{rest}{sep}per_page=100")),
            auth_inst(self.installation),
            prio,
            30,
            key,
        )
    }

    // ---- reads -------------------------------------------------------------------------

    pub fn repository(&self, prio: Priority) -> Result<Repository, Error> {
        self.get("", prio)
    }

    pub fn pull(&self, n: u64, prio: Priority) -> Result<Pull, Error> {
        self.get(&format!("/pulls/{n}"), prio)
    }

    pub fn open_pulls(&self, prio: Priority) -> Result<Vec<Pull>, Error> {
        self.list("/pulls?state=open", prio, None)
    }

    pub fn open_pulls_checked(&self, prio: Priority) -> Result<Vec<Pull>, Error> {
        let path = self.p("/pulls?state=open&per_page=100");
        self.client
            .get_paginated_array_checked(&path, auth_inst(self.installation), prio, 30)
    }

    /// Open pull requests with this head branch (in this repository).
    /// The most recently updated pull requests, open and closed: one page of 100 (the shadow
    /// observer's view of what the other queue did).
    pub fn recent_pulls(&self, prio: Priority) -> Result<Vec<Pull>, Error> {
        self.get(
            "/pulls?state=all&sort=updated&direction=desc&per_page=100",
            prio,
        )
    }

    pub fn pulls_with_head(&self, branch: &str, prio: Priority) -> Result<Vec<Pull>, Error> {
        let head = format!("{}:{branch}", self.repo.owner);
        self.list(
            &format!("/pulls?state=open&head={}", enc_query(&head)),
            prio,
            None,
        )
    }

    /// The paths a pull request changes, and how many entries GitHub listed (a rename is one
    /// entry with two paths). GitHub lists at most 3000 entries; the caller compares the count
    /// with the pull request's `changed_files`.
    pub fn pull_files(&self, n: u64, prio: Priority) -> Result<(Vec<String>, u64), Error> {
        let files: Vec<PullFile> = self.list(&format!("/pulls/{n}/files"), prio, None)?;
        let listed = files.len() as u64;
        // A renamed file counts at both of its paths (docs: conditions, renamed files), so a
        // `git mv` out of a guarded path cannot pass a `files` gate.
        Ok((
            files
                .into_iter()
                .flat_map(|f| f.previous_filename.into_iter().chain([f.filename]))
                .collect(),
            listed,
        ))
    }

    pub fn pull_commits(&self, n: u64, prio: Priority) -> Result<Vec<RepoCommit>, Error> {
        self.list(&format!("/pulls/{n}/commits"), prio, None)
    }

    pub fn reviews(&self, n: u64, prio: Priority) -> Result<Vec<Review>, Error> {
        self.list(&format!("/pulls/{n}/reviews"), prio, None)
    }

    /// The comments on issue or pull request `n`.
    pub fn issue_comments(&self, n: u64, prio: Priority) -> Result<Vec<IssueComment>, Error> {
        self.list(&format!("/issues/{n}/comments"), prio, None)
    }

    pub fn issue_comments_checked(
        &self,
        n: u64,
        prio: Priority,
    ) -> Result<Vec<IssueComment>, Error> {
        let path = self.p(&format!("/issues/{n}/comments?per_page=100"));
        self.client
            .get_paginated_array_checked(&path, auth_inst(self.installation), prio, 30)
    }

    /// The 100 newest moves of a branch of this repository, newest first.
    pub fn branch_activity(&self, branch: &str, prio: Priority) -> Result<Vec<Activity>, Error> {
        let r = format!("refs/heads/{branch}");
        self.get(
            &format!(
                "/activity?ref={}&direction=desc&per_page=100",
                enc_query(&r)
            ),
            prio,
        )
    }

    pub fn comment_reactions(&self, id: u64, prio: Priority) -> Result<Vec<Reaction>, Error> {
        self.list(&format!("/issues/comments/{id}/reactions"), prio, None)
    }

    /// A pull request of another repository the installation can read.
    pub fn foreign_pull(&self, full: &str, n: u64, prio: Priority) -> Result<Pull, Error> {
        let Some(r) = RepoName::parse(full) else {
            return Err(Error::NotFound {
                method: "GET".into(),
                path: full.into(),
            });
        };
        self.client.get_json(
            &format!(
                "/repos/{}/{}/pulls/{n}",
                enc_path(&r.owner),
                enc_path(&r.name)
            ),
            auth_inst(self.installation),
            prio,
        )
    }

    pub fn check_runs(&self, sha: &str, prio: Priority) -> Result<Vec<CheckRunJson>, Error> {
        self.list(
            &format!("/commits/{}/check-runs?filter=all", enc_path(sha)),
            prio,
            Some("check_runs"),
        )
    }

    pub fn check_suites(&self, sha: &str, prio: Priority) -> Result<Vec<CheckSuiteJson>, Error> {
        self.checked_list(
            &format!("/commits/{}/check-suites", enc_path(sha)),
            prio,
            "check_suites",
        )
    }

    pub fn check_suite_runs(&self, suite: u64, prio: Priority) -> Result<Vec<CheckRunJson>, Error> {
        self.checked_list(
            &format!("/check-suites/{suite}/check-runs?filter=all"),
            prio,
            "check_runs",
        )
    }

    /// Reads each listed suite and verifies the head and app identity of each run. A
    /// paginated remote read cannot by itself make a concurrent GitHub snapshot atomic.
    pub fn validated_suite_runs(
        &self,
        sha: &str,
        prio: Priority,
    ) -> Result<Vec<(CheckSuiteJson, CheckRunJson)>, Error> {
        let invalid = |detail: &str| Error::Decode {
            path: "check identity".into(),
            detail: detail.into(),
        };
        let mut suites_seen = std::collections::BTreeSet::new();
        let mut runs_seen = std::collections::BTreeSet::new();
        let mut result = Vec::new();
        for suite in self.check_suites(sha, prio)? {
            let app = suite
                .app
                .as_ref()
                .ok_or_else(|| invalid("suite app missing"))?;
            if suite.id == 0
                || !suites_seen.insert(suite.id)
                || suite.head_sha != sha
                || app.id.is_none_or(|id| id == 0)
                || app.slug.as_deref().is_none_or(str::is_empty)
                || suite
                    .created_at
                    .as_deref()
                    .and_then(crate::time::parse_rfc3339)
                    .is_none()
            {
                return Err(invalid("invalid suite identity"));
            }
            for run in self.check_suite_runs(suite.id, prio)? {
                let run_app = run.app.as_ref().ok_or_else(|| invalid("run app missing"))?;
                if run.id == 0
                    || !runs_seen.insert(run.id)
                    || run.head_sha != sha
                    || run.check_suite.as_ref().map(|s| s.id) != Some(suite.id)
                    || run_app.id != app.id
                    || run_app.slug != app.slug
                {
                    return Err(invalid("run does not match suite"));
                }
                result.push((suite.clone(), run));
            }
        }
        Ok(result)
    }

    /// The combined status with every page of its statuses (GitHub pages them: 30 by
    /// default, 100 at most).
    pub fn combined_status(&self, sha: &str, prio: Priority) -> Result<CombinedStatus, Error> {
        let path = self.p(&format!("/commits/{}/status?per_page=100", enc_path(sha)));
        let auth = auth_inst(self.installation);
        let first = self.client.send("GET", &path, auth, prio, None)?;
        let mut cs: CombinedStatus = first.json(&path)?;
        if let Some(next) = first.next_link() {
            let more: Vec<StatusJson> = self
                .client
                .get_paginated(&next, auth, prio, 30, |v| v.get("statuses").cloned())?;
            cs.statuses.extend(more);
        }
        Ok(cs)
    }

    /// Read a complete bounded list of the combined status contexts.
    pub fn combined_status_checked(
        &self,
        sha: &str,
        prio: Priority,
    ) -> Result<Vec<StatusJson>, Error> {
        let statuses: Vec<StatusJson> = self.checked_list(
            &format!("/commits/{}/status", enc_path(sha)),
            prio,
            "statuses",
        )?;
        let mut ids = std::collections::BTreeSet::new();
        if statuses.iter().any(|s| {
            s.id == 0
                || !ids.insert(s.id)
                || s.context.is_empty()
                || !matches!(
                    s.state.as_str(),
                    "pending" | "success" | "failure" | "error"
                )
        }) {
            return Err(Error::Decode {
                path: "combined status".into(),
                detail: "invalid status identity or state".into(),
            });
        }
        Ok(statuses)
    }

    /// The SHA a branch points to.
    pub fn branch_sha(&self, branch: &str, prio: Priority) -> Result<String, Error> {
        let r: GitRef = self.get(&format!("/git/ref/heads/{}", enc_path(branch)), prio)?;
        Ok(r.object.sha)
    }

    /// Branches whose name starts with `prefix`.
    pub fn matching_branches(&self, prefix: &str, prio: Priority) -> Result<Vec<GitRef>, Error> {
        self.list(
            &format!("/git/matching-refs/heads/{}", enc_path(prefix)),
            prio,
            None,
        )
    }

    pub fn git_commit(&self, sha: &str, prio: Priority) -> Result<GitCommit, Error> {
        self.get(&format!("/git/commits/{}", enc_path(sha)), prio)
    }

    pub fn compare(&self, base: &str, head: &str, prio: Priority) -> Result<Compare, Error> {
        self.get(
            &format!("/compare/{}...{}", enc_path(base), enc_path(head)),
            prio,
        )
    }

    /// The text of a file on a ref; `None` when it does not exist.
    pub fn file_content(
        &self,
        path: &str,
        reference: &str,
        prio: Priority,
    ) -> Result<Option<String>, Error> {
        #[derive(Deserialize)]
        struct Content {
            content: String,
        }
        match self.get::<Content>(
            &format!("/contents/{}?ref={}", enc_path(path), enc_query(reference)),
            prio,
        ) {
            Ok(c) => Ok(crate::jwt::base64_decode(&c.content)
                .map(|b| String::from_utf8_lossy(&b).into_owned())),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// `admin`, `maintain`, `write`, `triage`, `read` or `none`.
    pub fn collaborator_permission(&self, user: &str, prio: Priority) -> Result<String, Error> {
        #[derive(Deserialize)]
        struct P {
            permission: String,
        }
        let p: P = self.get(
            &format!("/collaborators/{}/permission", enc_path(user)),
            prio,
        )?;
        Ok(p.permission)
    }

    /// Classic branch protection (needs `administration: read`, which Schlange does not ask
    /// for; expect a 403 or 404 and use [`RepoApi::branch_rules`] instead).
    pub fn branch_protection(&self, branch: &str, prio: Priority) -> Result<Value, Error> {
        self.get(&format!("/branches/{}/protection", enc_path(branch)), prio)
    }

    /// Both active rulesets and classic protection, with app bindings preserved. A 404
    /// on a protected branch is ambiguous and never interpreted as absence.
    pub fn required_check_contexts(
        &self,
        branch: &str,
        prio: Priority,
    ) -> Result<Vec<RequiredContext>, Error> {
        let installation = self
            .client
            .installation_with_priority(self.installation, prio)?;
        let token_permissions = self
            .client
            .installation_token_permissions(self.installation)?;
        for permission in ["actions", "administration"] {
            if !matches!(
                installation.permissions.get(permission).map(String::as_str),
                Some("read" | "write" | "admin")
            ) {
                return Err(Error::Refused(format!(
                    "installation lacks {permission}: read"
                )));
            }
            if !matches!(
                token_permissions.get(permission).map(String::as_str),
                Some("read" | "write" | "admin")
            ) {
                return Err(Error::Refused(format!(
                    "installation token lacks {permission}: read"
                )));
            }
        }
        let path = format!("/branches/{}", enc_path(branch));
        let branch_info: Value = self.get(&path, prio)?;
        let protected = branch_info
            .get("protected")
            .and_then(Value::as_bool)
            .ok_or_else(|| Error::Decode {
                path: path.clone(),
                detail: "missing protected status".into(),
            })?;
        let mut contexts = Vec::new();
        let path = self.p(&format!(
            "/rules/branches/{}?per_page=100",
            enc_path(branch)
        ));
        let rules: Vec<BranchRule> = self.client.get_paginated_array_checked(
            &path,
            auth_inst(self.installation),
            prio,
            30,
        )?;
        for rule in rules {
            if rule.kind == "required_status_checks" {
                let items = rule
                    .parameters
                    .as_ref()
                    .and_then(|p| p.get("required_status_checks"))
                    .and_then(Value::as_array)
                    .ok_or_else(|| Error::Decode {
                        path: path.clone(),
                        detail: "missing ruleset required checks".into(),
                    })?;
                for item in items {
                    contexts.push(context_from_value(item, "integration_id").ok_or_else(|| {
                        Error::Decode {
                            path: path.clone(),
                            detail: "invalid ruleset required check".into(),
                        }
                    })?);
                }
            }
        }
        if protected {
            let protection = self.branch_protection(branch, prio)?;
            let status = protection
                .get("required_status_checks")
                .ok_or_else(|| Error::Decode {
                    path: path.clone(),
                    detail: "missing classic required_status_checks".into(),
                })?;
            if !status.is_null() {
                if let Some(checks) = status.get("checks") {
                    let checks = checks.as_array().ok_or_else(|| Error::Decode {
                        path: path.clone(),
                        detail: "invalid classic checks".into(),
                    })?;
                    for item in checks {
                        contexts.push(context_from_value(item, "app_id").ok_or_else(|| {
                            Error::Decode {
                                path: path.clone(),
                                detail: "invalid classic check".into(),
                            }
                        })?);
                    }
                } else {
                    let items = status
                        .get("contexts")
                        .and_then(Value::as_array)
                        .ok_or_else(|| Error::Decode {
                            path: path.clone(),
                            detail: "missing classic contexts".into(),
                        })?;
                    for item in items {
                        contexts.push(RequiredContext {
                            context: item
                                .as_str()
                                .ok_or_else(|| Error::Decode {
                                    path: path.clone(),
                                    detail: "invalid classic context".into(),
                                })?
                                .into(),
                            app_id: None,
                        });
                    }
                }
            }
        }
        contexts.sort();
        contexts.dedup();
        Ok(contexts)
    }

    /// The rules that apply to a branch, from rulesets (readable with `metadata: read`).
    pub fn branch_rules(&self, branch: &str, prio: Priority) -> Result<Vec<BranchRule>, Error> {
        self.list(&format!("/rules/branches/{}", enc_path(branch)), prio, None)
    }

    pub fn rulesets(&self, prio: Priority) -> Result<Vec<Ruleset>, Error> {
        self.list("/rulesets", prio, None)
    }

    pub fn ruleset(&self, id: u64, prio: Priority) -> Result<Ruleset, Error> {
        self.get(&format!("/rulesets/{id}"), prio)
    }

    // ---- the lease's fence (MQ-32): a reference outside `refs/heads/` -------------------

    /// The commit a reference outside `refs/heads/` points to (`name` without `refs/`);
    /// `None` on 404.
    pub fn fence_ref(&self, name: &str, prio: Priority) -> Result<Option<String>, Error> {
        match self.get::<GitRef>(&format!("/git/ref/{}", enc_path(name)), prio) {
            Ok(r) => Ok(Some(r.object.sha)),
            Err(Error::NotFound { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Creates a commit with this tree, parents and message, bypassing the write gate (the
    /// fence itself). Returns its SHA.
    pub fn fence_commit(
        &self,
        tree: &str,
        parents: &[String],
        message: &str,
        prio: Priority,
    ) -> Result<String, Error> {
        let path = self.p("/git/commits");
        let resp = self.client.send_unfenced(
            "POST",
            &path,
            auth_inst(self.installation),
            prio,
            Some(&json!({ "message": message, "tree": tree, "parents": parents })),
        )?;
        let c: GitCommit = resp.json(&path)?;
        Ok(c.sha)
    }

    /// Creates the fence reference; `Ok(false)` when it exists already (another process was
    /// first).
    pub fn fence_create(&self, name: &str, sha: &str, prio: Priority) -> Result<bool, Error> {
        match self.client.send_unfenced(
            "POST",
            &self.p("/git/refs"),
            auth_inst(self.installation),
            prio,
            Some(&json!({ "ref": format!("refs/{name}"), "sha": sha })),
        ) {
            Ok(_) => Ok(true),
            Err(Error::Status { status: 422, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// Moves the fence reference to `sha` only as a fast-forward (`force: false`): GitHub
    /// refuses the update when `sha` does not descend from the current commit, so of two
    /// processes that both build on the commit they read, only the first wins. `Ok(false)`
    /// when refused.
    pub fn fence_advance(&self, name: &str, sha: &str, prio: Priority) -> Result<bool, Error> {
        match self.client.send_unfenced(
            "PATCH",
            &self.p(&format!("/git/refs/{}", enc_path(name))),
            auth_inst(self.installation),
            prio,
            Some(&json!({ "sha": sha, "force": false })),
        ) {
            Ok(_) => Ok(true),
            Err(Error::Status { status: 422, .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    // ---- writes ------------------------------------------------------------------------

    pub fn create_ref(&self, branch: &str, sha: &str, prio: Priority) -> Result<(), Error> {
        self.send(
            "POST",
            "/git/refs",
            prio,
            Some(&json!({ "ref": format!("refs/heads/{branch}"), "sha": sha })),
        )
        .map(|_| ())
    }

    /// Create a ref once and retain only a matching 201 response as evidence. A malformed
    /// successful response remains ambiguous because the remote write may have happened.
    pub fn create_ref_receipt(
        &self,
        branch: &str,
        sha: &str,
        prio: Priority,
    ) -> Result<RefCreatedReceipt, Error> {
        if !self.client.has_execution_gate() {
            return Err(Error::Refused("execution gate is required".into()));
        }
        let path = self.p("/git/refs");
        let ambiguous = || Error::Ambiguous {
            method: "POST".into(),
            path: path.clone(),
            detail: "create-ref response did not prove the requested ref".into(),
        };
        let response = self
            .client
            .send_single_write(
                "POST",
                &path,
                auth_inst(self.installation),
                prio,
                &json!({ "ref": format!("refs/heads/{branch}"), "sha": sha }),
            )
            .map_err(|_| ambiguous())?;
        if response.status != 201 {
            return Err(ambiguous());
        }
        let created: GitRef = response.json(&path).map_err(|_| ambiguous())?;
        if created.ref_name != format!("refs/heads/{branch}") || created.object.sha != sha {
            return Err(ambiguous());
        }
        Ok(RefCreatedReceipt {
            repo: self.repo.full(),
            installation: self.installation,
            api: self.client.api_identity(),
            branch: branch.into(),
            sha: sha.into(),
        })
    }

    pub fn delete_ref(&self, branch: &str, prio: Priority) -> Result<(), Error> {
        match self.send(
            "DELETE",
            &format!("/git/refs/heads/{}", enc_path(branch)),
            prio,
            None,
        ) {
            Ok(_) => Ok(()),
            // 422 "Reference does not exist".
            Err(Error::Status { status: 422, .. }) => Err(Error::NotFound {
                method: "DELETE".into(),
                path: branch.into(),
            }),
            Err(e) => Err(e),
        }
    }

    /// Read, compare, then `PATCH` with `force: false` (see the module documentation for
    /// why this is not a true compare-and-swap on GitHub).
    pub fn update_ref_cas(
        &self,
        branch: &str,
        expected: &str,
        new: &str,
        prio: Priority,
    ) -> Result<(), CasError> {
        let actual = self.branch_sha(branch, prio).map_err(CasError::Api)?;
        if actual != expected {
            return Err(CasError::Stale { actual });
        }
        self.send(
            "PATCH",
            &format!("/git/refs/heads/{}", enc_path(branch)),
            prio,
            Some(&json!({ "sha": new, "force": false })),
        )
        .map(|_| ())
        .map_err(|e| match e {
            Error::Status { status: 422, .. } => CasError::NotFastForward,
            e => CasError::Api(e),
        })
    }

    /// Merges `head` (a SHA or branch) into `base` (a branch) with a merge commit.
    pub fn merge_into(
        &self,
        base: &str,
        head: &str,
        message: &str,
        prio: Priority,
    ) -> Result<MergeOutcome, Error> {
        let resp = self.send(
            "POST",
            "/merges",
            prio,
            Some(&json!({ "base": base, "head": head, "commit_message": message })),
        );
        match resp {
            Ok(r) if r.status == 204 => Ok(MergeOutcome::NothingToMerge),
            Ok(r) => {
                let c: RepoCommit = r.json("/merges")?;
                Ok(MergeOutcome::Merged {
                    sha: c.sha,
                    tree: c.commit.tree.sha,
                })
            }
            Err(Error::Status { status: 409, .. }) => Ok(MergeOutcome::Conflict),
            Err(e) => Err(e),
        }
    }

    pub fn create_pull(
        &self,
        title: &str,
        head: &str,
        base: &str,
        body: &str,
        draft: bool,
        prio: Priority,
    ) -> Result<Pull, Error> {
        self.send(
            "POST",
            "/pulls",
            prio,
            Some(&json!({ "title": title, "head": head, "base": base, "body": body, "draft": draft })),
        )?
        .json("/pulls")
    }

    pub fn close_pull(&self, n: u64, prio: Priority) -> Result<(), Error> {
        self.send(
            "PATCH",
            &format!("/pulls/{n}"),
            prio,
            Some(&json!({ "state": "closed" })),
        )
        .map(|_| ())
    }

    /// Squash-merges pull request `n` only if its head is still `sha`. A `None` title or
    /// message leaves it to GitHub (the repository's squash merge settings).
    pub fn squash_pull(
        &self,
        n: u64,
        sha: &str,
        title: Option<&str>,
        message: Option<&str>,
        prio: Priority,
    ) -> Result<PullMergeOutcome, Error> {
        self.merge_pull(n, sha, "squash", title, message, prio)
    }

    /// `PUT /pulls/{n}/merge` with a merge method (`squash`, `merge`, `rebase`) at head `sha`.
    pub fn merge_pull(
        &self,
        n: u64,
        sha: &str,
        method: &str,
        title: Option<&str>,
        message: Option<&str>,
        prio: Priority,
    ) -> Result<PullMergeOutcome, Error> {
        let mut body = json!({ "merge_method": method, "sha": sha });
        if let Some(t) = title {
            body["commit_title"] = json!(t);
        }
        if let Some(m) = message {
            body["commit_message"] = json!(m);
        }
        let resp = self.send("PUT", &format!("/pulls/{n}/merge"), prio, Some(&body));
        match resp {
            Ok(r) => {
                #[derive(Deserialize)]
                struct M {
                    sha: String,
                }
                let m: M = r.json("merge")?;
                Ok(PullMergeOutcome::Merged { sha: m.sha })
            }
            Err(Error::Status { status: 409, .. }) => Ok(PullMergeOutcome::HeadMoved),
            Err(Error::Status {
                status: 405,
                message,
                ..
            }) => Ok(PullMergeOutcome::NotMergeable(message)),
            Err(e) => Err(e),
        }
    }

    /// Creates a check run; returns its id.
    pub fn create_check_run(&self, body: &Value, prio: Priority) -> Result<u64, Error> {
        #[derive(Deserialize)]
        struct Id {
            id: u64,
        }
        let id: Id = self
            .send("POST", "/check-runs", prio, Some(body))?
            .json("/check-runs")?;
        Ok(id.id)
    }

    pub fn update_check_run(&self, id: u64, body: &Value, prio: Priority) -> Result<(), Error> {
        self.send("PATCH", &format!("/check-runs/{id}"), prio, Some(body))
            .map(|_| ())
    }

    /// Comments on an issue or pull request; returns the comment id.
    pub fn comment(&self, n: u64, body: &str, prio: Priority) -> Result<u64, Error> {
        #[derive(Deserialize)]
        struct Id {
            id: u64,
        }
        let id: Id = self
            .send(
                "POST",
                &format!("/issues/{n}/comments"),
                prio,
                Some(&json!({ "body": body })),
            )?
            .json("comments")?;
        Ok(id.id)
    }

    pub(crate) fn comment_reserved(
        &self,
        n: u64,
        body: &str,
        permit: &mut crate::client::CosmeticPermit,
    ) -> Result<u64, Error> {
        #[derive(Deserialize)]
        struct Id {
            id: u64,
        }
        let path = self.p(&format!("/issues/{n}/comments"));
        let id: Id = self
            .client
            .send_reserved_cosmetic(
                "POST",
                &path,
                auth_inst(self.installation),
                &json!({"body":body}),
                permit,
            )?
            .json("comments")?;
        Ok(id.id)
    }

    pub(crate) fn update_comment_reserved(
        &self,
        id: u64,
        body: &str,
        permit: &mut crate::client::CosmeticPermit,
    ) -> Result<(), Error> {
        let path = self.p(&format!("/issues/comments/{id}"));
        self.client
            .send_reserved_cosmetic(
                "PATCH",
                &path,
                auth_inst(self.installation),
                &json!({"body":body}),
                permit,
            )
            .map(|_| ())
    }

    pub fn update_comment(&self, id: u64, body: &str, prio: Priority) -> Result<(), Error> {
        self.send(
            "PATCH",
            &format!("/issues/comments/{id}"),
            prio,
            Some(&json!({ "body": body })),
        )
        .map(|_| ())
    }

    /// Reacts to an issue comment (`+1`, `eyes`, `rocket`, …).
    pub fn react_to_comment(
        &self,
        comment: u64,
        content: &str,
        prio: Priority,
    ) -> Result<(), Error> {
        self.send(
            "POST",
            &format!("/issues/comments/{comment}/reactions"),
            prio,
            Some(&json!({ "content": content })),
        )
        .map(|_| ())
    }

    pub fn add_labels(&self, n: u64, labels: &[&str], prio: Priority) -> Result<(), Error> {
        self.send(
            "POST",
            &format!("/issues/{n}/labels"),
            prio,
            Some(&json!({ "labels": labels })),
        )
        .map(|_| ())
    }

    pub fn add_assignees(&self, n: u64, users: &[String], prio: Priority) -> Result<(), Error> {
        self.send(
            "POST",
            &format!("/issues/{n}/assignees"),
            prio,
            Some(&json!({ "assignees": users })),
        )
        .map(|_| ())
    }

    pub fn remove_assignees(&self, n: u64, users: &[String], prio: Priority) -> Result<(), Error> {
        self.send(
            "DELETE",
            &format!("/issues/{n}/assignees"),
            prio,
            Some(&json!({ "assignees": users })),
        )
        .map(|_| ())
    }

    pub fn request_reviewers(
        &self,
        n: u64,
        users: &[String],
        teams: &[String],
        prio: Priority,
    ) -> Result<(), Error> {
        self.send(
            "POST",
            &format!("/pulls/{n}/requested_reviewers"),
            prio,
            Some(&json!({ "reviewers": users, "team_reviewers": teams })),
        )
        .map(|_| ())
    }

    /// Submits a review (`APPROVE`, `REQUEST_CHANGES`, `COMMENT`).
    pub fn create_review(
        &self,
        n: u64,
        event: &str,
        body: &str,
        prio: Priority,
    ) -> Result<(), Error> {
        self.send(
            "POST",
            &format!("/pulls/{n}/reviews"),
            prio,
            Some(&json!({ "event": event, "body": body })),
        )
        .map(|_| ())
    }

    pub fn dismiss_review(
        &self,
        n: u64,
        id: u64,
        message: &str,
        prio: Priority,
    ) -> Result<(), Error> {
        self.send(
            "PUT",
            &format!("/pulls/{n}/reviews/{id}/dismissals"),
            prio,
            Some(&json!({ "message": message, "event": "DISMISS" })),
        )
        .map(|_| ())
    }

    /// Merges the base into the pull request's head (`PUT /pulls/{n}/update-branch`), only
    /// while the head is `expected`.
    pub fn update_branch(&self, n: u64, expected: &str, prio: Priority) -> Result<(), Error> {
        self.send(
            "PUT",
            &format!("/pulls/{n}/update-branch"),
            prio,
            Some(&json!({ "expected_head_sha": expected })),
        )
        .map(|_| ())
    }

    /// Creates a commit from a tree and parents (git data API); returns its SHA.
    pub fn create_commit(
        &self,
        tree: &str,
        parents: &[String],
        message: &str,
        prio: Priority,
    ) -> Result<String, Error> {
        let c: ShaOnly = self
            .send(
                "POST",
                "/git/commits",
                prio,
                Some(&json!({ "tree": tree, "parents": parents, "message": message })),
            )?
            .json("/git/commits")?;
        Ok(c.sha)
    }

    /// Moves a branch, forcing (the actions' own head rewrites: squash).
    pub fn force_ref(&self, branch: &str, sha: &str, prio: Priority) -> Result<(), Error> {
        self.send(
            "PATCH",
            &format!("/git/refs/heads/{}", enc_path(branch)),
            prio,
            Some(&json!({ "sha": sha, "force": true })),
        )
        .map(|_| ())
    }

    /// Starts a workflow (`workflow_dispatch`).
    pub fn dispatch_workflow(
        &self,
        workflow: &str,
        reference: &str,
        inputs: &BTreeMap<String, String>,
        prio: Priority,
    ) -> Result<(), Error> {
        self.send(
            "POST",
            &format!("/actions/workflows/{}/dispatches", enc_path(workflow)),
            prio,
            Some(&json!({ "ref": reference, "inputs": inputs })),
        )
        .map(|_| ())
    }

    /// A GraphQL mutation on pull request `node` (draft state); `Err` with GraphQL's errors.
    pub fn set_draft(&self, node: &str, draft: bool, prio: Priority) -> Result<(), Error> {
        let query = if draft {
            "mutation($id: ID!) { convertPullRequestToDraft(input: {pullRequestId: $id}) { pullRequest { isDraft } } }"
        } else {
            "mutation($id: ID!) { markPullRequestReadyForReview(input: {pullRequestId: $id}) { pullRequest { isDraft } } }"
        };
        let r = self.client.send(
            "POST",
            "/graphql",
            auth_inst(self.installation),
            prio,
            Some(&json!({ "query": query, "variables": { "id": node } })),
        )?;
        let v: Value = r.json("/graphql")?;
        match v
            .get("errors")
            .and_then(|e| e.as_array())
            .and_then(|e| e.first())
        {
            Some(e) => Err(Error::Status {
                status: 422,
                message: e["message"].as_str().unwrap_or("GraphQL error").to_string(),
                method: "POST".into(),
                path: "/graphql".into(),
            }),
            None => Ok(()),
        }
    }

    /// A file of another repository of the same owner, on its default branch (`extends`).
    pub fn foreign_file(
        &self,
        repo: &str,
        path: &str,
        prio: Priority,
    ) -> Result<Option<String>, Error> {
        let other = RepoApi {
            client: self.client.clone(),
            repo: RepoName {
                owner: self.repo.owner.clone(),
                name: repo.to_string(),
            },
            installation: self.installation,
        };
        let def: Repository = other.get("", prio)?;
        other.file_content(path, &def.default_branch, prio)
    }

    pub fn remove_label(&self, n: u64, label: &str, prio: Priority) -> Result<(), Error> {
        match self.send(
            "DELETE",
            &format!("/issues/{n}/labels/{}", enc_query(label)),
            prio,
            None,
        ) {
            Ok(_) | Err(Error::NotFound { .. }) => Ok(()),
            Err(e) => Err(e),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum CasError {
    #[error("the ref is at {actual}, not at the expected commit")]
    Stale { actual: String },
    #[error("not a fast-forward")]
    NotFastForward,
    #[error(transparent)]
    Api(Error),
}

/// App-level calls (JWT).
impl Client {
    pub fn app(&self) -> Result<App, Error> {
        self.get_json("/app", Auth::App, Priority::Normal)
    }

    pub fn installation(&self, id: u64) -> Result<Installation, Error> {
        self.installation_with_priority(id, Priority::Normal)
    }

    pub fn installation_with_priority(
        &self,
        id: u64,
        prio: Priority,
    ) -> Result<Installation, Error> {
        self.get_json(&format!("/app/installations/{id}"), Auth::App, prio)
    }

    pub fn repo_installation(&self, repo: &RepoName) -> Result<Installation, Error> {
        self.get_json(
            &format!(
                "/repos/{}/{}/installation",
                enc_path(&repo.owner),
                enc_path(&repo.name)
            ),
            Auth::App,
            Priority::Normal,
        )
    }

    /// The newest webhook deliveries of the app.
    pub fn hook_deliveries(&self, per_page: u32) -> Result<Vec<HookDelivery>, Error> {
        self.get_json(
            &format!("/app/hook/deliveries?per_page={per_page}"),
            Auth::App,
            Priority::Normal,
        )
    }

    /// The core rate limit of an installation token (this call does not count against it).
    pub fn rate_limit(&self, installation: u64) -> Result<RateResource, Error> {
        #[derive(Deserialize)]
        struct Resources {
            core: RateResource,
        }
        #[derive(Deserialize)]
        struct R {
            resources: Resources,
        }
        let r: R = self.get_json(
            "/rate_limit",
            Auth::Installation(installation),
            Priority::Normal,
        )?;
        Ok(r.resources.core)
    }
}
