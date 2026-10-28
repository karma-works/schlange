//! `schlange serve`: the webhook endpoint, one engine worker per repository, reconciliation,
//! persistence.
//!
//! - **Listener** ([`crate::http`]): a small thread pool with read timeouts and deadlines, so a
//!   slow client never stalls webhooks or `/healthz`. `POST /webhook` verifies the signature
//!   (the secret must be at least [`MIN_SECRET_LEN`] bytes), drops delivery ids it already
//!   handled, parses the payload and hands the hints to the repository's worker; it answers
//!   202 at once. A delivery id is recorded only once the delivery was accepted: a delivery
//!   answered 400 or 503 is redelivered by GitHub and handled then. `GET /healthz` answers
//!   `ok` (503 when a worker stopped). `GET /status` needs `Authorization: Bearer <admin
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

use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use schlange_config::rules::Rules;
use schlange_core::engine::{Control, Freeze};
use schlange_core::journal::{Journal, is_side_effect};
use schlange_core::model::Time;
use schlange_core::{Decision, Event, State, step};

use crate::api::{RepoApi, RepoName};
use crate::client::{Client, Priority};
use crate::config::{ConfigCheck, Found, read_from_default_branch};
use crate::forge::{ForgeOptions, GithubForge};
use crate::http::{Handler, Limits, Request, Response, Server};
use crate::store::{SqliteJournal, Store};
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

/// A crash injected for tests: the worker stops dead at this point (no reply, no journal
/// entry after it), as a killed process would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashPoint {
    /// Right after the merge calls of a landing returned, before the reply reaches the engine.
    AfterLand,
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
}

pub enum Msg {
    Webhook(Box<Parsed>),
    /// A full reconciliation now.
    Reconcile,
    Shutdown,
    /// An operator's queue control from the admin API (pause, freeze); the worker answers
    /// once the engine applied it (and the state was journaled).
    Control(Control, Sender<Result<serde_json::Value, String>>),
}

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
}

struct Shared {
    statuses: Mutex<BTreeMap<String, WorkerStatus>>,
    decisions: Mutex<BTreeMap<String, VecDeque<String>>>,
    /// Workers that stopped, with the reason (health check).
    dead: Mutex<BTreeMap<String, String>>,
    /// Messages handed to workers and not yet handled (tests wait for 0).
    inflight: AtomicUsize,
    stop: AtomicBool,
}

/// Verifies, deduplicates and routes deliveries.
pub struct Router {
    secret: Vec<u8>,
    dedup: Mutex<Dedup>,
    store: Option<Store>,
    sinks: HashMap<String, Sender<Msg>>,
    shared: Arc<Shared>,
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

    /// Handles one delivery; returns the HTTP status and a short text. The delivery id is
    /// recorded only when the delivery was handled (2xx).
    pub fn handle(&self, header: &dyn Fn(&str) -> Option<String>, body: &[u8]) -> (u16, String) {
        if !verify_signature(&self.secret, body, header("x-hub-signature-256").as_deref()) {
            return (401, "bad signature".into());
        }
        let Some(event) = header("x-github-event") else {
            return (400, "no X-GitHub-Event".into());
        };
        let Some(id) = header("x-github-delivery") else {
            return (400, "no X-GitHub-Delivery".into());
        };
        if self.seen(&id) {
            return (200, "duplicate delivery".into());
        }
        let parsed = match parse(&event, body) {
            Ok(p) => p,
            Err(e) => return (400, e),
        };
        if parsed.hints.is_empty() {
            self.record(&id);
            return (200, "ignored".into());
        }
        let Some(sink) = parsed.repo.as_ref().and_then(|r| self.sinks.get(r)) else {
            self.record(&id);
            return (200, "repository not served".into());
        };
        self.shared.inflight.fetch_add(1, Ordering::SeqCst);
        if sink.send(Msg::Webhook(Box::new(parsed))).is_err() {
            self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
            return (503, "worker stopped".into());
        }
        self.record(&id);
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
}

/// The token scope for one installation: these repositories, and of Schlange's permissions
/// the ones the installation grants (asking for more fails).
fn token_permissions(granted: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let rank = |l: &str| match l {
        "admin" => 3,
        "write" => 2,
        "read" => 1,
        _ => 0,
    };
    crate::permissions::PERMISSIONS
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
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
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
            let e = installs
                .entry(inst.id)
                .or_insert_with(|| (Vec::new(), token_permissions(&inst.permissions)));
            e.0.push(repo.name.clone());
            repo_inst.push((repo.clone(), inst.id));
        }
        for (id, (names, perms)) in &installs {
            cfg.client.set_token_scope(*id, names, perms);
        }
        let mut sinks = HashMap::new();
        let mut threads = Vec::new();
        let mut senders = Vec::new();
        for (repo, installation) in repo_inst {
            let api = RepoApi {
                client: cfg.client.clone(),
                repo: repo.clone(),
                installation,
            };
            let mut forge = GithubForge::new(api, clock.clone(), opts.clone());
            forge.retry_base = cfg.retry_base;
            forge.retry_max = cfg.retry_max;
            forge.store = store.clone();
            forge.check = Some(cfg.check.clone());
            let journal = store.as_ref().map(|s| s.journal(&repo.full()));
            let state = State {
                host: schlange_core::engine::Host {
                    epoch: 0,
                    bot: opts.bot_login.clone(),
                    repository: repo.full(),
                },
                ..State::default()
            };
            let (tx, rx) = channel();
            sinks.insert(repo.full(), tx.clone());
            senders.push(tx);
            let mut worker = Worker {
                forge,
                clock: clock.clone(),
                state,
                rules: None,
                config_key: None,
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
                shared: shared.clone(),
                poll: cfg.poll,
                journal,
                seq: 0,
                restart_pending: false,
                recovered: None,
                crash: cfg.crash,
                dead: false,
            };
            worker.recover()?;
            threads.push(
                std::thread::Builder::new()
                    .name(format!("worker {}", repo.full()))
                    .spawn(move || worker.run())
                    .map_err(|e| e.to_string())?,
            );
        }
        let router = Arc::new(Router {
            secret: cfg.webhook_secret.clone(),
            dedup: Mutex::new(Dedup::new(10_000)),
            store: store.clone(),
            sinks,
            shared: shared.clone(),
        });
        let sh = shared.clone();
        let admin = cfg.admin_token.clone();
        let handler: Handler =
            Arc::new(move |req: &Request| route(req, &router, &sh, admin.as_deref()));
        let server = Server::start(cfg.listen, cfg.http.clone(), handler)
            .map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?;
        Ok(Service {
            addr: server.addr,
            shared,
            sinks: senders,
            threads,
            server: Some(server),
            store,
        })
    }

    pub fn webhook_url(&self) -> String {
        format!("http://{}/webhook", self.addr)
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
        shared.inflight.fetch_add(1, Ordering::SeqCst);
        if sink.send(Msg::Control(c, tx)).is_err() {
            shared.inflight.fetch_sub(1, Ordering::SeqCst);
            return Response::text(503, "worker stopped");
        }
        match rx.recv_timeout(Duration::from_secs(30)) {
            Ok(Ok(v)) => Response::json(200, v.to_string()),
            Ok(Err(e)) => Response::text(422, e),
            Err(_) => Response::text(503, "the worker did not answer in time"),
        }
    };
    let queue = status.map(|s| s.queue).unwrap_or(serde_json::Value::Null);
    match (req.method.as_str(), rest) {
        ("GET", ["merge-queue", "status"]) => Response::json(200, queue.to_string()),
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
            if dead.is_empty() {
                Response::text(200, "ok")
            } else {
                Response::text(
                    503,
                    dead.iter()
                        .map(|(r, why)| format!("worker {r} stopped: {why}"))
                        .collect::<Vec<_>>()
                        .join("\n"),
                )
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

struct Worker {
    forge: GithubForge,
    clock: Arc<dyn Clock>,
    state: State,
    rules: Option<Rules>,
    /// The configuration text last looked at (valid or not), to validate only on change.
    config_key: Option<String>,
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
    shared: Arc<Shared>,
    poll: Duration,
    journal: Option<SqliteJournal>,
    seq: u64,
    /// `Event::Restarted` is fed once the configuration is loaded.
    restart_pending: bool,
    recovered: Option<Vec<String>>,
    crash: Option<CrashPoint>,
    dead: bool,
    /// Comments whose sender's permission could not be read yet (retried each round).
    comments_waiting: Vec<CommentHint>,
}

impl Worker {
    /// Restores the state from the journal, if there is one.
    fn recover(&mut self) -> Result<(), String> {
        let Some(j) = &self.journal else {
            return Ok(());
        };
        let repo = self.forge.api.repo.full();
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
            self.restart_pending = true;
            self.recovered = Some(doubt);
        }
        Ok(())
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
            let mut hints = BTreeSet::new();
            let mut handled = 0;
            let mut reconcile = false;
            let mut stop = false;
            let mut take = |m: Msg, hints: &mut BTreeSet<Hint>, this: &mut Self| match m {
                Msg::Webhook(p) => {
                    this.webhooks += 1;
                    this.forge.note(format!("webhook {}", p.summary));
                    hints.extend(p.hints);
                    handled += 1;
                }
                Msg::Reconcile => {
                    reconcile = true;
                    handled += 1;
                }
                Msg::Shutdown => stop = true,
                Msg::Control(c, reply) => {
                    let r = this.control(c);
                    let _ = reply.send(r);
                    handled += 1;
                }
            };
            match self.rx.recv_timeout(self.poll) {
                Ok(m) => {
                    take(m, &mut hints, &mut self);
                    while let Ok(m) = self.rx.try_recv() {
                        take(m, &mut hints, &mut self);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => stop = true,
            }
            if stop || self.shared.stop.load(Ordering::SeqCst) {
                return;
            }
            let now = self.clock.now();
            if hints.contains(&Hint::Full) || now >= self.next_reconcile {
                reconcile = true;
            }
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
                for h in &hints {
                    if let Hint::Comment(c) = h {
                        self.comments_waiting.push((**c).clone());
                    }
                }
            }
            self.deliver_comments();
            let due: Vec<Time> = self.timers.iter().copied().filter(|t| *t <= now).collect();
            if !due.is_empty() {
                for t in due {
                    self.timers.remove(&t);
                }
                self.feed(Event::Tick);
            }
            for ev in self.forge.retry_pending() {
                self.feed(ev);
            }
            if self.dead {
                self.publish();
                return;
            }
            self.forge.flush_cosmetics();
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
        let key = format!("{path}\n{text}");
        if self.config_key.as_deref() == Some(key.as_str()) {
            return;
        }
        self.config_key = Some(key);
        match self.check.check(&path, &text) {
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
                });
                self.rules = Some(r);
                self.config_warnings = loaded.warnings;
                self.config_error = None;
                if changed {
                    // MQ-19: every pull request is evaluated again under the new rules.
                    self.forge
                        .note(format!("configuration changed ({path}); using the new one"));
                    self.feed(Event::ConfigurationChanged);
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

    fn reconcile(&mut self) {
        let now = self.clock.now();
        self.next_reconcile = now + self.every;
        self.load_config();
        let Some(rules) = self.rules.clone() else {
            return;
        };
        if self.restart_pending {
            self.restart_pending = false;
            self.feed(Event::Restarted);
        }
        match self.forge.reconcile(&self.state, &rules) {
            Ok((ev, sweep)) => {
                self.feed(ev);
                if self.dead {
                    return;
                }
                self.required_checks();
                if self.dead {
                    return;
                }
                self.forge.sweep(&sweep, &self.state);
                self.forge.hurry_pending();
                self.last_reconcile = Some(now);
                self.reconciliations += 1;
                match self.forge.unhandled_commands(&self.state) {
                    Ok(comments) if !comments.is_empty() => {
                        self.feed(Event::Comments { comments });
                    }
                    Ok(_) => {}
                    Err(e) => self
                        .forge
                        .note(format!("comments not reconciled (provisional): {e}")),
                }
            }
            Err(e) => self
                .forge
                .note(format!("reconciliation skipped (provisional): {e}")),
        }
    }

    fn read_hints(&mut self, hints: &BTreeSet<Hint>) {
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
        let numbers: Vec<u64> = numbers.into_iter().collect();
        let branches: Vec<String> = branches.into_iter().collect();
        let ev = self.forge.snapshot_partial(&numbers, &branches);
        self.feed(ev);
    }

    /// Hands waiting comments to the engine with their sender's permission, read now (the
    /// restriction is checked when the command runs). A comment whose permission cannot be
    /// read waits for the next round; one on a pull request the engine does not know yet
    /// waits too.
    fn deliver_comments(&mut self) {
        if self.rules.is_none() || self.comments_waiting.is_empty() {
            return;
        }
        let mut ready = Vec::new();
        let mut later = Vec::new();
        for c in std::mem::take(&mut self.comments_waiting) {
            if !self.state.pull_requests.contains_key(&c.number) {
                later.push(c);
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
                        later.push(c);
                        continue;
                    }
                }
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
                head_sha: self.forge.head_of(c.number),
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

    /// One event into the engine, its decisions to GitHub, the replies back. The state is
    /// checkpointed before the side effects, each side effect journaled before it is sent.
    fn feed(&mut self, event: Event) {
        let Some(rules) = self.rules.clone() else {
            return;
        };
        let mut queue = VecDeque::from([event]);
        while let Some(ev) = queue.pop_front() {
            if self.dead {
                return;
            }
            let now = self.clock.now();
            let (next, decisions) = step(&self.state, &rules, &ev, now);
            self.state = next;
            if let Some(j) = &mut self.journal
                && let Err(e) = j.checkpoint(&self.state)
            {
                self.die(format!("journal checkpoint failed: {e}"));
                return;
            }
            for d in &decisions {
                if let Decision::WakeAt { at } = d {
                    self.timers.insert(*at);
                    continue;
                }
                self.forge.note(format!("decision: {}", describe(d)));
                self.log_decision(d);
                let side = is_side_effect(d);
                let mut seq = None;
                if side && let Some(j) = &mut self.journal {
                    self.seq += 1;
                    if let Err(e) = j.intent(self.seq, d) {
                        self.die(format!("journal intent failed: {e}"));
                        return;
                    }
                    seq = Some(self.seq);
                }
                let replies = self.forge.execute(d);
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
        };
        self.shared
            .statuses
            .lock()
            .expect("status")
            .insert(st.repo.clone(), st);
    }
}

fn describe(d: &Decision) -> String {
    serde_json::to_string(d).unwrap_or_else(|_| format!("{d:?}"))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let p = token_permissions(&granted);
        assert!(!p.contains_key("administration"));
        assert!(!p.contains_key("workflows"));
        assert_eq!(p.get("contents").map(String::as_str), Some("write"));
        assert!(ct_eq(b"abc", b"abc") && !ct_eq(b"abc", b"abd") && !ct_eq(b"a", b"ab"));
    }
}
