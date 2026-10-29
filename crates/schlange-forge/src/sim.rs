//! The simulated forge: a git repository in memory (`git`), pull requests with real head
//! commits, CI as a script (`ci`), a virtual clock, and webhook delivery with fault injection
//! (dropped, duplicated, delayed and therefore reordered deliveries).
//!
//! Webhooks are hints: a delivery only names what changed (a pull request or a branch). The
//! runner then reads the current state from the forge and hands that truth to the engine.
//! With faults on, a periodic reconciliation reads everything.
//!
//! The forge also records what it did for the queue (cars built, landings, external commits),
//! so the invariant checker can judge the engine with full knowledge.
//!
//! The forge logic is generic over its git store ([`GitStore`]): `SimForge<Repo>` (the
//! default) keeps git in memory; `SimForge<LocalGit>` (feature `localgit`, see
//! [`crate::localgit`]) keeps it in real repositories on disk and runs every check as a real
//! process in a checkout (`real_ci`). Pull request metadata (number, author, labels, draft,
//! check runs), the clock and webhook delivery are the adapter's own in both cases, as in
//! Prow's `localgit` and uber/submitqueue's fake provider.

use std::collections::{BTreeMap, BTreeSet};

use schlange_config::rules::MergeMethod;
use schlange_core::engine::{
    CarPr, CommitMessage, Control, Cosmetic, Freeze, LandError, LandedCommit, short,
};
use schlange_core::model::{
    CheckRun, Comment, Conclusion, Dependency, DependencyState, PullRequest, Time,
};
use schlange_core::{Decision, Event};

use crate::ci::{CiScript, Rng, keyed_unit};
use crate::git::{GitStore, Repo, Sha, Tree};

#[path = "sim_actions.rs"]
pub mod actions;
pub use actions::{PostedCheck, PostedChecks, is_copy};

/// A virtual clock that only moves when the simulation advances it.
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtualClock {
    now: Time,
}

impl VirtualClock {
    pub fn now(&self) -> Time {
        self.now
    }

    /// Moves forward to `t`; the clock never goes back.
    pub fn advance_to(&mut self, t: Time) {
        self.now = self.now.max(t);
    }
}

/// A pull request as a scenario defines it, before it is opened.
#[derive(Clone, Debug)]
pub struct PrDef {
    pub number: u64,
    pub author: String,
    pub title: String,
    pub base: String,
    pub draft: bool,
    pub labels: BTreeSet<String>,
    /// Files the pull request changes, with their new content.
    pub files: Vec<(String, String)>,
    pub dependabot_update_types: Vec<String>,
    /// CI on the pull request itself (per-PR overrides already applied).
    pub scripts: Vec<CiScript>,
    /// The description (`Depends-On`, `Merge-After`).
    pub body: String,
    /// Opened from a fork.
    pub fork: bool,
    /// Logins whose review approves it.
    pub approvals: Vec<String>,
}

impl PrDef {
    /// A pull request by `author` on `main` changing `files`, without CI.
    pub fn new(number: u64, author: &str, files: Vec<(String, String)>) -> Self {
        PrDef {
            number,
            author: author.to_string(),
            title: format!("change {number}"),
            base: "main".into(),
            draft: false,
            labels: BTreeSet::new(),
            files,
            dependabot_update_types: vec![],
            scripts: vec![],
            body: String::new(),
            fork: false,
            approvals: vec![],
        }
    }
}

/// Which checkbox a person ticks in the queue's comments.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckBox {
    /// "Add this pull request to the merge queue" in the controls comment.
    Queue,
    /// "Put this pull request back into the queue" in the status comment.
    Requeue,
}

/// A comment as the simulated forge keeps it.
#[derive(Clone, Debug)]
pub struct SimComment {
    pub id: u64,
    pub number: u64,
    pub author: String,
    pub body: String,
    pub version: u64,
    pub last_editor: String,
    pub head_sha: String,
}

/// Things a scenario does to the forge at a point in time.
#[derive(Clone, Debug)]
pub enum Action {
    Open(u64),
    /// A new commit on the pull request; empty `files` touches its first file.
    Push {
        number: u64,
        files: Vec<(String, String)>,
    },
    Label {
        number: u64,
        label: String,
    },
    Unlabel {
        number: u64,
        label: String,
    },
    Draft {
        number: u64,
    },
    Ready {
        number: u64,
    },
    Close {
        number: u64,
    },
    /// A commit pushed straight to a base branch, outside the queue (MQ-06).
    PushBase {
        branch: String,
        files: Vec<(String, String)>,
    },
    /// Someone squash-merges the pull request by hand (MQ-09).
    MergeManually(u64),
    /// Someone pushes to the queue branch that checks this pull request (MQ-08).
    PushQueueBranch(u64),
    /// Someone force-pushes a commit with these parents (and the tip's tree plus `files`) to
    /// the queue branch that checks this pull request: a head that need not descend from the
    /// tested one (checker tests).
    ForcePushQueueBranch {
        number: u64,
        parents: Vec<Sha>,
        files: Vec<(String, String)>,
    },
    /// A push to the pull request by someone else than its author.
    PushAs {
        number: u64,
        by: String,
        files: Vec<(String, String)>,
    },
    /// Someone comments on the pull request.
    Comment {
        number: u64,
        by: String,
        body: String,
    },
    /// Someone ticks a checkbox in one of the queue's comments.
    Tick {
        number: u64,
        by: String,
        checkbox: CheckBox,
    },
    /// The description is edited.
    EditBody {
        number: u64,
        body: String,
    },
    /// Someone reviews the pull request (`APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`).
    Review {
        number: u64,
        by: String,
        state: String,
    },
    /// An operator pauses or unpauses the queue, or creates or deletes a freeze (API, CLI).
    Control(Control),
}

/// Webhook faults. All probabilities are per delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct Faults {
    pub seed: u64,
    pub drop: f64,
    pub duplicate: f64,
    pub delay: f64,
    pub max_delay: Time,
    /// Period of the reconciliation pass while there is work.
    pub reconcile_every: Time,
}

impl Faults {
    pub fn none() -> Self {
        Faults {
            seed: 0,
            drop: 0.0,
            duplicate: 0.0,
            delay: 0.0,
            max_delay: 0,
            reconcile_every: 0,
        }
    }

    /// The default disturbance for the I5 comparison.
    pub fn standard(seed: u64) -> Self {
        Faults {
            seed,
            drop: 0.15,
            duplicate: 0.15,
            delay: 0.3,
            max_delay: 45,
            reconcile_every: 60,
        }
    }

    pub fn is_none(&self) -> bool {
        self.drop == 0.0 && self.duplicate == 0.0 && self.delay == 0.0
    }
}

/// CI timing faults: runs that are not all alike (KONZEPT.md section 6). All zero: every run
/// takes exactly its script's duration, every check run exists as soon as CI starts, and a
/// closed draft's CI stops at once.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct CiTiming {
    pub seed: u64,
    /// Each run's duration varies by up to this fraction, up or down.
    pub jitter: f64,
    /// A check run is created up to this long after CI starts (late check-run creation); until
    /// then the draft has no run of that name at all.
    pub max_create_delay: Time,
    /// Closing a draft cancels its running checks up to this long later; the forge confirms
    /// the close (`CarClosed`) only then.
    pub max_cancel_delay: Time,
}

/// What a webhook delivery names.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Hint {
    PullRequest(u64),
    Branch(String),
    /// A comment was created or edited.
    Comment(u64),
}

/// What the forge hands to the runner, in time order.
#[derive(Clone, Debug)]
pub enum Item {
    /// A scenario action was applied.
    Action(String),
    /// A check run completed.
    Ci(String),
    /// A webhook delivery.
    Webhook {
        hint: Hint,
        text: String,
    },
    Tick,
    Reconcile,
    /// A reply to one of the engine's decisions, delivered late (`ForgeFaults`).
    Reply(Box<Event>),
}

enum Pending {
    Act(Action),
    CheckDone {
        number: u64,
        seq: u64,
        conclusion: Conclusion,
    },
    Deliver {
        hint: Hint,
        text: String,
    },
    Tick,
    Reconcile,
    Reply(Box<Event>),
    /// A check run that is created late.
    CreateRun {
        number: u64,
        run: CheckRun,
        conclusion: Option<Conclusion>,
        duration: Time,
    },
    /// Running checks of a closed draft are cancelled.
    Cancel(u64),
}

/// A car the forge built for the queue (or recorded for an in-place check).
#[derive(Clone, Debug)]
pub struct CarRecord {
    pub id: u64,
    pub queue: String,
    pub base_branch: String,
    pub branch: Option<String>,
    pub draft: Option<u64>,
    pub in_place: bool,
    pub base_sha: Sha,
    /// Per pull request in the car: number, the commit after merging it, and its tree.
    pub prefix: Vec<(u64, Sha, String)>,
    /// The pull request heads the car was built from (the tested heads, for I1).
    pub heads: Vec<(u64, Sha)>,
    pub head: Sha,
    pub tree: String,
    pub closed: bool,
}

/// One landing as the forge carried it out.
#[derive(Clone, Debug)]
pub struct LandingRecord {
    pub at: Time,
    pub car: u64,
    pub method: MergeMethod,
    pub queue: String,
    pub base_branch: String,
    pub expected_base: Sha,
    pub requested: Vec<CarPr>,
    pub commits: Vec<LandedCommit>,
    pub error: Option<LandError>,
    /// The pull requests and the car's draft as they were right before landing.
    pub before: Vec<PullRequest>,
    pub draft_before: Option<PullRequest>,
}

pub const BOT: &str = "schlange[bot]";

pub struct SimForge<G: GitStore = Repo> {
    clock: VirtualClock,
    pub repo: G,
    /// Every check runs as a process in a checkout of the tested commit (`localgit`); off,
    /// only checks with a `run` command do and the marker rules are evaluated in memory.
    pub real_ci: bool,
    /// How many check runs were real processes.
    pub processes: u64,
    pending: BTreeMap<(Time, u64), Pending>,
    order: u64,
    check_seq: u64,
    next_draft: u64,
    defs: BTreeMap<u64, PrDef>,
    prs: BTreeMap<u64, PullRequest>,
    queue_scripts: Vec<CiScript>,
    flake_seed: u64,
    attempts: BTreeMap<String, u32>,
    pushes: BTreeMap<u64, u32>,
    faults: Faults,
    rng: Rng,
    pub cars: BTreeMap<u64, CarRecord>,
    pub landings: Vec<LandingRecord>,
    /// Commits on base branches not made by the queue.
    pub external_commits: BTreeSet<Sha>,
    pub initial_commit: Sha,
    pub base_branches: BTreeSet<String>,
    /// Notes for the log (dropped deliveries, refused actions).
    pub notes: Vec<(Time, String)>,
    pub dropped: u64,
    pub duplicated: u64,
    pub ci_timing: CiTiming,
    /// The default branch (configuration and the configuration check apply to it).
    pub default_branch: String,
    /// Repository permissions per login (`sender-permission`); unknown logins have `none`.
    pub permissions: BTreeMap<String, String>,
    pub comments: BTreeMap<u64, SimComment>,
    next_comment: u64,
    /// Reactions the queue made: comment id and content.
    pub reactions: Vec<(u64, String)>,
    /// The queue's own comments per pull request: status and controls comment ids.
    status_comments: BTreeMap<u64, u64>,
    controls_comments: BTreeMap<u64, u64>,
    /// Checks the queue reported on pull requests (built-in protections): number, head, name,
    /// success. Kept apart from CI check runs, as GitHub's reads leave out the app's own runs.
    pub protection_checks: Vec<(u64, String, String, bool)>,
    /// Commit authors per pull request (`Name <email>`) and messages, in order.
    commit_log: BTreeMap<u64, Vec<(String, String)>>,
    /// Who pushed each pull request's head.
    pushers: BTreeMap<u64, String>,
    /// Squash commit messages the forge wrote, for the log: time, number, message.
    pub landed_messages: Vec<(Time, u64, String)>,
    /// Checks reported by rule actions.
    pub posted_checks: PostedChecks,
    /// Workflow dispatches: pull request, workflow, ref, inputs.
    pub dispatches: Vec<(u64, String, String, BTreeMap<String, String>)>,
    /// Branches deleted by `delete_head_branch`.
    pub deleted_branches: Vec<String>,
    next_copy: u64,
    next_review: u64,
    /// Operator controls in the order they were made, each with the number of landings that
    /// had happened before it (the checker judges every landing by the controls before it).
    pub controls: Vec<(usize, Control)>,
    /// Controls the engine has not been told yet (the runner hands them over at once, as the
    /// API of `serve` does).
    pub undelivered_controls: Vec<Control>,
    /// Checks the base branches' protection requires (`branch_protection_injection_mode`).
    pub required_checks: BTreeMap<String, Vec<String>>,
}

/// `Name <email>` of a simulated account.
pub fn sim_identity(login: &str) -> String {
    format!("{login} <{login}@users.noreply.example>")
}

impl SimForge<Repo> {
    /// A forge with git in memory and one base branch holding `files`.
    pub fn new(base: &str, files: Tree, queue_scripts: Vec<CiScript>, flake_seed: u64) -> Self {
        SimForge::with_git(Repo::new(), base, files, queue_scripts, flake_seed)
    }
}

impl<G: GitStore> SimForge<G> {
    /// A forge on an empty git store `repo`, with one base branch holding `files`.
    pub fn with_git(
        mut repo: G,
        base: &str,
        files: Tree,
        queue_scripts: Vec<CiScript>,
        flake_seed: u64,
    ) -> Self {
        let root = repo.commit(files, vec![], "initial commit");
        repo.create_ref(base, &root).expect("fresh repo");
        SimForge {
            clock: VirtualClock::default(),
            repo,
            real_ci: false,
            processes: 0,
            pending: BTreeMap::new(),
            order: 0,
            check_seq: 0,
            next_draft: 1000,
            defs: BTreeMap::new(),
            prs: BTreeMap::new(),
            queue_scripts,
            flake_seed,
            attempts: BTreeMap::new(),
            pushes: BTreeMap::new(),
            faults: Faults::none(),
            rng: Rng::new(0),
            cars: BTreeMap::new(),
            landings: Vec::new(),
            external_commits: BTreeSet::new(),
            initial_commit: root,
            base_branches: [base.to_string()].into(),
            notes: Vec::new(),
            dropped: 0,
            duplicated: 0,
            ci_timing: CiTiming::default(),
            default_branch: base.to_string(),
            permissions: BTreeMap::new(),
            comments: BTreeMap::new(),
            next_comment: 0,
            reactions: Vec::new(),
            status_comments: BTreeMap::new(),
            controls_comments: BTreeMap::new(),
            protection_checks: Vec::new(),
            commit_log: BTreeMap::new(),
            pushers: BTreeMap::new(),
            landed_messages: Vec::new(),
            posted_checks: PostedChecks::default(),
            dispatches: Vec::new(),
            deleted_branches: Vec::new(),
            next_copy: 2000,
            next_review: 0,
            controls: Vec::new(),
            undelivered_controls: Vec::new(),
            required_checks: BTreeMap::new(),
        }
    }

    /// Another branch at the initial commit (a destination for copies).
    pub fn add_branch(&mut self, name: &str) {
        let root = self.initial_commit.clone();
        self.repo.create_ref(name, &root).ok();
    }

    /// The pause and the freezes in force for the landing with index `landing` (made before it).
    pub fn controls_before(&self, landing: usize) -> (Option<String>, Vec<Freeze>) {
        let mut pause = None;
        let mut freezes: BTreeMap<u64, Freeze> = BTreeMap::new();
        for (n, c) in &self.controls {
            if *n > landing {
                break;
            }
            match c {
                Control::Pause { reason, .. } => pause = Some(reason.clone()),
                Control::Unpause => pause = None,
                Control::Freeze { freeze } => {
                    freezes.insert(freeze.id, freeze.clone());
                }
                Control::Unfreeze { id } => {
                    freezes.remove(id);
                }
            }
        }
        (pause, freezes.into_values().collect())
    }

    /// A comment as the engine sees it (with the sender's permission now).
    pub fn read_comment(&self, id: u64) -> Option<Comment> {
        let c = self.comments.get(&id)?;
        Some(Comment {
            id: c.id,
            number: c.number,
            author: c.author.clone(),
            sender: c.last_editor.clone(),
            body: c.body.clone(),
            edited: c.version > 0,
            version: c.version.to_string(),
            sender_permission: Some(
                self.permissions
                    .get(&c.last_editor)
                    .cloned()
                    .unwrap_or_else(|| "none".into()),
            ),
            by_queue: c.author == BOT,
            head_sha: Some(c.head_sha.clone()),
        })
    }

    /// Every comment (for reconciliation).
    pub fn all_comments(&self) -> Vec<Comment> {
        self.comments
            .keys()
            .filter_map(|id| self.read_comment(*id))
            .collect()
    }

    fn add_comment(&mut self, number: u64, author: &str, body: String) -> u64 {
        self.next_comment += 1;
        let id = self.next_comment;
        let head_sha = self
            .prs
            .get(&number)
            .map(|p| p.head_sha.clone())
            .unwrap_or_default();
        self.comments.insert(
            id,
            SimComment {
                id,
                number,
                author: author.to_string(),
                body,
                version: 0,
                last_editor: author.to_string(),
                head_sha,
            },
        );
        id
    }

    /// The text of the queue's own comment on a pull request (status or controls).
    pub fn queue_comment(&self, number: u64, checkbox: CheckBox) -> Option<&str> {
        let map = match checkbox {
            CheckBox::Queue => &self.controls_comments,
            CheckBox::Requeue => &self.status_comments,
        };
        map.get(&number)
            .and_then(|id| self.comments.get(id))
            .map(|c| c.body.as_str())
    }

    /// Comments on a pull request (bodies, oldest first).
    pub fn comments_on(&self, number: u64) -> Vec<&SimComment> {
        self.comments
            .values()
            .filter(|c| c.number == number)
            .collect()
    }

    /// The forge's own squash message ("inherit"): title `<title> (#n)`; body one `* <message>`
    /// line per commit when the pull request has more than one commit, else empty.
    fn inherited_message(&self, number: u64) -> (String, String) {
        let pr = &self.prs[&number];
        let log = self.commit_log.get(&number).cloned().unwrap_or_default();
        let body = if log.len() > 1 {
            log.iter()
                .map(|(_, m)| format!("* {m}"))
                .collect::<Vec<_>>()
                .join("\n")
        } else {
            String::new()
        };
        (format!("{} (#{number})", pr.title), body)
    }

    fn apply_cosmetic(&mut self, c: &Cosmetic) {
        match c {
            Cosmetic::Labels {
                number,
                add,
                remove,
            } => {
                let Some(p) = self.prs.get_mut(number) else {
                    return;
                };
                let before = p.labels.clone();
                p.labels.extend(add.iter().cloned());
                for l in remove {
                    p.labels.remove(l);
                }
                if p.labels != before {
                    self.emit(
                        Hint::PullRequest(*number),
                        format!("#{number} labeled by the queue"),
                    );
                }
            }
            Cosmetic::StatusComment { number, body }
            | Cosmetic::ControlsComment { number, body } => {
                let status = matches!(c, Cosmetic::StatusComment { .. });
                let existing = if status {
                    self.status_comments.get(number)
                } else {
                    self.controls_comments.get(number)
                }
                .copied();
                match existing.and_then(|id| self.comments.get_mut(&id)) {
                    Some(sc) => {
                        sc.body = body.clone();
                        sc.version += 1;
                        sc.last_editor = BOT.to_string();
                    }
                    None => {
                        let id = self.add_comment(*number, BOT, body.clone());
                        if status {
                            self.status_comments.insert(*number, id);
                        } else {
                            self.controls_comments.insert(*number, id);
                        }
                    }
                }
            }
            Cosmetic::Reply { number, body, .. } => {
                self.add_comment(*number, BOT, body.clone());
            }
            Cosmetic::React {
                comment, content, ..
            } => self.reactions.push((*comment, content.clone())),
            Cosmetic::ProtectionCheck {
                number,
                head_sha,
                name,
                ok,
                ..
            } => self
                .protection_checks
                .push((*number, head_sha.clone(), name.clone(), *ok)),
        }
    }

    pub fn set_faults(&mut self, faults: Faults) {
        self.rng = Rng::new(faults.seed);
        if faults.reconcile_every > 0 {
            self.push_pending(faults.reconcile_every, Pending::Reconcile);
        }
        self.faults = faults;
    }

    pub fn faults(&self) -> &Faults {
        &self.faults
    }

    pub fn now(&self) -> Time {
        self.clock.now()
    }

    pub fn define(&mut self, def: PrDef) {
        self.base_branches.insert(def.base.clone());
        self.defs.insert(def.number, def);
    }

    /// Applies a scenario action now (a push that races the queue, for example).
    pub fn act_now(&mut self, action: Action) -> String {
        self.act(action)
    }

    /// Delivers a reply to the engine at `at`.
    pub fn schedule_reply(&mut self, at: Time, event: Event) {
        self.push_pending(at, Pending::Reply(Box::new(event)));
    }

    /// The engine crashed: whatever was on its way to it (replies, webhook deliveries, its
    /// timers) is lost. Returns how many items were dropped.
    pub fn drop_engine_inbox(&mut self) -> usize {
        let before = self.pending.len();
        self.pending.retain(|_, p| {
            !matches!(
                p,
                Pending::Reply(_) | Pending::Deliver { .. } | Pending::Tick
            )
        });
        before - self.pending.len()
    }

    /// Schedules a scenario action at `at`.
    pub fn schedule(&mut self, at: Time, action: Action) {
        self.push_pending(at, Pending::Act(action));
    }

    fn push_pending(&mut self, at: Time, p: Pending) {
        self.order += 1;
        self.pending.insert((at, self.order), p);
    }

    /// Whether anything but reconciliation is pending.
    pub fn has_work(&self) -> bool {
        self.pending
            .values()
            .any(|p| !matches!(p, Pending::Reconcile))
    }

    /// Sends a webhook through the delivery shim.
    fn emit(&mut self, hint: Hint, text: String) {
        let at = self.now();
        if self.faults.is_none() {
            self.push_pending(at, Pending::Deliver { hint, text });
            return;
        }
        if self.rng.unit() < self.faults.drop {
            self.dropped += 1;
            self.notes.push((at, format!("webhook dropped: {text}")));
            return;
        }
        let delay = if self.rng.unit() < self.faults.delay {
            self.rng.upto(self.faults.max_delay)
        } else {
            0
        };
        if self.rng.unit() < self.faults.duplicate {
            self.duplicated += 1;
            let extra = self.rng.upto(self.faults.max_delay);
            self.push_pending(
                at + delay + extra,
                Pending::Deliver {
                    hint: hint.clone(),
                    text: format!("{text} (duplicate)"),
                },
            );
        }
        self.push_pending(at + delay, Pending::Deliver { hint, text });
    }

    /// The next item in time order, advancing the clock to it.
    pub fn next_item(&mut self) -> Option<(Time, Item)> {
        // Keep the periodic reconciliation alive while there is work (the work may have been
        // created after the last pass, by the engine's own decisions).
        if self.faults.reconcile_every > 0
            && self.has_work()
            && !self
                .pending
                .values()
                .any(|p| matches!(p, Pending::Reconcile))
        {
            let at = self.now() + self.faults.reconcile_every;
            self.push_pending(at, Pending::Reconcile);
        }
        loop {
            let (&key, _) = self.pending.iter().next()?;
            let pending = self.pending.remove(&key)?;
            self.clock.advance_to(key.0);
            let t = key.0;
            match pending {
                Pending::Act(a) => return Some((t, Item::Action(self.act(a)))),
                Pending::CheckDone {
                    number,
                    seq,
                    conclusion,
                } => {
                    let Some(text) = self.complete_check(number, seq, conclusion) else {
                        continue;
                    };
                    return Some((t, Item::Ci(text)));
                }
                Pending::Deliver { hint, text } => return Some((t, Item::Webhook { hint, text })),
                Pending::Tick => return Some((t, Item::Tick)),
                Pending::Reconcile => return Some((t, Item::Reconcile)),
                Pending::Reply(e) => return Some((t, Item::Reply(e))),
                Pending::CreateRun {
                    number,
                    run,
                    conclusion,
                    duration,
                } => {
                    let Some(text) = self.create_run(number, run, conclusion, duration) else {
                        continue;
                    };
                    return Some((t, Item::Ci(text)));
                }
                Pending::Cancel(number) => {
                    self.cancel_runs(number);
                    continue;
                }
            }
        }
    }

    fn complete_check(&mut self, number: u64, seq: u64, conclusion: Conclusion) -> Option<String> {
        let pr = self.prs.get_mut(&number)?;
        let run = pr.checks.iter_mut().find(|c| c.seq == seq)?;
        if run.conclusion.is_some() {
            return None; // cancelled meanwhile
        }
        run.conclusion = Some(conclusion);
        let text = format!(
            "#{number} check `{}` completed on {}: {}",
            run.name,
            short(&run.head_sha),
            conclusion_name(conclusion)
        );
        self.emit(Hint::PullRequest(number), text.clone());
        Some(text)
    }

    /// Creates a check run (late), unless its pull request was closed meanwhile.
    fn create_run(
        &mut self,
        number: u64,
        run: CheckRun,
        conclusion: Option<Conclusion>,
        duration: Time,
    ) -> Option<String> {
        let pr = self.prs.get_mut(&number)?;
        if pr.closed {
            return None;
        }
        let (seq, name, head) = (run.seq, run.name.clone(), run.head_sha.clone());
        pr.checks.push(run);
        let text = format!("#{number} check `{name}` started late on {}", short(&head));
        self.emit(Hint::PullRequest(number), text.clone());
        if let Some(c) = conclusion {
            let at = self.now() + duration;
            self.push_pending(
                at,
                Pending::CheckDone {
                    number,
                    seq,
                    conclusion: c,
                },
            );
        }
        Some(text)
    }

    /// Cancels the running checks of a (closed) pull request.
    fn cancel_runs(&mut self, number: u64) {
        if let Some(pr) = self.prs.get_mut(&number) {
            for r in pr.checks.iter_mut().filter(|r| r.conclusion.is_none()) {
                r.conclusion = Some(Conclusion::Cancelled);
            }
        }
    }

    /// Queue drafts on `base` that are open or still running CI after being closed (I4).
    pub fn drafts_with_ci(&self, base: &str) -> usize {
        self.prs
            .values()
            .filter(|p| p.author == BOT && p.base == base && !is_copy(p))
            .filter(|p| !p.closed || p.checks.iter().any(|c| c.conclusion.is_none()))
            .count()
    }

    /// Whether pull request `n`'s changes are all in `tree`.
    fn present(&self, n: u64, tree: &Tree) -> bool {
        let Some(pr) = self.prs.get(&n) else {
            return false;
        };
        let Some(base_tip) = self.repo.ref_sha(&pr.base) else {
            return false;
        };
        let head = pr.head_sha.clone();
        let Some(mb) = self.repo.merge_base(&base_tip, &head) else {
            return false;
        };
        let head_tree = self.repo.tree(&head);
        let changed = self.repo.changed_paths(&mb, &head);
        !changed.is_empty() && changed.iter().all(|p| tree.get(p) == head_tree.get(p))
    }

    /// Starts the CI scripts that apply to pull request `number` on its current head.
    fn start_ci(&mut self, number: u64, queue: bool) {
        let Some(pr) = self.prs.get(&number) else {
            return;
        };
        let head = pr.head_sha.clone();
        let scripts: Vec<CiScript> = if queue {
            self.queue_scripts
                .iter()
                .filter(|s| s.runs_on_queue())
                .cloned()
                .collect()
        } else {
            self.defs
                .get(&number)
                .map(|d| d.scripts.clone())
                .unwrap_or_default()
                .into_iter()
                .filter(|s| s.runs_on_pull_request())
                .collect()
        };
        let tree = self.repo.tree(&head).clone();
        let tree_id = self.repo.tree_id_of(&head);
        for script in scripts {
            self.check_seq += 1;
            let seq = self.check_seq;
            let run = CheckRun {
                name: script.check.clone(),
                app: script.app.clone(),
                head_sha: head.clone(),
                conclusion: None,
                seq,
                ..Default::default()
            };
            let key = format!("{tree_id}/{}", script.check);
            let attempt = self.attempts.entry(key.clone()).or_default();
            let draw = keyed_unit(self.flake_seed, &format!("{key}/{attempt}"));
            *attempt += 1;
            let conclusion = if self.real_ci || script.run.is_some() {
                let prs_present = !script.fail_when_prs.is_empty()
                    && script.fail_when_prs.iter().all(|n| self.present(*n, &tree));
                let env = [
                    ("SCHLANGE_CHECK", script.check.as_str()),
                    ("SCHLANGE_SHA", head.as_str()),
                ];
                let code = self
                    .repo
                    .run_in_checkout(&head, &script.shell_command(), &env);
                self.processes += 1;
                script.conclude_exit(code, prs_present, draw)
            } else {
                script.conclude(&tree, |n| self.present(n, &tree), draw)
            };
            let timing = &self.ci_timing;
            let mut duration = script.duration;
            let mut create = 0;
            if timing.jitter > 0.0 || timing.max_create_delay > 0 {
                let key = format!("{tree_id}/{}/{seq}", script.check);
                let j = keyed_unit(timing.seed, &format!("{key}/jitter")) * 2.0 - 1.0;
                duration = ((duration as f64) * (1.0 + timing.jitter * j))
                    .round()
                    .max(1.0) as Time;
                let c = keyed_unit(timing.seed, &format!("{key}/create"));
                create = (c * (timing.max_create_delay + 1) as f64) as Time;
            }
            if create > 0 {
                let at = self.now() + create;
                self.push_pending(
                    at,
                    Pending::CreateRun {
                        number,
                        run,
                        conclusion,
                        duration,
                    },
                );
                continue;
            }
            self.prs.get_mut(&number).expect("pr").checks.push(run);
            self.emit(
                Hint::PullRequest(number),
                format!(
                    "#{number} check `{}` started on {}",
                    script.check,
                    short(&head)
                ),
            );
            if let Some(c) = conclusion {
                let at = self.now() + duration;
                self.push_pending(
                    at,
                    Pending::CheckDone {
                        number,
                        seq,
                        conclusion: c,
                    },
                );
            }
        }
    }

    fn apply_files(tree: &mut Tree, files: &[(String, String)]) {
        for (p, c) in files {
            tree.insert(p.clone(), c.clone());
        }
    }

    fn act(&mut self, action: Action) -> String {
        match action {
            Action::Open(n) => {
                let Some(def) = self.defs.get(&n).cloned() else {
                    return format!("#{n} is not defined");
                };
                let Some(tip) = self.repo.ref_sha(&def.base) else {
                    return format!("#{n}: base `{}` does not exist", def.base);
                };
                let mut tree = self.repo.tree(&tip).clone();
                Self::apply_files(&mut tree, &def.files);
                let head = self
                    .repo
                    .commit(tree, vec![tip], &format!("{} (#{n})", def.title));
                let head_ref = format!("pr-{n}");
                self.repo.create_ref(&head_ref, &head).ok();
                let mut pr = PullRequest::new(n, &def.author, &def.base, &head_ref, &head);
                pr.title = def.title.clone();
                pr.draft = def.draft;
                pr.labels = def.labels.clone();
                pr.dependabot_update_types = def.dependabot_update_types.clone();
                pr.body = def.body.clone();
                pr.from_fork = def.fork;
                pr.approved_by = def.approvals.clone();
                for a in &def.approvals {
                    self.next_review += 1;
                    pr.reviews.push(schlange_core::model::Review {
                        id: self.next_review,
                        author: a.clone(),
                        state: "APPROVED".into(),
                        commit: Some(head.clone()),
                        body: String::new(),
                    });
                }
                self.prs.insert(n, pr);
                self.commit_log.insert(
                    n,
                    vec![(sim_identity(&def.author), format!("{} (#{n})", def.title))],
                );
                self.pushers.insert(n, def.author.clone());
                self.emit(Hint::PullRequest(n), format!("#{n} opened"));
                self.start_ci(n, false);
                let files: Vec<&str> = def.files.iter().map(|(p, _)| p.as_str()).collect();
                format!(
                    "#{n} opened by {} on {}{}{}",
                    def.author,
                    def.base,
                    if def.draft { " (draft)" } else { "" },
                    if files.is_empty() {
                        String::new()
                    } else {
                        format!(", files: {}", files.join(" "))
                    }
                )
            }
            Action::Push { number, files } => {
                let by = self
                    .prs
                    .get(&number)
                    .map(|p| p.author.clone())
                    .unwrap_or_default();
                self.act(Action::PushAs { number, by, files })
            }
            Action::PushAs { number, by, files } => {
                let Some(pr) = self.prs.get(&number).cloned() else {
                    return format!("push to unknown #{number}");
                };
                let k = {
                    let k = self.pushes.entry(number).or_default();
                    *k += 1;
                    *k
                };
                let files = if files.is_empty() {
                    let path = self
                        .defs
                        .get(&number)
                        .and_then(|d| d.files.first().map(|f| f.0.clone()))
                        .unwrap_or_else(|| format!("pr-{number}.txt"));
                    vec![(path, format!("push {k} by #{number}\n"))]
                } else {
                    files
                };
                let mut tree = self.repo.tree(&pr.head_sha).clone();
                Self::apply_files(&mut tree, &files);
                let head = self.repo.commit(
                    tree,
                    vec![pr.head_sha.clone()],
                    &format!("push {k} to #{number}"),
                );
                self.repo.update_ref(&pr.head_ref, &pr.head_sha, &head).ok();
                self.prs.get_mut(&number).expect("pr").head_sha = head.clone();
                self.commit_log
                    .entry(number)
                    .or_default()
                    .push((sim_identity(&by), format!("push {k} to #{number}")));
                self.pushers.insert(number, by.clone());
                self.emit(Hint::PullRequest(number), format!("#{number} synchronize"));
                self.start_ci(number, false);
                if by == pr.author {
                    format!("#{number} pushed, head {}", short(&head))
                } else {
                    format!("#{number} pushed by {by}, head {}", short(&head))
                }
            }
            Action::Comment { number, by, body } => {
                if !self.prs.contains_key(&number) {
                    return format!("comment on unknown #{number}");
                }
                let id = self.add_comment(number, &by, body.clone());
                self.emit(Hint::Comment(id), format!("#{number} comment {id} by {by}"));
                format!(
                    "{by} commented on #{number}: {}",
                    body.lines().next().unwrap_or("")
                )
            }
            Action::Tick {
                number,
                by,
                checkbox,
            } => {
                let map = match checkbox {
                    CheckBox::Queue => &self.controls_comments,
                    CheckBox::Requeue => &self.status_comments,
                };
                let Some(id) = map.get(&number).copied() else {
                    return format!(
                        "#{number} has no queue comment with a checkbox; nothing ticked"
                    );
                };
                let head = self
                    .prs
                    .get(&number)
                    .map(|p| p.head_sha.clone())
                    .unwrap_or_default();
                let c = self.comments.get_mut(&id).expect("comment");
                // The person vouches for the head they see when ticking.
                c.head_sha = head;
                if !c.body.contains("- [ ]") {
                    return format!("#{number}: the queue's comment has no open checkbox");
                }
                c.body = c.body.replacen("- [ ]", "- [x]", 1);
                c.version += 1;
                c.last_editor = by.clone();
                self.emit(
                    Hint::Comment(id),
                    format!("#{number} comment {id} edited by {by}"),
                );
                format!("{by} ticked the {checkbox:?} checkbox on #{number}")
            }
            Action::Review { number, by, state } => {
                self.next_review += 1;
                let id = self.next_review;
                let Some(p) = self.prs.get_mut(&number) else {
                    return format!("review of unknown #{number}");
                };
                p.requested_reviewers.retain(|r| *r != by);
                let commit = Some(p.head_sha.clone());
                p.reviews.push(schlange_core::model::Review {
                    id,
                    author: by.clone(),
                    state: state.clone(),
                    commit,
                    body: String::new(),
                });
                self.emit(Hint::PullRequest(number), format!("#{number} reviewed"));
                format!("{by} reviewed #{number}: {state}")
            }
            Action::Control(c) => {
                let text = match &c {
                    Control::Pause { reason, .. } => format!("the queue is paused: {reason}"),
                    Control::Unpause => "the queue is unpaused".into(),
                    Control::Freeze { freeze } => format!(
                        "freeze {} `{}` from {} to {}",
                        freeze.id,
                        freeze.reason,
                        freeze.start,
                        freeze.end.map_or("its removal".into(), |e| e.to_string())
                    ),
                    Control::Unfreeze { id } => format!("freeze {id} deleted"),
                };
                self.controls.push((self.landings.len(), c.clone()));
                self.undelivered_controls.push(c);
                text
            }
            Action::EditBody { number, body } => {
                if let Some(p) = self.prs.get_mut(&number) {
                    p.body = body;
                }
                self.emit(Hint::PullRequest(number), format!("#{number} edited"));
                format!("#{number} description edited")
            }
            Action::Label { number, label } => {
                if let Some(p) = self.prs.get_mut(&number) {
                    p.labels.insert(label.clone());
                }
                self.emit(Hint::PullRequest(number), format!("#{number} labeled"));
                format!("#{number} labeled {label}")
            }
            Action::Unlabel { number, label } => {
                if let Some(p) = self.prs.get_mut(&number) {
                    p.labels.remove(&label);
                }
                self.emit(Hint::PullRequest(number), format!("#{number} unlabeled"));
                format!("#{number} unlabeled {label}")
            }
            Action::Draft { number } => {
                if let Some(p) = self.prs.get_mut(&number) {
                    p.draft = true;
                }
                self.emit(
                    Hint::PullRequest(number),
                    format!("#{number} converted_to_draft"),
                );
                format!("#{number} converted to draft")
            }
            Action::Ready { number } => {
                if let Some(p) = self.prs.get_mut(&number) {
                    p.draft = false;
                }
                self.emit(
                    Hint::PullRequest(number),
                    format!("#{number} ready_for_review"),
                );
                format!("#{number} ready for review")
            }
            Action::Close { number } => {
                if let Some(p) = self.prs.get_mut(&number) {
                    p.closed = true;
                }
                self.emit(Hint::PullRequest(number), format!("#{number} closed"));
                format!("#{number} closed")
            }
            Action::PushBase { branch, files } => {
                let Some(tip) = self.repo.ref_sha(&branch) else {
                    return format!("push to unknown branch `{branch}`");
                };
                let mut tree = self.repo.tree(&tip).clone();
                Self::apply_files(&mut tree, &files);
                let c = self
                    .repo
                    .commit(tree, vec![tip.clone()], "direct push outside the queue");
                self.repo.update_ref(&branch, &tip, &c).expect("CAS at tip");
                self.external_commits.insert(c.clone());
                self.emit(Hint::Branch(branch.clone()), format!("push to {branch}"));
                format!("direct push to {branch}: {}", short(&c))
            }
            Action::MergeManually(n) => {
                let Some(pr) = self.prs.get(&n).cloned() else {
                    return format!("manual merge of unknown #{n}");
                };
                if pr.closed {
                    return format!("manual merge of #{n} refused: closed");
                }
                let tip = self.repo.ref_sha(&pr.base).expect("base");
                match self
                    .repo
                    .squash(&tip, &pr.head_sha, &format!("{} (#{n}) by hand", pr.title))
                {
                    Ok(c) => {
                        self.repo.update_ref(&pr.base, &tip, &c).expect("CAS");
                        self.external_commits.insert(c.clone());
                        let p = self.prs.get_mut(&n).expect("pr");
                        p.merged = true;
                        p.closed = true;
                        p.merge_commit_sha = Some(c.clone());
                        self.emit(
                            Hint::Branch(pr.base.clone()),
                            format!("push to {}", pr.base),
                        );
                        self.emit(Hint::PullRequest(n), format!("#{n} closed (merged)"));
                        format!("#{n} merged by hand onto {}: {}", pr.base, short(&c))
                    }
                    Err(_) => format!("manual merge of #{n} refused: conflict"),
                }
            }
            Action::ForcePushQueueBranch {
                number,
                parents,
                files,
            } => {
                let car = self
                    .cars
                    .values()
                    .filter(|c| !c.closed && !c.in_place)
                    .find(|c| c.prefix.iter().any(|(p, ..)| *p == number))
                    .cloned();
                let Some(car) = car else {
                    return format!("no open queue branch holds #{number}; nothing pushed");
                };
                let branch = car.branch.clone().expect("draft car");
                let tip = self.repo.ref_sha(&branch).expect("branch");
                let mut tree = self.repo.tree(&tip).clone();
                for (p, c) in files {
                    tree.insert(p, c);
                }
                let c = self.repo.commit(tree, parents, "foreign force push");
                self.repo.update_ref(&branch, &tip, &c).expect("CAS");
                if let Some(d) = car.draft
                    && let Some(p) = self.prs.get_mut(&d)
                {
                    p.head_sha = c.clone();
                    self.start_ci(d, true);
                }
                self.emit(Hint::Branch(branch.clone()), format!("push to {branch}"));
                format!("foreign force push to {branch}: {}", short(&c))
            }
            Action::PushQueueBranch(n) => {
                let car = self
                    .cars
                    .values()
                    .filter(|c| !c.closed && !c.in_place)
                    .find(|c| c.prefix.iter().any(|(p, ..)| *p == n))
                    .cloned();
                let Some(car) = car else {
                    return format!("no open queue branch holds #{n}; nothing pushed");
                };
                let branch = car.branch.clone().expect("draft car");
                let tip = self.repo.ref_sha(&branch).expect("branch");
                let mut tree = self.repo.tree(&tip).clone();
                tree.insert("foreign-edit.txt".into(), format!("edit of {branch}\n"));
                let c = self.repo.commit(tree, vec![tip.clone()], "foreign edit");
                self.repo.update_ref(&branch, &tip, &c).expect("CAS");
                if let Some(d) = car.draft
                    && let Some(p) = self.prs.get_mut(&d)
                {
                    p.head_sha = c.clone();
                    self.start_ci(d, true);
                }
                self.emit(Hint::Branch(branch.clone()), format!("push to {branch}"));
                format!("foreign push to {branch}: {}", short(&c))
            }
        }
    }

    /// Reads a pull request as it is now, with the fields the forge derives from git.
    pub fn read_pull_request(&self, n: u64) -> Option<PullRequest> {
        let mut pr = self.prs.get(&n)?.clone();
        if let Some(tip) = self.repo.ref_sha(&pr.base) {
            if let Some(mb) = self.repo.merge_base(&tip, &pr.head_sha) {
                pr.files = self.repo.changed_paths(&mb, &pr.head_sha);
            }
            pr.behind_base = !self.repo.is_ancestor(&tip, &pr.head_sha);
            pr.base_tip = Some(tip);
        }
        pr.head_tree = Some(self.repo.tree_id_of(&pr.head_sha));
        pr.commit_messages = self
            .commit_log
            .get(&n)
            .map(|l| l.iter().map(|(_, m)| m.clone()).collect())
            .unwrap_or_default();
        if !pr.reviews.is_empty() {
            let mut latest: BTreeMap<&str, &str> = BTreeMap::new();
            for r in &pr.reviews {
                latest.insert(&r.author, &r.state);
            }
            pr.approved_by = latest
                .into_iter()
                .filter(|(_, s)| *s == "APPROVED")
                .map(|(a, _)| a.to_string())
                .collect();
        }
        if pr.author == BOT {
            return Some(pr);
        }
        pr.commit_authors = self
            .commit_log
            .get(&n)
            .map(|l| l.iter().map(|(a, _)| a.clone()).collect())
            .unwrap_or_default();
        pr.head_pusher = self.pushers.get(&n).cloned();
        // Depends-On, resolved against this repository's pull requests.
        let (refs, _) = schlange_core::protections::depends_on(&pr.body);
        pr.dependencies = refs
            .iter()
            .filter(|r| r.repo.is_none())
            .map(|r| Dependency {
                reference: r.full(""),
                state: match self.prs.get(&r.number) {
                    Some(p) if p.merged => DependencyState::Merged,
                    Some(p) if p.closed => DependencyState::Closed,
                    Some(_) => DependencyState::Open,
                    None => DependencyState::Missing,
                },
            })
            .collect();
        // The configuration check, for pull requests against the default branch.
        if pr.base == self.default_branch {
            let head_tree = self.repo.tree(&pr.head_sha);
            pr.config_change = schlange_config::config_file::assess_change(
                &pr.files,
                |p| head_tree.get(p).cloned(),
                schlange_config::config_file::validate_without_schema,
            );
        }
        Some(pr)
    }

    pub fn read_branch(&self, name: &str) -> Option<Sha> {
        self.repo.ref_sha(name)
    }

    pub fn numbers(&self) -> Vec<u64> {
        self.prs.keys().copied().collect()
    }

    /// A truth read of these pull requests and branches.
    pub fn snapshot(&self, numbers: &[u64], branches: &[String], token: Option<u64>) -> Event {
        Event::Snapshot {
            pull_requests: numbers
                .iter()
                .filter_map(|n| self.read_pull_request(*n))
                .collect(),
            branches: branches
                .iter()
                .map(|b| (b.clone(), self.read_branch(b)))
                .collect(),
            token,
        }
    }

    /// A full reconciliation read: every pull request, every base and queue branch, and the
    /// branches the engine believes in (`known`), `None` when gone.
    pub fn snapshot_all(&self, known: &[String]) -> Event {
        let mut branches: Vec<String> = self
            .repo
            .ref_names()
            .into_iter()
            .filter(|r| !r.starts_with("pr-"))
            .collect();
        branches.extend(known.iter().cloned());
        branches.sort();
        branches.dedup();
        self.snapshot(&self.numbers(), &branches, None)
    }

    /// Open queue draft pull requests per base branch.
    pub fn open_drafts(&self) -> Vec<&PullRequest> {
        self.prs
            .values()
            .filter(|p| p.author == BOT && !p.closed && !is_copy(p))
            .collect()
    }

    fn conflict_partners(&mut self, base_sha: &str, prs: &[CarPr], i: usize) -> (bool, Vec<u64>) {
        let head = prs[i].head_sha.clone();
        if self.repo.squash(base_sha, &head, "probe").is_err() {
            return (true, vec![]);
        }
        let mut with = Vec::new();
        for p in &prs[..i] {
            if let Ok(m) = self.repo.merge(base_sha, &p.head_sha, "probe")
                && self.repo.merge(&m, &head, "probe").is_err()
            {
                with.push(p.number);
            }
        }
        if with.is_empty() {
            with = prs[..i].iter().map(|p| p.number).collect();
        }
        (false, with)
    }

    /// Carries out one decision of the engine and returns the forge's replies.
    pub fn execute(&mut self, decision: &Decision) -> Vec<Event> {
        match decision {
            Decision::BuildCar {
                car,
                queue,
                base_branch,
                branch,
                base_sha,
                prs,
                own,
                stack_on,
            } => {
                let mut cur = stack_on.clone().unwrap_or_else(|| base_sha.clone());
                let mut prefix = Vec::new();
                for (i, p) in prs.iter().enumerate() {
                    if stack_on.is_some() && !own.contains(&p.number) {
                        continue; // carried by the branch it is stacked on
                    }
                    match self
                        .repo
                        .merge(&cur, &p.head_sha, &format!("Merge #{}", p.number))
                    {
                        Ok(c) => {
                            prefix.push((p.number, c.clone(), self.repo.tree_id_of(&c)));
                            cur = c;
                        }
                        Err(_) => {
                            let (with_base, with) = self.conflict_partners(base_sha, prs, i);
                            return vec![Event::CarConflict {
                                car: *car,
                                number: p.number,
                                with_base,
                                with,
                            }];
                        }
                    }
                }
                if self.repo.create_ref(branch, &cur).is_err() {
                    self.notes
                        .push((self.now(), format!("queue branch {branch} already existed")));
                    let old = self.repo.ref_sha(branch).expect("exists");
                    self.repo.update_ref(branch, &old, &cur).expect("CAS");
                }
                let draft = self.next_draft;
                self.next_draft += 1;
                let mut pr = PullRequest::new(draft, BOT, base_branch, branch, &cur);
                pr.draft = true;
                pr.title = format!(
                    "merge queue: embarking {}",
                    own.iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                self.prs.insert(draft, pr);
                let tree = self.repo.tree_id_of(&cur);
                self.cars.insert(
                    *car,
                    CarRecord {
                        id: *car,
                        queue: queue.clone(),
                        base_branch: base_branch.clone(),
                        branch: Some(branch.clone()),
                        draft: Some(draft),
                        in_place: false,
                        base_sha: stack_on.clone().unwrap_or_else(|| base_sha.clone()),
                        prefix,
                        heads: prs.iter().map(|p| (p.number, p.head_sha.clone())).collect(),
                        head: cur.clone(),
                        tree: tree.clone(),
                        closed: false,
                    },
                );
                self.emit(Hint::PullRequest(draft), format!("#{draft} opened"));
                self.start_ci(draft, true);
                vec![Event::CarBuilt {
                    car: *car,
                    branch: branch.clone(),
                    head_sha: cur,
                    tree,
                    draft,
                }]
            }
            Decision::CheckInPlace {
                car,
                number,
                head_sha,
                ..
            } => {
                // Idempotent: a repeated announcement (after a restart) keeps the first record.
                if let Some(pr) = self.prs.get(number).cloned()
                    && !self.cars.contains_key(car)
                {
                    let base_sha = self.read_branch(&pr.base).unwrap_or_default();
                    let tree = self.repo.tree_id_of(head_sha);
                    self.cars.insert(
                        *car,
                        CarRecord {
                            id: *car,
                            queue: String::new(),
                            base_branch: pr.base.clone(),
                            branch: None,
                            draft: Some(*number),
                            in_place: true,
                            base_sha,
                            prefix: vec![(*number, head_sha.clone(), tree.clone())],
                            heads: vec![(*number, head_sha.clone())],
                            head: head_sha.clone(),
                            tree,
                            closed: false,
                        },
                    );
                }
                vec![]
            }
            Decision::CloseCar { car, branch, draft } => {
                if let Some(b) = branch
                    && self.repo.delete_ref(b)
                {
                    self.emit(Hint::Branch(b.clone()), format!("delete {b}"));
                }
                // The named draft and any open queue draft on the branch (the engine may not
                // know the draft's number after a lost reply).
                let drafts: Vec<u64> = self
                    .prs
                    .values()
                    .filter(|p| {
                        p.author == BOT
                            && (Some(p.number) == *draft
                                || branch.as_ref().is_some_and(|b| *b == p.head_ref))
                    })
                    .map(|p| p.number)
                    .collect();
                let mut running = false;
                for d in drafts {
                    let p = self.prs.get_mut(&d).expect("draft");
                    p.closed = true;
                    running |= p.checks.iter().any(|c| c.conclusion.is_none());
                }
                if let Some(r) = self.cars.get_mut(car) {
                    r.closed = true;
                }
                // Closing the draft cancels its CI (as `cancel-in-progress` setups do); the
                // forge confirms once nothing runs any more.
                let delay = if running && self.ci_timing.max_cancel_delay > 0 {
                    let u = keyed_unit(self.ci_timing.seed, &format!("cancel/{car}"));
                    1 + (u * self.ci_timing.max_cancel_delay as f64) as Time
                } else {
                    0
                };
                let numbers: Vec<u64> = self
                    .prs
                    .values()
                    .filter(|p| {
                        p.author == BOT
                            && (Some(p.number) == *draft
                                || branch.as_ref().is_some_and(|b| *b == p.head_ref))
                    })
                    .map(|p| p.number)
                    .collect();
                if delay == 0 {
                    for n in numbers {
                        self.cancel_runs(n);
                    }
                    vec![Event::CarClosed { car: *car }]
                } else {
                    let at = self.now() + delay;
                    for n in numbers {
                        self.push_pending(at, Pending::Cancel(n));
                    }
                    self.schedule_reply(at, Event::CarClosed { car: *car });
                    vec![]
                }
            }
            Decision::Refresh {
                token,
                numbers,
                branches,
            } => vec![self.snapshot(numbers, branches, Some(*token))],
            Decision::Land {
                car,
                queue,
                base_branch,
                expected_base_sha,
                prs,
                messages,
                method,
                head,
                ..
            } => vec![self.land(
                *car,
                queue,
                base_branch,
                expected_base_sha,
                prs,
                messages,
                *method,
                head.as_deref(),
            )],
            Decision::WakeAt { at } => {
                let at = (*at).max(self.now());
                self.push_pending(at, Pending::Tick);
                vec![]
            }
            Decision::Enqueue { .. }
            | Decision::Dequeue { .. }
            | Decision::ChecksEnded { .. }
            | Decision::EvaluationError { .. }
            | Decision::Merged { .. }
            | Decision::Halt { .. } => vec![],
            Decision::Cosmetic { cosmetic } => {
                self.apply_cosmetic(cosmetic);
                vec![]
            }
            Decision::Act { id, op, .. } => {
                let result = self.rule_action(op);
                vec![Event::ActionDone { id: *id, result }]
            }
        }
    }

    /// The commits of a pull request that are not on `onto` yet, oldest first (its first-parent
    /// history down to the first commit `onto` contains).
    fn own_commits(&self, head: &str, onto: &str) -> Vec<Sha> {
        let mut v: Vec<Sha> = self
            .repo
            .first_parent_history(head)
            .into_iter()
            .take_while(|c| !self.repo.is_ancestor(c, onto))
            .collect();
        v.reverse();
        v
    }

    /// Lands a car by moving the base (`fast-forward`) or with one merge commit of the batch
    /// branch (`merge-batch`). Every pull request must be open at its tested head; the base
    /// must be exactly the expected commit (compare-and-swap). A pull request counts as merged
    /// only once its head is reachable from the new base (MQ-05).
    #[allow(clippy::too_many_arguments)]
    fn land_branch(
        &mut self,
        car: u64,
        base_branch: &str,
        expected: &str,
        prs: &[CarPr],
        method: MergeMethod,
        head: &str,
    ) -> (Vec<LandedCommit>, Option<LandError>) {
        for p in prs {
            match self.prs.get(&p.number) {
                Some(t) if !t.closed && t.head_sha == p.head_sha => {}
                _ => return (vec![], Some(LandError::HeadChanged { number: p.number })),
            }
        }
        let tip = self.read_branch(base_branch).unwrap_or_default();
        if tip != expected {
            return (vec![], Some(LandError::BaseMoved { actual: tip }));
        }
        let record = self.cars.get(&car).cloned();
        let new = match method {
            MergeMethod::FastForward => {
                if !self.repo.is_ancestor(expected, head) {
                    let number = prs.first().map_or(0, |p| p.number);
                    return (
                        vec![],
                        Some(LandError::Refused {
                            number,
                            message: "the tested branch does not contain the base: not a fast \
                                      forward"
                                .into(),
                        }),
                    );
                }
                head.to_string()
            }
            _ => {
                let title = format!(
                    "Merged {}",
                    prs.iter()
                        .map(|p| format!("#{}", p.number))
                        .collect::<Vec<_>>()
                        .join(", ")
                );
                let message = format!("{title}\n\nMerged by the merge queue");
                match self.repo.merge(expected, head, &message) {
                    Ok(c) => c,
                    Err(_) => {
                        let number = prs.first().map_or(0, |p| p.number);
                        return (vec![], Some(LandError::Conflict { number }));
                    }
                }
            }
        };
        if let Err(e) = self.repo.update_ref(base_branch, expected, &new) {
            let actual = match e {
                crate::git::RefError::Stale { actual, .. } => actual.unwrap_or_default(),
                _ => String::new(),
            };
            return (vec![], Some(LandError::BaseMoved { actual }));
        }
        let mut commits = Vec::new();
        let in_place = record.as_ref().is_some_and(|r| r.in_place);
        let edited = !in_place
            && method == MergeMethod::FastForward
            && record.as_ref().is_some_and(|r| r.head != new);
        for (k, p) in prs.iter().enumerate() {
            let (sha, extra) = if method == MergeMethod::MergeBatch {
                (new.clone(), vec![])
            } else if edited && k + 1 == prs.len() {
                // An accepted `allow_queue_branch_edit`: the base moves onto the pushed head.
                (new.clone(), vec![])
            } else if in_place {
                let mut own = self.own_commits(head, expected);
                let last = own.pop().unwrap_or_else(|| head.to_string());
                (last, own)
            } else {
                let sha = record
                    .as_ref()
                    .and_then(|r| r.prefix.iter().find(|(n, ..)| *n == p.number))
                    .map(|(_, s, _)| s.clone())
                    .unwrap_or_else(|| new.clone());
                (sha, vec![])
            };
            if self.repo.is_ancestor(&p.head_sha, &new) {
                let pr = self.prs.get_mut(&p.number).expect("pr");
                pr.merged = true;
                pr.closed = true;
                pr.merge_commit_sha = Some(sha.clone());
            }
            commits.push(LandedCommit {
                number: p.number,
                tree: self.repo.tree_id_of(&sha),
                sha,
                extra,
            });
        }
        if method == MergeMethod::MergeBatch
            && let Some(d) = record.as_ref().and_then(|r| r.draft).filter(|_| !in_place)
            && let Some(pr) = self.prs.get_mut(&d)
        {
            pr.merged = true;
            pr.closed = true;
            pr.merge_commit_sha = Some(new.clone());
        }
        (commits, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn land(
        &mut self,
        car: u64,
        queue: &str,
        base_branch: &str,
        expected_base: &str,
        prs: &[CarPr],
        messages: &[CommitMessage],
        method: MergeMethod,
        head: Option<&str>,
    ) -> Event {
        let before: Vec<PullRequest> = prs
            .iter()
            .filter_map(|p| self.read_pull_request(p.number))
            .collect();
        let draft_before = self
            .cars
            .get(&car)
            .and_then(|c| c.draft)
            .and_then(|d| self.read_pull_request(d));
        let mut expected = expected_base.to_string();
        let mut commits = Vec::new();
        let mut error = None;
        if method.lands_branch() {
            let head = head
                .map(str::to_string)
                .or_else(|| self.cars.get(&car).map(|c| c.head.clone()))
                .unwrap_or_default();
            let (c, e) = self.land_branch(car, base_branch, expected_base, prs, method, &head);
            commits = c;
            error = e;
        }
        for p in prs.iter().filter(|_| !method.lands_branch()) {
            let Some(truth) = self.prs.get(&p.number).cloned() else {
                error = Some(LandError::HeadChanged { number: p.number });
                break;
            };
            if truth.closed || truth.head_sha != p.head_sha {
                error = Some(LandError::HeadChanged { number: p.number });
                break;
            }
            let tip = self.read_branch(base_branch).unwrap_or_default();
            if tip != expected {
                error = Some(LandError::BaseMoved { actual: tip });
                break;
            }
            let (title, inherited) = self.inherited_message(p.number);
            let message = match messages.iter().find(|m| m.number == p.number) {
                Some(m) => {
                    let title = m.title.clone().unwrap_or(title);
                    let body = m.full_body(&inherited);
                    if body.is_empty() {
                        title
                    } else {
                        format!("{title}\n\n{body}")
                    }
                }
                None if inherited.is_empty() => title,
                None => format!("{title}\n\n{inherited}"),
            };
            let mut extra = Vec::new();
            let landed = match method {
                MergeMethod::Merge => {
                    let head_ref = truth.head_ref.clone();
                    let m = format!(
                        "Merge pull request #{} from {head_ref}\n\n{}",
                        p.number, truth.title
                    );
                    self.repo.merge(&expected, &p.head_sha, &m)
                }
                MergeMethod::Rebase => {
                    let own = self.own_commits(&p.head_sha, &expected);
                    let mut cur = Ok(expected.clone());
                    for c in &own {
                        let Ok(at) = cur.clone() else { break };
                        let msg = self.repo.message(c);
                        cur = self.repo.cherry_pick(&at, c, &msg);
                        if let Ok(n) = &cur {
                            extra.push(n.clone());
                        }
                    }
                    if own.is_empty() {
                        cur = Err(crate::git::Conflict(vec![]));
                    }
                    extra.pop();
                    cur
                }
                _ => self.repo.squash(&expected, &p.head_sha, &message),
            };
            let Ok(c) = landed else {
                error = Some(LandError::Conflict { number: p.number });
                break;
            };
            if let Err(e) = self.repo.update_ref(base_branch, &expected, &c) {
                error = Some(LandError::BaseMoved {
                    actual: match e {
                        crate::git::RefError::Stale { actual, .. } => actual.unwrap_or_default(),
                        _ => String::new(),
                    },
                });
                break;
            }
            let pr = self.prs.get_mut(&p.number).expect("pr");
            pr.merged = true;
            pr.closed = true;
            pr.merge_commit_sha = Some(c.clone());
            let now = self.now();
            let logged = match method {
                MergeMethod::Squash => message.clone(),
                _ => self.repo.message(&c),
            };
            self.landed_messages.push((now, p.number, logged));
            commits.push(LandedCommit {
                number: p.number,
                sha: c.clone(),
                tree: self.repo.tree_id_of(&c),
                extra,
            });
            expected = c;
        }
        if !commits.is_empty() {
            self.emit(
                Hint::Branch(base_branch.to_string()),
                format!("push to {base_branch}"),
            );
        }
        for c in &commits {
            self.emit(
                Hint::PullRequest(c.number),
                format!("#{} closed (merged)", c.number),
            );
        }
        self.landings.push(LandingRecord {
            at: self.now(),
            car,
            method,
            queue: queue.to_string(),
            base_branch: base_branch.to_string(),
            expected_base: expected_base.to_string(),
            requested: prs.to_vec(),
            commits: commits.clone(),
            error: error.clone(),
            before,
            draft_before,
        });
        Event::Landed {
            car,
            commits,
            error,
        }
    }
}

pub fn conclusion_name(c: Conclusion) -> String {
    serde_json::to_value(c)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_only_moves_forward() {
        let mut c = VirtualClock::default();
        c.advance_to(10);
        c.advance_to(5);
        assert_eq!(c.now(), 10);
    }

    fn def(n: u64, files: &[(&str, &str)], scripts: Vec<CiScript>) -> PrDef {
        PrDef {
            number: n,
            author: "a".into(),
            title: format!("pr {n}"),
            base: "main".into(),
            draft: false,
            labels: Default::default(),
            files: files
                .iter()
                .map(|(p, c)| (p.to_string(), c.to_string()))
                .collect(),
            dependabot_update_types: vec![],
            scripts,
            body: String::new(),
            fork: false,
            approvals: vec![],
        }
    }

    #[test]
    fn ci_runs_report_in_time_order() {
        let mut f = SimForge::new("main", Tree::new(), vec![], 0);
        let ci = vec![
            CiScript::new("slow", 300, Some(Conclusion::Success)),
            CiScript::new("fast", 60, Some(Conclusion::Failure)),
            CiScript::new("never", 1, None),
        ];
        f.define(def(1, &[("a", "1")], ci));
        f.schedule(10, Action::Open(1));
        let mut ci_times = Vec::new();
        while let Some((t, item)) = f.next_item() {
            if matches!(item, Item::Ci(_)) {
                ci_times.push(t);
            }
        }
        // `never` never completes.
        assert_eq!(ci_times, vec![70, 310]);
        assert_eq!(f.now(), 310);
    }

    #[test]
    fn car_land_and_conflict_classification() {
        let mut f = SimForge::new("main", [("r".into(), "0".into())].into(), vec![], 0);
        f.define(def(1, &[("a", "1")], vec![]));
        f.define(def(2, &[("a", "2")], vec![]));
        f.define(def(3, &[("r", "3")], vec![]));
        for n in 1..=3 {
            f.schedule(0, Action::Open(n));
        }
        while f.next_item().is_some() {}
        let base = f.read_branch("main").unwrap();
        let pr = |f: &SimForge, n| CarPr {
            number: n,
            head_sha: f.read_pull_request(n).unwrap().head_sha,
        };
        let prs = vec![pr(&f, 1), pr(&f, 2)];
        let r = f.execute(&Decision::BuildCar {
            car: 1,
            queue: "q".into(),
            base_branch: "main".into(),
            branch: "mq/x".into(),
            base_sha: base.clone(),
            prs,
            own: vec![1, 2],
            stack_on: None,
        });
        assert!(matches!(
            &r[..],
            [Event::CarConflict { number: 2, with_base: false, with, .. }] if with == &vec![1]
        ));
        // A direct push changing `r` makes #3 conflict with the base.
        f.schedule(
            1,
            Action::PushBase {
                branch: "main".into(),
                files: vec![("r".into(), "x".into())],
            },
        );
        while f.next_item().is_some() {}
        let base2 = f.read_branch("main").unwrap();
        let r = f.execute(&Decision::BuildCar {
            car: 2,
            queue: "q".into(),
            base_branch: "main".into(),
            branch: "mq/y".into(),
            base_sha: base2.clone(),
            prs: vec![pr(&f, 3)],
            own: vec![3],
            stack_on: None,
        });
        assert!(matches!(
            &r[..],
            [Event::CarConflict {
                with_base: true,
                ..
            }]
        ));
        // Landing with a stale base is refused (compare-and-swap).
        let r = f.execute(&Decision::Land {
            car: 9,
            queue: "q".into(),
            base_branch: "main".into(),
            expected_base_sha: base,
            prs: vec![pr(&f, 1)],
            expected_tree: String::new(),
            messages: vec![],
            method: Default::default(),
            head: None,
            draft: None,
        });
        assert!(matches!(
            &r[..],
            [Event::Landed {
                error: Some(LandError::BaseMoved { .. }),
                ..
            }]
        ));
        let r = f.execute(&Decision::Land {
            car: 9,
            queue: "q".into(),
            base_branch: "main".into(),
            expected_base_sha: base2,
            prs: vec![pr(&f, 1)],
            expected_tree: String::new(),
            messages: vec![],
            method: Default::default(),
            head: None,
            draft: None,
        });
        assert!(
            matches!(&r[..], [Event::Landed { error: None, commits, .. }] if commits.len() == 1)
        );
        assert!(f.read_pull_request(1).unwrap().merged);
    }
}
