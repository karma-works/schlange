//! `GithubForge`: the engine's decisions as GitHub API calls, and GitHub's state as the
//! engine's truth reads.
//!
//! - **Cars.** `BuildCar` creates the queue branch (`<prefix><10 hex>`) at the expected base
//!   (`POST /git/refs`), merges each pull request head into it with the merges API, and opens a
//!   draft pull request on it. A merge conflict deletes the branch again and replies
//!   `CarConflict`.
//! - **Landing.** Each pull request is squash-merged with `PUT /pulls/{n}/merge` and the
//!   tested head as `sha`. Right before, the base is read and compared with the expected SHA;
//!   right after, the landed commit's parent and tree are read and compared with the expected
//!   base and the tree the car tested for that prefix. A different tree stops the landing; the
//!   engine then halts the queue (MQ-02). GitHub has no compare-and-swap on the base; see
//!   [`crate::api`].
//! - **Unknown outcomes.** A write that failed with 5xx or a timeout may have happened
//!   (MQ-35). The adapter reads before it decides: a pull request that is merged after a
//!   failed merge call counts as landed (never merged twice), a branch or draft that exists
//!   after a failed create is used.
//! - **Provisional failures.** Reads that fail (5xx, 404 storms, rate limits) never become
//!   facts: a pull request that cannot be read is left out of the snapshot (the engine keeps
//!   what it knew), a branch is reported missing only when a canary read of the default branch
//!   works. A decision that could not be carried out is kept and retried with backoff; the
//!   engine waits for its reply (W-24).
//! - **Cosmetics.** The queue's own check run on each queued pull request is cosmetic: it uses
//!   the lowest priority and waits when the budget is low. Check runs of Schlange's own app
//!   are ignored when reading checks, so they never feed back into conditions.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::json;

use schlange_config::rules::Rules;
use schlange_core::engine::{
    CarPr, CommitMessage, Cosmetic as EngineCosmetic, LandError, LandedCommit, short,
};
use schlange_core::model::{CheckRun, Comment, Dependency, DependencyState, PullRequest, Time};
use schlange_core::{Decision, Event, State};

use crate::api::{MergeOutcome, Pull, PullMergeOutcome, RepoApi};
use crate::client::{Error, Priority};
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

pub struct GithubForge {
    pub api: RepoApi,
    clock: Arc<dyn Clock>,
    pub opts: ForgeOptions,
    cars: BTreeMap<u64, CarInfo>,
    pending: Vec<PendingOp>,
    cosmetics: VecDeque<Cosmetic>,
    queue_checks: BTreeMap<u64, u64>,
    heads: RefCell<BTreeMap<u64, String>>,
    default_branch: RefCell<Option<String>>,
    /// What the adapter did, newest last (bounded).
    pub log: VecDeque<String>,
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Read the reviews of each pull request (only when a queue rule asks for the
    /// `approved-by` trailer).
    pub read_reviews: bool,
    /// The Mergify schema for the "configuration changed" check; without it the check runs
    /// stages 1, 3 and 4 of the validator.
    pub schema: Option<Arc<schlange_config::Schema>>,
    pub features: Option<Arc<schlange_config::FeatureMap>>,
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

impl GithubForge {
    pub fn new(api: RepoApi, clock: Arc<dyn Clock>, opts: ForgeOptions) -> Self {
        GithubForge {
            api,
            clock,
            opts,
            cars: BTreeMap::new(),
            pending: Vec::new(),
            cosmetics: VecDeque::new(),
            queue_checks: BTreeMap::new(),
            heads: RefCell::new(BTreeMap::new()),
            default_branch: RefCell::new(None),
            log: VecDeque::new(),
            retry_base: Duration::from_secs(2),
            retry_max: Duration::from_secs(60),
            read_reviews: false,
            schema: None,
            features: None,
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
        match (&self.schema, &self.features) {
            (Some(schema), Some(features)) => {
                schlange_config::validate(file, text, schema, features, false).findings
            }
            _ => schlange_config::config_file::validate_without_schema(file, text),
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
        self.pending.len()
    }

    pub fn cosmetic_count(&self) -> usize {
        self.cosmetics.len()
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

    /// Reads a pull request with everything the engine evaluates.
    pub fn read_pr(&self, n: u64, prio: Priority) -> Result<PullRequest, Error> {
        let p = self.api.pull(n, prio)?;
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
            });
        }
        for s in self.api.combined_status(&head, prio)?.statuses {
            pr.checks.push(CheckRun {
                name: s.context.clone(),
                app: None,
                head_sha: head.clone(),
                conclusion: status_conclusion(&s.state),
                seq: s.id,
            });
        }
        pr.head_tree = Some(self.api.git_commit(&head, prio)?.tree.sha);
        if ours {
            // The queue's own draft: only its checks and tree matter.
            return Ok(pr);
        }
        pr.files = self.api.pull_files(n, prio)?;
        let tip = self.api.branch_sha(&p.base.ref_name, prio)?;
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

    /// A branch's SHA; `Ok(None)` only when GitHub says 404 *and* a canary read of the
    /// default branch works (a 404 storm is not taken for a deleted branch, W-24).
    pub fn read_branch_checked(&self, name: &str, prio: Priority) -> Result<Option<String>, Error> {
        match self.api.branch_sha(name, prio) {
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
                    Ok(()) => self.note(format!("closed orphan queue draft #{n}")),
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

    /// Retries deferred decisions that are due; returns the replies for the engine.
    pub fn retry_pending(&mut self) -> Vec<Event> {
        let now = Instant::now();
        let due: Vec<PendingOp> = {
            let (due, rest): (Vec<_>, Vec<_>) = std::mem::take(&mut self.pending)
                .into_iter()
                .partition(|p| p.next <= now);
            self.pending = rest;
            due
        };
        let mut out = Vec::new();
        for mut op in due {
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
                    let delay = self
                        .retry_base
                        .saturating_mul(1 << op.attempts.min(10))
                        .min(self.retry_max);
                    op.next = Instant::now() + delay;
                    op.last_error = e.to_string();
                    self.pending.push(op);
                }
            }
        }
        out
    }

    /// Deferred decisions with their last error (for status and doctor).
    pub fn pending_summary(&self) -> Vec<String> {
        self.pending
            .iter()
            .map(|p| {
                format!(
                    "{} (attempt {}): {}",
                    decision_name(&p.decision),
                    p.attempts,
                    p.last_error
                )
            })
            .collect()
    }

    /// Makes the next retry of deferred decisions due now (after a reconciliation).
    pub fn hurry_pending(&mut self) {
        let now = Instant::now();
        for p in &mut self.pending {
            p.next = p.next.min(now);
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
                ..
            } => self
                .build_car(*car, base_branch, branch, base_sha, prs, own)
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
                messages,
                ..
            } => self
                .land(*car, base_branch, expected_base_sha, prs, messages)
                .map(|e| vec![e]),
            _ => Ok(vec![]),
        }
    }

    /// Carries out one decision. Failures are deferred (retried), never reported as facts.
    pub fn execute(&mut self, d: &Decision) -> Vec<Event> {
        match d {
            Decision::BuildCar { .. }
            | Decision::Refresh { .. }
            | Decision::Land { .. }
            | Decision::CloseCar { .. } => {
                if let Decision::CloseCar { car, .. } = d {
                    // A car closed before it was built: forget its pending build.
                    let car = *car;
                    self.pending.retain(
                        |p| !matches!(&p.decision, Decision::BuildCar { car: c, .. } if *c == car),
                    );
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

    fn build_car(
        &mut self,
        car: u64,
        base_branch: &str,
        branch: &str,
        base_sha: &str,
        prs: &[CarPr],
        own: &[u64],
    ) -> Result<Event, Error> {
        let prio = Priority::Normal;
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
        match self.api.create_ref(branch, base_sha, prio) {
            Ok(()) => {}
            Err(e @ (Error::Ambiguous { .. } | Error::Status { status: 422, .. })) => {
                // Did it happen? Use it when it points at the base.
                if self.api.branch_sha(branch, prio).ok().as_deref() != Some(base_sha) {
                    return Err(e);
                }
            }
            Err(e) => return Err(e),
        }
        let mut cur = base_sha.to_string();
        let mut tree = self.api.git_commit(base_sha, prio)?.tree.sha;
        let mut prefix = Vec::new();
        for (i, p) in prs.iter().enumerate() {
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
                Err(e) => {
                    let _ = self.api.delete_ref(branch, prio);
                    return Err(e);
                }
            };
            match outcome {
                MergeOutcome::Merged { sha, tree: t } => {
                    cur = sha;
                    tree = t;
                }
                MergeOutcome::NothingToMerge => {}
                MergeOutcome::Conflict => {
                    let _ = self.api.delete_ref(branch, prio);
                    let with_base = i == 0
                        || self
                            .api
                            .pull(p.number, prio)
                            .map(|pr| pr.mergeable == Some(false))
                            .unwrap_or(false);
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
            let p = self.api.pull(d, prio)?;
            if p.user.login == self.opts.bot_login && p.state == "open" {
                self.api.close_pull(d, prio)?;
            }
        }
        if let Some(b) = branch {
            match self.api.delete_ref(b, prio) {
                Ok(()) | Err(Error::NotFound { .. }) => {}
                Err(e) => return Err(e),
            }
        }
        self.cars.remove(&car);
        Ok(())
    }

    /// Squash-lands the pull requests in order. Safe to repeat: a pull request that is
    /// already merged onto the expected base counts as landed, so nothing lands twice.
    fn land(
        &mut self,
        car: u64,
        base_branch: &str,
        expected_base: &str,
        prs: &[CarPr],
        messages: &[CommitMessage],
    ) -> Result<Event, Error> {
        let prio = Priority::Landing;
        let prefix = self
            .cars
            .get(&car)
            .map(|c| c.prefix.clone())
            .unwrap_or_default();
        let mut expected = expected_base.to_string();
        let mut commits = Vec::new();
        let mut error = None;
        for p in prs {
            let pull = self.api.pull(p.number, prio)?;
            let landed_sha = if pull.merged {
                // Merged already: by an earlier attempt whose reply was lost (MQ-35), or by
                // hand. Ours only when it sits right on the expected base.
                let sha = pull.merge_commit_sha.clone().unwrap_or_default();
                let c = self.api.git_commit(&sha, prio)?;
                if c.parents.first().map(|x| x.sha.as_str()) != Some(expected.as_str()) {
                    error = Some(LandError::HeadChanged { number: p.number });
                    break;
                }
                self.note(format!(
                    "#{} is already merged as {} (an earlier attempt landed it)",
                    p.number,
                    short(&sha)
                ));
                sha
            } else {
                if pull.state == "closed" || pull.head.sha != p.head_sha {
                    error = Some(LandError::HeadChanged { number: p.number });
                    break;
                }
                let tip = self.api.branch_sha(base_branch, prio)?;
                if tip != expected {
                    error = Some(LandError::BaseMoved { actual: tip });
                    break;
                }
                // `inherit` leaves title and body to GitHub's squash settings; with trailers
                // on an inherited body the adapter writes that body itself, as GitHub would.
                let (title, message) = match messages.iter().find(|m| m.number == p.number) {
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
                match self.api.squash_pull(
                    p.number,
                    &p.head_sha,
                    title.as_deref(),
                    message.as_deref(),
                    prio,
                ) {
                    Ok(PullMergeOutcome::Merged { sha }) => sha,
                    Ok(PullMergeOutcome::HeadMoved) => {
                        error = Some(LandError::HeadChanged { number: p.number });
                        break;
                    }
                    Ok(PullMergeOutcome::NotMergeable(msg)) => {
                        // "Not mergeable" also comes back when it was merged a moment ago.
                        let again = self.api.pull(p.number, prio)?;
                        if again.merged {
                            again.merge_commit_sha.unwrap_or_default()
                        } else {
                            self.note(format!("#{} refused by GitHub: {msg}", p.number));
                            error = Some(LandError::Conflict { number: p.number });
                            break;
                        }
                    }
                    Err(e @ (Error::Ambiguous { .. } | Error::Unavailable { .. })) => {
                        // MQ-35: did it merge? Read before anything else.
                        let again = self.api.pull(p.number, prio)?;
                        if again.merged {
                            self.note(format!(
                                "#{}: merge call failed ({e}) but the merge happened",
                                p.number
                            ));
                            again.merge_commit_sha.unwrap_or_default()
                        } else if commits.is_empty() {
                            return Err(e);
                        } else {
                            // Part of the batch landed: retry the rest later; what landed is
                            // found merged on the next attempt.
                            return Err(e);
                        }
                    }
                    Err(e) => return Err(e),
                }
            };
            let c = self.api.git_commit(&landed_sha, prio)?;
            if c.parents.first().map(|x| x.sha.as_str()) != Some(expected.as_str()) {
                self.note(format!(
                    "#{} landed on {} instead of the expected base {} (no CAS on GitHub)",
                    p.number,
                    c.parents.first().map(|x| short(&x.sha)).unwrap_or("?"),
                    short(&expected)
                ));
            }
            commits.push(LandedCommit {
                number: p.number,
                sha: landed_sha.clone(),
                tree: c.tree.sha.clone(),
            });
            expected = landed_sha;
            if let Some((_, tested)) = prefix.iter().find(|(n, _)| *n == p.number)
                && *tested != c.tree.sha
            {
                self.note(format!(
                    "#{} landed with tree {} but the queue tested {}: stopping",
                    p.number,
                    short(&c.tree.sha),
                    short(tested)
                ));
                break;
            }
        }
        Ok(Event::Landed {
            car,
            commits,
            error,
        })
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
