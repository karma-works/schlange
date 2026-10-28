//! `GithubForge`: the engine's decisions as GitHub API calls, and GitHub's state as the
//! engine's truth reads.
//!
//! - **Cars.** `BuildCar` creates the queue branch (`<prefix><10 hex>`) at the expected base
//!   (`POST /git/refs`), merges each pull request head into it with the merges API, and opens a
//!   draft pull request on it. A merge conflict deletes the branch again and replies
//!   `CarConflict`; a permanent refusal (403 without `workflows: write`, 422) replies
//!   `CarRefused`, so the pull request leaves the queue with GitHub's reason instead of
//!   blocking it (G4).
//! - **Landing** (see [`crate::api`] for why GitHub has no compare-and-swap on the base). Each
//!   pull request is squash-merged with `PUT /pulls/{n}/merge` and the tested head as `sha`.
//!   Every attempt re-validates first, from fresh reads: the pull request is open at the tested
//!   head, and the base is exactly the expected commit, read immediately before the call.
//!   Afterwards the landed commit's parent and tree are compared with the expected base and the
//!   tree the car tested for that prefix; a different tree stops the landing and the engine
//!   halts the queue (MQ-02). What is left open is the time between that base read and GitHub
//!   executing the call (one request); a push inside it is caught afterwards, not prevented.
//! - **A landing is never retried here** (G1). When the merge call fails, the adapter reports
//!   `Failed` to the engine (after a backoff), and the engine reads the pull requests and the
//!   base, checks its conditions again, and decides anew: a pull request dequeued in the
//!   meantime (a `halt` label) is never merged by a stale retry. `CloseCar` also drops anything
//!   still pending for its car.
//! - **Transient refusals** (G3): a 405 ("base branch was modified", mergeability still being
//!   computed) or 409 is retried a few times, each time after reading the pull request and the
//!   base again. A permanent refusal (403, 422, a 405 that stays) ends the landing with
//!   `LandError::Refused` and GitHub's message plus a hint: the pull request is dequeued with
//!   `PR_UNEXPECTEDLY_FAILED_TO_MERGE` instead of being retried forever (G4).
//! - **Unknown outcomes.** A write that failed with 5xx or a timeout may have happened
//!   (MQ-35). The adapter reads before it decides: a pull request that is merged after a
//!   failed merge call counts as landed (never merged twice), a branch or draft that exists
//!   after a failed create is used.
//! - **Read your writes.** GitHub's reads can lag behind writes. The adapter remembers what it
//!   wrote: a pull request it merged stays merged, a draft it closed stays closed, and a branch
//!   read that returns a commit the adapter moved the branch away from is read again (and
//!   reported as unavailable if it stays stale), so the engine's verification reads are not
//!   older than the adapter's own writes.
//! - **Provisional failures.** Reads that fail (5xx, 404 storms, rate limits) never become
//!   facts: a pull request that cannot be read is left out of the snapshot (the engine keeps
//!   what it knew), a branch is reported missing only when a canary read of the default branch
//!   works. A build, close or read that could not be carried out is kept and retried with
//!   backoff; the engine waits for its reply (W-24).
//! - **`mergeable: null`** is "not computed yet", never "no conflict": the adapter reads again
//!   a few times and otherwise retries the build later.
//! - **Cosmetics.** The queue's own check run on each queued pull request is cosmetic: it uses
//!   the lowest priority and waits when the budget is low. Check runs of Schlange's own app
//!   are ignored when reading checks, so they never feed back into conditions.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use schlange_config::rules::{MergeMethod, Rules};
use schlange_core::engine::{
    CarPr, CommitMessage, Cosmetic as EngineCosmetic, LandError, LandedCommit, short,
};
use schlange_core::model::{CheckRun, Comment, Dependency, DependencyState, PullRequest, Time};
use schlange_core::{Decision, Event, State};

use crate::api::{MergeOutcome, Pull, PullMergeOutcome, RepoApi};
use crate::client::{Error, Priority};
use crate::store::{AuditRecord, Store};
use crate::time::Clock;
use crate::webhook::{conclusion, status_conclusion};

#[derive(Clone, Debug)]
pub struct ForgeOptions {
    /// The app's bot login (`<slug>[bot]`): author of the queue's draft pull requests.
    pub bot_login: String,
    /// The app's slug: its own check runs are ignored when reading checks.
    pub app_slug: String,
    /// Name of the queue's own check run on queued pull requests.
    pub check_name: String,
}

#[derive(Clone, Debug, Default)]
struct CarInfo {
    /// Per pull request in the car: number and the tree after merging it.
    prefix: Vec<(u64, String)>,
}

struct PendingOp {
    decision: Decision,
    attempts: u32,
    next: Instant,
    last_error: String,
}

/// A failed landing, reported to the engine once its backoff has passed.
struct DelayedReply {
    car: u64,
    due: Instant,
    event: Event,
}

#[derive(Clone, Debug)]
enum Cosmetic {
    Start {
        number: u64,
        queue: String,
    },
    Update {
        number: u64,
        summary: String,
    },
    Finish {
        number: u64,
        conclusion: &'static str,
        title: String,
        summary: String,
    },
    /// Labels, comments, reactions and protection checks the engine decided.
    Show(EngineCosmetic),
}

/// Is `name` a queue branch: `prefix` plus exactly ten lower-case hex characters?
pub fn is_queue_branch(name: &str, prefix: &str) -> bool {
    name.strip_prefix(prefix).is_some_and(|rest| {
        rest.len() == 10 && rest.chars().all(|c| matches!(c, '0'..='9' | 'a'..='f'))
    })
}

pub fn queue_prefixes(rules: &Rules) -> BTreeSet<String> {
    rules
        .queue_rules
        .iter()
        .map(|q| q.queue_branch_prefix.clone())
        .collect()
}

/// How long a branch tip the adapter moved away from counts as a stale read.
const STALE_WINDOW: Duration = Duration::from_secs(300);
const CLOSED_CARS_MAX: usize = 4096;

pub struct GithubForge {
    pub api: RepoApi,
    clock: Arc<dyn Clock>,
    pub opts: ForgeOptions,
    cars: BTreeMap<u64, CarInfo>,
    pending: Vec<PendingOp>,
    delayed: Vec<DelayedReply>,
    /// Failed landings in a row per car (backoff).
    land_failures: BTreeMap<u64, u32>,
    /// Cars the engine closed: nothing is carried out for them any more.
    closed_cars: BTreeSet<u64>,
    closed_order: VecDeque<u64>,
    cosmetics: VecDeque<Cosmetic>,
    queue_checks: BTreeMap<u64, u64>,
    heads: RefCell<BTreeMap<u64, String>>,
    default_branch: RefCell<Option<String>>,
    /// Read your writes: per branch, the commits the adapter moved it away from, and when.
    left_tips: BTreeMap<String, Vec<(String, Instant)>>,
    /// Pull requests the adapter merged (merge commit), and drafts it closed.
    known_merged: BTreeMap<u64, String>,
    known_closed: BTreeSet<u64>,
    /// What the adapter did, newest last (bounded).
    pub log: VecDeque<String>,
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Attempts for a transient 405/409 on the merge call, each after fresh reads.
    pub transient_retries: u32,
    /// Reads of `mergeable` while GitHub still computes it.
    pub mergeable_polls: u32,
    /// Car prefix trees across restarts, and the audit of every landing.
    pub store: Option<Store>,
    /// Read the reviews of each pull request (only when a queue rule asks for the
    /// `approved-by` trailer).
    pub read_reviews: bool,
    /// The validator `serve` loads the configuration with (schema and feature map), also used
    /// for the "configuration changed" check; without it (tests of the adapter alone) the
    /// check runs stages 1, 3 and 4.
    pub check: Option<Arc<crate::config::ConfigCheck>>,
    /// The queue's own status and controls comments: (pull request, marker) → comment id.
    own_comments: BTreeMap<(u64, &'static str), u64>,
}

const LOG_MAX: usize = 2000;

fn dependabot_update_types(messages: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for m in messages {
        for line in m.lines() {
            if let Some(v) = line.trim().strip_prefix("update-type:") {
                let v = v.trim().to_string();
                if !v.is_empty() && !out.contains(&v) {
                    out.push(v);
                }
            }
        }
    }
    out
}

/// GitHub's refusal as a dequeue text, with a hint for the operator.
pub fn refusal_text(status: u16, message: &str) -> String {
    let lower = message.to_ascii_lowercase();
    let hint = if lower.contains("workflow") {
        "the app needs `workflows: write` to merge changes under .github/workflows/ (grant it, or merge such pull requests by hand; `schlange doctor` lists the permissions)"
    } else if status == 405 || lower.contains("protect") || lower.contains("rule") {
        "branch protection or a ruleset blocks the app; it must be allowed to bypass them (`schlange doctor`, check ruleset-exception)"
    } else if status == 422 {
        "GitHub rejected the request; check the repository's merge settings (squash merging must be allowed)"
    } else if status == 403 {
        "the app lacks a permission for this (`schlange doctor`, check app-permissions)"
    } else {
        "see the service log"
    };
    format!("GitHub refused the merge (HTTP {status}): {message}. Hint: {hint}")
}

impl GithubForge {
    pub fn new(api: RepoApi, clock: Arc<dyn Clock>, opts: ForgeOptions) -> Self {
        GithubForge {
            api,
            clock,
            opts,
            cars: BTreeMap::new(),
            pending: Vec::new(),
            delayed: Vec::new(),
            land_failures: BTreeMap::new(),
            closed_cars: BTreeSet::new(),
            closed_order: VecDeque::new(),
            cosmetics: VecDeque::new(),
            queue_checks: BTreeMap::new(),
            heads: RefCell::new(BTreeMap::new()),
            default_branch: RefCell::new(None),
            left_tips: BTreeMap::new(),
            known_merged: BTreeMap::new(),
            known_closed: BTreeSet::new(),
            log: VecDeque::new(),
            retry_base: Duration::from_secs(2),
            retry_max: Duration::from_secs(60),
            transient_retries: 3,
            mergeable_polls: 6,
            store: None,
            read_reviews: false,
            check: None,
            own_comments: BTreeMap::new(),
        }
    }

    /// The head of a pull request as last read.
    pub fn head_of(&self, n: u64) -> Option<String> {
        self.heads.borrow().get(&n).cloned()
    }

    /// Validates a configuration text: with the schema all five stages (support findings are
    /// notes), else without it.
    fn validate_config(&self, file: &str, text: &str) -> Vec<schlange_config::validate::Finding> {
        match &self.check {
            Some(c) => {
                schlange_config::validate(file, text, &c.schema, &c.features, false).findings
            }
            None => schlange_config::config_file::validate_without_schema(file, text),
        }
    }

    /// Resolves the `Depends-On` references of a body.
    fn dependencies(&self, body: &str, prio: Priority) -> Vec<Dependency> {
        let own = self.api.repo.full();
        let (refs, _) = schlange_core::protections::depends_on(body);
        refs.iter()
            .take(schlange_core::protections::MAX_DEPENDENCIES)
            .map(|r| {
                let reference = r.full(&own);
                let state = if r.repo.as_ref().is_some_and(|x| {
                    !x.split('/')
                        .next()
                        .unwrap_or("")
                        .eq_ignore_ascii_case(&self.api.repo.owner)
                }) {
                    DependencyState::OtherOwner
                } else {
                    let read = if r.is_local(&own) {
                        self.api.pull(r.number, prio)
                    } else {
                        self.api
                            .foreign_pull(r.repo.as_deref().unwrap_or(""), r.number, prio)
                    };
                    match read {
                        Ok(p) if p.merged => DependencyState::Merged,
                        Ok(p) if p.state == "closed" => DependencyState::Closed,
                        Ok(_) => DependencyState::Open,
                        Err(_) => DependencyState::Missing,
                    }
                };
                Dependency { reference, state }
            })
            .collect()
    }

    pub fn note(&mut self, text: impl Into<String>) {
        let text = text.into();
        eprintln!("schlange[{}]: {text}", self.api.repo.full());
        if self.log.len() >= LOG_MAX {
            self.log.pop_front();
        }
        self.log.push_back(text);
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len() + self.delayed.len()
    }

    pub fn cosmetic_count(&self) -> usize {
        self.cosmetics.len()
    }

    /// A short pause between transient retries and stale re-reads.
    fn pause(&self) -> Duration {
        (self.retry_base / 2).min(Duration::from_secs(2))
    }

    // ---- read your writes --------------------------------------------------------------

    /// The adapter moved `branch` from `from` to `to`.
    fn moved(&mut self, branch: &str, from: &str) {
        let now = Instant::now();
        let list = self.left_tips.entry(branch.to_string()).or_default();
        list.retain(|(_, at)| now.duration_since(*at) < STALE_WINDOW);
        list.push((from.to_string(), now));
        if list.len() > 64 {
            list.remove(0);
        }
    }

    fn is_left_tip(&self, branch: &str, sha: &str) -> bool {
        self.left_tips.get(branch).is_some_and(|l| {
            l.iter()
                .any(|(s, at)| s == sha && at.elapsed() < STALE_WINDOW)
        })
    }

    fn closed_car(&mut self, car: u64) {
        if self.closed_cars.insert(car) {
            self.closed_order.push_back(car);
            if self.closed_order.len() > CLOSED_CARS_MAX
                && let Some(old) = self.closed_order.pop_front()
            {
                self.closed_cars.remove(&old);
            }
        }
    }

    // ---- reads -------------------------------------------------------------------------

    pub fn default_branch(&self, prio: Priority) -> Result<String, Error> {
        if let Some(b) = self.default_branch.borrow().clone() {
            return Ok(b);
        }
        let b = self.api.repository(prio)?.default_branch;
        *self.default_branch.borrow_mut() = Some(b.clone());
        Ok(b)
    }

    fn is_queue_head(&self, head_ref: &str, prefixes: &BTreeSet<String>) -> bool {
        prefixes.iter().any(|p| head_ref.starts_with(p.as_str()))
    }

    /// A pull request as GitHub returns it, corrected by what the adapter wrote itself (merged
    /// and closed are final; a read that says otherwise lags behind).
    fn pull(&self, n: u64, prio: Priority) -> Result<Pull, Error> {
        let mut p = self.api.pull(n, prio)?;
        if !p.merged
            && let Some(sha) = self.known_merged.get(&n)
        {
            p.merged = true;
            p.state = "closed".into();
            p.merge_commit_sha = Some(sha.clone());
        }
        if self.known_closed.contains(&n) {
            p.state = "closed".into();
        }
        Ok(p)
    }

    /// Reads a pull request with everything the engine evaluates.
    pub fn read_pr(&self, n: u64, prio: Priority) -> Result<PullRequest, Error> {
        let p = self.pull(n, prio)?;
        self.pr_from(&p, prio)
    }

    fn pr_from(&self, p: &Pull, prio: Priority) -> Result<PullRequest, Error> {
        let n = p.number;
        let mut pr = PullRequest::new(
            n,
            &p.user.login,
            &p.base.ref_name,
            &p.head.ref_name,
            &p.head.sha,
        );
        pr.title = p.title.clone();
        pr.draft = p.draft;
        pr.closed = p.state == "closed";
        pr.merged = p.merged;
        pr.merge_commit_sha = p.merge_commit_sha.clone().filter(|_| p.merged);
        pr.labels = p.labels.iter().map(|l| l.name.clone()).collect();
        self.heads.borrow_mut().insert(n, p.head.sha.clone());
        if pr.closed {
            return Ok(pr);
        }
        let ours = p.user.login == self.opts.bot_login;
        let head = p.head.sha.clone();
        for c in self.api.check_runs(&head, prio)? {
            let app = c.app.as_ref().and_then(|a| a.slug.clone());
            if app.as_deref() == Some(self.opts.app_slug.as_str()) {
                continue;
            }
            pr.checks.push(CheckRun {
                name: c.name.clone(),
                app,
                head_sha: c.head_sha.clone(),
                conclusion: conclusion(&c.status, c.conclusion.as_deref()),
                seq: c.id,
                ..Default::default()
            });
        }
        // Every page of the combined status (more than 30 contexts are common in monorepos).
        for s in self.api.combined_status(&head, prio)?.statuses {
            pr.checks.push(CheckRun {
                name: s.context.clone(),
                app: None,
                head_sha: head.clone(),
                conclusion: status_conclusion(&s.state),
                seq: s.id,
                status: true,
                ..Default::default()
            });
        }
        pr.head_tree = Some(self.api.git_commit(&head, prio)?.tree.sha);
        if ours {
            // The queue's own draft: only its checks and tree matter.
            return Ok(pr);
        }
        pr.files = self.api.pull_files(n, prio)?;
        let tip = self.branch_tip(&p.base.ref_name, prio)?;
        let cmp = self.api.compare(&tip, &head, prio)?;
        pr.behind_base = cmp.behind_by > 0;
        pr.base_tip = Some(tip);
        pr.body = p.body.clone().unwrap_or_default();
        pr.from_fork = match (&p.head.repo, &p.base.repo) {
            (Some(h), Some(b)) => h.full_name != b.full_name,
            // A deleted head repository counts as a fork (docs: `from-fork`).
            (None, _) => true,
            (Some(_), None) => false,
        };
        let commits = self.api.pull_commits(n, prio)?;
        pr.commit_authors = commits
            .iter()
            .filter_map(|c| c.commit.author.as_ref())
            .map(|a| format!("{} <{}>", a.name, a.email))
            .collect();
        pr.head_pusher = commits
            .last()
            .and_then(|c| c.author.as_ref())
            .map(|u| u.login.clone());
        pr.head_by_queue = pr.head_pusher.as_deref() == Some(self.opts.bot_login.as_str());
        if p.user.login == "dependabot[bot]" {
            let msgs: Vec<String> = commits.iter().map(|c| c.commit.message.clone()).collect();
            pr.dependabot_update_types = dependabot_update_types(&msgs);
        }
        if self.read_reviews {
            let mut latest: BTreeMap<String, String> = BTreeMap::new();
            for r in self.api.reviews(n, prio)? {
                if let Some(u) = r.user {
                    // Comments do not change a reviewer's verdict.
                    if r.state != "COMMENTED" {
                        latest.insert(u.login, r.state);
                    }
                }
            }
            pr.approved_by = latest
                .into_iter()
                .filter(|(_, s)| s == "APPROVED")
                .map(|(u, _)| u)
                .collect();
        }
        pr.dependencies = self.dependencies(&pr.body, prio);
        if p.base.ref_name == self.default_branch(prio)?
            && pr
                .files
                .iter()
                .any(|f| schlange_config::config_file::is_config_path(f))
        {
            let mut texts = BTreeMap::new();
            for name in schlange_config::config_file::NAMES {
                if let Some(t) = self.api.file_content(name, &head, prio)? {
                    texts.insert(name.to_string(), t);
                }
            }
            pr.config_change = schlange_config::config_file::assess_change(
                &pr.files,
                |path| texts.get(path).cloned(),
                |file, text| self.validate_config(file, text),
            );
        }
        Ok(pr)
    }

    /// A branch's SHA, read again while it shows a commit the adapter moved the branch away
    /// from (read lag); `Unavailable` when it stays stale.
    fn branch_tip(&self, name: &str, prio: Priority) -> Result<String, Error> {
        for _ in 0..5 {
            let sha = self.api.branch_sha(name, prio)?;
            if !self.is_left_tip(name, &sha) {
                return Ok(sha);
            }
            std::thread::sleep(self.pause() / 2);
        }
        Err(Error::Unavailable {
            method: "GET".into(),
            path: format!("/git/ref/heads/{name}"),
            detail: "the read still shows a commit the queue moved the branch away from (stale)"
                .into(),
        })
    }

    /// A branch's SHA; `Ok(None)` only when GitHub says 404 *and* a canary read of the
    /// default branch works (a 404 storm is not taken for a deleted branch, W-24).
    pub fn read_branch_checked(&self, name: &str, prio: Priority) -> Result<Option<String>, Error> {
        match self.branch_tip(name, prio) {
            Ok(s) => Ok(Some(s)),
            Err(e @ Error::NotFound { .. }) => {
                let default = self.default_branch(prio)?;
                if default == name {
                    return Err(e);
                }
                self.api.branch_sha(&default, prio)?;
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// A snapshot of exactly these pull requests and branches, or an error when any read
    /// failed (for `Refresh`, whose reply the engine acts on).
    pub fn snapshot(
        &self,
        numbers: &[u64],
        branches: &[String],
        token: Option<u64>,
        prio: Priority,
    ) -> Result<Event, Error> {
        let mut prs = Vec::new();
        for n in numbers {
            prs.push(self.read_pr(*n, prio)?);
        }
        let mut b = BTreeMap::new();
        for name in branches {
            b.insert(name.clone(), self.read_branch_checked(name, prio)?);
        }
        Ok(Event::Snapshot {
            pull_requests: prs,
            branches: b,
            token,
        })
    }

    /// A snapshot that leaves out whatever could not be read (for hints and reconciliation).
    pub fn snapshot_partial(&mut self, numbers: &[u64], branches: &[String]) -> Event {
        let mut prs = Vec::new();
        let mut failed = Vec::new();
        for n in numbers {
            match self.read_pr(*n, Priority::Normal) {
                Ok(p) => prs.push(p),
                Err(e) => failed.push(format!("#{n}: {e}")),
            }
        }
        let mut b = BTreeMap::new();
        for name in branches {
            match self.read_branch_checked(name, Priority::Normal) {
                Ok(s) => {
                    b.insert(name.clone(), s);
                }
                Err(e) => failed.push(format!("{name}: {e}")),
            }
        }
        for f in failed {
            self.note(format!("read left out (provisional): {f}"));
        }
        Event::Snapshot {
            pull_requests: prs,
            branches: b,
            token: None,
        }
    }

    /// The full reconciliation read: every open pull request, the ones the engine knows,
    /// base branches, the engine's branches and every queue branch on GitHub. Also returns
    /// what the orphan sweep needs.
    pub fn reconcile(&mut self, state: &State, rules: &Rules) -> Result<(Event, Sweep), Error> {
        let open = self.api.open_pulls(Priority::Normal)?;
        let prefixes = queue_prefixes(rules);
        let mut queue_refs = Vec::new();
        for p in &prefixes {
            for r in self.api.matching_branches(p, Priority::Normal)? {
                if let Some(name) = r.ref_name.strip_prefix("refs/heads/") {
                    queue_refs.push(name.to_string());
                }
            }
        }
        let mut numbers: BTreeSet<u64> = open.iter().map(|p| p.number).collect();
        numbers.extend(
            state
                .pull_requests
                .values()
                .filter(|p| !p.closed && !p.merged)
                .map(|p| p.number),
        );
        numbers.extend(state.queued.keys().copied());
        let mut branches: BTreeSet<String> = state.trains.keys().cloned().collect();
        branches.extend(state.branches.keys().cloned());
        branches.extend(
            open.iter()
                .filter(|p| !self.is_queue_head(&p.head.ref_name, &prefixes))
                .map(|p| p.base.ref_name.clone()),
        );
        branches.extend(queue_refs.iter().cloned());
        if let Ok(d) = self.default_branch(Priority::Normal) {
            branches.insert(d);
        }
        let numbers: Vec<u64> = numbers.into_iter().collect();
        let branches: Vec<String> = branches.into_iter().collect();
        let ev = self.snapshot_partial(&numbers, &branches);
        let drafts = open
            .iter()
            .filter(|p| {
                p.user.login == self.opts.bot_login
                    && self.is_queue_head(&p.head.ref_name, &prefixes)
                    && !self.known_closed.contains(&p.number)
            })
            .map(|p| (p.number, p.head.ref_name.clone()))
            .collect();
        Ok((
            ev,
            Sweep {
                queue_refs,
                drafts,
                prefixes,
            },
        ))
    }

    /// Deletes queue branches and closes queue drafts that belong to no car of the engine
    /// (left over after a restart, a lost delete, or a 404 during cleanup). I3 on GitHub.
    pub fn sweep(&mut self, sweep: &Sweep, state: &State) {
        let mut branches = BTreeSet::new();
        let mut drafts = BTreeSet::new();
        for t in state.trains.values() {
            for c in &t.cars {
                branches.extend(c.branch.clone());
                drafts.extend(c.draft);
            }
        }
        for p in &self.pending {
            if let Decision::BuildCar { branch, .. } = &p.decision {
                branches.insert(branch.clone());
            }
        }
        for (n, head) in &sweep.drafts {
            if !drafts.contains(n) && !branches.contains(head) {
                match self.api.close_pull(*n, Priority::Normal) {
                    Ok(()) => {
                        self.known_closed.insert(*n);
                        self.note(format!("closed orphan queue draft #{n}"))
                    }
                    Err(e) => self.note(format!("closing orphan draft #{n} failed: {e}")),
                }
            }
        }
        for b in &sweep.queue_refs {
            let ours = sweep.prefixes.iter().any(|p| is_queue_branch(b, p));
            if ours && !branches.contains(b) {
                match self.api.delete_ref(b, Priority::Normal) {
                    Ok(()) | Err(Error::NotFound { .. }) => {
                        self.note(format!("deleted orphan queue branch {b}"))
                    }
                    Err(e) => self.note(format!("deleting orphan branch {b} failed: {e}")),
                }
            }
        }
    }

    // ---- decisions ---------------------------------------------------------------------

    fn backoff(&self, attempts: u32) -> Duration {
        self.retry_base
            .saturating_mul(1 << attempts.min(10))
            .min(self.retry_max)
    }

    fn defer(&mut self, decision: &Decision, e: Error) {
        self.note(format!(
            "{} deferred, will retry: {e}",
            decision_name(decision)
        ));
        let base = self.retry_base;
        self.pending.push(PendingOp {
            decision: decision.clone(),
            attempts: 1,
            next: Instant::now() + base,
            last_error: e.to_string(),
        });
    }

    /// Retries deferred decisions that are due and releases due landing failures; returns
    /// the replies for the engine.
    pub fn retry_pending(&mut self) -> Vec<Event> {
        let now = Instant::now();
        let mut out = Vec::new();
        let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.delayed)
            .into_iter()
            .partition(|d| d.due <= now);
        self.delayed = rest;
        out.extend(due.into_iter().map(|d| d.event));
        let due: Vec<PendingOp> = {
            let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|p| p.next <= now);
            self.pending = rest;
            due
        };
        for mut op in due {
            if decision_car(&op.decision).is_some_and(|c| self.closed_cars.contains(&c))
                && !matches!(op.decision, Decision::CloseCar { .. })
            {
                continue;
            }
            match self.try_execute(&op.decision) {
                Ok(events) => {
                    self.note(format!(
                        "{} succeeded after {} attempt(s)",
                        decision_name(&op.decision),
                        op.attempts + 1
                    ));
                    out.extend(events);
                }
                Err(e) => {
                    if op.attempts < 5 || op.attempts.is_power_of_two() {
                        self.note(format!(
                            "{} failed again (attempt {}): {e}",
                            decision_name(&op.decision),
                            op.attempts + 1
                        ));
                    }
                    op.attempts += 1;
                    op.next = Instant::now() + self.backoff(op.attempts);
                    op.last_error = e.to_string();
                    self.pending.push(op);
                }
            }
        }
        out
    }

    /// Deferred decisions with their last error (for status and doctor).
    pub fn pending_summary(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .pending
            .iter()
            .map(|p| {
                format!(
                    "{} (attempt {}): {}",
                    decision_name(&p.decision),
                    p.attempts,
                    p.last_error
                )
            })
            .collect();
        out.extend(self.delayed.iter().map(|d| {
            format!(
                "land car {}: failed, the queue reads and decides again after a backoff",
                d.car
            )
        }));
        out
    }

    /// Makes the next retry of deferred decisions due now (after a reconciliation).
    pub fn hurry_pending(&mut self) {
        let now = Instant::now();
        for p in &mut self.pending {
            p.next = p.next.min(now);
        }
        for d in &mut self.delayed {
            d.due = d.due.min(now);
        }
    }

    fn try_execute(&mut self, d: &Decision) -> Result<Vec<Event>, Error> {
        match d {
            Decision::BuildCar {
                car,
                base_branch,
                branch,
                base_sha,
                prs,
                own,
                stack_on,
                ..
            } => self
                .build_car(
                    *car,
                    base_branch,
                    branch,
                    base_sha,
                    prs,
                    own,
                    stack_on.as_deref(),
                )
                .map(|e| vec![e]),
            // The engine keeps the car's slot until the close is confirmed (I4). The draft's CI
            // stops through the repository's own `cancel-in-progress` setup; the adapter
            // confirms once the draft is closed and the branch is gone.
            Decision::CloseCar { car, branch, draft } => self
                .close_car(*car, branch.as_deref(), *draft)
                .map(|_| vec![Event::CarClosed { car: *car }]),
            Decision::Refresh {
                token,
                numbers,
                branches,
            } => self
                .snapshot(numbers, branches, Some(*token), Priority::Landing)
                .map(|e| vec![e]),
            Decision::Land {
                car,
                base_branch,
                expected_base_sha,
                prs,
                expected_tree,
                messages,
                method,
                head,
                draft,
                ..
            } => if method.lands_branch() {
                self.land_branch(
                    *car,
                    base_branch,
                    expected_base_sha,
                    prs,
                    expected_tree,
                    *method,
                    head.as_deref().unwrap_or_default(),
                    *draft,
                )
            } else {
                self.land(
                    *car,
                    base_branch,
                    expected_base_sha,
                    prs,
                    expected_tree,
                    messages,
                    *method,
                )
            }
            .map(|e| vec![e]),
            _ => Ok(vec![]),
        }
    }

    /// Carries out one decision. Failures of builds, closes and reads are deferred (retried);
    /// a failed landing goes back to the engine, which re-validates before landing again.
    pub fn execute(&mut self, d: &Decision) -> Vec<Event> {
        match d {
            Decision::Land { car, .. } => {
                let car = *car;
                if self.closed_cars.contains(&car) {
                    self.note(format!("land car {car}: the car is closed; not landing"));
                    return vec![];
                }
                match self.try_execute(d) {
                    Ok(events) => {
                        self.land_failures.remove(&car);
                        events
                    }
                    Err(e) => {
                        let k = self.land_failures.entry(car).or_insert(0);
                        *k += 1;
                        let k = *k;
                        let delay = self.backoff(k - 1);
                        self.note(format!(
                            "land car {car} failed ({e}); not retried here: the queue reads and decides again in {} ms",
                            delay.as_millis()
                        ));
                        self.delayed.push(DelayedReply {
                            car,
                            due: Instant::now() + delay,
                            event: Event::Failed {
                                decision: d.clone(),
                                error: e.to_string(),
                            },
                        });
                        vec![]
                    }
                }
            }
            Decision::BuildCar { .. } | Decision::Refresh { .. } | Decision::CloseCar { .. } => {
                if let Decision::CloseCar { car, .. } = d {
                    // Whatever is still pending for this car (a build, a landing failure to
                    // report) is dropped: the engine is done with it (G1).
                    let car = *car;
                    self.closed_car(car);
                    self.pending
                        .retain(|p| decision_car(&p.decision) != Some(car));
                    self.delayed.retain(|r| r.car != car);
                    self.land_failures.remove(&car);
                }
                match self.try_execute(d) {
                    Ok(events) => events,
                    Err(e) => {
                        self.defer(d, e);
                        vec![]
                    }
                }
            }
            Decision::CheckInPlace { car, .. } => {
                self.cars.insert(*car, CarInfo::default());
                vec![]
            }
            Decision::Enqueue { number, queue, .. } => {
                self.cosmetics.push_back(Cosmetic::Start {
                    number: *number,
                    queue: queue.clone(),
                });
                vec![]
            }
            Decision::Dequeue { number, reason, .. } => {
                self.cosmetics.push_back(Cosmetic::Finish {
                    number: *number,
                    conclusion: "neutral",
                    title: format!("Left the queue: {}", reason.code.unwrap_or("dequeued")),
                    summary: reason.describe(),
                });
                vec![]
            }
            Decision::ChecksEnded { number, reason, .. } => {
                self.cosmetics.push_back(Cosmetic::Update {
                    number: *number,
                    summary: format!("Still queued; checks ended: {}", reason.describe()),
                });
                vec![]
            }
            Decision::Merged { number, sha, .. } => {
                self.cosmetics.push_back(Cosmetic::Finish {
                    number: *number,
                    conclusion: "success",
                    title: "Merged by the queue".into(),
                    summary: format!("Landed as {sha}."),
                });
                vec![]
            }
            Decision::EvaluationError { number, message } => {
                self.note(format!("#{number}: condition not evaluable: {message}"));
                vec![]
            }
            Decision::Halt {
                base_branch,
                reason,
            } => {
                self.note(format!("HALT queue of {base_branch}: {reason}"));
                vec![]
            }
            Decision::WakeAt { .. } => vec![],
            Decision::Cosmetic { cosmetic } => {
                self.cosmetics.push_back(Cosmetic::Show(cosmetic.clone()));
                vec![]
            }
        }
    }

    /// Whether pull request `n` conflicts with the base alone. `mergeable: null` means GitHub
    /// has not computed it yet: read again a few times; `None` when it stays unknown.
    fn conflicts_with_base(&self, n: u64, prio: Priority) -> Option<bool> {
        for i in 0..self.mergeable_polls.max(1) {
            if i > 0 {
                std::thread::sleep(self.pause());
            }
            match self.api.pull(n, prio) {
                Ok(p) => {
                    if let Some(m) = p.mergeable {
                        return Some(!m);
                    }
                }
                Err(_) => return None,
            }
        }
        None
    }

    #[allow(clippy::too_many_arguments)]
    fn build_car(
        &mut self,
        car: u64,
        base_branch: &str,
        branch: &str,
        base_sha: &str,
        prs: &[CarPr],
        own: &[u64],
        stack_on: Option<&str>,
    ) -> Result<Event, Error> {
        let prio = Priority::Normal;
        // `fast-forward`, `merge-batch`: the branch starts at the car ahead's head and only
        // this car's own pull requests are merged into it.
        let start = stack_on.unwrap_or(base_sha);
        // A branch left by an earlier attempt is rebuilt from scratch.
        match self.api.branch_sha(branch, prio) {
            Ok(_) => {
                self.api.delete_ref(branch, prio).or_else(|e| match e {
                    Error::NotFound { .. } => Ok(()),
                    e => Err(e),
                })?;
            }
            Err(Error::NotFound { .. }) => {}
            Err(e) => return Err(e),
        }
        match self.api.create_ref(branch, start, prio) {
            Ok(()) => {}
            Err(e @ (Error::Ambiguous { .. } | Error::Status { status: 422, .. })) => {
                // Did it happen? Use it when it points at the start.
                if self.api.branch_sha(branch, prio).ok().as_deref() != Some(start) {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
        let mut cur = start.to_string();
        let mut tree = self.api.git_commit(start, prio)?.tree.sha;
        let mut prefix = Vec::new();
        for (i, p) in prs.iter().enumerate() {
            if stack_on.is_some() && !own.contains(&p.number) {
                continue; // carried by the branch it is stacked on
            }
            let msg = format!("Merge #{} into {branch}", p.number);
            let outcome = match self.api.merge_into(branch, &p.head_sha, &msg, prio) {
                Ok(o) => o,
                Err(Error::Ambiguous { .. }) => {
                    // Read the branch: a merge commit of cur and the head means it happened.
                    let tip = self.api.branch_sha(branch, prio)?;
                    let c = self.api.git_commit(&tip, prio)?;
                    let parents: Vec<&str> = c.parents.iter().map(|x| x.sha.as_str()).collect();
                    if tip != cur && parents == [cur.as_str(), p.head_sha.as_str()] {
                        MergeOutcome::Merged {
                            sha: tip,
                            tree: c.tree.sha,
                        }
                    } else if tip == cur {
                        self.api.merge_into(branch, &p.head_sha, &msg, prio)?
                    } else {
                        let _ = self.api.delete_ref(branch, prio);
                        return Err(Error::Unavailable {
                            method: "POST".into(),
                            path: "/merges".into(),
                            detail: format!("queue branch {branch} moved during the build"),
                        });
                    }
                }
                Err(Error::Status {
                    status: status @ (403 | 422),
                    message,
                    ..
                }) => {
                    // GitHub refuses this pull request's changes for good (a workflow file
                    // without `workflows: write`, a validation error): it leaves the queue
                    // with the reason; the others get a new car.
                    let _ = self.api.delete_ref(branch, prio);
                    let text = refusal_text(status, &message);
                    self.note(format!("car {car}: #{} refused: {text}", p.number));
                    return Ok(Event::CarRefused {
                        car,
                        number: p.number,
                        message: text,
                    });
                }
                Err(e) => {
                    let _ = self.api.delete_ref(branch, prio);
                    return Err(e);
                }
            };
            match outcome {
                MergeOutcome::Merged { sha, tree: t } => {
                    self.moved(branch, &cur);
                    cur = sha;
                    tree = t;
                }
                MergeOutcome::NothingToMerge => {}
                MergeOutcome::Conflict => {
                    let _ = self.api.delete_ref(branch, prio);
                    let with_base = if i == 0 {
                        true
                    } else {
                        match self.conflicts_with_base(p.number, prio) {
                            Some(b) => b,
                            None => {
                                return Err(Error::Unavailable {
                                    method: "GET".into(),
                                    path: format!("/pulls/{}", p.number),
                                    detail:
                                        "mergeability not computed yet (null); the build is retried"
                                            .into(),
                                });
                            }
                        }
                    };
                    let with = if with_base {
                        vec![]
                    } else {
                        prs[..i].iter().map(|x| x.number).collect()
                    };
                    self.note(format!(
                        "car {car}: #{} does not merge into {branch}",
                        p.number
                    ));
                    return Ok(Event::CarConflict {
                        car,
                        number: p.number,
                        with_base,
                        with,
                    });
                }
            }
            prefix.push((p.number, tree.clone()));
        }
        let title = format!(
            "merge queue: embarking {}",
            own.iter()
                .map(|n| format!("#{n}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        let body = format!(
            "Schlange tests these pull requests together with `{base_branch}` at {}:\n\n{}\n\nDo not edit this pull request; the queue closes it.",
            short(base_sha),
            prs.iter()
                .map(|p| format!("- #{} at {}", p.number, short(&p.head_sha)))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let draft = match self
            .api
            .create_pull(&title, branch, base_branch, &body, true, prio)
        {
            Ok(p) => p.number,
            Err(e @ (Error::Ambiguous { .. } | Error::Status { status: 422, .. })) => {
                match self.api.pulls_with_head(branch, prio)?.first() {
                    Some(p) => p.number,
                    None => return Err(e),
                }
            }
            Err(e) => return Err(e),
        };
        if let Some(s) = &self.store
            && let Err(e) = s.save_car(&self.api.repo.full(), car, &prefix)
        {
            self.note(format!("car {car}: prefix trees not stored: {e}"));
        }
        self.cars.insert(car, CarInfo { prefix });
        self.note(format!(
            "car {car}: {branch} at {}, draft #{draft}",
            short(&cur)
        ));
        Ok(Event::CarBuilt {
            car,
            branch: branch.to_string(),
            head_sha: cur,
            tree,
            draft,
        })
    }

    fn close_car(
        &mut self,
        car: u64,
        branch: Option<&str>,
        draft: Option<u64>,
    ) -> Result<(), Error> {
        let prio = Priority::Normal;
        if let Some(d) = draft {
            let p = self.pull(d, prio)?;
            if p.user.login == self.opts.bot_login && p.state == "open" {
                self.api.close_pull(d, prio)?;
            }
            if p.user.login == self.opts.bot_login {
                self.known_closed.insert(d);
            }
        }
        if let Some(b) = branch {
            match self.api.delete_ref(b, prio) {
                Ok(()) | Err(Error::NotFound { .. }) => {}
                Err(e) => return Err(e),
            }
            self.left_tips.remove(b);
        }
        self.cars.remove(&car);
        if let Some(s) = &self.store {
            let _ = s.delete_car(&self.api.repo.full(), car);
        }
        Ok(())
    }

    /// The tree after each pull request of a car: from memory, or from the store after a
    /// restart.
    fn prefix_of(&self, car: u64) -> Vec<(u64, String)> {
        if let Some(c) = self.cars.get(&car) {
            return c.prefix.clone();
        }
        self.store
            .as_ref()
            .and_then(|s| s.load_car(&self.api.repo.full(), car).ok().flatten())
            .unwrap_or_default()
    }

    /// Squash-lands the pull requests in order. Every merge call is preceded by fresh reads
    /// of the pull request (open, at the tested head) and of the base (exactly the expected
    /// commit); a pull request that is already merged onto the expected base counts as landed,
    /// so nothing lands twice. Errors are returned, never retried here.
    /// The commits after `from` up to `to` on `to`'s first-parent line, oldest first (`to`
    /// included); `None` when `from` is not reached within `limit` commits.
    fn chain(
        &self,
        from: &str,
        to: &str,
        limit: usize,
        prio: Priority,
    ) -> Result<Option<Vec<String>>, Error> {
        let mut out = vec![];
        let mut cur = to.to_string();
        for _ in 0..limit {
            if cur == from {
                out.reverse();
                return Ok(Some(out));
            }
            let c = self.api.git_commit(&cur, prio)?;
            out.push(cur);
            match c.parents.first() {
                Some(p) => cur = p.sha.clone(),
                None => return Ok(None),
            }
        }
        Ok(None)
    }

    /// Lands a car that merges its batch branch itself: `fast-forward` moves the base onto the
    /// tested head (`PATCH /git/refs`, never forced), `merge-batch` merges the draft pull
    /// request with a merge commit. Fresh reads first: every pull request is open at its tested
    /// head and the base is exactly the expected commit. GitHub marks a pull request merged
    /// once its head is reachable from the base (MQ-05); the adapter reports each one with the
    /// commit that carries it.
    #[allow(clippy::too_many_arguments)]
    fn land_branch(
        &mut self,
        car: u64,
        base_branch: &str,
        expected: &str,
        prs: &[CarPr],
        expected_tree: &str,
        method: MergeMethod,
        head: &str,
        draft: Option<u64>,
    ) -> Result<Event, Error> {
        let prio = Priority::Landing;
        let landed = |commits, error| {
            Ok(Event::Landed {
                car,
                commits,
                error,
            })
        };
        for p in prs {
            let pull = self.pull(p.number, prio)?;
            if pull.state == "closed" || pull.head.sha != p.head_sha {
                return landed(vec![], Some(LandError::HeadChanged { number: p.number }));
            }
        }
        let tip = self.branch_tip(base_branch, prio)?;
        if tip != expected {
            return landed(vec![], Some(LandError::BaseMoved { actual: tip }));
        }
        let first = prs.first().map_or(0, |p| p.number);
        let new = match (method, draft) {
            (MergeMethod::MergeBatch, Some(d)) => {
                let title = format!(
                    "Merged {}",
                    prs.iter()
                        .map(|p| format!("#{}", p.number))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                match self.api.merge_pull(
                    d,
                    head,
                    "merge",
                    Some(&title),
                    Some("Merged by the Schlange merge queue"),
                    prio,
                )? {
                    PullMergeOutcome::Merged { sha } => sha,
                    PullMergeOutcome::HeadMoved => {
                        return landed(vec![], Some(LandError::HeadChanged { number: first }));
                    }
                    PullMergeOutcome::NotMergeable(m) => {
                        return landed(
                            vec![],
                            Some(LandError::Refused {
                                number: first,
                                message: refusal_text(405, &m),
                            }),
                        );
                    }
                }
            }
            _ => match self.api.update_ref_cas(base_branch, expected, head, prio) {
                Ok(()) => head.to_string(),
                Err(crate::api::CasError::Stale { actual }) => {
                    return landed(vec![], Some(LandError::BaseMoved { actual }));
                }
                Err(crate::api::CasError::NotFastForward) => {
                    return landed(
                        vec![],
                        Some(LandError::Refused {
                            number: first,
                            message: "GitHub refused the update: not a fast forward".into(),
                        }),
                    );
                }
                Err(crate::api::CasError::Api(e)) => return Err(e),
            },
        };
        self.moved(base_branch, expected);
        let c = self.api.git_commit(&new, prio)?;
        if c.tree.sha != expected_tree {
            self.note(format!(
                "car {car}: landed tree {} differs from the tested tree {}",
                short(&c.tree.sha),
                short(expected_tree)
            ));
        }
        // The commits on the base's first-parent line: one per pull request for a
        // fast-forwarded batch branch, the pull request's own commits in place, one merge
        // commit for `merge-batch`.
        let line = if method == MergeMethod::MergeBatch {
            vec![new.clone()]
        } else {
            self.chain(expected, &new, 256, prio)?
                .unwrap_or_else(|| vec![new.clone()])
        };
        let mut commits = Vec::new();
        for (k, p) in prs.iter().enumerate() {
            let (sha, extra) = if line.len() == prs.len() {
                (line[k].clone(), vec![])
            } else if prs.len() == 1 {
                let mut l = line.clone();
                let last = l.pop().unwrap_or_else(|| new.clone());
                (last, l)
            } else {
                (new.clone(), vec![])
            };
            let tree = self.api.git_commit(&sha, prio)?.tree.sha;
            self.known_merged.insert(p.number, sha.clone());
            commits.push(LandedCommit {
                number: p.number,
                sha,
                tree,
                extra,
            });
        }
        if let Some(d) = draft {
            self.known_closed.insert(d);
        }
        landed(commits, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn land(
        &mut self,
        car: u64,
        base_branch: &str,
        expected_base: &str,
        prs: &[CarPr],
        expected_tree: &str,
        messages: &[CommitMessage],
        method: MergeMethod,
    ) -> Result<Event, Error> {
        let prio = Priority::Landing;
        let rebase = method == MergeMethod::Rebase;
        let prefix = self.prefix_of(car);
        let mut expected = expected_base.to_string();
        let mut commits = Vec::new();
        let mut error = None;
        'prs: for (idx, p) in prs.iter().enumerate() {
            let tested_tree = prefix
                .iter()
                .find(|(n, _)| *n == p.number)
                .map(|(_, t)| t.clone())
                .or_else(|| (idx + 1 == prs.len()).then(|| expected_tree.to_string()));
            let mut transient = 0;
            let mut note = String::new();
            let landed_sha = loop {
                let pull = self.pull(p.number, prio)?;
                if pull.merged {
                    // Merged already: by an earlier attempt whose reply was lost (MQ-35), or by
                    // hand. Ours only when it sits right on the expected base.
                    let sha = pull.merge_commit_sha.clone().unwrap_or_default();
                    let c = self.api.git_commit(&sha, prio)?;
                    let on_expected = if rebase {
                        self.chain(&expected, &sha, 256, prio)?.is_some()
                    } else {
                        c.parents.first().map(|x| x.sha.as_str()) == Some(expected.as_str())
                    };
                    if !on_expected {
                        error = Some(LandError::HeadChanged { number: p.number });
                        break 'prs;
                    }
                    if note.is_empty() {
                        note = "found merged (an earlier attempt landed it)".into();
                    }
                    self.note(format!(
                        "#{} is already merged as {} ({note})",
                        p.number,
                        short(&sha)
                    ));
                    break sha;
                }
                if pull.state == "closed" || pull.head.sha != p.head_sha {
                    error = Some(LandError::HeadChanged { number: p.number });
                    break 'prs;
                }
                // The base right before the write: the merge call has no expected base, so
                // this read is the guard (G2). Never a merge call after a wait without it.
                let tip = self.branch_tip(base_branch, prio)?;
                if tip != expected {
                    error = Some(LandError::BaseMoved { actual: tip });
                    break 'prs;
                }
                // `inherit` leaves title and body to GitHub's squash settings; with trailers
                // on an inherited body the adapter writes that body itself, as GitHub would.
                let (title, message) = match messages.iter().find(|m| m.number == p.number) {
                    // `commit_message_format` is for squash commits; merge and rebase keep
                    // GitHub's own messages.
                    _ if method != MergeMethod::Squash => (None, None),
                    None => (None, None),
                    Some(m) => {
                        let inherited = if m.body.is_none() && !m.trailers.is_empty() {
                            self.inherited_body(&pull, prio)?
                        } else {
                            String::new()
                        };
                        let body = if m.body.is_none() && m.trailers.is_empty() {
                            None
                        } else {
                            Some(m.full_body(&inherited))
                        };
                        (m.title.clone(), body)
                    }
                };
                match self.api.merge_pull(
                    p.number,
                    &p.head_sha,
                    method.as_str(),
                    title.as_deref(),
                    message.as_deref(),
                    prio,
                ) {
                    Ok(PullMergeOutcome::Merged { sha }) => {
                        self.known_merged.insert(p.number, sha.clone());
                        self.moved(base_branch, &expected);
                        break sha;
                    }
                    Ok(PullMergeOutcome::HeadMoved) => {
                        // 409: the head moved, or GitHub is still updating the pull request.
                        // The next round reads it again.
                        if transient < self.transient_retries {
                            transient += 1;
                            self.note(format!(
                                "#{}: 409 from the merge call; reading again (attempt {transient})",
                                p.number
                            ));
                            std::thread::sleep(self.pause());
                            continue;
                        }
                        error = Some(LandError::HeadChanged { number: p.number });
                        break 'prs;
                    }
                    Ok(PullMergeOutcome::NotMergeable(msg)) => {
                        // 405: "base branch was modified", mergeability still being computed,
                        // or a refusal that stays (protection). Also when it was merged a
                        // moment ago: the next round reads.
                        if transient < self.transient_retries {
                            transient += 1;
                            note = format!("after a 405 ({msg})");
                            self.note(format!(
                                "#{}: 405 from the merge call ({msg}); reading again (attempt {transient})",
                                p.number
                            ));
                            std::thread::sleep(self.pause());
                            continue;
                        }
                        let again = self.pull(p.number, prio)?;
                        if again.merged {
                            continue;
                        }
                        let text = refusal_text(405, &msg);
                        self.note(format!("#{}: {text}", p.number));
                        error = Some(if again.mergeable == Some(false) {
                            LandError::Conflict { number: p.number }
                        } else {
                            LandError::Refused {
                                number: p.number,
                                message: text,
                            }
                        });
                        break 'prs;
                    }
                    Err(Error::Status {
                        status, message, ..
                    }) if (400..500).contains(&status) => {
                        // A definitive refusal (403 without a permission, 422): retrying the
                        // same head cannot help (G4).
                        let again = self.pull(p.number, prio)?;
                        if again.merged {
                            continue;
                        }
                        let text = refusal_text(status, &message);
                        self.note(format!("#{}: {text}", p.number));
                        error = Some(LandError::Refused {
                            number: p.number,
                            message: text,
                        });
                        break 'prs;
                    }
                    Err(e @ (Error::Ambiguous { .. } | Error::Unavailable { .. })) => {
                        // MQ-35: did it merge? Read before anything else.
                        let again = self.pull(p.number, prio)?;
                        if again.merged {
                            note = format!("the merge call failed ({e}) but the merge happened");
                            continue;
                        }
                        return Err(e);
                    }
                    // Rate limits included: the call was not sent again after the wait; the
                    // engine reads and decides anew.
                    Err(e) => return Err(e),
                }
            };
            let c = self.api.git_commit(&landed_sha, prio)?;
            // A rebase puts the pull request's commits on the base, `landed_sha` last.
            let mut extra = if rebase {
                self.chain(&expected, &landed_sha, 256, prio)?
                    .unwrap_or_default()
            } else {
                vec![]
            };
            extra.pop();
            let entry = extra.first().cloned().unwrap_or_else(|| landed_sha.clone());
            let parent = if entry == landed_sha {
                c.parents.first().map(|x| x.sha.clone()).unwrap_or_default()
            } else {
                self.api
                    .git_commit(&entry, prio)?
                    .parents
                    .first()
                    .map(|x| x.sha.clone())
                    .unwrap_or_default()
            };
            if parent != expected {
                note = format!(
                    "landed on {} instead of the expected base {} (a push raced the merge call)",
                    short(&parent),
                    short(&expected)
                );
                self.note(format!("#{}: {note}", p.number));
            }
            self.audit(AuditRecord {
                repo: self.api.repo.full(),
                at: self.clock.now(),
                car,
                number: p.number,
                tested_head: p.head_sha.clone(),
                tested_tree: tested_tree.clone(),
                base_before: expected.clone(),
                landed_sha: landed_sha.clone(),
                landed_tree: c.tree.sha.clone(),
                landed_parent: parent,
                note,
            });
            commits.push(LandedCommit {
                number: p.number,
                sha: landed_sha.clone(),
                tree: c.tree.sha.clone(),
                extra,
            });
            expected = landed_sha;
            match tested_tree {
                Some(tested) if tested != c.tree.sha => {
                    self.note(format!(
                        "#{} landed with tree {} but the queue tested {}: stopping",
                        p.number,
                        short(&c.tree.sha),
                        short(&tested)
                    ));
                    break;
                }
                Some(_) => {}
                None => self.note(format!(
                    "#{}: no record of the tree the car tested for it; only the batch's final tree is compared",
                    p.number
                )),
            }
        }
        Ok(Event::Landed {
            car,
            commits,
            error,
        })
    }

    fn audit(&mut self, r: AuditRecord) {
        self.note(format!(
            "audit: #{} car {} tested {} (tree {}) on {}, landed {} (tree {}, parent {}){}",
            r.number,
            r.car,
            short(&r.tested_head),
            r.tested_tree.as_deref().map(short).unwrap_or("?"),
            short(&r.base_before),
            short(&r.landed_sha),
            short(&r.landed_tree),
            short(&r.landed_parent),
            if r.note.is_empty() {
                String::new()
            } else {
                format!("; {}", r.note)
            }
        ));
        if let Some(s) = &self.store
            && let Err(e) = s.audit(&r)
        {
            self.note(format!("audit record not stored: {e}"));
        }
    }

    /// The squash body GitHub writes under the repository's setting
    /// (`squash_merge_commit_message`: `PR_BODY`, `COMMIT_MESSAGES` (the default), `BLANK`).
    fn inherited_body(&self, pull: &Pull, prio: Priority) -> Result<String, Error> {
        let setting = self
            .api
            .repository(prio)?
            .squash_merge_commit_message
            .unwrap_or_else(|| "COMMIT_MESSAGES".into());
        Ok(match setting.as_str() {
            "PR_BODY" => pull.body.clone().unwrap_or_default(),
            "BLANK" => String::new(),
            _ => self
                .api
                .pull_commits(pull.number, prio)?
                .iter()
                .map(|c| format!("* {}", c.commit.message))
                .collect::<Vec<_>>()
                .join("\n\n"),
        })
    }

    /// Sends the cosmetic writes that the budget allows, in order.
    pub fn flush_cosmetics(&mut self) {
        while let Some(c) = self.cosmetics.front().cloned() {
            match self.cosmetic(&c) {
                Ok(()) => {
                    self.cosmetics.pop_front();
                }
                Err(Error::Deferred { .. } | Error::RateLimited { .. }) => break,
                Err(e) => {
                    self.note(format!("cosmetic write dropped: {e}"));
                    self.cosmetics.pop_front();
                }
            }
        }
    }

    fn cosmetic(&mut self, c: &Cosmetic) -> Result<(), Error> {
        let prio = Priority::Cosmetic;
        match c {
            Cosmetic::Start { number, queue } => {
                let Some(head) = self.heads.borrow().get(number).cloned() else {
                    return Ok(());
                };
                let id = self.api.create_check_run(
                    &json!({
                        "name": self.opts.check_name,
                        "head_sha": head,
                        "status": "in_progress",
                        "output": {
                            "title": format!("Queued in `{queue}`"),
                            "summary": "Schlange queued this pull request."
                        }
                    }),
                    prio,
                )?;
                self.queue_checks.insert(*number, id);
            }
            Cosmetic::Update { number, summary } => {
                if let Some(id) = self.queue_checks.get(number).copied() {
                    self.api.update_check_run(
                        id,
                        &json!({ "output": { "title": "Queued", "summary": summary } }),
                        prio,
                    )?;
                }
            }
            Cosmetic::Finish {
                number,
                conclusion,
                title,
                summary,
            } => {
                if let Some(id) = self.queue_checks.get(number).copied() {
                    self.api.update_check_run(
                        id,
                        &json!({
                            "status": "completed",
                            "conclusion": conclusion,
                            "output": { "title": title, "summary": summary }
                        }),
                        prio,
                    )?;
                    self.queue_checks.remove(number);
                }
            }
            Cosmetic::Show(c) => self.show(c)?,
        }
        Ok(())
    }
}

impl GithubForge {
    /// Carries out a cosmetic decision of the engine.
    fn show(&mut self, c: &EngineCosmetic) -> Result<(), Error> {
        let prio = Priority::Cosmetic;
        match c {
            EngineCosmetic::Labels {
                number,
                add,
                remove,
            } => {
                if !add.is_empty() {
                    let add: Vec<&str> = add.iter().map(String::as_str).collect();
                    self.api.add_labels(*number, &add, prio)?;
                }
                for l in remove {
                    self.api.remove_label(*number, l, prio)?;
                }
            }
            EngineCosmetic::StatusComment { number, body } => {
                self.upsert(*number, schlange_core::commands::STATUS_MARKER, body)?
            }
            EngineCosmetic::ControlsComment { number, body } => {
                self.upsert(*number, schlange_core::commands::CONTROLS_MARKER, body)?
            }
            EngineCosmetic::Reply { number, to, body } => {
                // Issue comments have no threads; a reply quotes nothing and stands alone.
                let _ = to;
                self.api.comment(*number, body, prio)?;
            }
            EngineCosmetic::React {
                comment, content, ..
            } => match self.api.react_to_comment(*comment, content, prio) {
                Ok(()) | Err(Error::NotFound { .. }) => {}
                Err(e) => return Err(e),
            },
            EngineCosmetic::ProtectionCheck {
                head_sha,
                name,
                ok,
                title,
                summary,
                ..
            } => {
                self.api.create_check_run(
                    &json!({
                        "name": name,
                        "head_sha": head_sha,
                        "status": "completed",
                        "conclusion": if *ok { "success" } else { "failure" },
                        "output": { "title": title, "summary": summary }
                    }),
                    prio,
                )?;
            }
        }
        Ok(())
    }

    /// Creates or edits the queue's own comment with `marker` on pull request `n`.
    fn upsert(&mut self, n: u64, marker: &'static str, body: &str) -> Result<(), Error> {
        let prio = Priority::Cosmetic;
        let id = match self.own_comments.get(&(n, marker)).copied() {
            Some(id) => Some(id),
            None => self
                .api
                .issue_comments(n, prio)?
                .into_iter()
                .find(|c| c.user.login == self.opts.bot_login && c.body.contains(marker))
                .map(|c| c.id),
        };
        if let Some(id) = id {
            match self.api.update_comment(id, body, prio) {
                Ok(()) => {
                    self.own_comments.insert((n, marker), id);
                    return Ok(());
                }
                Err(Error::NotFound { .. }) => {
                    self.own_comments.remove(&(n, marker));
                }
                Err(e) => return Err(e),
            }
        }
        let id = self.api.comment(n, body, prio)?;
        self.own_comments.insert((n, marker), id);
        Ok(())
    }

    /// Commands among the repository's most recent comments that the engine has not handled
    /// and that the queue has not acknowledged (a 👍 of the app): lost webhooks, or commands
    /// from before a restart. Edits are left out: the list does not say who edited.
    pub fn unhandled_commands(&mut self, state: &State) -> Result<Vec<Comment>, Error> {
        let prio = Priority::Normal;
        let mut out = Vec::new();
        for c in self.api.recent_comments(prio)? {
            let Some(n) = c
                .issue_url
                .rsplit('/')
                .next()
                .and_then(|x| x.parse::<u64>().ok())
            else {
                continue;
            };
            if c.user.login == self.opts.bot_login
                || !state.pull_requests.contains_key(&n)
                || state.seen_comments.contains(&(c.id, c.updated_at.clone()))
                || schlange_core::commands::parse(&c.body)
                    == schlange_core::commands::Parsed::NotACommand
            {
                continue;
            }
            let acknowledged = self
                .api
                .comment_reactions(c.id, prio)?
                .iter()
                .any(|r| r.user.login == self.opts.bot_login);
            if acknowledged {
                continue;
            }
            let permission = self.api.collaborator_permission(&c.user.login, prio)?;
            out.push(Comment {
                id: c.id,
                number: n,
                author: c.user.login.clone(),
                sender: c.user.login.clone(),
                body: c.body.clone(),
                edited: false,
                version: c.updated_at.clone(),
                sender_permission: Some(permission),
                by_queue: false,
                head_sha: self.head_of(n),
            });
        }
        Ok(out)
    }
}

/// What the orphan sweep needs from a reconciliation read.
#[derive(Clone, Debug, Default)]
pub struct Sweep {
    pub queue_refs: Vec<String>,
    /// Open draft pull requests of the bot on queue branches: number and head branch.
    pub drafts: Vec<(u64, String)>,
    pub prefixes: BTreeSet<String>,
}

/// The car a side effect belongs to.
fn decision_car(d: &Decision) -> Option<u64> {
    match d {
        Decision::BuildCar { car, .. }
        | Decision::CloseCar { car, .. }
        | Decision::Land { car, .. }
        | Decision::CheckInPlace { car, .. } => Some(*car),
        _ => None,
    }
}

fn decision_name(d: &Decision) -> String {
    match d {
        Decision::BuildCar { car, .. } => format!("build car {car}"),
        Decision::CloseCar { car, .. } => format!("close car {car}"),
        Decision::Refresh { token, .. } => format!("read (token {token})"),
        Decision::Land { car, .. } => format!("land car {car}"),
        other => format!("{other:?}"),
    }
}

impl schlange_forge::Forge for GithubForge {
    fn now(&self) -> Time {
        self.clock.now()
    }

    fn read_pull_request(&self, number: u64) -> Option<PullRequest> {
        self.read_pr(number, Priority::Normal).ok()
    }

    fn read_branch(&self, name: &str) -> Option<String> {
        self.read_branch_checked(name, Priority::Normal)
            .ok()
            .flatten()
    }

    fn execute(&mut self, decision: &Decision) -> Vec<Event> {
        GithubForge::execute(self, decision)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn queue_branch_pattern_is_strict() {
        assert!(is_queue_branch(
            "mergify/merge-queue/0123456789",
            "mergify/merge-queue/"
        ));
        assert!(!is_queue_branch(
            "mergify/merge-queue/012345678",
            "mergify/merge-queue/"
        ));
        assert!(!is_queue_branch(
            "mergify/merge-queue/0123456789a",
            "mergify/merge-queue/"
        ));
        assert!(!is_queue_branch(
            "mergify/merge-queue/ABCDEF0123",
            "mergify/merge-queue/"
        ));
        assert!(!is_queue_branch(
            "feature/0123456789",
            "mergify/merge-queue/"
        ));
    }

    #[test]
    fn dependabot_metadata_from_commit_messages() {
        let msgs = vec![
            "Bump x from 1.0 to 1.1\n\n---\nupdated-dependencies:\n- dependency-name: x\n  dependency-type: direct:production\n  update-type: version-update:semver-minor\n...\n".to_string(),
            "Bump y\n\nupdate-type: version-update:semver-patch\n".to_string(),
            "unrelated".to_string(),
        ];
        assert_eq!(
            dependabot_update_types(&msgs),
            vec!["version-update:semver-minor", "version-update:semver-patch"]
        );
    }
}
