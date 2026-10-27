//! `schlange serve`: the webhook endpoint, one engine worker per repository, reconciliation.
//!
//! - The HTTP listener (`tiny_http`, blocking, one thread) verifies the signature, drops
//!   repeated delivery ids, parses the payload and hands the hints to the repository's worker.
//!   It answers 202 at once; work happens in the worker. A blocking server is enough: GitHub
//!   sends a few deliveries per second at most, and it keeps the binary small (no async
//!   runtime).
//! - Each worker is the single writer for its repository (KONZEPT.md, section 6): it owns the
//!   engine state and the [`GithubForge`], turns hints into reads, feeds the reads to the
//!   engine, carries out the decisions, fires the engine's timers, retries deferred
//!   decisions, and runs a full reconciliation at start and every `reconcile_every` seconds
//!   (progress without webhooks, MQ-30), followed by the orphan sweep.
//! - `GET /healthz` answers `ok`; `GET /status` returns each worker's state as JSON.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use schlange_config::rules::Rules;
use schlange_core::model::Time;
use schlange_core::{Decision, Event, State, step};

use crate::api::{RepoApi, RepoName};
use crate::client::{Client, Priority};
use crate::forge::{ForgeOptions, GithubForge};
use crate::time::Clock;
use crate::webhook::{Dedup, Hint, Parsed, parse, verify_signature};

/// Where the configuration comes from.
#[derive(Clone, Debug)]
pub enum ConfigSource {
    /// `.mergify.yml` (or `.mergify/config.yml`, `.github/mergify.yml`) on the default branch,
    /// read again at every reconciliation.
    Repository,
    /// A fixed configuration text (from `--config`).
    Text(String),
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
    /// How often the worker wakes when nothing arrives.
    pub poll: Duration,
    /// Name of the queue's own check run.
    pub check_name: String,
    /// First and longest delay for retrying deferred decisions.
    pub retry_base: Duration,
    pub retry_max: Duration,
}

pub enum Msg {
    Webhook(Box<Parsed>),
    /// A full reconciliation now.
    Reconcile,
    Shutdown,
}

/// What `/status` shows of a worker.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct WorkerStatus {
    pub repo: String,
    pub queued: Vec<u64>,
    pub merged: Vec<u64>,
    pub halted: BTreeMap<String, String>,
    pub cars: usize,
    pub last_reconcile: Option<Time>,
    pub reconciliations: u64,
    pub webhooks: u64,
    pub pending: Vec<String>,
    pub cosmetics_waiting: usize,
    pub config_error: Option<String>,
    pub log_tail: Vec<String>,
    /// The engine state.
    pub state: serde_json::Value,
}

struct Shared {
    statuses: Mutex<BTreeMap<String, WorkerStatus>>,
    /// Messages handed to workers and not yet handled (tests wait for 0).
    inflight: AtomicUsize,
    stop: AtomicBool,
}

/// Verifies, deduplicates and routes deliveries.
pub struct Router {
    secret: Vec<u8>,
    dedup: Mutex<Dedup>,
    sinks: HashMap<String, Sender<Msg>>,
    shared: Arc<Shared>,
}

impl Router {
    /// Handles one delivery; returns the HTTP status and a short text.
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
        if self.dedup.lock().expect("dedup").check(&id) {
            return (200, "duplicate delivery".into());
        }
        let parsed = match parse(&event, body) {
            Ok(p) => p,
            Err(e) => return (400, e),
        };
        if parsed.hints.is_empty() {
            return (200, "ignored".into());
        }
        let Some(sink) = parsed.repo.as_ref().and_then(|r| self.sinks.get(r)) else {
            return (200, "repository not served".into());
        };
        self.shared.inflight.fetch_add(1, Ordering::SeqCst);
        if sink.send(Msg::Webhook(Box::new(parsed))).is_err() {
            self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
            return (503, "worker stopped".into());
        }
        (202, "accepted".into())
    }
}

pub struct Service {
    pub addr: SocketAddr,
    shared: Arc<Shared>,
    sinks: Vec<Sender<Msg>>,
    threads: Vec<JoinHandle<()>>,
    server: Arc<tiny_http::Server>,
}

impl Service {
    /// Starts the listener and one worker per repository.
    pub fn start(cfg: ServeConfig, clock: Arc<dyn Clock>) -> Result<Service, String> {
        let app = cfg
            .client
            .app()
            .map_err(|e| format!("cannot read the app (GET /app): {e}"))?;
        let opts = ForgeOptions {
            bot_login: format!("{}[bot]", app.slug),
            app_slug: app.slug.clone(),
            check_name: cfg.check_name.clone(),
        };
        let server = Arc::new(
            tiny_http::Server::http(cfg.listen)
                .map_err(|e| format!("cannot listen on {}: {e}", cfg.listen))?,
        );
        let addr = server
            .server_addr()
            .to_ip()
            .ok_or("listener has no IP address")?;
        let shared = Arc::new(Shared {
            statuses: Mutex::new(BTreeMap::new()),
            inflight: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        });
        let mut sinks = HashMap::new();
        let mut threads = Vec::new();
        let mut senders = Vec::new();
        for repo in &cfg.repos {
            let installation = match cfg.installation {
                Some(i) => i,
                None => {
                    cfg.client
                        .repo_installation(repo)
                        .map_err(|e| format!("{}: no installation: {e}", repo.full()))?
                        .id
                }
            };
            let api = RepoApi {
                client: cfg.client.clone(),
                repo: repo.clone(),
                installation,
            };
            let mut forge = GithubForge::new(api, clock.clone(), opts.clone());
            forge.retry_base = cfg.retry_base;
            forge.retry_max = cfg.retry_max;
            let (tx, rx) = channel();
            sinks.insert(repo.full(), tx.clone());
            senders.push(tx);
            let worker = Worker {
                forge,
                clock: clock.clone(),
                state: State::default(),
                rules: None,
                rules_text: None,
                config_error: None,
                config: cfg.config.clone(),
                timers: BTreeSet::new(),
                rx,
                every: cfg.reconcile_every.max(1),
                next_reconcile: 0,
                last_reconcile: None,
                reconciliations: 0,
                webhooks: 0,
                shared: shared.clone(),
                poll: cfg.poll,
            };
            threads.push(
                std::thread::Builder::new()
                    .name(format!("worker {}", repo.full()))
                    .spawn(move || worker.run())
                    .map_err(|e| e.to_string())?,
            );
        }
        let router = Router {
            secret: cfg.webhook_secret.clone(),
            dedup: Mutex::new(Dedup::new(10_000)),
            sinks,
            shared: shared.clone(),
        };
        let srv = server.clone();
        let sh = shared.clone();
        threads.push(
            std::thread::Builder::new()
                .name("webhooks".into())
                .spawn(move || serve_http(&srv, &router, &sh))
                .map_err(|e| e.to_string())?,
        );
        Ok(Service {
            addr,
            shared,
            sinks: senders,
            threads,
            server,
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

    /// Asks every worker for a full reconciliation now.
    pub fn reconcile_now(&self) {
        for s in &self.sinks {
            self.shared.inflight.fetch_add(1, Ordering::SeqCst);
            if s.send(Msg::Reconcile).is_err() {
                self.shared.inflight.fetch_sub(1, Ordering::SeqCst);
            }
        }
    }

    pub fn shutdown(self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        for s in &self.sinks {
            let _ = s.send(Msg::Shutdown);
        }
        self.server.unblock();
        for t in self.threads {
            let _ = t.join();
        }
    }

    /// Blocks until the service stops (for the binary).
    pub fn wait(self) {
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn serve_http(server: &tiny_http::Server, router: &Router, shared: &Shared) {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            return;
        }
        let mut req = match server.recv() {
            Ok(r) => r,
            Err(_) => return,
        };
        let url = req.url().to_string();
        let method = req.method().to_string();
        let (status, text, json) = match (method.as_str(), url.as_str()) {
            ("POST", "/webhook") | ("POST", "/") => {
                let mut body = Vec::new();
                // GitHub caps payloads at 25 MB.
                let ok = req
                    .as_reader()
                    .take(25 * 1024 * 1024 + 1)
                    .read_to_end(&mut body)
                    .is_ok();
                if !ok || body.len() > 25 * 1024 * 1024 {
                    (413, "payload too large".to_string(), false)
                } else {
                    let headers: Vec<(String, String)> = req
                        .headers()
                        .iter()
                        .map(|h| {
                            (
                                h.field.as_str().as_str().to_ascii_lowercase(),
                                h.value.as_str().to_string(),
                            )
                        })
                        .collect();
                    let get = |name: &str| {
                        headers
                            .iter()
                            .find(|(k, _)| k == name)
                            .map(|(_, v)| v.clone())
                    };
                    let (s, t) = router.handle(&get, &body);
                    (s, t, false)
                }
            }
            ("GET", "/healthz") => (200, "ok".into(), false),
            ("GET", "/status") => {
                let s = shared.statuses.lock().expect("status").clone();
                (
                    200,
                    serde_json::to_string_pretty(&s).unwrap_or_default(),
                    true,
                )
            }
            _ => (404, "not found".into(), false),
        };
        let mut resp = tiny_http::Response::from_string(text).with_status_code(status);
        if json && let Ok(h) = tiny_http::Header::from_bytes("content-type", "application/json") {
            resp = resp.with_header(h);
        }
        let _ = req.respond(resp);
    }
}

struct Worker {
    forge: GithubForge,
    clock: Arc<dyn Clock>,
    state: State,
    rules: Option<Rules>,
    rules_text: Option<String>,
    config_error: Option<String>,
    config: ConfigSource,
    timers: BTreeSet<Time>,
    rx: Receiver<Msg>,
    every: Time,
    next_reconcile: Time,
    last_reconcile: Option<Time>,
    reconciliations: u64,
    webhooks: u64,
    shared: Arc<Shared>,
    poll: Duration,
}

const CONFIG_PATHS: [&str; 3] = [".mergify.yml", ".mergify/config.yml", ".github/mergify.yml"];

impl Worker {
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
            } else if !hints.is_empty() {
                self.read_hints(&hints);
            }
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
            self.forge.flush_cosmetics();
            self.publish();
            self.shared.inflight.fetch_sub(handled, Ordering::SeqCst);
        }
    }

    fn load_config(&mut self) {
        let text = match &self.config {
            ConfigSource::Text(t) => Some(t.clone()),
            ConfigSource::Repository => {
                let api = &self.forge.api;
                let branch = match api.repository(Priority::Normal) {
                    Ok(r) => r.default_branch,
                    Err(e) => {
                        self.forge.note(format!("configuration not read: {e}"));
                        return;
                    }
                };
                let mut found = None;
                for path in CONFIG_PATHS {
                    match api.file_content(path, &branch, Priority::Normal) {
                        Ok(Some(t)) => {
                            found = Some(t);
                            break;
                        }
                        Ok(None) => {}
                        Err(e) => {
                            self.forge.note(format!("configuration not read: {e}"));
                            return;
                        }
                    }
                }
                found
            }
        };
        let Some(text) = text else {
            self.config_error = Some("no .mergify.yml on the default branch".into());
            return;
        };
        if self.rules_text.as_deref() == Some(text.as_str()) {
            return;
        }
        match Rules::from_yaml(&text) {
            Ok(r) => {
                if self.rules.is_some() {
                    self.forge.note("configuration changed; using the new one");
                }
                self.rules = Some(r);
                self.rules_text = Some(text);
                self.config_error = None;
            }
            Err(e) => {
                // Keep the last good configuration.
                self.config_error = Some(format!("configuration invalid, not applied: {e}"));
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
        match self.forge.reconcile(&self.state, &rules) {
            Ok((ev, sweep)) => {
                self.feed(ev);
                self.forge.sweep(&sweep, &self.state);
                self.forge.hurry_pending();
                self.last_reconcile = Some(now);
                self.reconciliations += 1;
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

    /// One event into the engine, its decisions to GitHub, the replies back.
    fn feed(&mut self, event: Event) {
        let Some(rules) = self.rules.clone() else {
            return;
        };
        let mut queue = vec![event];
        while !queue.is_empty() {
            let ev = queue.remove(0);
            let now = self.clock.now();
            let (next, decisions) = step(&self.state, &rules, &ev, now);
            self.state = next;
            for d in &decisions {
                if let Decision::WakeAt { at } = d {
                    self.timers.insert(*at);
                    continue;
                }
                self.forge.note(format!("decision: {}", describe(d)));
                queue.extend(self.forge.execute(d));
            }
        }
    }

    fn publish(&self) {
        let st = WorkerStatus {
            repo: self.forge.api.repo.full(),
            queued: self.state.queued.keys().copied().collect(),
            merged: self
                .state
                .outcomes
                .iter()
                .filter(|(_, o)| matches!(o, schlange_core::engine::Outcome::Merged { .. }))
                .map(|(n, _)| *n)
                .collect(),
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
            log_tail: self
                .forge
                .log
                .iter()
                .rev()
                .take(40)
                .rev()
                .cloned()
                .collect(),
            state: serde_json::to_value(&self.state).unwrap_or_default(),
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
