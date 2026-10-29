//! `schlange serve`: the webhook endpoint, one engine worker per repository, reconciliation,
//! persistence.
//!
//! - **Listener** ([`crate::http`]): a small thread pool with read timeouts and deadlines, so a
//!   slow client never stalls webhooks or `/healthz`. `POST /webhook` verifies the signature
//!   (the secret must be at least [`MIN_SECRET_LEN`] bytes), drops delivery ids it already
//!   handled, parses the payload and hands the hints to the repository's worker; it answers
//!   202 at once. A delivery id is recorded only once the delivery was accepted: a delivery
//!   answered 400 or 503 is redelivered by GitHub and handled then. `GET /healthz` answers
//!   `ok` (503 when a worker stopped or its loop heartbeat is stale). A long reconciliation
//!   can also trigger the heartbeat alarm; it is an operational signal, not a restart.
//!   `GET /status` needs `Authorization: Bearer <admin
//!   token>` and is off without one: it shows pull request data.
//! - **Workers.** Each is the single writer for its repository (KONZEPT.md, section 6): it owns
//!   the engine state and the [`GithubForge`], turns hints into reads, feeds them to the
//!   engine, carries out the decisions, fires the engine's timers, retries deferred
//!   decisions, and runs a full reconciliation at start and every `reconcile_every` seconds
//!   (progress without webhooks, MQ-30), followed by the orphan sweep.
//! - **Journal** ([`crate::store`]): with a data directory, every engine step is checkpointed
//!   before its side effects and every side effect is journaled as an intent before it is
//!   sent. After a crash the worker restarts from the checkpoint, feeds `Event::Restarted`
//!   (every outstanding request is in doubt) and a full read of GitHub (MQ-31). A journal that
//!   cannot be written stops the worker before any further side effect.
//! - **Configuration** ([`crate::config`]): read from the default branch only and validated by
//!   the same pipeline as `config validate`; an invalid one is not applied, the last valid one
//!   stays in force and the error shows in the log, in `/status` and in the doctor.
//! - **Shadow mode** ([`crate::shadow`], S-SHADOW): with `shadow` the workers decide as usual
//!   but hand no write to GitHub; a write gate on the client refuses any that slips through.
//!   What Schlange would have done and what Mergify did go into the shadow report.
//! - **Lease** ([`crate::lease`], MQ-32): with `lease` a worker acts only while it holds its
//!   repository's lease (local SQLite row plus a fence reference on GitHub, checked before every
//!   write). A follower reads nothing and writes nothing; it takes over when the lease expires
//!   and steps down when a write finds the fence moved.
//! - **Event log** ([`crate::events`]): every decision and operator control, queryable through
//!   `GET /repos/{owner}/{repo}/logs`.

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use schlange_config::rules::Rules;
use schlange_core::engine::{Control, Freeze};
use schlange_core::journal::{Journal, is_side_effect};
use schlange_core::model::Time;
use schlange_core::{Decision, Event, State, step};

use crate::api::{RepoApi, RepoName};
use crate::client::{Client, Priority};
use crate::config::{ConfigCheck, Found, read_from_default_branch};
use crate::events::EventLog;
use crate::forge::{ForgeOptions, GithubForge};
use crate::http::{Handler, Limits, Request, Response, Server};
use crate::lease::{Lease, LeaseConfig, LeaseGate, LeaseStatus, ShadowGate};
use crate::shadow::{Shadow, ShadowConfig};
use crate::store::{
    EventQuery, InboxClaim, InboxDelivery, LEASE_LOST, ShadowFact, SqliteJournal, Store,
};
use crate::time::Clock;
use crate::webhook::{CommentHint, Dedup, Hint, Parsed, parse, verify_signature};

/// Shorter webhook secrets are refused (GitHub recommends a long random string).
pub const MIN_SECRET_LEN: usize = 16;
/// `/status` shows at most this many outcomes.
pub const STATUS_OUTCOMES: usize = 200;
const DECISION_LOG_MAX: usize = 10_000;
const DELIVERIES_KEPT: u64 = 50_000;

/// Where the configuration comes from.
#[derive(Clone, Debug)]
pub enum ConfigSource {
    /// The configuration file on the default branch, searched in the documented order
    /// (`schlange_config::config_file::NAMES`), read again at every reconciliation. Two files
    /// are an error; the last good configuration stays in use.
    Repository,
    /// A fixed configuration text (from `--config`), validated the same way.
    Text(String),
}

/// Tests: the worker stops before the first `Land` it would send, until released (a process
/// paused by the scheduler or a debugger, MQ-32).
#[derive(Default)]
pub struct PauseHook {
    state: Mutex<(bool, bool, bool)>,
    cv: std::sync::Condvar,
}

impl PauseHook {
    pub fn new() -> Arc<Self> {
        Arc::new(PauseHook::default())
    }

    /// Whether a worker is paused at the hook now.
    pub fn reached(&self) -> bool {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).0
    }

    /// Lets the paused worker go on.
    pub fn release(&self) {
        self.state.lock().unwrap_or_else(|p| p.into_inner()).1 = true;
        self.cv.notify_all();
    }

    fn pause_once(&self) {
        let mut g = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if g.2 {
            return;
        }
        g.2 = true;
        g.0 = true;
        while !g.1 {
            g = self.cv.wait(g).unwrap_or_else(|p| p.into_inner());
        }
        g.0 = false;
    }
}

/// A crash injected for tests: the worker stops dead at this point (no reply, no journal
/// entry after it), as a killed process would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// Right after the merge calls of a landing returned, before the reply reaches the engine.
    AfterLand,
    /// Tests: stop before a nonempty identity publication checkpoint.
    BeforeIdentityPublication,
    /// Tests: stop after a nonempty repository identity publication checkpoint.
    AfterIdentityPublication,
    /// Tests: stop after session rotation, before fence confirmation.
    AfterStatusSessionRotation,
    BeforeHeldStatusAck,
    AfterHeldStatusAck,
    /// Synthetic worker stops after build verification and before the atomic reply transaction.
    BeforeBuildCommit,
    /// Synthetic worker stops immediately after the atomic build reply transaction.
    AfterBuildCommit,
}

#[derive(Clone)]
pub struct ServeConfig {
    pub client: Client,
    pub repos: Vec<RepoName>,
    /// Installation id; `None` looks it up per repository.
    pub installation: Option<u64>,
    pub webhook_secret: Vec<u8>,
    pub listen: SocketAddr,
    /// Seconds between full reconciliations.
    pub reconcile_every: Time,
    pub config: ConfigSource,
    /// The validator for the configuration (schema, feature map).
    pub check: Arc<ConfigCheck>,
    /// How often the worker wakes when nothing arrives.
    pub poll: Duration,
    /// Name of the queue's own check run.
    pub check_name: String,
    /// First and longest delay for retrying deferred decisions.
    pub retry_base: Duration,
    pub retry_max: Duration,
    /// Data directory for the SQLite database; `None` keeps everything in memory (tests).
    pub data_dir: Option<PathBuf>,
    /// Bearer token for `GET /status`; `None` turns `/status` off.
    pub admin_token: Option<String>,
    pub http: Limits,
    /// Tests only.
    pub crash: Option<CrashPoint>,
    /// Shadow mode: decide, never write (S-SHADOW).
    pub shadow: Option<ShadowConfig>,
    /// One writer per repository across processes (MQ-32); `None`: no lease (a single
    /// process, tests).
    pub lease: Option<LeaseConfig>,
    /// Tests only: pause before the first landing.
    pub pause_before_land: Option<Arc<PauseHook>>,
    /// Tests only: pause a nonempty identity batch before final attestation.
    pub identity_test_before_attest: Option<Arc<PauseHook>>,
    pub identity_test_before_release: Option<Arc<PauseHook>>,
    pub identity_test_after_session_rotate: Option<Arc<PauseHook>>,
    pub identity_test_before_status_ack: Option<Arc<PauseHook>>,
    pub identity_test_release_count: Option<Arc<AtomicU64>>,
    /// Tests only: adds virtual seconds to the monotonic heartbeat clock.
    pub heartbeat_test_offset: Option<Arc<AtomicU64>>,
    /// Tests only: counts actionable watchdog observations.
    pub watchdog_test_observations: Option<Arc<AtomicU64>>,
    /// Tests only: counts watchdog samples, including failed reads.
    pub watchdog_test_samples: Option<Arc<AtomicU64>>,
    /// Tests only: counts watchdog polling cycles, including absent views.
    pub watchdog_test_cycles: Option<Arc<AtomicU64>>,
    /// Tests only: highest count of completed PR reads in a deferred sample.
    pub watchdog_test_partial_prs: Option<Arc<AtomicU64>>,
    /// Tests only: completed, current observations including non-actionable ones.
    pub watchdog_test_completed: Option<Arc<AtomicU64>>,
    /// Tests only: pause an actionable observation before the generation check.
    pub watchdog_test_pause: Option<Arc<PauseHook>>,
    /// Tests only: pause before dispatching a watchdog reconciliation.
    pub watchdog_test_reconcile_pause: Option<Arc<PauseHook>>,
}

pub enum Msg {
    Webhook(Box<Parsed>, u64),
    /// A full reconciliation now.
    Reconcile,
    /// A signed malformed delivery also invalidates the ingress fence.
    ReconcileEpoch(u64),
    #[cfg(feature = "test-hooks")]
    TestSessionRefresh {
        session: String,
        revision: u64,
        seq: u64,
        request: Decision,
        pause: Arc<PauseHook>,
    },
    #[cfg(feature = "test-hooks")]
    TestSessionBuild {
        session: String,
        revision: u64,
        seq: u64,
        request: Decision,
        pause: Arc<PauseHook>,
        after_create: bool,
        confirm_all: bool,
    },
    Shutdown,
    /// An operator's queue control from the admin API (pause, freeze); the worker answers
    /// once the engine applied it (and the state was journaled).
    Control(Control, Sender<Result<serde_json::Value, String>>, u64),
    /// A read of the engine state (explain, simulate), answered by the worker.
    Query(QueryFn, Sender<Result<serde_json::Value, String>>),
}

/// A question to a worker about its engine state, under its rules and validator.
pub type QueryFn = Box<
    dyn FnOnce(&State, Option<&Rules>, &ConfigCheck) -> Result<serde_json::Value, String> + Send,
>;

/// What `/status` shows of a worker.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct WorkerStatus {
    pub repo: String,
    pub queued: Vec<u64>,
    /// The newest merged pull requests (at most [`STATUS_OUTCOMES`]).
    pub merged: Vec<u64>,
    pub halted: BTreeMap<String, String>,
    pub cars: usize,
    pub last_reconcile: Option<Time>,
    pub reconciliations: u64,
    pub webhooks: u64,
    pub pending: Vec<String>,
    pub cosmetics_waiting: usize,
    /// The configuration was rejected (the last valid one, if any, stays in force).
    pub config_error: Option<String>,
    /// Stage 5: features of the configuration Schlange does not support yet.
    pub config_warnings: Vec<String>,
    /// Restarted from the journal (with the side effects that were in doubt).
    pub recovered: Option<Vec<String>>,
    pub persistent: bool,
    pub log_tail: Vec<String>,
    /// The engine state (outcomes cut to the newest [`STATUS_OUTCOMES`]).
    pub state: serde_json::Value,
    /// The queue in the shape of the merge queue status API (`schlange_core::status`).
    pub queue: serde_json::Value,
    /// `single` (no lease), `leader`, `follower` (another process holds the lease), `shadow`.
    pub role: String,
    pub lease: Option<LeaseStatus>,
    /// Shadow mode: decisions answered without writing.
    pub shadow_suppressed: u64,
    /// Writes the client's gate refused (never sent).
    pub writes_refused: u64,
}

struct Shared {
    statuses: Mutex<BTreeMap<String, WorkerStatus>>,
    decisions: Mutex<BTreeMap<String, VecDeque<String>>>,
    /// Workers that stopped, with the reason (health check).
    dead: Mutex<BTreeMap<String, String>>,
    heartbeat: Mutex<BTreeMap<String, Instant>>,
    heartbeat_limit: Duration,
    heartbeat_test_offset: Option<Arc<AtomicU64>>,
    watchdog_views: Mutex<BTreeMap<String, WatchdogView>>,
    watchdog_generation: AtomicU64,
    ingress_epochs: Mutex<BTreeMap<String, u64>>,
    watchdog_memo_total: Arc<AtomicUsize>,
    watchdog_errors: Mutex<BTreeMap<String, String>>,
    /// Messages handed to workers and not yet handled (tests wait for 0).
    inflight: AtomicUsize,
    stop: AtomicBool,
    events: Arc<EventLog>,
    /// Shadow mode: the facts of each repository, as the worker last published them.
    shadow: Mutex<BTreeMap<String, Vec<ShadowFact>>>,
    store: Option<Store>,
}

#[derive(Clone)]
struct WatchdogView {
    generation: u64,
    fingerprint: String,
    state: State,
    rules: Rules,
}

fn watchdog_enabled(leading: bool, dead: bool, shadow: bool) -> bool {
    leading && !dead && !shadow
}

fn watchdog_fingerprint(state: &State, rules: &Rules) -> Option<String> {
    let mut stable_state = serde_json::to_value(state).ok()?;
    let map = stable_state.as_object_mut()?;
    // Cosmetics are never inputs to queue decisions (rule 9), and time is supplied afresh
    // when evaluating the snapshot. Their changes cannot invalidate an observation.
    map.remove("now");
    map.remove("cosmetics");
    Some(format!("{rules:?}:{stable_state}"))
}

fn update_watchdog_view(
    views: &mut BTreeMap<String, WatchdogView>,
    generation: &AtomicU64,
    repo: &str,
    state: &State,
    rules: &Rules,
    fingerprint: Option<String>,
) {
    let Some(fingerprint) = fingerprint else {
        views.remove(repo);
        return;
    };
    if views
        .get(repo)
        .is_none_or(|old| old.fingerprint != fingerprint)
    {
        let generation = generation.fetch_add(1, Ordering::SeqCst) + 1;
        views.insert(
            repo.to_string(),
            WatchdogView {
                generation,
                fingerprint,
                state: state.clone(),
                rules: rules.clone(),
            },
        );
    }
}

fn supervise_watchdog(shared: &Shared, repo: &str, run: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(run)).is_err() {
        shared
            .watchdog_errors
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(repo.to_string(), "observer panicked".into());
    }
}

fn stale_workers(beats: &BTreeMap<String, Instant>, now: Instant, limit: Duration) -> Vec<String> {
    beats
        .iter()
        .filter(|(_, at)| now.saturating_duration_since(**at) > limit)
        .map(|(repo, _)| repo.clone())
        .collect()
}

/// Whether a previously validated view still has no accepted-but-unprocessed input.
fn ingress_valid(shared: &Shared, repo: &str, validated: u64, processed: u64) -> bool {
    !shared.stop.load(Ordering::SeqCst)
        && validated == processed
        && shared
            .ingress_epochs
            .lock()
            .ok()
            .is_some_and(|e| e.get(repo).copied().unwrap_or(0) == validated)
}

/// One-use write sequence; matching bytes avoids ambiguous JSON and cross-target redirects.
#[allow(dead_code)]
struct BuildAttempts {
    writes: Vec<(String, Vec<u8>)>,
    repo: String,
    api_base: String,
    installation: u64,
    bot_login: String,
    request: Decision,
    state: Mutex<BuildAttemptState>,
}

#[derive(Default)]
struct BuildAttemptState {
    next: usize,
    in_flight: bool,
    observed: bool,
    response: Option<Vec<u8>>,
    blocked: bool,
    merged: Option<(String, String)>,
}

#[allow(dead_code)]
impl BuildAttempts {
    fn new(
        repo: &str,
        decision: &Decision,
        api_base: &str,
        installation: u64,
        bot_login: &str,
    ) -> Result<Self, String> {
        let Decision::BuildCar {
            base_branch,
            branch,
            base_sha,
            prs,
            own,
            stack_on,
            ..
        } = decision
        else {
            return Err("not a build".into());
        };
        if prs.len() != 1 || stack_on.is_some() || own.as_slice() != [prs[0].number] {
            return Err("unsupported build".into());
        }
        let repo = crate::RepoName::parse(repo).ok_or("invalid repository")?;
        let root = format!(
            "/repos/{}/{}",
            crate::client::enc_path(&repo.owner),
            crate::client::enc_path(&repo.name)
        );
        let title = format!("merge queue: embarking #{}", own[0]);
        let body = format!(
            "Schlange tests these pull requests together with `{base_branch}` at {}:\n\n- #{} at {}\n\n## Datenschutz\n\nSee the constituent pull requests for their data protection details.\n\nDo not edit this pull request; the queue closes it.",
            schlange_core::engine::short(base_sha),
            prs[0].number,
            schlange_core::engine::short(&prs[0].head_sha)
        );
        let writes = vec![
            (format!("{root}/git/refs"), serde_json::json!({"ref":format!("refs/heads/{branch}"),"sha":base_sha}).to_string().into_bytes()),
            (format!("{root}/merges"), serde_json::json!({"base":branch,"head":prs[0].head_sha,"commit_message":format!("Merge #{} into {branch}",prs[0].number)}).to_string().into_bytes()),
            (format!("{root}/pulls"), serde_json::json!({"title":title,"head":branch,"base":base_branch,"body":body,"draft":true}).to_string().into_bytes()),
        ];
        Ok(Self {
            writes,
            repo: repo.full(),
            api_base: api_base.trim_end_matches('/').into(),
            installation,
            bot_login: bot_login.into(),
            request: decision.clone(),
            state: Mutex::new(BuildAttemptState::default()),
        })
    }

    fn permit(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "build attempt state unavailable")?;
        if state.in_flight || state.blocked {
            return Err("build attempt unresolved".into());
        }
        let Some((expected_path, expected_body)) = self.writes.get(state.next) else {
            return Err("build attempts exhausted".into());
        };
        if method != "POST" || path != expected_path || body != Some(expected_body.as_slice()) {
            return Err("build attempt mismatch".into());
        }
        state.in_flight = true;
        Ok(())
    }

    fn observe_response(
        &self,
        method: &str,
        path: &str,
        status: u16,
        body: &[u8],
    ) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "build attempt state unavailable")?;
        if !state.in_flight
            || state.blocked
            || state.observed
            || method != "POST"
            || self.writes.get(state.next).is_none_or(|(p, _)| p != path)
        {
            return Err("build attempt unresolved".into());
        }
        if status != 201 || body.len() > 1024 * 1024 {
            state.blocked = true;
            return Err("build outcome requires recovery".into());
        }
        state.observed = true;
        state.response = Some(body.to_vec());
        Ok(())
    }

    fn confirm(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "build attempt state unavailable")?;
        if !state.in_flight || !state.observed || state.blocked {
            return Err("build attempt unresolved".into());
        }
        state.in_flight = false;
        state.observed = false;
        state.response = None;
        state.next += 1;
        Ok(())
    }

    fn confirm_checked(
        &self,
        stage: usize,
        check: impl FnOnce() -> Result<Option<(String, String)>, String>,
    ) -> Result<(), String> {
        {
            let state = self
                .state
                .lock()
                .map_err(|_| "build attempt state unavailable")?;
            if state.next != stage || !state.in_flight || !state.observed || state.blocked {
                return Err("build attempt unresolved".into());
            }
        }
        let checked = check();
        let mut state = self
            .state
            .lock()
            .map_err(|_| "build attempt state unavailable")?;
        if state.next != stage || !state.in_flight || !state.observed || state.blocked {
            return Err("build attempt unresolved".into());
        }
        let merged = match checked {
            Ok(value) => value,
            Err(_) => {
                state.blocked = true;
                return Err("build evidence requires recovery".into());
            }
        };
        if let Some(value) = merged {
            state.merged = Some(value);
        }
        state.in_flight = false;
        state.observed = false;
        state.response = None;
        state.next += 1;
        Ok(())
    }

    fn confirm_create(&self, api: &crate::api::RepoApi) -> Result<(), String> {
        self.confirm_checked(0, || {
            if api.client.base() != self.api_base
                || api.installation != self.installation
                || api.repo.full() != self.repo
            {
                return Err("build context changed".into());
            }
            let Decision::BuildCar {
                branch,
                base_branch,
                base_sha,
                prs,
                ..
            } = &self.request
            else {
                return Err("invalid build".into());
            };
            let raw = self
                .state
                .lock()
                .map_err(|_| "build state")?
                .response
                .clone()
                .ok_or("missing response")?;
            let created: crate::api::GitRef =
                serde_json::from_slice(&raw).map_err(|_| "invalid create response")?;
            let source = api
                .pull(prs[0].number, crate::client::Priority::Normal)
                .map_err(|_| "source read")?;
            if created.ref_name != format!("refs/heads/{branch}")
                || created.object.sha != *base_sha
                || api.repo.full() != self.repo
                || api
                    .branch_sha(branch, crate::client::Priority::Normal)
                    .map_err(|_| "branch read")?
                    != *base_sha
                || api
                    .branch_sha(base_branch, crate::client::Priority::Normal)
                    .map_err(|_| "base read")?
                    != *base_sha
                || source.number != prs[0].number
                || source
                    .base
                    .repo
                    .as_ref()
                    .is_none_or(|r| r.full_name != self.repo)
                || source.state != "open"
                || source.merged
                || source.head.sha != prs[0].head_sha
                || source.base.ref_name != *base_branch
                || source.base.sha != *base_sha
            {
                return Err("create evidence changed".into());
            }
            Ok(None)
        })
    }

    fn confirm_merge(
        &self,
        api: &crate::api::RepoApi,
        outcome: &crate::api::MergeOutcome,
    ) -> Result<(), String> {
        self.confirm_checked(1, || {
            if api.client.base() != self.api_base
                || api.installation != self.installation
                || api.repo.full() != self.repo
            {
                return Err("build context changed".into());
            }
            let Decision::BuildCar {
                branch,
                base_branch,
                base_sha,
                prs,
                ..
            } = &self.request
            else {
                return Err("invalid build".into());
            };
            let crate::api::MergeOutcome::Merged { sha, tree } = outcome else {
                return Err("missing merge receipt".into());
            };
            let raw = self
                .state
                .lock()
                .map_err(|_| "build state")?
                .response
                .clone()
                .ok_or("missing response")?;
            let created: crate::api::RepoCommit =
                serde_json::from_slice(&raw).map_err(|_| "invalid merge response")?;
            if created.sha != *sha || created.commit.tree.sha != *tree {
                return Err("merge response changed".into());
            }
            let hex = |s: &str| s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit());
            if !hex(sha) || !hex(tree) {
                return Err("invalid merge id".into());
            }
            let commit = api
                .git_commit(sha, crate::client::Priority::Normal)
                .map_err(|_| "commit read")?;
            let source = api
                .pull(prs[0].number, crate::client::Priority::Normal)
                .map_err(|_| "source read")?;
            if api.repo.full() != self.repo
                || api
                    .branch_sha(branch, crate::client::Priority::Normal)
                    .map_err(|_| "branch read")?
                    != *sha
                || api
                    .branch_sha(base_branch, crate::client::Priority::Normal)
                    .map_err(|_| "base read")?
                    != *base_sha
                || source.number != prs[0].number
                || source
                    .base
                    .repo
                    .as_ref()
                    .is_none_or(|r| r.full_name != self.repo)
                || source.state != "open"
                || source.merged
                || source.head.sha != prs[0].head_sha
                || source.base.ref_name != *base_branch
                || source.base.sha != *base_sha
                || commit.sha != *sha
                || commit.tree.sha != *tree
                || commit.parents.len() != 2
                || commit.parents[0].sha != *base_sha
                || commit.parents[1].sha != prs[0].head_sha
            {
                return Err("merge evidence changed".into());
            }
            Ok(Some((sha.clone(), tree.clone())))
        })
    }

    fn confirm_draft(
        &self,
        api: &crate::api::RepoApi,
        response: &crate::api::Pull,
        bot: &str,
    ) -> Result<(), String> {
        self.confirm_checked(2, || {
            if bot != self.bot_login {
                return Err("build bot changed".into());
            }
            if api.client.base() != self.api_base
                || api.installation != self.installation
                || api.repo.full() != self.repo
            {
                return Err("build context changed".into());
            }
            let Decision::BuildCar {
                branch,
                base_branch,
                base_sha,
                prs,
                ..
            } = &self.request
            else {
                return Err("invalid build".into());
            };
            let merged = self
                .state
                .lock()
                .map_err(|_| "build state")?
                .merged
                .clone()
                .ok_or("missing merge")?;
            let raw = self
                .state
                .lock()
                .map_err(|_| "build state")?
                .response
                .clone()
                .ok_or("missing response")?;
            let created: crate::api::Pull =
                serde_json::from_slice(&raw).map_err(|_| "invalid draft response")?;
            let draft = api
                .pull(response.number, crate::client::Priority::Normal)
                .map_err(|_| "draft read")?;
            let source = api
                .pull(prs[0].number, crate::client::Priority::Normal)
                .map_err(|_| "source read")?;
            let valid_draft = |p: &crate::api::Pull| {
                p.number != 0
                    && p.state == "open"
                    && p.draft
                    && !p.merged
                    && p.user.login == bot
                    && p.head.ref_name == *branch
                    && p.head.sha == merged.0
                    && p.head
                        .repo
                        .as_ref()
                        .is_some_and(|r| r.full_name == self.repo)
                    && p.base.ref_name == *base_branch
                    && p.base.sha == *base_sha
                    && p.base
                        .repo
                        .as_ref()
                        .is_some_and(|r| r.full_name == self.repo)
            };
            if api.repo.full() != self.repo
                || !valid_draft(response)
                || !valid_draft(&created)
                || response.number != created.number
                || !valid_draft(&draft)
                || response.number != draft.number
                || api
                    .branch_sha(branch, crate::client::Priority::Normal)
                    .map_err(|_| "branch read")?
                    != merged.0
                || api
                    .branch_sha(base_branch, crate::client::Priority::Normal)
                    .map_err(|_| "base read")?
                    != *base_sha
                || source.number != prs[0].number
                || source
                    .base
                    .repo
                    .as_ref()
                    .is_none_or(|r| r.full_name != self.repo)
                || source.state != "open"
                || source.merged
                || source.head.sha != prs[0].head_sha
                || source.base.ref_name != *base_branch
                || source.base.sha != *base_sha
            {
                return Err("draft evidence changed".into());
            }
            Ok(None)
        })
    }
}

impl crate::client::WriteGate for BuildAttempts {
    fn permit(&self, _: &str, _: &str) -> Result<(), String> {
        Err("build requires a request body".into())
    }
    fn permit_body(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(), String> {
        self.permit(method, path, body)
    }
    fn response(&self, method: &str, path: &str, status: u16, body: &[u8]) -> Result<(), String> {
        self.observe_response(method, path, status, body)
    }
}

/// Private capability for one claimed build. No network request is sent while a lock is held.
#[allow(dead_code)]
struct ServiceBuildGate {
    ticket: crate::store::BuildWriteTicket,
    journal: SqliteJournal,
    plan: BuildAttempts,
    shared: Arc<Shared>,
    lease: Arc<Lease>,
    clock: Arc<dyn Clock>,
    processed_ingress: u64,
    #[cfg(feature = "test-hooks")]
    test_after_create: Option<(Arc<PauseHook>, RepoApi)>,
    #[cfg(feature = "test-hooks")]
    test_pause_after_create: bool,
    #[cfg(feature = "test-hooks")]
    test_confirm_all: bool,
}

#[allow(dead_code)]
impl ServiceBuildGate {
    fn verified(
        self: &Arc<Self>,
        gate: &Arc<dyn crate::client::WriteGate>,
        result: crate::forge::SessionBuildResult,
        forge: &GithubForge,
    ) -> Result<VerifiedBuild, String> {
        let expected: Arc<dyn crate::client::WriteGate> = self.clone();
        if !receipt_gate_matches(&expected, gate, &result) {
            return Err("build receipt gate changed".into());
        }
        {
            let state = self
                .plan
                .state
                .lock()
                .map_err(|_| "build state unavailable")?;
            let Event::CarBuilt { head_sha, tree, .. } = result.event() else {
                return Err("missing build result".into());
            };
            if state.next != 3
                || state.in_flight
                || state.blocked
                || state.merged.as_ref() != Some(&(head_sha.clone(), tree.clone()))
            {
                return Err("build stages incomplete".into());
            }
        }
        forge
            .verify_session_build_once(&self.ticket.request, &result)
            .map_err(|_| "build verification failed")?;
        if !ingress_valid(
            &self.shared,
            &self.ticket.repo,
            self.ticket.ingress,
            self.processed_ingress,
        ) || !self.lease.journal_is_current()
        {
            return Err("build fence changed".into());
        }
        self.journal
            .build_ticket_current(&self.ticket, self.ticket.ingress, || self.clock.now())
            .map_err(|_| "build ticket unavailable")?;
        self.lease.check().map_err(|_| "build lease unavailable")?;
        if !ingress_valid(
            &self.shared,
            &self.ticket.repo,
            self.ticket.ingress,
            self.processed_ingress,
        ) {
            return Err("build ingress changed".into());
        }
        Ok(VerifiedBuild {
            gate: self.clone(),
            result,
        })
    }
}

/// Only the worker can construct this after the scoped build and evidence checks succeed.
pub(crate) struct VerifiedBuild {
    gate: Arc<ServiceBuildGate>,
    result: crate::forge::SessionBuildResult,
}
impl VerifiedBuild {
    pub(crate) fn ticket(&self) -> &crate::store::BuildWriteTicket {
        &self.gate.ticket
    }
    pub(crate) fn event(&self) -> &Event {
        self.result.event()
    }
    pub(crate) fn prefix(&self) -> &[(u64, String)] {
        self.result.prefix()
    }
}

fn receipt_gate_matches(
    expected: &Arc<dyn crate::client::WriteGate>,
    supplied: &Arc<dyn crate::client::WriteGate>,
    result: &crate::forge::SessionBuildResult,
) -> bool {
    Arc::ptr_eq(expected, supplied) && result.belongs_to_gate(supplied)
}

#[cfg(feature = "test-hooks")]
pub fn test_receipt_gate_matches(
    expected: &Arc<dyn crate::client::WriteGate>,
    supplied: &Arc<dyn crate::client::WriteGate>,
    result: &crate::forge::SessionBuildResult,
) -> bool {
    receipt_gate_matches(expected, supplied, result)
}

impl crate::client::WriteGate for ServiceBuildGate {
    fn permit(&self, _: &str, _: &str) -> Result<(), String> {
        Err("build requires a request body".into())
    }
    fn permit_body(&self, method: &str, path: &str, body: Option<&[u8]>) -> Result<(), String> {
        if !ingress_valid(
            &self.shared,
            &self.ticket.repo,
            self.ticket.ingress,
            self.processed_ingress,
        ) {
            return Err("build ingress changed".into());
        }
        if self
            .shared
            .dead
            .lock()
            .map_err(|_| "worker state unavailable")?
            .contains_key(&self.ticket.repo)
            || !self.lease.journal_is_current()
            || self.lease.status().fence.as_deref() != Some(self.ticket.fence.as_str())
        {
            return Err("build fence unavailable".into());
        }
        self.journal
            .build_ticket_current(&self.ticket, self.ticket.ingress, || self.clock.now())
            .map_err(|_| "build ticket unavailable")?;
        self.plan.permit(method, path, body)?;
        self.lease.check().map_err(|_| "build lease unavailable")?;
        if !ingress_valid(
            &self.shared,
            &self.ticket.repo,
            self.ticket.ingress,
            self.processed_ingress,
        ) {
            return Err("build ingress changed".into());
        }
        Ok(())
    }
    fn response(&self, method: &str, path: &str, status: u16, body: &[u8]) -> Result<(), String> {
        self.plan.observe_response(method, path, status, body)?;
        #[cfg(feature = "test-hooks")]
        if let Some((hook, api)) = &self.test_after_create
            && path.ends_with("/git/refs")
        {
            self.plan.confirm_create(api)?;
            if self.test_pause_after_create {
                hook.pause_once();
            }
        }
        #[cfg(feature = "test-hooks")]
        if self.test_confirm_all
            && let Some((_, api)) = &self.test_after_create
        {
            if path.ends_with("/merges") {
                let commit: crate::api::RepoCommit =
                    serde_json::from_slice(body).map_err(|_| "invalid merge response")?;
                let outcome = crate::api::MergeOutcome::Merged {
                    sha: commit.sha,
                    tree: commit.commit.tree.sha,
                };
                self.plan.confirm_merge(api, &outcome)?;
            } else if path.ends_with("/pulls") {
                let draft: crate::api::Pull =
                    serde_json::from_slice(body).map_err(|_| "invalid draft response")?;
                self.plan.confirm_draft(api, &draft, &self.plan.bot_login)?;
            }
        }
        Ok(())
    }
}

/// Test-only access to the stage plan. This is not a service execution capability.
#[cfg(feature = "test-hooks")]
pub struct TestBuildAttempts(Arc<BuildAttempts>);

#[cfg(feature = "test-hooks")]
impl TestBuildAttempts {
    pub fn test_replace_response(&self, body: Vec<u8>) {
        self.0.state.lock().unwrap().response = Some(body);
    }
    pub fn new(
        repo: &str,
        decision: &Decision,
        api_base: &str,
        installation: u64,
        bot_login: &str,
    ) -> Result<Self, String> {
        Ok(Self(Arc::new(BuildAttempts::new(
            repo,
            decision,
            api_base,
            installation,
            bot_login,
        )?)))
    }
    pub fn gate(&self) -> Arc<dyn crate::client::WriteGate> {
        self.0.clone()
    }
    pub fn confirm(&self) -> Result<(), String> {
        self.0.confirm()
    }
    pub fn confirm_create(&self, api: &crate::api::RepoApi) -> Result<(), String> {
        self.0.confirm_create(api)
    }
    pub fn confirm_merge(
        &self,
        api: &crate::api::RepoApi,
        outcome: &crate::api::MergeOutcome,
    ) -> Result<(), String> {
        self.0.confirm_merge(api, outcome)
    }
    pub fn confirm_draft(
        &self,
        api: &crate::api::RepoApi,
        response: &crate::api::Pull,
        bot: &str,
    ) -> Result<(), String> {
        self.0.confirm_draft(api, response, bot)
    }
}

impl Shared {
    fn watchdog_views(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, WatchdogView>> {
        self.watchdog_views
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }
    fn heartbeat_now(&self) -> Instant {
        Instant::now()
            + Duration::from_secs(
                self.heartbeat_test_offset
                    .as_ref()
                    .map_or(0, |o| o.load(Ordering::SeqCst)),
            )
    }
    fn beat(&self, repo: &str) {
        self.heartbeat
            .lock()
            .expect("heartbeat")
            .insert(repo.to_string(), self.heartbeat_now());
    }
}

/// Verifies, deduplicates and routes deliveries.
pub struct Router {
    secret: Vec<u8>,
    dedup: Mutex<Dedup>,
    store: Option<Store>,
    sinks: HashMap<String, Sender<Msg>>,
    shared: Arc<Shared>,
    #[cfg(test)]
    after_seen_test_barrier: Option<Arc<std::sync::Barrier>>,
}

impl Router {
    fn seen(&self, id: &str) -> bool {
        self.dedup.lock().expect("dedup").seen(id)
            || self
                .store
                .as_ref()
                .is_some_and(|s| s.delivery_seen(id).unwrap_or(false))
    }

    fn record(&self, id: &str) {
        self.dedup.lock().expect("dedup").record(id);
        if let Some(s) = &self.store {
            let _ = s.record_delivery(id, crate::time::unix_now(), DELIVERIES_KEPT);
        }
    }

    /// A signed malformed event may still represent a real change. Invalidate the named
    /// repository, or all served repositories if its identity cannot be recovered.
    fn invalidate_malformed(&self, body: &[u8]) {
        let repo = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| {
                v.get("repository")?
                    .get("full_name")?
                    .as_str()
                    .map(str::to_owned)
            });
        let Ok(mut epochs) = self.shared.ingress_epochs.lock() else {
            self.shared.stop.store(true, Ordering::SeqCst);
            return;
        };
        for (name, sink) in &self.sinks {
            if repo.as_ref().is_some_and(|r| r != name) {
                continue;
            }
            let epoch = epochs.entry(name.clone()).or_default();
            let Some(next) = epoch.checked_add(1) else {
                self.shared.stop.store(true, Ordering::SeqCst);
                return;
            };
            *epoch = next;
            self.shared.inflight.fetch_add(1, Ordering::SeqCst);
            if sink.send(Msg::ReconcileEpoch(next)).is_err() {
                self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    /// Wake held workers for durable pending input, without replaying its commands.
    fn wake_pending(&self) -> Result<(), String> {
        let Some(store) = &self.store else {
            return Ok(());
        };
        let mut epochs = self
            .shared
            .ingress_epochs
            .lock()
            .map_err(|_| "ingress fence unavailable".to_string())?;
        for (repo, sink) in &self.sinks {
            if !store.has_pending_inbox(repo).map_err(|e| e.to_string())? {
                continue;
            }
            let epoch = epochs.entry(repo.clone()).or_default();
            let next = epoch
                .checked_add(1)
                .ok_or_else(|| "ingress fence exhausted".to_string())?;
            *epoch = next;
            self.shared.inflight.fetch_add(1, Ordering::SeqCst);
            if sink.send(Msg::ReconcileEpoch(next)).is_err() {
                self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
                return Err("worker stopped while waking durable inbox".into());
            }
        }
        Ok(())
    }

    /// Handles one delivery; returns the HTTP status and a short text. The delivery id is
    /// recorded only when the delivery was handled (2xx).
    pub fn handle(&self, header: &dyn Fn(&str) -> Option<String>, body: &[u8]) -> (u16, String) {
        if !verify_signature(&self.secret, body, header("x-hub-signature-256").as_deref()) {
            return (401, "bad signature".into());
        }
        if self.shared.stop.load(Ordering::SeqCst) {
            return (503, "service stopping".into());
        }
        let Some(event) = header("x-github-event") else {
            return (400, "no X-GitHub-Event".into());
        };
        let Some(id) = header("x-github-delivery") else {
            return (400, "no X-GitHub-Delivery".into());
        };
        if self.store.is_none() && self.seen(&id) {
            return (200, "duplicate delivery".into());
        }
        #[cfg(test)]
        if let Some(barrier) = &self.after_seen_test_barrier {
            barrier.wait();
        }
        let mut parsed = match parse(&event, body) {
            Ok(p) => p,
            Err(e) => {
                self.invalidate_malformed(body);
                return (400, e);
            }
        };
        if parsed.hints.is_empty() {
            parsed.hints.push(Hint::Full);
        }
        let Some(sink) = parsed.repo.as_ref().and_then(|r| self.sinks.get(r)) else {
            return (404, "repository not served".into());
        };
        let Some(repo) = parsed.repo.as_ref() else {
            return (503, "repository missing".into());
        };
        let Ok(mut epochs) = self.shared.ingress_epochs.lock() else {
            self.shared.stop.store(true, Ordering::SeqCst);
            return (503, "ingress fence unavailable".into());
        };
        if self.shared.stop.load(Ordering::SeqCst) {
            return (503, "service stopping".into());
        }
        let epoch = epochs.entry(repo.clone()).or_default();
        let Some(next) = epoch.checked_add(1) else {
            self.shared.stop.store(true, Ordering::SeqCst);
            return (503, "ingress fence exhausted".into());
        };
        // Serialize the durable claim with the ingress fence. The "initial" namespace is
        // unbound intake, not a trusted adoption epoch or replay authorization.
        if let Some(store) = &self.store {
            let item = InboxDelivery {
                repo: repo.clone(),
                epoch: "initial".into(),
                guid: id.clone(),
                event: event.clone(),
                body: body.to_vec(),
                received_at: crate::time::unix_now(),
            };
            match store.accept_inbox(&item, 10_000) {
                Ok(InboxClaim::Processed) => return (200, "duplicate delivery".into()),
                Ok(InboxClaim::Duplicate) if self.dedup.lock().expect("dedup").seen(&id) => {
                    return (200, "duplicate delivery".into());
                }
                Ok(InboxClaim::Inserted | InboxClaim::Duplicate) => {}
                Err(_) => {
                    *epoch = next;
                    self.shared.stop.store(true, Ordering::SeqCst);
                    return (503, "durable inbox unavailable".into());
                }
            }
        }
        *epoch = next;
        self.shared.inflight.fetch_add(1, Ordering::SeqCst);
        let sent = if self.store.is_some() {
            // The durable payload is not yet safe to feed to the engine: wake reconciliation,
            // retain it unprocessed, and keep comment/editor provenance in SQLite.
            sink.send(Msg::ReconcileEpoch(next))
        } else {
            sink.send(Msg::Webhook(Box::new(parsed), next))
        };
        if sent.is_err() {
            self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
            return (503, "worker stopped".into());
        }
        if self.store.is_some() {
            self.dedup.lock().expect("dedup").record(&id);
        } else {
            self.record(&id);
        }
        drop(epochs);
        (202, "accepted".into())
    }
}

/// Constant-time comparison (the admin token).
fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub struct Service {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
    sinks: Vec<Sender<Msg>>,
    threads: Vec<JoinHandle<()>>,
    server: Option<Server>,
    pub store: Option<Store>,
    /// Shadow mode: the gate that refuses every write (tests read its attempts).
    pub shadow_gate: Option<Arc<ShadowGate>>,
    pub leases: Vec<Arc<Lease>>,
}

/// The token scope for one installation: these repositories, and of Schlange's permissions
/// the ones the installation grants (asking for more fails).
/// The permissions to ask for in installation tokens: those of the mode's list the
/// installation grants (a read-only shadow app gets read-only tokens that still read everything).
fn token_permissions(granted: &BTreeMap<String, String>, shadow: bool) -> BTreeMap<String, String> {
    let rank = |l: &str| match l {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    };
    crate::permissions::for_mode(shadow)
        .iter()
        .filter(|(name, level, _)| {
            granted
                .get(*name)
                .is_some_and(|have| rank(have) >= rank(level))
        })
        .map(|(n, l, _)| (n.to_string(), l.to_string()))
        .collect()
}

impl Service {
    #[cfg(feature = "test-hooks")]
    pub fn test_start_session_build(
        &self,
        session: &str,
        revision: u64,
        seq: u64,
        request: Decision,
        pause: Arc<PauseHook>,
        after_create: bool,
        confirm_all: bool,
    ) -> Result<(), String> {
        if self.sinks.len() != 1 {
            return Err("test requires exactly one repository".into());
        }
        self.shared.inflight.fetch_add(1, Ordering::SeqCst);
        self.sinks[0]
            .send(Msg::TestSessionBuild {
                session: session.into(),
                revision,
                seq,
                request,
                pause,
                after_create,
                confirm_all,
            })
            .map_err(|e| {
                self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
                e.to_string()
            })
    }
    /// Synthetic harness only: exercises the real worker read path without enabling release.
    #[cfg(feature = "test-hooks")]
    pub fn test_start_session_refresh(
        &self,
        session: &str,
        revision: u64,
        seq: u64,
        request: Decision,
        pause: Arc<PauseHook>,
    ) -> Result<(), String> {
        if self.sinks.len() != 1 {
            return Err("test requires exactly one repository".into());
        }
        self.shared.inflight.fetch_add(1, Ordering::SeqCst);
        self.sinks[0]
            .send(Msg::TestSessionRefresh {
                session: session.into(),
                revision,
                seq,
                request,
                pause,
            })
            .map_err(|e| {
                self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
                e.to_string()
            })
    }
    /// Starts the listener and one worker per repository.
    pub fn start(cfg: ServeConfig, clock: Arc<dyn Clock>) -> Result<Service, String> {
        if cfg.webhook_secret.len() < MIN_SECRET_LEN {
            return Err(format!(
                "the webhook secret is empty or shorter than {MIN_SECRET_LEN} bytes; use a long random string"
            ));
        }
        let app = cfg
            .client
            .app()
            .map_err(|e| format!("cannot read the app (GET /app): {e}"))?;
        let opts = ForgeOptions {
            bot_login: format!("{}[bot]", app.slug),
            app_slug: app.slug.clone(),
            check_name: cfg.check_name.clone(),
        };
        let store = match &cfg.data_dir {
            Some(d) => Some(Store::open(d).map_err(|e| e.to_string())?),
            None => None,
        };
        let shared = Arc::new(Shared {
            statuses: Mutex::new(BTreeMap::new()),
            decisions: Mutex::new(BTreeMap::new()),
            dead: Mutex::new(BTreeMap::new()),
            heartbeat: Mutex::new(BTreeMap::new()),
            heartbeat_limit: Duration::from_secs(1800).max(cfg.poll.saturating_mul(3)),
            heartbeat_test_offset: cfg.heartbeat_test_offset.clone(),
            watchdog_views: Mutex::new(BTreeMap::new()),
            watchdog_generation: AtomicU64::new(0),
            ingress_epochs: Mutex::new(BTreeMap::new()),
            watchdog_memo_total: Arc::new(AtomicUsize::new(0)),
            watchdog_errors: Mutex::new(BTreeMap::new()),
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            events: Arc::new(EventLog::new(store.clone())),
            shadow: Mutex::new(BTreeMap::new()),
            store: store.clone(),
        });
        // Shadow mode: nothing but reads leaves this process (S-SHADOW).
        let shadow_gate = cfg.shadow.as_ref().map(|s| {
            let g = Arc::new(ShadowGate::new(s.panic_on_write));
            cfg.client
                .set_write_gate(Some(g.clone() as Arc<dyn crate::client::WriteGate>));
            g
        });
        // Installations and their downscoped tokens (W-22).
        let mut installs: BTreeMap<u64, (Vec<String>, BTreeMap<String, String>)> = BTreeMap::new();
        let mut repo_inst = Vec::new();
        for repo in &cfg.repos {
            let inst = match cfg.installation {
                Some(i) => cfg
                    .client
                    .installation(i)
                    .map_err(|e| format!("installation {i}: {e}"))?,
                None => cfg
                    .client
                    .repo_installation(repo)
                    .map_err(|e| format!("{}: no installation: {e}", repo.full()))?,
            };
            let e = installs.entry(inst.id).or_insert_with(|| {
                (
                    Vec::new(),
                    token_permissions(&inst.permissions, cfg.shadow.is_some()),
                )
            });
            e.0.push(repo.name.clone());
            repo_inst.push((repo.clone(), inst.id));
        }
        for (id, (names, perms)) in &installs {
            cfg.client.set_token_scope(*id, names, perms);
        }
        let mut sinks = HashMap::new();
        let mut threads = Vec::new();
        let mut senders = Vec::new();
        // The lease of each repository (not in shadow mode, which never writes).
        let mut leases = Vec::new();
        if let (Some(lc), None) = (&cfg.lease, &cfg.shadow) {
            for (repo, installation) in &repo_inst {
                let api = RepoApi {
                    client: cfg.client.clone(),
                    repo: repo.clone(),
                    installation: *installation,
                };
                leases.push(Arc::new(Lease::new(
                    api,
                    store.clone(),
                    lc.clone(),
                    clock.clone(),
                )));
            }
            cfg.client
                .set_write_gate(Some(Arc::new(LeaseGate::new(leases.clone()))));
        }
        for (repo, installation) in repo_inst {
            let api = RepoApi {
                client: cfg.client.clone(),
                repo: repo.clone(),
                installation,
            };
            let maker = ForgeMaker {
                api,
                clock: clock.clone(),
                opts: opts.clone(),
                retry_base: cfg.retry_base,
                retry_max: cfg.retry_max,
                store: store.clone(),
                check: cfg.check.clone(),
            };
            let forge = maker.make();
            let observer_forge = maker.make();
            let lease = leases
                .iter()
                .find(|l| l.repo_full() == repo.full())
                .cloned();
            let shadow = cfg
                .shadow
                .clone()
                .map(|c| Shadow::new(c, &repo.full(), store.clone()));
            // Shadow mode keeps no journal: it has no side effects to guard.
            let journal = if shadow.is_some() {
                None
            } else {
                store.as_ref().map(|s| match &lease {
                    Some(l) => s.journal(&repo.full()).fenced(l.holder()),
                    None => s.journal(&repo.full()),
                })
            };
            let state = State {
                identity_bootstrap_hold: true,
                host: schlange_core::engine::Host {
                    epoch: 0,
                    bot: opts.bot_login.clone(),
                    repository: repo.full(),
                },
                ..State::default()
            };
            let (tx, rx) = channel();
            let observer_tx = tx.clone();
            let observer_shared = shared.clone();
            let observer_clock = clock.clone();
            let observer_repo = repo.full();
            let observer_test_observations = cfg.watchdog_test_observations.clone();
            let observer_test_samples = cfg.watchdog_test_samples.clone();
            let observer_test_cycles = cfg.watchdog_test_cycles.clone();
            let observer_test_partial_prs = cfg.watchdog_test_partial_prs.clone();
            let observer_test_completed = cfg.watchdog_test_completed.clone();
            let observer_test_pause = cfg.watchdog_test_pause.clone();
            let observer_test_reconcile_pause = cfg.watchdog_test_reconcile_pause.clone();
            sinks.insert(repo.full(), tx.clone());
            senders.push(tx);
            let mut worker = Worker {
                forge,
                maker,
                shadow,
                lease: lease.clone(),
                leading: lease.is_none(),
                shadow_published: std::cell::Cell::new(usize::MAX),
                pause_before_land: cfg.pause_before_land.clone(),
                identity_test_before_attest: cfg.identity_test_before_attest.clone(),
                identity_test_before_release: cfg.identity_test_before_release.clone(),
                identity_test_after_session_rotate: cfg.identity_test_after_session_rotate.clone(),
                identity_test_before_status_ack: cfg.identity_test_before_status_ack.clone(),
                identity_test_release_count: cfg.identity_test_release_count.clone(),
                clock: clock.clone(),
                state,
                rules: None,
                config_key: None,
                pending_config_change: false,
                comments_waiting: Vec::new(),
                config_error: None,
                config_warnings: Vec::new(),
                config: cfg.config.clone(),
                check: cfg.check.clone(),
                timers: BTreeSet::new(),
                rx,
                every: cfg.reconcile_every.max(1),
                next_reconcile: 0,
                last_reconcile: None,
                reconciliations: 0,
                webhooks: 0,
                processed_ingress_epoch: 0,
                validated_ingress_epoch: None,
                shared: shared.clone(),
                poll: cfg.poll,
                journal,
                journal_writable: false,
                seq: 0,
                restart_pending: false,
                recovered: None,
                status_session: None,
                session_revision: None,
                pending_status: None,
                #[cfg(feature = "test-hooks")]
                test_refresh_pause: None,
                crash: cfg.crash,
                dead: false,
            };
            if worker.lease.is_none() && worker.shadow.is_none() {
                worker.recover().inspect_err(|_| {
                    shared.stop.store(true, Ordering::SeqCst);
                })?;
            }
            shared.beat(&repo.full());
            let worker_alive = Arc::new(AtomicBool::new(true));
            let alive_on_exit = worker_alive.clone();
            threads.push(
                std::thread::Builder::new()
                    .name(format!("worker {}", repo.full()))
                    .spawn(move || {
                        let result =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run()));
                        alive_on_exit.store(false, Ordering::SeqCst);
                        if let Err(e) = result {
                            std::panic::resume_unwind(e);
                        }
                    })
                    .map_err(|e| {
                        shared.stop.store(true, Ordering::SeqCst);
                        e.to_string()
                    })?,
            );
            threads.push(
                std::thread::Builder::new()
                    .name(format!("watchdog {observer_repo}"))
                    .spawn(move || {
                        supervise_watchdog(&observer_shared, &observer_repo, || {
                            let mut tracker = crate::watchdog::Watchdog::default();
                            let mut rotation = crate::watchdog::Rotation::default();
                            let mut probes = crate::watchdog::Probes::with_total(
                                observer_shared.watchdog_memo_total.clone(),
                            );
                            let started = observer_shared.heartbeat_now();
                            let mut next_sample = started;
                            while !observer_shared.stop.load(Ordering::SeqCst)
                                && worker_alive.load(Ordering::SeqCst)
                            {
                                let now = observer_shared.heartbeat_now();
                                if now < next_sample {
                                    std::thread::sleep(Duration::from_secs(1));
                                    continue;
                                }
                                next_sample = now + Duration::from_secs(60);
                                let view = observer_shared
                                    .watchdog_views()
                                    .get(&observer_repo)
                                    .cloned();
                                let Some(view) = view else {
                                    tracker.observe(0, None, 0, 60);
                                    probes = crate::watchdog::Probes::with_total(
                                        observer_shared.watchdog_memo_total.clone(),
                                    );
                                    if let Some(count) = &observer_test_cycles {
                                        count.fetch_add(1, Ordering::SeqCst);
                                    }
                                    continue;
                                };
                                let sample = crate::watchdog::candidate(
                                    &observer_forge,
                                    &mut probes,
                                    crate::watchdog::CandidateInput {
                                        state: &view.state,
                                        rules: &view.rules,
                                        now: &|| observer_clock.now(),
                                        generation: view.generation,
                                        cursor: rotation.cursor,
                                        clock: &|| observer_shared.heartbeat_now(),
                                        valid: &|| {
                                            !observer_shared.stop.load(Ordering::SeqCst)
                                                && worker_alive.load(Ordering::SeqCst)
                                                && observer_shared
                                                    .watchdog_views()
                                                    .get(&observer_repo)
                                                    .is_some_and(|v| {
                                                        v.generation == view.generation
                                                    })
                                        },
                                    },
                                );
                                let crate::watchdog::Sample::Done(candidate, deadline) = sample
                                else {
                                    if let crate::watchdog::Sample::Pending(completed) = sample
                                        && let Some(count) = &observer_test_partial_prs
                                    {
                                        count.fetch_max(completed as u64, Ordering::SeqCst);
                                    }
                                    rotation.advance(false, false);
                                    if let Some(count) = &observer_test_samples {
                                        count.fetch_add(1, Ordering::SeqCst);
                                    }
                                    if let Some(count) = &observer_test_cycles {
                                        count.fetch_add(1, Ordering::SeqCst);
                                    }
                                    continue;
                                };
                                if candidate.is_some()
                                    && let Some(hook) = &observer_test_pause
                                {
                                    hook.pause_once();
                                }
                                let current = !observer_shared.stop.load(Ordering::SeqCst)
                                    && worker_alive.load(Ordering::SeqCst)
                                    && deadline
                                        .is_none_or(|end| observer_shared.heartbeat_now() <= end)
                                    && observer_shared
                                        .watchdog_views()
                                        .get(&observer_repo)
                                        .is_some_and(|v| v.generation == view.generation);
                                let candidate = if current { candidate.as_deref() } else { None };
                                if current
                                    && deadline.is_some()
                                    && let Some(count) = &observer_test_completed
                                {
                                    count.fetch_add(1, Ordering::SeqCst);
                                }
                                if candidate.is_some()
                                    && let Some(count) = &observer_test_observations
                                {
                                    count.fetch_add(1, Ordering::SeqCst);
                                }
                                let reconcile = tracker.observe(
                                    view.generation,
                                    candidate,
                                    observer_shared
                                        .heartbeat_now()
                                        .saturating_duration_since(started)
                                        .as_secs(),
                                    60,
                                );
                                rotation.advance(candidate.is_some(), reconcile);
                                if reconcile {
                                    if let Some(hook) = &observer_test_reconcile_pause {
                                        hook.pause_once();
                                    }
                                    observer_shared.inflight.fetch_add(1, Ordering::SeqCst);
                                    if observer_tx.send(Msg::Reconcile).is_err() {
                                        observer_shared.inflight.fetch_sub(1, Ordering::SeqCst);
                                    }
                                }
                                if let Some(count) = &observer_test_samples {
                                    count.fetch_add(1, Ordering::SeqCst);
                                }
                                if let Some(count) = &observer_test_cycles {
                                    count.fetch_add(1, Ordering::SeqCst);
                                }
                            }
                        });
                    })
                    .map_err(|e| {
                        shared.stop.store(true, Ordering::SeqCst);
                        e.to_string()
                    })?,
            );
        }
        let router = Arc::new(Router {
            secret: cfg.webhook_secret.clone(),
            dedup: Mutex::new(Dedup::new(10_000)),
            store: store.clone(),
            sinks,
            shared: shared.clone(),
            #[cfg(test)]
            after_seen_test_barrier: None,
        });
        if let Err(e) = router.wake_pending() {
            shared.stop.store(true, Ordering::SeqCst);
            return Err(format!("durable inbox startup failed: {e}"));
        }
        let sh = shared.clone();
        let admin = cfg.admin_token.clone();
        let handler: Handler =
            Arc::new(move |req: &Request| route(req, &router, &sh, admin.as_deref()));
        let server = Server::start(cfg.listen, cfg.http.clone(), handler).map_err(|e| {
            shared.stop.store(true, Ordering::SeqCst);
            format!("cannot listen on {}: {e}", cfg.listen)
        })?;
        Ok(Service {
            addr: server.addr,
            shared,
            sinks: senders,
            threads,
            server: Some(server),
            store,
            shadow_gate,
            leases,
        })
    }

    /// The shadow report of a repository (S-SHADOW).
    pub fn shadow_report(&self, repo: &str) -> crate::shadow::ShadowReport {
        shadow_report(&self.shared, repo)
    }

    /// The event log of a repository, newest first.
    pub fn events(
        &self,
        repo: &str,
        q: &EventQuery,
    ) -> Result<Vec<crate::store::EventRecord>, String> {
        self.shared.events.query(repo, q)
    }

    pub fn webhook_url(&self) -> String {
        format!("http://{}/webhook", self.addr)
    }

    /// Tests: observe shutdown without holding the service or signalling it.
    pub fn shutdown_probe(&self) -> impl Fn() -> bool + Send + 'static {
        let shared = self.shared.clone();
        move || shared.stop.load(Ordering::SeqCst)
    }

    /// Messages not yet handled by a worker.
    pub fn inflight(&self) -> usize {
        self.shared.inflight.load(Ordering::SeqCst)
    }

    pub fn status(&self) -> BTreeMap<String, WorkerStatus> {
        self.shared.statuses.lock().expect("status").clone()
    }

    /// The decisions of a repository's engine, oldest first (JSON lines, bounded).
    pub fn decision_log(&self, repo: &str) -> Vec<String> {
        self.shared
            .decisions
            .lock()
            .expect("decisions")
            .get(repo)
            .map(|d| d.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Workers that stopped, with the reason.
    pub fn dead_workers(&self) -> BTreeMap<String, String> {
        self.shared.dead.lock().expect("dead").clone()
    }

    /// Asks every worker for a full reconciliation now.
    pub fn reconcile_now(&self) {
        for s in &self.sinks {
            self.shared.inflight.fetch_add(1, Ordering::SeqCst);
            if s.send(Msg::Reconcile).is_err() {
                self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    pub fn shutdown(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for l in &self.leases {
            l.release();
        }
        for s in &self.sinks {
            let _ = s.send(Msg::Shutdown);
        }
        if let Some(srv) = self.server.take() {
            srv.shutdown();
        }
        for t in self.threads {
            let _ = t.join();
        }
    }

    /// Blocks until the workers stop (for the binary).
    pub fn wait(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }
}

/// Whether the request carries the admin token (constant-time comparison).
fn authorized(req: &Request, token: &str) -> bool {
    let given = req
        .header("authorization")
        .and_then(|a| a.strip_prefix("Bearer "))
        .unwrap_or("");
    ct_eq(given.as_bytes(), token.as_bytes())
}

/// Unix seconds of an API timestamp; `timezone` must be UTC (the only zone Schlange reads).
fn api_time(v: &serde_json::Value, timezone: &str) -> Result<Option<i64>, String> {
    match v {
        serde_json::Value::Null => Ok(None),
        serde_json::Value::String(s) => {
            if !matches!(timezone, "UTC" | "Etc/UTC" | "GMT" | "Z" | "") {
                return Err(format!(
                    "time zone `{timezone}` is not supported; use UTC or an offset in the timestamp"
                ));
            }
            schlange_core::time::parse_timestamp(s)
                .map(Some)
                .map_err(|e| format!("`{s}` is not a timestamp ({e:?})"))
        }
        _ => Err("a timestamp must be a string".into()),
    }
}

/// The documented queue API under `/repos/{owner}/{repository}/…`, behind the admin token:
/// `GET merge-queue/status`, `GET|PUT|DELETE merge-queue/pause` (also `queue/pause`), and
/// `GET|POST scheduled_freeze`, `DELETE scheduled_freeze/{id}`. Shapes follow the public API
/// reference; texts are ours.
fn repo_api(req: &Request, router: &Router, shared: &Shared, path: &str) -> Response {
    let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
    let ["repos", owner, name, rest @ ..] = &parts[..] else {
        return Response::text(404, "not found");
    };
    let repo = format!("{owner}/{name}");
    let Some(sink) = router.sinks.get(&repo) else {
        return Response::text(404, "repository not served");
    };
    let status = shared.statuses.lock().expect("status").get(&repo).cloned();
    let body: serde_json::Value = if req.body.is_empty() {
        serde_json::Value::Null
    } else {
        match serde_json::from_slice(&req.body) {
            Ok(v) => v,
            Err(e) => return Response::text(400, format!("body is not JSON: {e}")),
        }
    };
    let control = |c: Control| -> Response {
        let (tx, rx) = channel();
        let Ok(mut epochs) = shared.ingress_epochs.lock() else {
            shared.stop.store(true, Ordering::SeqCst);
            return Response::text(503, "ingress fence unavailable");
        };
        let epoch = epochs.entry(repo.clone()).or_default();
        let Some(next) = epoch.checked_add(1) else {
            shared.stop.store(true, Ordering::SeqCst);
            return Response::text(503, "ingress fence exhausted");
        };
        *epoch = next;
        shared.inflight.fetch_add(1, Ordering::SeqCst);
        let sent = sink.send(Msg::Control(c, tx, next));
        drop(epochs);
        if sent.is_err() {
            shared.inflight.fetch_sub(1, Ordering::SeqCst);
            return Response::text(503, "worker stopped");
        }
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(v)) => Response::json(200, v.to_string()),
            Ok(Err(e)) => Response::text(422, e),
            Err(_) => Response::text(503, "the worker did not answer in time"),
        }
    };
    let query = |f: QueryFn| -> Response {
        let (tx, rx) = channel();
        shared.inflight.fetch_add(1, Ordering::SeqCst);
        if sink.send(Msg::Query(f, tx)).is_err() {
            shared.inflight.fetch_sub(1, Ordering::SeqCst);
            return Response::text(503, "worker stopped");
        }
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(v)) => Response::json(200, v.to_string()),
            Ok(Err(e)) => Response::text(422, e),
            Err(_) => Response::text(503, "the worker did not answer in time"),
        }
    };
    let params = query_params(&req.url);
    let param = |k: &str| params.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
    let queue = status.map(|s| s.queue).unwrap_or(serde_json::Value::Null);
    match (req.method.as_str(), rest) {
        ("GET", ["merge-queue", "status"]) => Response::json(200, queue.to_string()),
        ("GET", ["pulls", n, "explain"]) => {
            let Ok(n) = n.parse::<u64>() else {
                return Response::text(404, "not a pull request number");
            };
            query(Box::new(move |state, rules, _| {
                let rules = rules.ok_or("no valid configuration yet")?;
                let ex = schlange_core::explain::explain(state, rules, n);
                let mut v = serde_json::to_value(&ex).map_err(|e| e.to_string())?;
                v["text"] = serde_json::Value::String(ex.to_text());
                Ok(v)
            }))
        }
        ("POST", ["pulls", n, "simulator"]) => {
            let Ok(n) = n.parse::<u64>() else {
                return Response::text(404, "not a pull request number");
            };
            let Some(text) = body["mergify_yml"].as_str().map(str::to_string) else {
                return Response::text(422, "the body needs `mergify_yml`");
            };
            query(Box::new(move |state, _, check| {
                simulate(state, check, &text, n)
            }))
        }
        ("GET", ["logs"]) => {
            let per_page = param("per_page")
                .and_then(|v| v.parse::<u64>().ok())
                .unwrap_or(10)
                .clamp(1, 100);
            let since = match param("received_from") {
                None => None,
                Some(v) => match v
                    .parse::<u64>()
                    .ok()
                    .or_else(|| crate::time::parse_rfc3339(&v))
                {
                    Some(t) => Some(t),
                    None => return Response::text(422, "received_from is not a timestamp"),
                },
            };
            let q = EventQuery {
                pr: param("pull_request").and_then(|v| v.parse().ok()),
                since,
                before: param("cursor").and_then(|v| v.parse().ok()),
                kind: param("event_type"),
                limit: per_page,
            };
            match shared.events.query(&repo, &q) {
                Ok(list) => {
                    let next = (list.len() as u64 == per_page)
                        .then(|| list.last().map(|e| e.id.to_string()))
                        .flatten();
                    Response::json(
                        200,
                        serde_json::json!({
                            "size": list.len(),
                            "per_page": per_page,
                            "next_cursor": next,
                            "events": list.iter().map(crate::events::api_json).collect::<Vec<_>>(),
                        })
                        .to_string(),
                    )
                }
                Err(e) => Response::text(500, e),
            }
        }
        ("GET", ["audit"]) => {
            let Some(store) = &shared.store else {
                return Response::json(
                    200,
                    serde_json::json!({ "records": [], "note": "no data directory: nothing is recorded" })
                        .to_string(),
                );
            };
            let pr = param("pull_request").and_then(|v| v.parse::<u64>().ok());
            match store.audits(&repo) {
                Ok(list) => {
                    let list: Vec<_> = list
                        .into_iter()
                        .filter(|r| pr.is_none_or(|n| r.number == n))
                        .collect();
                    Response::json(200, serde_json::json!({ "records": list }).to_string())
                }
                Err(e) => Response::text(500, e.to_string()),
            }
        }
        ("GET", ["shadow", "report"]) => Response::json(
            200,
            serde_json::to_string(&shadow_report(shared, &repo)).unwrap_or_default(),
        ),
        ("GET", ["merge-queue" | "queue", "pause"]) => {
            let p = &queue["pause"];
            if p.is_null() {
                Response::json(200, serde_json::json!({ "paused": false }).to_string())
            } else {
                Response::json(200, p.to_string())
            }
        }
        ("PUT", ["merge-queue" | "queue", "pause"]) => {
            let Some(reason) = body["reason"].as_str().filter(|r| !r.is_empty()) else {
                return Response::text(422, "a pause needs a `reason`");
            };
            if reason.chars().count() > 255 {
                return Response::text(422, "the reason is longer than 255 characters");
            }
            control(Control::Pause {
                reason: reason.to_string(),
                by: Some("admin API".into()),
            })
        }
        ("DELETE", ["merge-queue" | "queue", "pause"]) | ("POST", ["queue", "pause", "delete"]) => {
            control(Control::Unpause)
        }
        ("GET", ["scheduled_freeze"]) => Response::json(
            200,
            serde_json::json!({ "scheduled_freezes": queue["freezes"] }).to_string(),
        ),
        ("POST", ["scheduled_freeze"]) => {
            let tz = body["timezone"].as_str().unwrap_or("UTC");
            let Some(reason) = body["reason"].as_str().filter(|r| !r.is_empty()) else {
                return Response::text(422, "a freeze needs a `reason`");
            };
            let start = match api_time(&body["start"], tz) {
                Ok(s) => s,
                Err(e) => return Response::text(422, e),
            };
            let end = match api_time(&body["end"], tz) {
                Ok(e) => e,
                Err(e) => return Response::text(422, e),
            };
            let list = |key: &str| -> Result<Vec<String>, String> {
                match &body[key] {
                    serde_json::Value::Null => Ok(vec![]),
                    serde_json::Value::Array(items) => items
                        .iter()
                        .map(|c| {
                            c.as_str().map(str::to_string).ok_or_else(|| {
                                format!("`{key}` must be a list of condition strings")
                            })
                        })
                        .collect(),
                    _ => Err(format!("`{key}` must be a list")),
                }
            };
            let (matching, exclude) =
                match (list("matching_conditions"), list("exclude_conditions")) {
                    (Ok(m), Ok(x)) => (m, x),
                    (Err(e), _) | (_, Err(e)) => return Response::text(422, e),
                };
            for c in matching.iter().chain(&exclude) {
                if let Err(e) = schlange_config::conditions::parse_atom(c) {
                    return Response::text(422, format!("condition `{c}`: {e}"));
                }
            }
            control(Control::Freeze {
                freeze: Freeze {
                    id: 0,
                    reason: reason.to_string(),
                    // 0: now (the worker reads its clock).
                    start: start.unwrap_or(0).max(0) as Time,
                    end: end.map(|e| e as Time),
                    matching,
                    exclude,
                },
            })
        }
        ("DELETE", ["scheduled_freeze", id]) | ("POST", ["scheduled_freeze", id, "delete"]) => {
            match id.parse::<u64>() {
                Ok(id) => control(Control::Unfreeze { id }),
                Err(_) => Response::text(404, "no such freeze"),
            }
        }
        _ => Response::text(404, "not found"),
    }
}

/// The query parameters of a URL, percent-decoded.
fn query_params(url: &str) -> Vec<(String, String)> {
    let Some((_, q)) = url.split_once('?') else {
        return vec![];
    };
    let decode = |s: &str| {
        let b = s.replace('+', " ");
        let b = b.as_bytes();
        let mut out = Vec::with_capacity(b.len());
        let mut i = 0;
        while i < b.len() {
            if b[i] == b'%'
                && i + 2 < b.len()
                && let Ok(v) = u8::from_str_radix(&String::from_utf8_lossy(&b[i + 1..i + 3]), 16)
            {
                out.push(v);
                i += 3;
            } else {
                out.push(b[i]);
                i += 1;
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    };
    q.split('&')
        .filter(|p| !p.is_empty())
        .map(|p| match p.split_once('=') {
            Some((k, v)) => (decode(k), decode(v)),
            None => (decode(p), String::new()),
        })
        .collect()
}

/// The shadow report from the facts the worker published (or the database).
fn shadow_report(shared: &Shared, repo: &str) -> crate::shadow::ShadowReport {
    let facts = shared
        .shadow
        .lock()
        .expect("shadow")
        .get(repo)
        .cloned()
        .or_else(|| {
            shared
                .store
                .as_ref()
                .and_then(|s| s.shadow_facts(repo).ok())
        })
        .unwrap_or_default();
    crate::shadow::report(repo, &facts)
}

/// `POST pulls/{n}/simulator`: a new configuration, validated like `config validate`, evaluated
/// against the pull request as the engine knows it, without acting (S-SIMULATE). The answer has
/// the public simulator's `title`, `summary` and `deprecations`, and the full explanation.
pub fn simulate(
    state: &State,
    check: &ConfigCheck,
    text: &str,
    n: u64,
) -> Result<serde_json::Value, String> {
    let loaded = check
        .check(".mergify.yml", text)
        .map_err(|e| format!("the configuration is invalid: {e}"))?;
    let ex = schlange_core::explain::explain(state, &loaded.rules, n);
    let title = match ex.state.as_str() {
        "queued" => "The pull request would stay in the queue".to_string(),
        "not-queued" | "held" | "waiting-for-conditions" | "dequeued" => {
            let would = ex
                .rules
                .iter()
                .find(|r| r.conditions.holds)
                .and_then(|_| ex.queue_rules.iter().find(|q| q.queue_conditions.holds));
            match would {
                Some(q) if ex.holds.is_empty() => {
                    format!("The pull request would be queued in `{}`", q.queue)
                }
                _ => "The pull request would not be queued".to_string(),
            }
        }
        other => format!("The pull request is {other}"),
    };
    let mut v = serde_json::json!({
        "title": title,
        "summary": ex.to_text(),
        "deprecations": [],
        "warnings": loaded.warnings,
    });
    v["explanation"] = serde_json::to_value(&ex).map_err(|e| e.to_string())?;
    Ok(v)
}

fn route(req: &Request, router: &Router, shared: &Shared, admin: Option<&str>) -> Response {
    let path = req.url.split('?').next().unwrap_or("");
    if path.starts_with("/repos/") {
        let Some(token) = admin else {
            return Response::text(404, "the queue API is off (no admin token configured)");
        };
        if !authorized(req, token) {
            return Response::text(401, "needs Authorization: Bearer <admin token>");
        }
        return repo_api(req, router, shared, path);
    }
    match (req.method.as_str(), path) {
        ("POST", "/webhook") | ("POST", "/") => {
            let get = |name: &str| req.header(name).map(str::to_string);
            let (s, t) = router.handle(&get, &req.body);
            Response::text(s, t)
        }
        ("GET", "/healthz") => {
            let dead = shared.dead.lock().expect("dead").clone();
            let stale = stale_workers(
                &shared.heartbeat.lock().expect("heartbeat"),
                shared.heartbeat_now(),
                shared.heartbeat_limit,
            );
            let watchdog_errors = shared
                .watchdog_errors
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .clone();
            if dead.is_empty() && stale.is_empty() && watchdog_errors.is_empty() {
                Response::text(200, "ok")
            } else {
                let mut problems: Vec<String> = dead
                    .iter()
                    .map(|(r, why)| format!("worker {r} stopped: {why}"))
                    .collect();
                problems.extend(stale.iter().map(|r| format!("worker {r} heartbeat stale")));
                problems.extend(
                    watchdog_errors
                        .iter()
                        .map(|(r, why)| format!("watchdog {r}: {why}")),
                );
                Response::text(503, problems.join("\n"))
            }
        }
        ("GET", "/status") => {
            let Some(token) = admin else {
                return Response::text(404, "status is off (no admin token configured)");
            };
            let given = req
                .header("authorization")
                .and_then(|a| a.strip_prefix("Bearer "))
                .unwrap_or("");
            if !ct_eq(given.as_bytes(), token.as_bytes()) {
                return Response::text(401, "needs Authorization: Bearer <admin token>");
            }
            let s = shared.statuses.lock().expect("status").clone();
            Response::json(200, serde_json::to_string_pretty(&s).unwrap_or_default())
        }
        _ => Response::text(404, "not found"),
    }
}

/// What a worker needs to make a fresh adapter (after it lost and regained its lease).
struct ForgeMaker {
    api: RepoApi,
    clock: Arc<dyn Clock>,
    opts: ForgeOptions,
    retry_base: Duration,
    retry_max: Duration,
    store: Option<Store>,
    check: Arc<ConfigCheck>,
}

impl ForgeMaker {
    fn make(&self) -> GithubForge {
        let mut forge = GithubForge::new(self.api.clone(), self.clock.clone(), self.opts.clone());
        forge.retry_base = self.retry_base;
        forge.retry_max = self.retry_max;
        forge.store = self.store.clone();
        forge.check = Some(self.check.clone());
        forge
    }
}

struct Worker {
    forge: GithubForge,
    maker: ForgeMaker,
    shadow: Option<Shadow>,
    lease: Option<Arc<Lease>>,
    /// Whether this worker may act (always without a lease).
    leading: bool,
    /// The number of shadow facts last published.
    shadow_published: std::cell::Cell<usize>,
    pause_before_land: Option<Arc<PauseHook>>,
    identity_test_before_attest: Option<Arc<PauseHook>>,
    identity_test_before_release: Option<Arc<PauseHook>>,
    identity_test_after_session_rotate: Option<Arc<PauseHook>>,
    identity_test_before_status_ack: Option<Arc<PauseHook>>,
    identity_test_release_count: Option<Arc<AtomicU64>>,
    clock: Arc<dyn Clock>,
    state: State,
    rules: Option<Rules>,
    /// The configuration text last looked at (valid or not), to validate only on change.
    config_key: Option<String>,
    pending_config_change: bool,
    config_error: Option<String>,
    config_warnings: Vec<String>,
    config: ConfigSource,
    check: Arc<ConfigCheck>,
    timers: BTreeSet<Time>,
    rx: Receiver<Msg>,
    every: Time,
    next_reconcile: Time,
    last_reconcile: Option<Time>,
    reconciliations: u64,
    webhooks: u64,
    processed_ingress_epoch: u64,
    validated_ingress_epoch: Option<u64>,
    shared: Arc<Shared>,
    poll: Duration,
    journal: Option<SqliteJournal>,
    /// False when local history cannot be safely associated with the current lease lineage.
    journal_writable: bool,
    seq: u64,
    /// `Event::Restarted` is fed once the configuration is loaded.
    restart_pending: bool,
    recovered: Option<Vec<String>>,
    status_session: Option<String>,
    session_revision: Option<u64>,
    pending_status: Option<(u64, InboxDelivery)>,
    #[cfg(feature = "test-hooks")]
    test_refresh_pause: Option<Arc<PauseHook>>,
    crash: Option<CrashPoint>,
    dead: bool,
    /// Comments whose sender's permission could not be read yet (retried each round), each
    /// with the head the engine knew for its pull request when the delivery arrived (before
    /// the read it triggers).
    comments_waiting: Vec<(CommentHint, Option<String>)>,
}

impl Worker {
    #[cfg(feature = "test-hooks")]
    fn test_session_build(
        &mut self,
        session: &str,
        revision: u64,
        seq: u64,
        request: &Decision,
        pause: &Arc<PauseHook>,
        after_create: bool,
        confirm_all: bool,
    ) {
        let result = (|| -> Result<(), String> {
            let lease = self.lease.clone().ok_or("missing lease")?;
            if !self.leading || !self.journal_writable || !lease.journal_is_current() {
                return Err("untrusted build lineage".into());
            }
            lease.check()?;
            let epoch = self.processed_ingress_epoch;
            let repo = self.forge.api.repo.full();
            let fence = lease.status().fence.ok_or("missing fence")?;
            let plan = BuildAttempts::new(
                &repo,
                request,
                self.forge.api.client.base(),
                self.forge.api.installation,
                &self.forge.opts.bot_login,
            )?;
            let journal = self.journal.as_ref().ok_or("missing journal")?;
            let shared = self.shared.clone();
            let epochs = shared
                .ingress_epochs
                .lock()
                .map_err(|_| "ingress unavailable")?;
            if shared.stop.load(Ordering::SeqCst)
                || epochs.get(&repo).copied().unwrap_or(0) != epoch
            {
                return Err("build ingress changed".into());
            }
            let ticket = journal
                .claim_session_build(session, revision, seq, request, epoch, &fence, || {
                    self.clock.now()
                })
                .map_err(|e| e.to_string())?;
            drop(epochs);
            let gate_journal = self
                .shared
                .store
                .as_ref()
                .ok_or("missing store")?
                .journal(&repo)
                .fenced(&ticket.holder);
            let gate = Arc::new(ServiceBuildGate {
                ticket,
                journal: gate_journal,
                plan,
                shared,
                lease,
                clock: self.clock.clone(),
                processed_ingress: epoch,
                test_after_create: (after_create || confirm_all)
                    .then(|| (pause.clone(), self.maker.api.clone())),
                test_pause_after_create: after_create,
                test_confirm_all: confirm_all,
            });
            if !after_create && !confirm_all {
                pause.pause_once();
            }
            let mut forge = self.maker.make();
            let gate_dyn: Arc<dyn crate::client::WriteGate> = gate.clone();
            forge.api.client = forge.api.client.with_execution_gate(gate_dyn.clone());
            let built = forge
                .execute_session_build_once(request)
                .map_err(|e| e.to_string())?;
            if !confirm_all {
                return Err("test build requires explicit completion".into());
            }
            let verified = gate.verified(&gate_dyn, built, &forge)?;
            if !after_create {
                pause.pause_once();
            }
            if self.crash == Some(CrashPoint::BeforeBuildCommit) {
                return Err("injected crash before build commit".into());
            }
            gate.lease.check()?;
            let epochs = gate
                .shared
                .ingress_epochs
                .lock()
                .map_err(|_| "ingress unavailable")?;
            if gate.shared.stop.load(Ordering::SeqCst)
                || epochs.get(&repo).copied().unwrap_or(0) != epoch
                || self.processed_ingress_epoch != epoch
            {
                return Err("build ingress changed before commit".into());
            }
            let rules = self.rules.as_ref().ok_or("missing rules")?;
            let journal = self.journal.as_mut().ok_or("missing journal")?;
            let committed = journal
                .commit_verified_build(&verified, epoch, rules, self.clock.now(), || {
                    self.clock.now()
                })
                .map_err(|e| e.to_string())?;
            if self.crash == Some(CrashPoint::AfterBuildCommit) {
                return Err("injected crash after build commit".into());
            }
            self.state = committed.state;
            drop(epochs);
            Err("test build committed".into())
        })();
        self.die(format!("test build stopped: {}", result.unwrap_err()));
    }

    /// Restores the state from the journal, if there is one.
    fn recover(&mut self) -> Result<(), String> {
        let Some(j) = &mut self.journal else {
            return Ok(());
        };
        let repo = self.forge.api.repo.full();
        if j.has_pending_inbox()
            .map_err(|e| format!("{repo}: inbox unreadable: {e}"))?
        {
            if self.status_session.is_some()
                && let Some((recovered, revision, item)) =
                    j.pending_status_snapshot().map_err(|e| e.to_string())?
            {
                self.state = recovered.state;
                self.state.identity_bootstrap_hold = true;
                self.state.identity_discovery_epoch = None;
                self.state.identity_recovery_prepared = false;
                self.state.identity_recovery_clean = false;
                self.pending_status = Some((revision, item));
            }
            // Preserve the old journal untouched: no pending payload is replayed into the
            // engine until sender/head provenance and atomic completion are wired.
            self.journal_writable = false;
            self.state.identity_bootstrap_hold = true;
            self.validated_ingress_epoch = None;
            self.restart_pending = true;
            self.recovered = Some(vec!["durable inbox awaits safe replay".into()]);
            self.forge
                .note("durable inbox awaits safe replay; repository stays held");
            return Ok(());
        }
        if j.has_inbox_redecide()
            .map_err(|e| format!("{repo}: re-decision marker unreadable: {e}"))?
        {
            let recovered = j
                .held_redecide_snapshot()
                .map_err(|e| format!("{repo}: held recovery unreadable: {e}"))?
                .ok_or_else(|| format!("{repo}: held recovery checkpoint missing"))?;
            self.state = recovered.state;
            self.state.identity_bootstrap_hold = true;
            self.state.identity_discovery_epoch = None;
            self.state.identity_recovery_prepared = false;
            self.state.identity_recovery_clean = false;
            self.journal_writable = false;
            self.validated_ingress_epoch = None;
            self.restart_pending = true;
            self.recovered = Some(vec!["durable re-decision required".into()]);
            return Ok(());
        }
        let r = j
            .recover()
            .map_err(|e| format!("{repo}: journal unreadable: {e}"))?;
        self.seq = j.max_seq().map_err(|e| e.to_string())?;
        if let Some(r) = r {
            let doubt: Vec<String> = r
                .in_doubt
                .iter()
                .map(|(seq, d)| format!("#{seq} {}", describe(d)))
                .collect();
            self.forge.note(format!(
                "restarted from the journal: {} queued, {} side effect(s) in doubt{}",
                r.state.queued.len(),
                doubt.len(),
                if doubt.is_empty() {
                    String::new()
                } else {
                    format!(": {}", doubt.join("; "))
                }
            ));
            self.state = r.state;
            self.state.identity_bootstrap_hold = true;
            self.state.identity_discovery_epoch = None;
            self.state.identity_recovery_prepared = false;
            self.state.identity_recovery_clean = false;
            if self.journal_writable {
                if let Some(session) = self.status_session.as_deref() {
                    if !r.in_doubt.is_empty() {
                        self.journal_writable = false;
                    } else {
                        let revision = self
                            .shared
                            .store
                            .as_ref()
                            .ok_or_else(|| format!("{repo}: missing session store"))?
                            .inbox_revision(&repo)
                            .map_err(|e| e.to_string())?;
                        self.state = j
                            .commit_session_hold(session, revision, || self.clock.now())
                            .map_err(|e| format!("{repo}: session bootstrap hold failed: {e}"))?;
                        self.session_revision = Some(
                            revision
                                .checked_add(1)
                                .ok_or_else(|| format!("{repo}: session revision exhausted"))?,
                        );
                        // Held reconciliation is not yet migrated to session transactions.
                        self.journal_writable = false;
                    }
                } else {
                    j.checkpoint(&self.state)
                        .map_err(|e| format!("{repo}: bootstrap hold checkpoint failed: {e}"))?;
                }
            }
            self.restart_pending = true;
            self.recovered = Some(doubt);
        }
        Ok(())
    }

    fn fresh_state(&self) -> State {
        State {
            identity_bootstrap_hold: true,
            host: self.state.host.clone(),
            history: self.state.history.clone(),
            ..State::default()
        }
    }

    /// The lease was taken: start from the journal when it is this process's own (or the
    /// predecessor's on the same data directory), else from GitHub's state alone.
    fn take_over(&mut self) {
        self.leading = true;
        self.validated_ingress_epoch = None;
        self.status_session = None;
        self.session_revision = None;
        self.pending_status = None;
        self.forge = self.maker.make();
        self.state = self.fresh_state();
        self.timers.clear();
        self.comments_waiting.clear();
        self.next_reconcile = 0;
        // The journal is current only when the lease was taken over from this data directory's
        // last leader (no other process acted since its last write).
        self.journal_writable = self.lease.as_ref().is_some_and(|l| l.journal_is_current());
        if self.journal_writable {
            if let (Some(lease), Some(journal)) = (&self.lease, &self.journal) {
                let _pending = match journal.has_pending_inbox() {
                    Ok(pending) => pending,
                    Err(e) => {
                        self.journal_writable = false;
                        self.die(format!("inbox recovery failed: {e}"));
                        return;
                    }
                };
                {
                    if lease.check().is_err() {
                        self.journal_writable = false;
                        self.die("lease fence unavailable before session rotation".into());
                        return;
                    }
                    let session = match journal.rotate_status_session(|| self.clock.now()) {
                        Ok(session) => session,
                        Err(e) => {
                            self.journal_writable = false;
                            self.die(format!("status session rotation failed: {e}"));
                            return;
                        }
                    };
                    if self.crash == Some(CrashPoint::AfterStatusSessionRotation)
                        && session.is_some()
                    {
                        self.journal_writable = false;
                        self.die("injected crash after status session rotation".into());
                        return;
                    }
                    if let Some(hook) = &self.identity_test_after_session_rotate {
                        hook.pause_once();
                    }
                    if lease.check().is_err() {
                        self.journal_writable = false;
                        self.die("lease fence changed after session rotation".into());
                        return;
                    }
                    if let Some(session) = session {
                        let Some(fence) = lease.status().fence else {
                            self.journal_writable = false;
                            self.die("status session fence missing".into());
                            return;
                        };
                        if let Err(e) =
                            journal.confirm_rotated_status_session(&session, &fence, || {
                                self.clock.now()
                            })
                        {
                            self.journal_writable = false;
                            self.die(format!("status session confirmation failed: {e}"));
                            return;
                        }
                        self.status_session = Some(session);
                    }
                }
            }
            if let Err(e) = self.recover() {
                self.journal_writable = false;
                self.die(format!("journal recovery failed under lease: {e}"));
                return;
            }
        } else if self.journal.is_some() {
            self.forge
                .note("lease lineage is untrusted; preserving local journal and holding decisions");
        }
        self.restart_pending = true;
        self.forge.note(format!(
            "took the lease ({}); acting for {}",
            self.lease.as_ref().map(|l| l.holder()).unwrap_or(""),
            self.forge.api.repo.full()
        ));
    }

    /// Another process holds the lease: forget everything in flight, act no more.
    fn step_down(&mut self, why: &str) {
        if self.leading {
            self.forge.note(format!("stepped down: {why}"));
        }
        self.leading = false;
        self.validated_ingress_epoch = None;
        let log = std::mem::take(&mut self.forge.log);
        self.forge = self.maker.make();
        self.forge.log = log;
        self.state = self.fresh_state();
        self.timers.clear();
        self.comments_waiting.clear();
        self.restart_pending = false;
    }

    /// A previously trusted writer must stop if a renewal loses journal continuity.
    fn require_current_lineage(&mut self) -> bool {
        if self.journal_writable && self.lease.as_ref().is_some_and(|l| !l.journal_is_current()) {
            self.journal_writable = false;
            self.die("journal lineage lost during lease renewal".into());
            return false;
        }
        true
    }

    fn die(&mut self, why: String) {
        self.forge.note(format!("worker stops: {why}"));
        self.shared
            .dead
            .lock()
            .expect("dead")
            .insert(self.forge.api.repo.full(), why);
        self.dead = true;
    }

    fn run(mut self) {
        loop {
            self.shared.beat(&self.forge.api.repo.full());
            let mut hints = BTreeSet::new();
            let mut handled = 0;
            let mut reconcile = false;
            let mut stop = false;
            let mut take = |m: Msg, hints: &mut BTreeSet<Hint>, this: &mut Self| match m {
                Msg::Webhook(p, epoch) => {
                    this.processed_ingress_epoch = this.processed_ingress_epoch.max(epoch);
                    this.webhooks += 1;
                    this.forge.note(format!("webhook {}", p.summary));
                    hints.extend(p.hints);
                    handled += 1;
                }
                Msg::Reconcile => {
                    reconcile = true;
                    handled += 1;
                }
                Msg::ReconcileEpoch(epoch) => {
                    this.processed_ingress_epoch = this.processed_ingress_epoch.max(epoch);
                    reconcile = true;
                    handled += 1;
                }
                #[cfg(feature = "test-hooks")]
                Msg::TestSessionBuild {
                    session,
                    revision,
                    seq,
                    request,
                    pause,
                    after_create,
                    confirm_all,
                } => {
                    handled += 1;
                    this.test_session_build(
                        &session,
                        revision,
                        seq,
                        &request,
                        &pause,
                        after_create,
                        confirm_all,
                    );
                }
                #[cfg(feature = "test-hooks")]
                Msg::TestSessionRefresh {
                    session,
                    revision,
                    seq,
                    request,
                    pause,
                } => {
                    handled += 1;
                    let state = this
                        .journal
                        .as_ref()
                        .and_then(|j| j.held_redecide_snapshot().ok().flatten())
                        .map(|r| r.state);
                    if let (Some(state), Some(rules)) = (state, this.rules.clone()) {
                        this.state = state;
                        this.status_session = Some(session);
                        this.session_revision = Some(revision);
                        this.validated_ingress_epoch = Some(this.processed_ingress_epoch);
                        this.test_refresh_pause = Some(pause);
                        this.execute_session_refreshes(vec![(seq, request)], &rules);
                    } else {
                        this.die("test refresh has no seeded state or rules".into());
                    }
                }
                Msg::Shutdown => stop = true,
                Msg::Control(c, reply, epoch) => {
                    this.processed_ingress_epoch = this.processed_ingress_epoch.max(epoch);
                    if !this.state.identity_bootstrap_hold {
                        this.feed(Event::IdentityBootstrapHeld);
                    }
                    let r = control_after_hold(
                        this.leading,
                        this.dead,
                        this.state.identity_bootstrap_hold,
                    )
                    .and_then(|()| this.control(c));
                    let _ = reply.send(r);
                    reconcile = true;
                    handled += 1;
                }
                Msg::Query(f, reply) => {
                    let _ = reply.send(f(&this.state, this.rules.as_ref(), &this.check));
                    handled += 1;
                }
            };
            match self.rx.recv_timeout(self.poll) {
                Ok(m) => {
                    take(m, &mut hints, &mut self);
                    while !self.dead
                        && let Ok(m) = self.rx.try_recv()
                    {
                        take(m, &mut hints, &mut self);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => stop = true,
            }
            if self.dead {
                self.publish();
                self.shared.inflight.fetch_sub(handled, Ordering::SeqCst);
                return;
            }
            if stop || self.shared.stop.load(Ordering::SeqCst) {
                return;
            }
            if let Some(l) = self.lease.clone() {
                let held = l.ensure();
                if !self.require_current_lineage() {
                    self.shared.inflight.fetch_sub(handled, Ordering::SeqCst);
                    return;
                }
                if held && !self.leading {
                    self.take_over();
                } else if !held && self.leading {
                    self.step_down("another process holds the lease");
                }
                if self.dead {
                    self.publish();
                    self.shared.inflight.fetch_sub(handled, Ordering::SeqCst);
                    return;
                }
                if !self.leading {
                    // A follower drops hints: the leader acts on them, and a takeover starts
                    // with a full reconciliation.
                    self.publish();
                    self.shared.inflight.fetch_sub(handled, Ordering::SeqCst);
                    continue;
                }
            }
            let now = self.clock.now();
            if hints.contains(&Hint::Full) || now >= self.next_reconcile {
                reconcile = true;
            }
            // The head each commented pull request had in the engine's view before anything
            // is read: a command vouches at most for that one (a push that lands between the
            // comment and the read must not be attributed to the commenter).
            let arrived: Vec<(CommentHint, Option<String>)> = hints
                .iter()
                .filter_map(|h| match h {
                    Hint::Comment(c) => Some((
                        (**c).clone(),
                        self.state
                            .pull_requests
                            .get(&c.number)
                            .map(|p| p.head_sha.clone()),
                    )),
                    _ => None,
                })
                .collect();
            if reconcile {
                self.reconcile();
            }
            if !hints.is_empty() {
                if !reconcile {
                    // A push to the default branch may change the configuration (MQ-19).
                    let default = self.forge.default_branch(Priority::Normal).ok();
                    if hints
                        .iter()
                        .any(|h| matches!(h, Hint::Branch(b) if Some(b) == default.as_ref()))
                    {
                        self.load_config();
                    }
                    self.read_hints(&hints);
                }
                self.comments_waiting.extend(arrived);
            }
            self.deliver_comments();
            let due: Vec<Time> = self.timers.iter().copied().filter(|t| *t <= now).collect();
            if !due.is_empty() && !self.state.identity_bootstrap_hold {
                for t in due {
                    self.timers.remove(&t);
                }
                self.feed(Event::Tick);
            }
            // Deferred direct writes stay paused until each individual send can be fenced.
            if self.dead {
                self.publish();
                return;
            }
            self.publish();
            self.shared.inflight.fetch_sub(handled, Ordering::SeqCst);
        }
    }

    fn load_config(&mut self) {
        let keep = |has: bool| {
            if has {
                "the last valid configuration stays in force"
            } else {
                "nothing is queued until a valid one is on the default branch"
            }
        };
        let (path, text) = match &self.config {
            ConfigSource::Text(t) => ("--config".to_string(), t.clone()),
            ConfigSource::Repository => {
                match read_from_default_branch(&self.forge.api, Priority::Normal) {
                    Ok(Found::One(path, text)) => (path, text),
                    Ok(Found::None) => {
                        self.config_error = Some(format!(
                            "no configuration file on the default branch (searched {}); {}",
                            schlange_config::config_file::NAMES.join(", "),
                            keep(self.rules.is_some())
                        ));
                        return;
                    }
                    Ok(Found::Ambiguous(e)) => {
                        // Two files: an error; keep the last valid configuration.
                        let msg = format!(
                            "configuration not applied ({}): {e}",
                            keep(self.rules.is_some())
                        );
                        if self.config_error.as_deref() != Some(msg.as_str()) {
                            self.forge.note(msg.clone());
                        }
                        self.config_error = Some(msg);
                        self.config_key = None;
                        return;
                    }
                    Err(e) => {
                        self.forge.note(format!("configuration not read: {e}"));
                        return;
                    }
                }
            }
        };
        let remote = crate::config::extended_text(&self.forge.api, &text, Priority::Normal);
        let key = format!("{path}\n{text}\n{remote:?}");
        if self.config_key.as_deref() == Some(key.as_str()) {
            return;
        }
        self.config_key = Some(key);
        let own = self.forge.api.repo.full();
        let loaded = self.check.check_extended(&path, &text, Some(&own), |name| {
            remote.unwrap_or_else(|| Err(format!("repository `{name}` was not read")))
        });
        match loaded {
            Ok(loaded) => {
                let changed = self.rules.is_some();
                for w in &loaded.warnings {
                    self.forge.note(format!("configuration warning: {w}"));
                }
                let r = loaded.rules;
                self.forge.read_reviews = r.queue_rules.iter().any(|q| {
                    q.commit_message_format.as_ref().is_some_and(|f| {
                        f.trailers
                            .contains(&schlange_config::rules::Trailer::ApprovedBy)
                    })
                }) || r.pull_request_rules.iter().any(|p| {
                    p.actions.iter().any(|a| {
                        use schlange_config::actions::RuleAction as A;
                        matches!(
                            a,
                            A::Review { .. }
                                | A::DismissReviews { .. }
                                | A::RequestReviews { .. }
                                | A::Comment { .. }
                                | A::Close { .. }
                        )
                    })
                });
                self.rules = Some(r);
                self.config_warnings = loaded.warnings;
                self.config_error = None;
                if changed || self.state.identity_bootstrap_hold {
                    // MQ-19: every pull request is evaluated again under the new rules.
                    self.forge
                        .note(format!("configuration changed ({path}); using the new one"));
                    if self.state.identity_bootstrap_hold {
                        self.pending_config_change = true;
                    } else {
                        self.feed(Event::ConfigurationChanged);
                    }
                }
            }
            Err(e) => {
                let keep = keep(self.rules.is_some());
                self.config_error = Some(format!(
                    "configuration {path} invalid, not applied ({keep}): {e}"
                ));
                self.forge
                    .note(self.config_error.clone().unwrap_or_default());
            }
        }
    }

    /// Provisional safety fence for facts already known to the worker. A failed identity
    /// read must never leave those facts eligible for decisions. Fresh-batch release is not
    /// yet wired; this deliberately retains the hold.
    fn hold_known_identity(&mut self) -> bool {
        let mut bases: BTreeMap<String, Vec<u64>> = BTreeMap::new();
        for p in self
            .state
            .pull_requests
            .values()
            .filter(|p| !p.closed && !p.merged)
        {
            bases.entry(p.base.clone()).or_default().push(p.number);
        }
        for base in self.state.trains.keys() {
            bases.entry(base.clone()).or_default();
        }
        let default = self.forge.default_branch(Priority::Normal);
        if let Ok(base) = &default {
            bases.entry(base.clone()).or_default();
        }
        let discovered = self.forge.api.open_pulls(Priority::Normal);
        if let Ok(open) = &discovered {
            for p in open {
                bases
                    .entry(p.base.ref_name.clone())
                    .or_default()
                    .push(p.number);
            }
        }
        if !bases.is_empty() {
            let bases = bases
                .into_iter()
                .map(|(base, numbers)| {
                    let generation = self
                        .state
                        .check_identity_generations
                        .get(&base)
                        .copied()
                        .unwrap_or(0)
                        .saturating_add(1);
                    (base, (generation, numbers))
                })
                .collect();
            self.feed(Event::CheckIdentitiesUnavailable { bases });
        }
        if let Err(e) = discovered {
            self.forge
                .note(format!("identity discovery unavailable: {e}"));
            return false;
        }
        if let Err(e) = default {
            self.forge
                .note(format!("identity default branch unavailable: {e}"));
            return false;
        }
        true
    }

    /// Stages only complete, twice-read identity views; the repository bootstrap hold stays
    /// in place until dispatch fencing and deferred-event replay have also succeeded.
    fn stage_identity_batches(&mut self, captured: u64, started: Instant) {
        let repo = self.forge.api.repo.full();
        let shared = self.shared.clone();
        let fence = || {
            !shared.stop.load(Ordering::SeqCst)
                && started.elapsed() <= Duration::from_secs(120)
                && shared
                    .ingress_epochs
                    .lock()
                    .ok()
                    .is_some_and(|e| e.get(&repo).copied().unwrap_or(0) == captured)
        };
        let mut targets = self.state.check_identity_targets.clone();
        let open = match self.forge.api.open_pulls(Priority::Normal) {
            Ok(open) => open,
            Err(_) => return,
        };
        if !fence() {
            return;
        }
        let observed: BTreeMap<_, _> = open
            .into_iter()
            .map(|p| (p.number, (p.base.ref_name, p.head.sha)))
            .collect();
        let mut moves = Vec::new();
        for (from, numbers) in &self.state.check_identity_targets {
            for number in numbers {
                let Some((to, head)) = observed.get(number) else {
                    continue;
                };
                if from == to {
                    continue;
                }
                if !targets.get(to).is_some_and(|s| s.contains(number)) {
                    return;
                }
                let direct = match self.forge.api.pull(*number, Priority::Normal) {
                    Ok(p) => p,
                    Err(_) => return,
                };
                if direct.state != "open"
                    || direct.merged
                    || direct.base.ref_name != *to
                    || direct.head.sha != *head
                    || !fence()
                {
                    return;
                }
                moves.push((
                    *number,
                    from.clone(),
                    to.clone(),
                    head.clone(),
                    self.state
                        .check_identity_epochs
                        .get(from)
                        .copied()
                        .unwrap_or(0),
                    self.state
                        .check_identity_epochs
                        .get(to)
                        .copied()
                        .unwrap_or(0),
                ));
            }
        }
        for (number, from, _, _, _, _) in &moves {
            targets.get_mut(from).expect("source").remove(number);
        }
        let mut batches = Vec::new();
        for (base, numbers) in &targets {
            let numbers: Vec<_> = numbers.iter().copied().collect();
            let batch =
                match self
                    .forge
                    .read_identity_batch(base, &numbers, Priority::Normal, &fence)
                {
                    Ok(batch) => batch,
                    Err(e) => {
                        self.forge
                            .note(format!("identity batch for {base} unavailable: {e}"));
                        return;
                    }
                };
            let epoch = self
                .state
                .check_identity_epochs
                .get(base)
                .copied()
                .unwrap_or(0);
            let generation = self
                .state
                .check_identity_generations
                .get(base)
                .copied()
                .unwrap_or(0)
                .saturating_add(1);
            batches.push((base.clone(), epoch, generation, batch));
        }
        if !fence() || !self.leading || self.lease.as_ref().is_some_and(|l| l.check().is_err()) {
            return;
        }
        for (number, _, to, head, _, _) in &moves {
            let direct = match self.forge.api.pull(*number, Priority::Normal) {
                Ok(p) => p,
                Err(_) => return,
            };
            if direct.state != "open"
                || direct.merged
                || direct.base.ref_name != *to
                || direct.head.sha != *head
                || !fence()
            {
                return;
            }
        }
        let expected_open: BTreeSet<_> = batches
            .iter()
            .flat_map(|(base, _, _, batch)| {
                batch
                    .pull_requests
                    .iter()
                    .filter(|p| !p.closed)
                    .map(move |p| (base.clone(), p.number, p.head_sha.clone()))
            })
            .collect();
        if !expected_open.is_empty()
            && let Some(hook) = &self.identity_test_before_attest
        {
            hook.pause_once();
        }
        for (base, _, _, staged) in &batches {
            let numbers: Vec<_> = staged.pull_requests.iter().map(|p| p.number).collect();
            match self
                .forge
                .read_identity_batch(base, &numbers, Priority::Normal, &fence)
            {
                Ok(current) if current == *staged => {}
                _ => return,
            }
        }
        let open = match self.forge.api.open_pulls(Priority::Normal) {
            Ok(open) => open,
            Err(_) => return,
        };
        let observed_open: BTreeSet<_> = open
            .into_iter()
            .map(|p| (p.base.ref_name, p.number, p.head.sha))
            .collect();
        if observed_open != expected_open
            || !fence()
            || self.processed_ingress_epoch != captured
            || !self.leading
            || self.lease.as_ref().is_some_and(|l| l.check().is_err())
        {
            return;
        }
        let mut verified = BTreeMap::new();
        for (base, epoch, generation, batch) in batches {
            let mut checks = Vec::new();
            let mut bound = Vec::new();
            for requirement in batch.requirements {
                if requirement.context == self.forge.opts.check_name {
                    continue;
                }
                match requirement.app_id {
                    Some(app_id) => bound.push(schlange_core::model::RequiredCheck {
                        context: requirement.context,
                        app_id,
                    }),
                    None => checks.push(requirement.context),
                }
            }
            verified.insert(
                base,
                schlange_core::engine::VerifiedIdentityBase {
                    epoch,
                    generation,
                    base_tip: batch.base_tip,
                    required_checks: checks,
                    bound_required_checks: bound,
                    pull_requests: batch.pull_requests,
                },
            );
        }
        if !fence() {
            return;
        }
        if self.crash == Some(CrashPoint::BeforeIdentityPublication) && !expected_open.is_empty() {
            self.die("injected crash before identity publication".into());
            return;
        }
        self.feed(Event::IdentityRepositoryVerified { bases: verified });
        if self.crash == Some(CrashPoint::AfterIdentityPublication) && !expected_open.is_empty() {
            self.die("injected crash after identity publication".into());
            return;
        }
        if !self.state.check_identity_targets.is_empty()
            || !fence()
            || self.processed_ingress_epoch != captured
            || !self.leading
            || self.lease.as_ref().is_some_and(|l| l.check().is_err())
        {
            return;
        }
        self.feed(Event::IdentityDiscoveryVerified {
            ingress_epoch: captured,
        });
        if self.pending_config_change
            && self.state.identity_discovery_epoch == Some(captured)
            && fence()
            && self.leading
            && self.lease.as_ref().is_none_or(|l| l.check().is_ok())
        {
            self.feed(Event::IdentityRecoveryConfigApplied {
                ingress_epoch: captured,
            });
            if !self.dead && self.state.identity_discovery_epoch == Some(captured) {
                self.pending_config_change = false;
            }
        }
        // The verified view remains held. A durable obligation prevents its later release
        // until recovery has performed a separately fenced re-decision.
        let status_lease_ok = if self.pending_status.is_some() {
            match &self.lease {
                Some(lease) if lease.journal_is_current() => match lease.check() {
                    Ok(()) => true,
                    Err(error) => {
                        self.forge
                            .note(format!("status lease check unavailable: {error}"));
                        false
                    }
                },
                _ => false,
            }
        } else {
            false
        };
        if self.state.identity_discovery_epoch == Some(captured)
            && let (Some((revision, item)), Some(session), Some(journal)) = (
                &self.pending_status,
                &self.status_session,
                &mut self.journal,
            )
            && status_lease_ok
            && fence()
        {
            if let Some(hook) = &self.identity_test_before_status_ack {
                hook.pause_once();
            }
            if self
                .lease
                .as_ref()
                .is_none_or(|lease| lease.check().is_err())
            {
                self.forge
                    .note("status acknowledgment lease fence unavailable");
                return;
            }
            let Ok(epochs) = shared.ingress_epochs.lock() else {
                return;
            };
            if epochs.get(&repo).copied().unwrap_or(0) == captured
                && self.processed_ingress_epoch == captured
                && started.elapsed() <= Duration::from_secs(120)
            {
                if self.crash == Some(CrashPoint::BeforeHeldStatusAck) {
                    drop(epochs);
                    self.die("injected crash before held status acknowledgment".into());
                    return;
                }
                match journal
                    .commit_held_status(item, *revision, &self.state, session, || self.clock.now())
                {
                    Ok(()) => {
                        if self.crash == Some(CrashPoint::AfterHeldStatusAck) {
                            drop(epochs);
                            self.die("injected crash after held status acknowledgment".into());
                            return;
                        }
                        self.session_revision = revision.checked_add(1);
                        self.pending_status = None;
                    }
                    Err(e) => self
                        .forge
                        .note(format!("held status checkpoint unavailable: {e}")),
                }
            } else {
                self.forge.note(format!(
                    "held status ingress mismatch: observed={} captured={} processed={}",
                    epochs.get(&repo).copied().unwrap_or(0),
                    captured,
                    self.processed_ingress_epoch
                ));
            }
            drop(epochs);
            if self.pending_status.is_none() {
                match journal.pending_status_snapshot() {
                    Ok(Some((_, next_revision, next_item))) => {
                        self.pending_status = Some((next_revision, next_item));
                        self.next_reconcile = 0;
                    }
                    Ok(None) => {}
                    Err(e) => self
                        .forge
                        .note(format!("next status recovery unavailable: {e}")),
                }
            }
        }
        // The first, deliberately narrow recovery case: no PR or prior side effect exists.
        // All nonempty or recovered repositories remain held until full recovery is built.
        let empty = self.state.pull_requests.is_empty()
            && self.state.queued.is_empty()
            && self.state.trains.is_empty()
            && self.state.closing.is_empty()
            && self.state.pending_commands.is_empty()
            && self.state.actions.pending.is_empty()
            && self.comments_waiting.is_empty()
            && self.recovered.is_none()
            && !self.restart_pending
            && !self.pending_config_change
            && self.seq == 0
            && self.lease.as_ref().is_some_and(|l| l.journal_is_current());
        let journal_clean = match &self.journal {
            Some(_) if !self.journal_writable => false,
            Some(j) => {
                !j.has_inbox_redecide().unwrap_or(true)
                    && j.recover()
                        .ok()
                        .flatten()
                        .is_some_and(|r| r.in_doubt.is_empty())
            }
            None => false,
        };
        if empty
            && journal_clean
            && fence()
            && self.processed_ingress_epoch == captured
            && self.leading
            && self.lease.as_ref().is_none_or(|l| l.check().is_ok())
        {
            self.feed(Event::IdentityRecoveryCleanFinalized {
                ingress_epoch: captured,
            });
            if self.state.identity_recovery_clean
                && fence()
                && self.processed_ingress_epoch == captured
                && self.leading
                && self.lease.as_ref().is_none_or(|l| l.check().is_ok())
            {
                if captured > 0
                    && let Some(hook) = &self.identity_test_before_release
                {
                    hook.pause_once();
                }
                if !fence()
                    || !self.leading
                    || self.lease.as_ref().is_some_and(|l| l.check().is_err())
                {
                    return;
                }
                // The release step performs only an SQLite checkpoint and emits no decisions.
                // Serialize it with ingress enqueue so a newly accepted input cannot slip
                // between this final comparison and the checkpoint.
                let shared = self.shared.clone();
                let Ok(epochs) = shared.ingress_epochs.lock() else {
                    shared.stop.store(true, Ordering::SeqCst);
                    return;
                };
                if epochs.get(&repo).copied().unwrap_or(0) != captured
                    || self.processed_ingress_epoch != captured
                {
                    return;
                }
                self.feed(Event::IdentityBootstrapReleased {
                    ingress_epoch: captured,
                });
                if !self.state.identity_bootstrap_hold {
                    self.validated_ingress_epoch = Some(captured);
                }
                if !self.state.identity_bootstrap_hold
                    && let Some(count) = &self.identity_test_release_count
                {
                    count.fetch_add(1, Ordering::SeqCst);
                }
                drop(epochs);
            }
        }
    }

    fn reconcile(&mut self) {
        if !self.state.identity_bootstrap_hold {
            self.feed(Event::IdentityBootstrapHeld);
        }
        if self.state.identity_bootstrap_hold
            && self.status_session.is_some()
            && self.pending_status.is_none()
            && let Some(journal) = &self.journal
        {
            match journal.pending_status_snapshot() {
                Ok(Some((_, revision, item))) => self.pending_status = Some((revision, item)),
                Ok(None) => {}
                Err(e) => self
                    .forge
                    .note(format!("pending status recovery unavailable: {e}")),
            }
        }
        let repo = self.forge.api.repo.full();
        let shared = self.shared.clone();
        let captured = match shared.ingress_epochs.lock() {
            Ok(epochs) => epochs.get(&repo).copied().unwrap_or(0),
            Err(_) => {
                self.die("identity ingress fence unavailable".into());
                return;
            }
        };
        let started = Instant::now();
        let _ = self.hold_known_identity();
        // The heads before this round's reads: a command recovered below vouches at most for
        // these.
        let heads_before: BTreeMap<u64, String> = self
            .state
            .pull_requests
            .iter()
            .map(|(n, p)| (*n, p.head_sha.clone()))
            .collect();
        let now = self.clock.now();
        self.next_reconcile = now + self.every;
        self.load_config();
        let Some(rules) = self.rules.clone() else {
            return;
        };
        if !self.hold_known_identity() {
            return;
        }
        if self.state.identity_bootstrap_hold && !self.state.identity_recovery_prepared {
            self.feed(Event::IdentityRecoveryPrepared);
        }
        if self.state.identity_bootstrap_hold {
            if captured == self.processed_ingress_epoch {
                self.stage_identity_batches(captured, started);
            }
            // Even a clean empty release must not run provisional work in this turn.
            self.last_reconcile = Some(now);
            self.reconciliations += 1;
            return;
        }
        if self.restart_pending && !self.state.identity_bootstrap_hold {
            self.restart_pending = false;
            self.feed(Event::Restarted);
        }
        let view = match &self.shadow {
            Some(sh) => sh.real_view(&self.state),
            None => self.state.clone(),
        };
        match self.forge.reconcile(&view, &rules) {
            Ok((ev, _sweep)) => {
                let ev = match &mut self.shadow {
                    Some(sh) => sh.patch(ev, &self.forge, now),
                    None => ev,
                };
                self.feed(ev);
                if self.dead || !self.leading {
                    return;
                }
                self.required_checks();
                if self.dead || !self.leading {
                    return;
                }
                if let Some(sh) = &mut self.shadow {
                    sh.observe(&mut self.forge, &self.state, now);
                }
                self.last_reconcile = Some(now);
                self.reconciliations += 1;
                if !self.state.identity_bootstrap_hold {
                    match self.forge.unhandled_commands(&self.state, &heads_before) {
                        Ok(comments) if !comments.is_empty() => {
                            self.feed(Event::Comments { comments });
                        }
                        Ok(_) => {}
                        Err(e) => self
                            .forge
                            .note(format!("comments not reconciled (provisional): {e}")),
                    }
                }
            }
            Err(e) => self
                .forge
                .note(format!("reconciliation skipped (provisional): {e}")),
        }
    }

    fn read_hints(&mut self, hints: &BTreeSet<Hint>) {
        if !self.state.identity_bootstrap_hold {
            self.feed(Event::IdentityBootstrapHeld);
        }
        if self.state.identity_bootstrap_hold {
            self.reconcile();
            return;
        }
        if !self.hold_known_identity() {
            return;
        }
        if self.rules.is_none() {
            return;
        }
        let mut numbers = BTreeSet::new();
        let mut branches = BTreeSet::new();
        for h in hints {
            match h {
                Hint::PullRequest(n) => {
                    numbers.insert(*n);
                }
                Hint::Branch(b) => {
                    branches.insert(b.clone());
                }
                Hint::Sha(sha) => numbers.extend(
                    self.state
                        .pull_requests
                        .values()
                        .filter(|p| &p.head_sha == sha && !p.closed)
                        .map(|p| p.number),
                ),
                Hint::Comment(c) => {
                    numbers.insert(c.number);
                }
                Hint::Full => {}
            }
        }
        if numbers.is_empty() && branches.is_empty() {
            return;
        }
        let mut numbers: Vec<u64> = numbers.into_iter().collect();
        let mut branches: Vec<String> = branches.into_iter().collect();
        if let Some(sh) = &self.shadow {
            (numbers, branches) = sh.real_reads(&numbers, &branches);
        }
        let ev = self.forge.snapshot_partial(&numbers, &branches);
        let ev = match &mut self.shadow {
            Some(sh) => sh.patch(ev, &self.forge, self.clock.now()),
            None => ev,
        };
        self.feed(ev);
    }

    /// Hands waiting comments to the engine with their sender's permission, read now (the
    /// restriction is checked when the command runs). A comment whose permission cannot be
    /// read waits for the next round; one on a pull request the engine does not know yet
    /// waits too.
    fn deliver_comments(&mut self) {
        if self.state.identity_bootstrap_hold
            || self.rules.is_none()
            || self.comments_waiting.is_empty()
        {
            return;
        }
        let mut ready = Vec::new();
        let mut later = Vec::new();
        for (c, head_before) in std::mem::take(&mut self.comments_waiting) {
            if self.state.check_identity_unavailable.contains(&c.number)
                || !self.state.pull_requests.contains_key(&c.number)
            {
                later.push((c, head_before));
                continue;
            }
            let by_queue = c.author == self.forge.opts.bot_login;
            let permission = if by_queue && c.sender == c.author {
                Some("none".to_string())
            } else {
                match self
                    .forge
                    .api
                    .collaborator_permission(&c.sender, Priority::Normal)
                {
                    Ok(p) => Some(p),
                    Err(e) => {
                        self.forge.note(format!(
                            "permission of {} not read (comment {} waits): {e}",
                            c.sender, c.id
                        ));
                        later.push((c, head_before));
                        continue;
                    }
                }
            };
            // The head the command vouches for: the one GitHub's activity log shows at the
            // moment of the comment (or of the tick), else the one known before it arrived.
            let vouches = by_queue && c.sender != c.author
                || (!by_queue && !c.edited && is_queue_command(&c.body));
            let head_sha = if vouches {
                match self.forge.vouched_head(
                    &self.state,
                    c.number,
                    &c.version,
                    head_before.clone(),
                ) {
                    Ok(h) => h,
                    Err(e) => {
                        self.forge
                            .note(format!("head at comment {} not read (it waits): {e}", c.id));
                        later.push((c, head_before));
                        continue;
                    }
                }
            } else {
                self.forge.head_of(c.number)
            };
            ready.push(schlange_core::model::Comment {
                id: c.id,
                number: c.number,
                author: c.author.clone(),
                sender: c.sender.clone(),
                body: c.body.clone(),
                edited: c.edited,
                version: c.version.clone(),
                sender_permission: permission,
                by_queue,
                head_sha,
            });
        }
        // A comment waits a bounded number of rounds for its pull request.
        later.truncate(1000);
        self.comments_waiting = later;
        if !ready.is_empty() {
            self.feed(Event::Comments { comments: ready });
        }
    }

    fn log_decision(&self, d: &Decision) {
        let mut all = self.shared.decisions.lock().expect("decisions");
        let log = all.entry(self.forge.api.repo.full()).or_default();
        if log.len() >= DECISION_LOG_MAX {
            log.pop_front();
        }
        log.push_back(describe(d));
    }

    /// Queue presentation effects after the state/intent transaction commits.
    fn record_session_decision(&mut self, decision: &Decision) {
        if let Decision::WakeAt { at } = decision {
            self.timers.insert(*at);
            return;
        }
        self.forge.note(format!("decision: {}", describe(decision)));
        self.log_decision(decision);
        if session_presentation(decision) {
            let replies = self.forge.execute(decision);
            debug_assert!(replies.is_empty());
        }
    }

    /// Check the accepted/processed input fence and current remote lease before an effect.
    fn dispatch_is_current(&self) -> bool {
        let Some(epoch) = self.validated_ingress_epoch else {
            return false;
        };
        let repo = self.forge.api.repo.full();
        self.leading
            && (!self.journal_writable
                || self.lease.as_ref().is_some_and(|l| l.journal_is_current()))
            && ingress_valid(&self.shared, &repo, epoch, self.processed_ingress_epoch)
            && self.lease.as_ref().is_none_or(|l| l.check().is_ok())
            && ingress_valid(&self.shared, &repo, epoch, self.processed_ingress_epoch)
    }

    /// Process journaled read decisions serially. Every reply is committed against evidence
    /// captured before the HTTP read; unsupported successor effects remain in doubt.
    fn execute_session_refreshes(
        &mut self,
        assigned: Vec<(u64, Decision)>,
        rules: &schlange_config::rules::Rules,
    ) {
        let mut work: VecDeque<_> = assigned.into();
        while let Some((seq, request)) = work.pop_front() {
            if !matches!(request, Decision::Refresh { .. }) {
                self.die("unsupported session successor effect".into());
                return;
            }
            let (Some(session), Some(revision), Some(epoch), Some(lease)) = (
                self.status_session.as_deref(),
                self.session_revision,
                self.validated_ingress_epoch,
                self.lease.as_ref(),
            ) else {
                self.die("refresh lacks session fences".into());
                return;
            };
            if !self.leading || !lease.journal_is_current() || lease.check().is_err() {
                self.die("refresh lease unavailable before read".into());
                return;
            }
            let shared = self.shared.clone();
            let repo = self.forge.api.repo.full();
            let Ok(epochs) = shared.ingress_epochs.lock() else {
                self.die("refresh ingress unavailable".into());
                return;
            };
            if epochs.get(&repo).copied().unwrap_or(0) != epoch
                || self.processed_ingress_epoch != epoch
            {
                drop(epochs);
                self.die("refresh ingress changed before read".into());
                return;
            }
            let Some(journal) = &self.journal else {
                drop(epochs);
                self.die("refresh journal unavailable".into());
                return;
            };
            let ticket = match journal.capture_session_refresh(
                session,
                revision,
                seq,
                &request,
                epoch,
                || self.clock.now(),
            ) {
                Ok(ticket) => ticket,
                Err(error) => {
                    drop(epochs);
                    self.die(format!("refresh capture failed: {error}"));
                    return;
                }
            };
            drop(epochs);
            let Decision::Refresh {
                token,
                numbers,
                branches,
            } = &request
            else {
                unreachable!()
            };
            let reply =
                match self.forge.consistent_refresh(
                    numbers,
                    branches,
                    *token,
                    Priority::Landing,
                    || {
                        !shared.stop.load(Ordering::SeqCst)
                            && shared.ingress_epochs.lock().ok().is_some_and(|epochs| {
                                epochs.get(&repo).copied().unwrap_or(0) == epoch
                            })
                    },
                ) {
                    Ok(reply) => reply,
                    Err(error) => {
                        self.die(format!("refresh read failed: {error}"));
                        return;
                    }
                };
            #[cfg(feature = "test-hooks")]
            if let Some(hook) = self.test_refresh_pause.take() {
                hook.pause_once();
            }
            if self
                .lease
                .as_ref()
                .is_none_or(|lease| !lease.journal_is_current() || lease.check().is_err())
            {
                self.die("refresh lease changed after read".into());
                return;
            }
            let Ok(epochs) = shared.ingress_epochs.lock() else {
                self.die("refresh ingress unavailable after read".into());
                return;
            };
            let current = epochs.get(&repo).copied().unwrap_or(0);
            if current != epoch || self.processed_ingress_epoch != epoch {
                drop(epochs);
                self.die("refresh ingress changed after read".into());
                return;
            }
            let Some(journal) = &mut self.journal else {
                drop(epochs);
                self.die("refresh journal unavailable after read".into());
                return;
            };
            let committed = match journal.commit_captured_refresh(
                &ticket,
                current,
                rules,
                &reply,
                self.clock.now(),
                || self.clock.now(),
            ) {
                Ok(committed) => committed,
                Err(error) => {
                    drop(epochs);
                    self.die(format!("refresh reply commit failed: {error}"));
                    return;
                }
            };
            self.state = committed.state;
            self.session_revision = Some(committed.revision);
            drop(epochs);
            for decision in &committed.decisions {
                self.record_session_decision(decision);
            }
            work.extend(committed.intents);
        }
    }

    /// One event into the engine, its decisions to GitHub, the replies back. The state is
    /// checkpointed before the side effects, each side effect journaled before it is sent.
    fn feed(&mut self, event: Event) {
        if !self.require_current_lineage() {
            return;
        }
        if matches!(event, Event::IdentityBootstrapHeld)
            && !self.state.identity_bootstrap_hold
            && let Some(session) = self.status_session.as_deref()
        {
            let Some(revision) = self.session_revision else {
                self.die("session hold lacks revision".into());
                return;
            };
            if self
                .lease
                .as_ref()
                .is_none_or(|lease| !lease.journal_is_current() || lease.check().is_err())
            {
                self.die("session hold lease unavailable".into());
                return;
            }
            let shared = self.shared.clone();
            let Ok(epochs) = shared.ingress_epochs.lock() else {
                self.die("session hold ingress unavailable".into());
                return;
            };
            let Some(journal) = &mut self.journal else {
                drop(epochs);
                self.die("session hold journal unavailable".into());
                return;
            };
            let held = match journal.commit_session_hold(session, revision, || self.clock.now()) {
                Ok(state) => state,
                Err(error) => {
                    drop(epochs);
                    self.die(format!("session hold checkpoint failed: {error}"));
                    return;
                }
            };
            self.state = held;
            self.session_revision = revision.checked_add(1);
            self.validated_ingress_epoch = None;
            self.journal_writable = false;
            drop(epochs);
            return;
        }
        let Some(rules) = self.rules.clone() else {
            return;
        };
        let mut queue = VecDeque::from([event]);
        while let Some(ev) = queue.pop_front() {
            if matches!(ev, Event::IdentityBootstrapHeld) {
                self.validated_ingress_epoch = None;
            } else if !self.state.identity_bootstrap_hold {
                let current = self.validated_ingress_epoch.is_some_and(|epoch| {
                    ingress_valid(
                        &self.shared,
                        &self.forge.api.repo.full(),
                        epoch,
                        self.processed_ingress_epoch,
                    )
                });
                if !current {
                    self.die("identity input changed before decision processing".into());
                    return;
                }
            }
            if self.dead {
                return;
            }
            if !self.leading {
                return;
            }
            let now = self.clock.now();
            let (mut next, decisions) = step(&self.state, &rules, &ev, now);
            let session_step = self.status_session.is_some() && !next.identity_bootstrap_hold;
            if session_step {
                for decision in &decisions {
                    if let Decision::WakeAt { at } = decision {
                        next.wakeups.insert(*at);
                    }
                }
                if decisions
                    .iter()
                    .any(|d| is_side_effect(d) && !matches!(d, Decision::Refresh { .. }))
                {
                    self.die("session step needs side-effect recovery".into());
                    return;
                }
                let (Some(session), Some(revision), Some(epoch), Some(lease)) = (
                    self.status_session.as_deref(),
                    self.session_revision,
                    self.validated_ingress_epoch,
                    self.lease.as_ref(),
                ) else {
                    self.die("session step lacks durable fences".into());
                    return;
                };
                if !lease.journal_is_current() || lease.check().is_err() {
                    self.die("session step lease unavailable".into());
                    return;
                }
                let shared = self.shared.clone();
                let repo = self.forge.api.repo.full();
                let Ok(epochs) = shared.ingress_epochs.lock() else {
                    self.die("session ingress unavailable".into());
                    return;
                };
                if epochs.get(&repo).copied().unwrap_or(0) != epoch
                    || self.processed_ingress_epoch != epoch
                {
                    drop(epochs);
                    self.die("session ingress changed".into());
                    return;
                }
                let Some(journal) = &mut self.journal else {
                    drop(epochs);
                    self.die("session journal unavailable".into());
                    return;
                };
                let assigned =
                    match journal.commit_session_step(session, revision, &next, &decisions, || {
                        self.clock.now()
                    }) {
                        Ok(assigned) => assigned,
                        Err(e) => {
                            drop(epochs);
                            self.die(format!("session checkpoint failed: {e}"));
                            return;
                        }
                    };
                self.session_revision = revision.checked_add(1);
                self.state = next;
                drop(epochs);
                for decision in &decisions {
                    self.record_session_decision(decision);
                }
                self.execute_session_refreshes(assigned, &rules);
                return;
            } else {
                self.state = next;
            }
            if !session_step
                && !self.journal_writable
                && self.journal.is_some()
                && !self.state.identity_bootstrap_hold
            {
                self.die("untrusted journal lineage cannot dispatch decisions".into());
                return;
            }
            if !session_step
                && self.journal_writable
                && let Some(j) = &mut self.journal
                && let Err(e) = j.checkpoint(&self.state)
            {
                if e.to_string().contains(LEASE_LOST) {
                    self.step_down("the journal belongs to the process that took the lease");
                    return;
                }
                self.die(format!("journal checkpoint failed: {e}"));
                return;
            }
            for d in &decisions {
                if let Decision::WakeAt { at } = d {
                    self.timers.insert(*at);
                    continue;
                }
                let side = is_side_effect(d);
                if side && !self.dispatch_is_current() {
                    self.die("identity or lease fence changed before side effect".into());
                    return;
                }
                let mut seq = None;
                if side && let Some(j) = &mut self.journal {
                    self.seq += 1;
                    if let Err(e) = j.intent(self.seq, d) {
                        if e.to_string().contains(LEASE_LOST) {
                            self.step_down(
                                "the journal belongs to the process that took the lease",
                            );
                            return;
                        }
                        self.die(format!("journal intent failed: {e}"));
                        return;
                    }
                    seq = Some(self.seq);
                }
                if matches!(d, Decision::Land { .. })
                    && let Some(h) = &self.pause_before_land
                {
                    h.pause_once();
                }
                if side && !self.dispatch_is_current() {
                    self.die("identity or lease fence changed before dispatch".into());
                    return;
                }
                self.forge.note(format!("decision: {}", describe(d)));
                self.log_decision(d);
                let repo = self.forge.api.repo.full();
                let trigger = if self.shadow.is_some() {
                    "Schlange engine (shadow: not carried out)"
                } else {
                    "Schlange engine"
                };
                if let Some(e) = crate::events::from_decision(&repo, d, now, trigger) {
                    self.shared.events.add(e);
                }
                let replies = match &mut self.shadow {
                    Some(sh) => sh.execute(d, &mut self.forge, now),
                    None => self.forge.execute(d),
                };
                if self.crash == Some(CrashPoint::AfterLand) && matches!(d, Decision::Land { .. }) {
                    self.die("injected crash after a landing (test)".into());
                    return;
                }
                if let (Some(seq), Some(j)) = (seq, &mut self.journal)
                    && let Err(e) = j.done(seq)
                {
                    self.die(format!("journal failed: {e}"));
                    return;
                }
                queue.extend(replies);
            }
        }
    }

    /// Reads the checks the rulesets require on the default branch and on every base with a
    /// queue (`branch_protection_injection_mode`), and tells the engine when they changed. The
    /// queue's own check is never taken over (it must not gate itself, W-05). A read that fails
    /// changes nothing.
    fn required_checks(&mut self) {
        let mut bases: BTreeSet<String> = self.state.trains.keys().cloned().collect();
        if let Ok(d) = self.forge.default_branch(Priority::Normal) {
            bases.insert(d);
        }
        for base in bases {
            match self.forge.api.branch_rules(&base, Priority::Normal) {
                Ok(rules) => {
                    let mut checks: Vec<String> = rules
                        .iter()
                        .flat_map(|r| r.required_checks())
                        .filter(|c| *c != self.forge.opts.check_name)
                        .collect();
                    checks.sort();
                    checks.dedup();
                    let known = self
                        .state
                        .required_checks
                        .get(&base)
                        .cloned()
                        .unwrap_or_default();
                    if checks != known {
                        self.forge.note(format!(
                            "required checks on {base}: {}",
                            if checks.is_empty() {
                                "none".into()
                            } else {
                                checks.join(", ")
                            }
                        ));
                        self.feed(Event::RequiredChecks { base, checks });
                    }
                }
                Err(e) => self.forge.note(format!(
                    "required checks of {base} not read (provisional): {e}"
                )),
            }
        }
    }

    /// Applies an operator's control through the engine; answers with the resulting object.
    fn control(&mut self, mut c: Control) -> Result<serde_json::Value, String> {
        if self.state.identity_bootstrap_hold {
            return Err("queue controls are unavailable while identity recovery is held".into());
        }
        if self.rules.is_none() {
            return Err("no valid configuration yet; the queue does not run".into());
        }
        match &mut c {
            Control::Freeze { freeze } => {
                let now = self.clock.now();
                if freeze.start == 0 {
                    freeze.start = now;
                }
                if freeze.end.is_some_and(|e| e <= now || e <= freeze.start) {
                    return Err("the freeze ends in the past or before it starts".into());
                }
                freeze.id = self.state.freezes.keys().max().copied().unwrap_or(0) + 1;
            }
            Control::Unfreeze { id } if !self.state.freezes.contains_key(id) => {
                return Err(format!("no freeze {id}"));
            }
            _ => {}
        }
        self.feed(Event::Control { control: c.clone() });
        if self.dead {
            return Err("the worker stopped".into());
        }
        self.shared.events.add(crate::events::from_control(
            &self.forge.api.repo.full(),
            &c,
            self.clock.now(),
            "admin API",
        ));
        self.publish();
        let rules = self.rules.clone().unwrap_or_default();
        let st = schlange_core::status::queue_status(&self.state, &rules);
        Ok(match c {
            Control::Pause { .. } | Control::Unpause => match st.pause {
                Some(p) => serde_json::to_value(p).unwrap_or_default(),
                None => serde_json::json!({ "paused": false }),
            },
            Control::Freeze { freeze } => st
                .freezes
                .iter()
                .find(|f| f.id == freeze.id)
                .and_then(|f| serde_json::to_value(f).ok())
                .unwrap_or_default(),
            Control::Unfreeze { id } => serde_json::json!({ "deleted": id }),
        })
    }

    fn publish(&self) {
        {
            let repo = self.forge.api.repo.full();
            let mut views = self.shared.watchdog_views();
            if watchdog_enabled(self.leading, self.dead, self.shadow.is_some()) {
                if let Some(rules) = &self.rules {
                    update_watchdog_view(
                        &mut views,
                        &self.shared.watchdog_generation,
                        &repo,
                        &self.state,
                        rules,
                        watchdog_fingerprint(&self.state, rules),
                    );
                } else {
                    views.remove(&repo);
                }
            } else {
                views.remove(&repo);
            }
        }
        let mut state = serde_json::to_value(&self.state).unwrap_or_default();
        if let Some(o) = state.get_mut("outcomes").and_then(|o| o.as_object_mut()) {
            let extra = o.len().saturating_sub(STATUS_OUTCOMES);
            let old: Vec<String> = o.keys().take(extra).cloned().collect();
            for k in old {
                o.remove(&k);
            }
        }
        let merged: Vec<u64> = self
            .state
            .outcomes
            .iter()
            .filter(|(_, o)| matches!(o, schlange_core::engine::Outcome::Merged { .. }))
            .map(|(n, _)| *n)
            .collect();
        let st = WorkerStatus {
            repo: self.forge.api.repo.full(),
            queued: self.state.queued.keys().copied().collect(),
            merged: merged[merged.len().saturating_sub(STATUS_OUTCOMES)..].to_vec(),
            halted: self
                .state
                .trains
                .iter()
                .filter_map(|(b, t)| t.halted.clone().map(|h| (b.clone(), h)))
                .collect(),
            cars: self.state.trains.values().map(|t| t.cars.len()).sum(),
            last_reconcile: self.last_reconcile,
            reconciliations: self.reconciliations,
            webhooks: self.webhooks,
            pending: self.forge.pending_summary(),
            cosmetics_waiting: self.forge.cosmetic_count(),
            config_error: self.config_error.clone(),
            config_warnings: self.config_warnings.clone(),
            recovered: self.recovered.clone(),
            persistent: self.journal.is_some(),
            log_tail: self
                .forge
                .log
                .iter()
                .rev()
                .take(40)
                .rev()
                .cloned()
                .collect(),
            state,
            queue: self
                .rules
                .as_ref()
                .map(|r| {
                    serde_json::to_value(schlange_core::status::queue_status(&self.state, r))
                        .unwrap_or_default()
                })
                .unwrap_or_default(),
            role: if self.shadow.is_some() {
                "shadow"
            } else if self.lease.is_none() {
                "single"
            } else if self.leading {
                "leader"
            } else {
                "follower"
            }
            .into(),
            lease: self.lease.as_ref().map(|l| l.status()),
            shadow_suppressed: self.shadow.as_ref().map(|s| s.suppressed).unwrap_or(0),
            writes_refused: self.forge.api.client.stats().writes_refused,
        };
        if let Some(sh) = &self.shadow
            && sh.facts().len() != self.shadow_published.get()
        {
            self.shadow_published.set(sh.facts().len());
            self.shared
                .shadow
                .lock()
                .expect("shadow")
                .insert(st.repo.clone(), sh.facts().to_vec());
        }
        self.shared
            .statuses
            .lock()
            .expect("status")
            .insert(st.repo.clone(), st);
    }
}

fn control_after_hold(leading: bool, dead: bool, held: bool) -> Result<(), String> {
    if dead {
        return Err("queue control unavailable: worker stopped during recovery".into());
    }
    if !leading {
        return Err("this process does not hold the repository's lease; ask the leader".into());
    }
    if held {
        return Err("queue controls are unavailable while identity recovery is held".into());
    }
    Ok(())
}

fn session_presentation(decision: &Decision) -> bool {
    matches!(
        decision,
        Decision::Cosmetic { .. }
            | Decision::Enqueue { .. }
            | Decision::Dequeue { .. }
            | Decision::ChecksEnded { .. }
            | Decision::Merged { .. }
    )
}

fn describe(d: &Decision) -> String {
    serde_json::to_string(d).unwrap_or_else(|_| format!("{d:?}"))
}

/// Whether a comment is a `queue` command (or its alias `requeue`): the commands that vouch
/// for the commits of a pull request.
fn is_queue_command(body: &str) -> bool {
    use schlange_core::commands::{Name, Parsed, parse};
    matches!(
        parse(body),
        Parsed::Command {
            name: Name::Queue | Name::Requeue,
            ..
        }
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn build_attempts_bind_stage_path_and_exact_payload_once() {
        use schlange_core::engine::CarPr;
        let decision = Decision::BuildCar {
            car: 1,
            queue: "default".into(),
            base_branch: "main".into(),
            branch: "queue/a".into(),
            base_sha: "base".into(),
            prs: vec![CarPr {
                number: 7,
                head_sha: "head".into(),
            }],
            own: vec![7],
            stack_on: None,
        };
        let plan = BuildAttempts::new("o/r", &decision, "http://test", 1, "bot").unwrap();
        let create = serde_json::json!({"ref":"refs/heads/queue/a", "sha":"base"}).to_string();
        let wrong = serde_json::json!({"ref":"refs/heads/main", "sha":"base"}).to_string();
        assert!(
            plan.permit("POST", "/repos/o/r/git/refs", Some(wrong.as_bytes()))
                .is_err()
        );
        let wrong_sha = serde_json::json!({"ref":"refs/heads/queue/a", "sha":"other"}).to_string();
        assert!(
            plan.permit("POST", "/repos/o/r/git/refs", Some(wrong_sha.as_bytes()))
                .is_err()
        );
        assert!(
            plan.permit(
                "POST",
                "/repos/o/r/git/refs",
                Some(br#"{"ref":"refs/heads/queue/a","ref":"refs/heads/queue/a","sha":"base"}"#)
            )
            .is_err()
        );
        assert!(plan.permit("POST", "/repos/o/r/git/refs", None).is_err());
        assert!(
            plan.permit("POST", "/repos/o/r/git/refs", Some(create.as_bytes()))
                .is_ok()
        );
        assert!(
            plan.permit("POST", "/repos/o/r/git/refs", Some(create.as_bytes()))
                .is_err(),
            "same stage cannot be resent"
        );
        let merge = serde_json::json!({"base":"queue/a", "head":"head", "commit_message":"Merge #7 into queue/a"}).to_string();
        assert!(
            plan.permit("POST", "/repos/o/r/merges", Some(merge.as_bytes()))
                .is_err(),
            "an in-flight create cannot authorize a merge"
        );
        plan.observe_response("POST", "/repos/o/r/git/refs", 201, b"{}")
            .unwrap();
        assert!(
            plan.permit("POST", "/repos/o/r/merges", Some(merge.as_bytes()))
                .is_err(),
            "status alone must not complete a stage"
        );
        plan.confirm().unwrap();
        assert!(
            plan.permit("POST", "/repos/o/other/merges", Some(merge.as_bytes()))
                .is_err()
        );
        assert!(
            plan.permit("POST", "/repos/o/r/merges", Some(merge.as_bytes()))
                .is_ok()
        );
        assert!(
            plan.permit("POST", "/repos/o/r/merges", Some(merge.as_bytes()))
                .is_err()
        );
        plan.observe_response("POST", "/repos/o/r/merges", 201, b"{}")
            .unwrap();
        plan.confirm().unwrap();
        let draft = serde_json::json!({"title":"merge queue: embarking #7", "head":"queue/a", "base":"main", "body":"Schlange tests these pull requests together with `main` at base:\n\n- #7 at head\n\n## Datenschutz\n\nSee the constituent pull requests for their data protection details.\n\nDo not edit this pull request; the queue closes it.", "draft":true}).to_string();
        assert!(
            plan.permit("POST", "/repos/o/r/pulls", Some(draft.as_bytes()))
                .is_ok()
        );
        plan.observe_response("POST", "/repos/o/r/pulls", 201, b"{}")
            .unwrap();
        plan.confirm().unwrap();
        assert!(
            plan.permit("POST", "/repos/o/r/pulls", Some(draft.as_bytes()))
                .is_err(),
            "completed plan cannot be replayed"
        );
        let failed = BuildAttempts::new("o/r", &decision, "http://test", 1, "bot").unwrap();
        failed
            .permit("POST", "/repos/o/r/git/refs", Some(create.as_bytes()))
            .unwrap();
        assert!(
            failed
                .observe_response("POST", "/repos/o/r/git/refs", 401, b"{}")
                .is_err()
        );
        assert!(
            failed
                .permit("POST", "/repos/o/r/git/refs", Some(create.as_bytes()))
                .is_err(),
            "401 cannot replay the uncertain write"
        );
        assert!(
            failed
                .permit("POST", "/repos/o/r/merges", Some(merge.as_bytes()))
                .is_err()
        );
        let concurrent =
            Arc::new(BuildAttempts::new("o/r", &decision, "http://test", 1, "bot").unwrap());
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let plan = concurrent.clone();
                let barrier = barrier.clone();
                let body = create.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    plan.permit("POST", "/repos/o/r/git/refs", Some(body.as_bytes()))
                        .is_ok()
                })
            })
            .collect();
        barrier.wait();
        let successes = threads
            .into_iter()
            .map(|t| t.join().unwrap())
            .filter(|ok| *ok)
            .count();
        assert_eq!(
            successes, 1,
            "concurrent clones cannot authorize the same stage"
        );
    }
    #[test]
    fn session_presentation_includes_merged_notification() {
        let merged = schlange_core::Decision::Merged {
            number: 1,
            queue: "default".into(),
            sha: "head".into(),
        };
        assert!(super::session_presentation(&merged));
    }
    #[test]
    fn failed_bootstrap_hold_must_not_acknowledge_control() {
        assert!(super::control_after_hold(true, true, false).is_err());
        assert!(super::control_after_hold(true, false, true).is_err());
        assert!(super::control_after_hold(false, false, false).is_err());
        assert!(super::control_after_hold(true, false, false).is_ok());
    }
    use super::*;

    #[test]
    fn watchdog_view_ignores_cosmetic_churn_but_not_a_pause() {
        let rules = Rules::default();
        let state = State::default();
        let original = watchdog_fingerprint(&state, &rules);
        let mut cosmetic = state.clone();
        cosmetic.now = 500;
        cosmetic
            .cosmetics
            .status
            .insert(17, "status changed".into());
        assert_eq!(watchdog_fingerprint(&cosmetic, &rules), original);
        cosmetic.pause = Some(schlange_core::engine::Pause {
            reason: "maintenance".into(),
            at: 500,
            by: None,
        });
        assert_ne!(watchdog_fingerprint(&cosmetic, &rules), original);
    }

    #[test]
    fn invalid_watchdog_fingerprint_removes_view_and_prevents_aba() {
        let rules = Rules::default();
        let state = State::default();
        let mut views = BTreeMap::new();
        let generation = AtomicU64::new(0);
        update_watchdog_view(
            &mut views,
            &generation,
            "o/r",
            &state,
            &rules,
            watchdog_fingerprint(&state, &rules),
        );
        let first = views["o/r"].generation;
        update_watchdog_view(&mut views, &generation, "o/r", &state, &rules, None);
        assert!(!views.contains_key("o/r"));
        update_watchdog_view(
            &mut views,
            &generation,
            "o/r",
            &state,
            &rules,
            watchdog_fingerprint(&state, &rules),
        );
        assert_ne!(views["o/r"].generation, first);
    }

    #[test]
    fn watchdog_observes_only_a_live_non_shadow_leader() {
        assert!(watchdog_enabled(true, false, false));
        assert!(!watchdog_enabled(false, false, false));
        assert!(!watchdog_enabled(true, true, false));
        assert!(!watchdog_enabled(true, false, true));
    }

    #[test]
    fn heartbeat_alarm_clears_when_worker_resumes() {
        let now = std::time::Instant::now();
        let mut beats = BTreeMap::new();
        beats.insert("o/r".to_string(), now);
        let limit = Duration::from_secs(60);
        assert!(stale_workers(&beats, now + limit, limit).is_empty());
        assert_eq!(
            stale_workers(&beats, now + limit + Duration::from_secs(1), limit),
            vec!["o/r"]
        );
        beats.insert("o/r".to_string(), now + limit + Duration::from_secs(1));
        assert!(stale_workers(&beats, now + limit + Duration::from_secs(1), limit).is_empty());
    }

    #[test]
    fn health_endpoint_reports_and_clears_stale_worker() {
        let shared = Arc::new(Shared {
            statuses: Mutex::new(BTreeMap::new()),
            decisions: Mutex::new(BTreeMap::new()),
            dead: Mutex::new(BTreeMap::new()),
            heartbeat: Mutex::new(BTreeMap::new()),
            heartbeat_limit: Duration::from_secs(60),
            heartbeat_test_offset: None,
            watchdog_views: Mutex::new(BTreeMap::new()),
            watchdog_generation: AtomicU64::new(0),
            ingress_epochs: Mutex::new(BTreeMap::new()),
            watchdog_memo_total: Arc::new(AtomicUsize::new(0)),
            watchdog_errors: Mutex::new(BTreeMap::new()),
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            events: Arc::new(EventLog::new(None)),
            shadow: Mutex::new(BTreeMap::new()),
            store: None,
        });
        let router = Router {
            secret: vec![],
            dedup: Mutex::new(Dedup::new(10)),
            store: None,
            sinks: HashMap::new(),
            shared: shared.clone(),
            #[cfg(test)]
            after_seen_test_barrier: None,
        };
        let request = Request {
            method: "GET".into(),
            url: "/healthz".into(),
            headers: vec![],
            body: vec![],
        };
        shared.beat("o/r");
        assert_eq!(route(&request, &router, &shared, None).status, 200);
        shared
            .heartbeat
            .lock()
            .unwrap()
            .insert("o/r".into(), Instant::now() - Duration::from_secs(61));
        let alarm = route(&request, &router, &shared, None);
        assert_eq!(alarm.status, 503);
        assert!(
            String::from_utf8(alarm.body)
                .unwrap()
                .contains("worker o/r heartbeat stale")
        );
        shared.beat("o/r");
        assert_eq!(route(&request, &router, &shared, None).status, 200);
        supervise_watchdog(&shared, "o/r", || {
            let _guard = shared.watchdog_views.lock().unwrap();
            panic!("injected observer panic");
        });
        assert!(
            shared.watchdog_views().is_empty(),
            "worker can still publish after observer panic"
        );
        let alarm = route(&request, &router, &shared, None);
        assert_eq!(alarm.status, 503);
        assert!(
            String::from_utf8(alarm.body)
                .unwrap()
                .contains("watchdog o/r: observer panicked")
        );
    }

    #[test]
    fn token_scope_asks_only_for_granted_needed_permissions() {
        let granted: BTreeMap<String, String> = [
            ("metadata", "read"),
            ("contents", "write"),
            ("pull_requests", "write"),
            ("checks", "write"),
            ("statuses", "read"),
            ("administration", "read"),
        ]
        .into_iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let p = token_permissions(&granted, false);
        assert_eq!(p.get("administration").map(String::as_str), Some("read"));
        assert!(!p.contains_key("actions"));
        assert!(!p.contains_key("workflows"));
        assert_eq!(p.get("contents").map(String::as_str), Some("write"));
        // Shadow mode asks for read access only, even from an app that could write.
        let p = token_permissions(&granted, true);
        assert_eq!(p.get("contents").map(String::as_str), Some("read"));
        assert_eq!(p.get("pull_requests").map(String::as_str), Some("read"));
        assert!(p.values().all(|l| l == "read"), "{p:?}");
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"a", b"ab"));
    }
    #[test]
    fn accepted_webhook_advances_only_its_repository_ingress_epoch_before_delivery() {
        let shared = Arc::new(Shared {
            statuses: Mutex::new(BTreeMap::new()),
            decisions: Mutex::new(BTreeMap::new()),
            dead: Mutex::new(BTreeMap::new()),
            heartbeat: Mutex::new(BTreeMap::new()),
            heartbeat_limit: Duration::from_secs(60),
            heartbeat_test_offset: None,
            watchdog_views: Mutex::new(BTreeMap::new()),
            watchdog_generation: AtomicU64::new(0),
            watchdog_memo_total: Arc::new(AtomicUsize::new(0)),
            watchdog_errors: Mutex::new(BTreeMap::new()),
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            events: Arc::new(EventLog::new(None)),
            shadow: Mutex::new(BTreeMap::new()),
            store: None,
            ingress_epochs: Mutex::new(BTreeMap::new()),
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let secret = b"test webhook secret long enough".to_vec();
        let router = Router {
            secret: secret.clone(),
            dedup: Mutex::new(Dedup::new(10)),
            store: None,
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: shared.clone(),
            #[cfg(test)]
            after_seen_test_barrier: None,
        };
        let body = br#"{"repository":{"full_name":"o/r"},"ref":"refs/heads/main","after":"abcd","deleted":false}"#;
        let signature = crate::webhook::sign(&secret, body);
        let header = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("push".into()),
            "x-github-delivery" => Some("one".into()),
            _ => None,
        };
        assert_eq!(router.handle(&header, body).0, 202);
        assert!(matches!(rx.try_recv(), Ok(Msg::Webhook(_, 1))));
        assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&1));
        assert!(!ingress_valid(&shared, "o/r", 0, 0));
        assert!(!ingress_valid(&shared, "o/r", 1, 0));
        assert!(ingress_valid(&shared, "o/r", 1, 1));
        assert!(
            !shared
                .ingress_epochs
                .lock()
                .unwrap()
                .contains_key("other/repo")
        );
        assert_eq!(router.handle(&header, body).0, 200);
        assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&1));
        let ignored = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("repository".into()),
            "x-github-delivery" => Some("two".into()),
            _ => None,
        };
        assert_eq!(router.handle(&ignored, body).0, 202);
        assert!(matches!(rx.try_recv(), Ok(Msg::Webhook(_, 2))));
        assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&2));
        let malformed = br#"{"repository":{"full_name":"o/r"}}"#;
        let sig = crate::webhook::sign(&secret, malformed);
        let bad_header = |key: &str| match key {
            "x-hub-signature-256" => Some(sig.clone()),
            "x-github-event" => Some("push".into()),
            "x-github-delivery" => Some("three".into()),
            _ => None,
        };
        assert_eq!(router.handle(&bad_header, malformed).0, 400);
        assert!(matches!(rx.try_recv(), Ok(Msg::ReconcileEpoch(3))));
        assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&3));
        std::thread::scope(|scope| {
            let req = Request {
                method: "PUT".into(),
                url: "/repos/o/r/merge-queue/pause".into(),
                headers: vec![],
                body: br#"{"reason":"maintenance"}"#.to_vec(),
            };
            let router_ref = &router;
            let shared_ref = &shared;
            let job = scope.spawn(move || {
                repo_api(&req, router_ref, shared_ref, "/repos/o/r/merge-queue/pause")
            });
            let Msg::Control(_, reply, epoch) = rx.recv_timeout(Duration::from_secs(2)).unwrap()
            else {
                panic!("expected control");
            };
            assert_eq!(epoch, 4);
            assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&4));
            reply.send(Ok(serde_json::Value::Null)).unwrap();
            assert_eq!(job.join().unwrap().status, 200);
        });
        shared
            .ingress_epochs
            .lock()
            .unwrap()
            .insert("o/r".into(), u64::MAX);
        let overflow = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("push".into()),
            "x-github-delivery" => Some("four".into()),
            _ => None,
        };
        assert_eq!(router.handle(&overflow, body).0, 503);
        assert!(shared.stop.load(Ordering::SeqCst));
        assert!(rx.try_recv().is_err());
    }

    fn inbox_test_shared() -> Arc<Shared> {
        Arc::new(Shared {
            statuses: Mutex::new(BTreeMap::new()),
            decisions: Mutex::new(BTreeMap::new()),
            dead: Mutex::new(BTreeMap::new()),
            heartbeat: Mutex::new(BTreeMap::new()),
            heartbeat_limit: Duration::from_secs(60),
            heartbeat_test_offset: None,
            watchdog_views: Mutex::new(BTreeMap::new()),
            watchdog_generation: AtomicU64::new(0),
            watchdog_memo_total: Arc::new(AtomicUsize::new(0)),
            watchdog_errors: Mutex::new(BTreeMap::new()),
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
            events: Arc::new(EventLog::new(None)),
            shadow: Mutex::new(BTreeMap::new()),
            store: None,
            ingress_epochs: Mutex::new(BTreeMap::new()),
        })
    }

    #[test]
    fn edited_checkbox_ack_survives_router_crash_before_worker_processing() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-inbox-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let secret = b"test webhook secret long enough".to_vec();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let signature = crate::webhook::sign(&secret, body);
        let header = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("issue_comment".into()),
            "x-github-delivery" => Some("checkbox-1".into()),
            _ => None,
        };
        let router = Router {
            secret: secret.clone(),
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: None,
        };
        assert_eq!(router.handle(&header, body).0, 202);
        drop(rx); // process dies before the queued message is handled
        drop(router);
        let reopened = Store::open(&dir).unwrap();
        let pending = reopened.pending_inbox("o/r", "initial").unwrap();
        if pending.is_empty() {
            // The legacy path persists only the GUID; a restarted router suppresses GitHub's
            // redelivery even though the worker never processed the editor-bearing payload.
            assert!(reopened.delivery_seen("checkbox-1").unwrap());
            let (tx, rx) = std::sync::mpsc::channel();
            let restarted = Router {
                secret,
                dedup: Mutex::new(Dedup::new(10)),
                store: Some(reopened.clone()),
                sinks: HashMap::from([("o/r".into(), tx)]),
                shared: inbox_test_shared(),
                after_seen_test_barrier: None,
            };
            assert_eq!(restarted.handle(&header, body).0, 200);
            assert!(rx.try_recv().is_err());
        }
        assert_eq!(
            pending.len(),
            1,
            "an acknowledged edit must remain replayable after worker crash"
        );
        let parsed = crate::webhook::parse(&pending[0].event, &pending[0].body).unwrap();
        assert!(parsed.hints.iter().any(|h| matches!(h, Hint::Comment(c) if c.author == "schlange[bot]" && c.sender == "alice" && c.edited && c.id == 77)));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn concurrent_duplicate_checkbox_webhooks_are_enqueued_only_once() {
        let (tx, rx) = std::sync::mpsc::channel();
        let secret = b"test webhook secret long enough".to_vec();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let signature = crate::webhook::sign(&secret, body);
        let router = Arc::new(Router {
            secret,
            dedup: Mutex::new(Dedup::new(10)),
            store: None,
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: Some(Arc::new(std::sync::Barrier::new(2))),
        });
        std::thread::scope(|scope| {
            let send = || {
                let header = |key: &str| match key {
                    "x-hub-signature-256" => Some(signature.clone()),
                    "x-github-event" => Some("issue_comment".into()),
                    "x-github-delivery" => Some("checkbox-2".into()),
                    _ => None,
                };
                router.handle(&header, body).0
            };
            let a = scope.spawn(send);
            let b = scope.spawn(send);
            let mut statuses = [a.join().unwrap(), b.join().unwrap()];
            statuses.sort();
            assert_eq!(statuses, [200, 202], "one accepted and one duplicate");
        });
        assert_eq!(
            rx.try_iter().count(),
            1,
            "duplicate delivery must be enqueued once"
        );
    }
    #[test]
    fn store_backed_concurrent_duplicate_checkboxes_are_claimed_once() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-concurrent-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let secret = b"test webhook secret long enough".to_vec();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let signature = crate::webhook::sign(&secret, body);
        let router = Arc::new(Router {
            secret,
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: Some(Arc::new(std::sync::Barrier::new(2))),
        });
        std::thread::scope(|scope| {
            let send = || {
                let header = |key: &str| match key {
                    "x-hub-signature-256" => Some(signature.clone()),
                    "x-github-event" => Some("issue_comment".into()),
                    "x-github-delivery" => Some("checkbox-2".into()),
                    _ => None,
                };
                router.handle(&header, body).0
            };
            let a = scope.spawn(send);
            let b = scope.spawn(send);
            let mut statuses = [a.join().unwrap(), b.join().unwrap()];
            statuses.sort();
            assert_eq!(statuses, [200, 202], "one accepted and one duplicate");
        });
        assert_eq!(
            rx.try_iter().count(),
            1,
            "duplicate delivery must be enqueued once"
        );
        assert_eq!(store.pending_inbox("o/r", "initial").unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn durable_claim_failure_invalidates_ingress_before_503() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-claim-error-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let db = rusqlite::Connection::open(dir.join(crate::store::DB_FILE)).unwrap();
        db.execute_batch("CREATE TRIGGER reject_inbox BEFORE INSERT ON inbox BEGIN SELECT RAISE(ABORT, 'injected'); END;").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = inbox_test_shared();
        let secret = b"test webhook secret long enough".to_vec();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let signature = crate::webhook::sign(&secret, body);
        let header = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("issue_comment".into()),
            "x-github-delivery" => Some("failure".into()),
            _ => None,
        };
        let router = Router {
            secret,
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: shared.clone(),
            after_seen_test_barrier: None,
        };
        assert_eq!(router.handle(&header, body).0, 503);
        assert_eq!(
            shared.ingress_epochs.lock().unwrap().get("o/r"),
            Some(&1),
            "failed claim must invalidate prior evidence"
        );
        assert!(
            shared.stop.load(Ordering::SeqCst),
            "failed claim must fail-stop processing"
        );
        assert!(rx.try_recv().is_err());
        assert!(store.pending_inbox("o/r", "initial").unwrap().is_empty());
        db.execute_batch("DROP TRIGGER reject_inbox").unwrap();
        assert_eq!(
            router.handle(&header, body).0,
            503,
            "stopped router cannot acknowledge a later retry"
        );
        assert!(store.pending_inbox("o/r", "initial").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn failed_wake_keeps_durable_edit_and_duplicate_can_wake_successor() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-wake-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        drop(rx);
        let secret = b"test webhook secret long enough".to_vec();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let signature = crate::webhook::sign(&secret, body);
        let header = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("issue_comment".into()),
            "x-github-delivery" => Some("wake-failure".into()),
            _ => None,
        };
        let first = Router {
            secret: secret.clone(),
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: None,
        };
        assert_eq!(first.handle(&header, body).0, 503);
        assert_eq!(store.pending_inbox("o/r", "initial").unwrap().len(), 1);
        drop(first);
        let (tx, rx) = std::sync::mpsc::channel();
        let successor = Router {
            secret,
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: None,
        };
        assert_eq!(successor.handle(&header, body).0, 202);
        assert!(matches!(rx.try_recv(), Ok(Msg::ReconcileEpoch(1))));
        assert_eq!(
            store.pending_inbox("o/r", "initial").unwrap().len(),
            1,
            "wake must not mark the event processed"
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn startup_pending_wake_preserves_original_checkbox_payload() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-startup-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#.to_vec();
        let item = InboxDelivery {
            repo: "o/r".into(),
            epoch: "old-session".into(),
            guid: "guid".into(),
            event: "issue_comment".into(),
            body,
            received_at: 1,
        };
        store.accept_inbox(&item, 10).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = inbox_test_shared();
        let router = Router {
            secret: vec![],
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: shared.clone(),
            after_seen_test_barrier: None,
        };
        router.wake_pending().unwrap();
        assert!(matches!(rx.try_recv(), Ok(Msg::ReconcileEpoch(1))));
        assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&1));
        assert_eq!(
            store.pending_inbox("o/r", "old-session").unwrap(),
            vec![item]
        );
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn same_delivery_id_with_different_authenticated_payload_fails_closed() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-collision-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let shared = inbox_test_shared();
        let secret = b"test webhook secret long enough".to_vec();
        let router = Router {
            secret: secret.clone(),
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: shared.clone(),
            after_seen_test_barrier: None,
        };
        let first = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let altered = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"mallory"}}"#;
        let header = |body: &[u8]| {
            let signature = crate::webhook::sign(&secret, body);
            move |key: &str| match key {
                "x-hub-signature-256" => Some(signature.clone()),
                "x-github-event" => Some("issue_comment".into()),
                "x-github-delivery" => Some("same-guid".into()),
                _ => None,
            }
        };
        assert_eq!(router.handle(&header(first), first).0, 202);
        assert!(matches!(rx.try_recv(), Ok(Msg::ReconcileEpoch(1))));
        assert_eq!(router.handle(&header(altered), altered).0, 503);
        assert!(shared.stop.load(Ordering::SeqCst));
        assert_eq!(shared.ingress_epochs.lock().unwrap().get("o/r"), Some(&2));
        let pending = store.pending_inbox("o/r", "initial").unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].body, first);
        let _ = std::fs::remove_dir_all(dir);
    }
    #[test]
    fn processed_durable_duplicate_is_not_reenqueued_after_restart() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "schlange-router-processed-{}-{nonce}",
            std::process::id()
        ));
        let store = Store::open(&dir).unwrap();
        let secret = b"test webhook secret long enough".to_vec();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#;
        let signature = crate::webhook::sign(&secret, body);
        let header = |key: &str| match key {
            "x-hub-signature-256" => Some(signature.clone()),
            "x-github-event" => Some("issue_comment".into()),
            "x-github-delivery" => Some("done".into()),
            _ => None,
        };
        let (tx, rx) = std::sync::mpsc::channel();
        let first = Router {
            secret: secret.clone(),
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: None,
        };
        assert_eq!(first.handle(&header, body).0, 202);
        assert!(matches!(rx.try_recv(), Ok(Msg::ReconcileEpoch(1))));
        let item = store.pending_inbox("o/r", "initial").unwrap().remove(0);
        assert!(store.lease_acquire("o/r", "a", 100, 130).unwrap());
        store
            .journal("o/r")
            .fenced("a")
            .commit_inbox_step(&item, 0, &State::default(), &[], || 101)
            .unwrap();
        drop(first);
        let (tx, rx) = std::sync::mpsc::channel();
        let successor = Router {
            secret,
            dedup: Mutex::new(Dedup::new(10)),
            store: Some(store.clone()),
            sinks: HashMap::from([("o/r".into(), tx)]),
            shared: inbox_test_shared(),
            after_seen_test_barrier: None,
        };
        assert_eq!(successor.handle(&header, body).0, 200);
        assert!(
            rx.try_recv().is_err(),
            "processed delivery must not wake as pending"
        );
        assert!(!store.has_pending_inbox("o/r").unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
