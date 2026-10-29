//! The HTTP side of the GitHub client: authentication (app JWT, cached installation tokens),
//! one central request budget per process, retries with jitter, `Retry-After`, ETags.
//!
//! **Budget.** Every request takes a token from one bucket. Landing requests always go
//! (they may run the bucket into debt), normal requests wait for a token, cosmetic requests
//! (the queue's own check run, comments, labels) are refused with [`Error::Deferred`] while
//! the bucket is below its reserve or while GitHub asked us to wait. So landing comes before
//! cosmetics (MQ-34).
//!
//! **Rate limits.** A 429, or a 403 that says "rate limit", blocks the whole client until
//! `Retry-After` (or `x-ratelimit-reset` when `x-ratelimit-remaining` is 0; one minute for a
//! secondary limit without a header). A rate-limited read is repeated after the wait. A
//! rate-limited **write is never repeated here**: the world may have changed during the wait
//! (a push to the base, a label), so the write comes back as [`Error::RateLimited`] and the
//! caller re-validates from fresh reads before it writes again (for landing: the engine's
//! verification read, KONZEPT.md section 6).
//!
//! **Where requests go.** Only to the configured API base: an absolute URL (a `Link` header,
//! a redirect) on another host is refused, so the token never leaves for another host. Plain
//! `http://` is refused unless the configuration allows it explicitly (the fake GitHub in
//! tests). A renamed repository answers with a redirect (301 for reads, 307 for writes) to the
//! new location on the same host; the client follows it.
//!
//! **Tokens.** Installation tokens can be downscoped per installation
//! ([`Client::set_token_scope`]): only the served repositories and only the permissions
//! Schlange needs (W-22).
//!
//! **Retries.** Reads (`GET`) and `DELETE` are repeated on 5xx, transport errors and, a few
//! times, on 404 (W-24: GitHub has had storms of false 404s). A write that fails with 5xx or
//! a transport error is *not* repeated here: the effect may have happened (MQ-35). It comes
//! back as [`Error::Ambiguous`] and the caller reads the state before deciding.
//!
//! **Write gate.** A [`WriteGate`] set on the client ([`Client::set_write_gate`]) is asked
//! before every request that may change GitHub (every method but `GET`; installation tokens
//! excepted). Shadow mode sets one that refuses everything (S-SHADOW); the lease sets one that
//! lets a write through only while this process holds the repository's lease and its fence on
//! GitHub is still the one it wrote (MQ-32). A refused request is never sent
//! ([`Error::Refused`]). The lease's own compare-and-swap goes through
//! [`Client::send_unfenced`].
//!
//! **ETags.** `GET` responses with an ETag are cached; the next read sends `If-None-Match`
//! and a 304 (which GitHub does not count against the rate limit) returns the cached body.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;

use crate::jwt::AppKey;
use crate::time::{parse_rfc3339, unix_now};

/// Decides, before a request that may change GitHub is sent, whether it may be sent.
pub trait WriteGate: Send + Sync {
    /// `Err(why)` refuses the request; it is not sent.
    fn permit(&self, method: &str, path: &str) -> Result<(), String>;
    /// Restrict the exact outgoing body as well as its destination.
    fn permit_body(&self, method: &str, path: &str, _body: Option<&[u8]>) -> Result<(), String> {
        self.permit(method, path)
    }
    /// Observe the response before the client considers retries or redirects.
    fn response(
        &self,
        _method: &str,
        _path: &str,
        _status: u16,
        _body: &[u8],
    ) -> Result<(), String> {
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Priority {
    /// Landing and the reads right before it.
    Landing,
    /// Reads, reconciliation, building queue branches.
    Normal,
    /// Read-only background work: defer before consuming the worker's reserve.
    Background,
    /// The queue's own check run, comments, labels, reactions.
    Cosmetic,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Auth {
    /// The app's JWT (`/app/...`).
    App,
    /// An installation access token.
    Installation(u64),
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// A definitive answer from GitHub (4xx other than rate limits).
    #[error("{method} {path}: HTTP {status}: {message}")]
    Status {
        method: String,
        path: String,
        status: u16,
        message: String,
    },
    /// 404 after the retries. GitHub's 404s are not always the truth (W-24): callers treat it
    /// as provisional unless a canary read shows that GitHub answers correctly.
    #[error("{method} {path}: not found")]
    NotFound { method: String, path: String },
    /// A write failed so that its effect is unknown (5xx, timeout, connection lost).
    #[error("{method} {path}: outcome unknown: {detail}")]
    Ambiguous {
        method: String,
        path: String,
        detail: String,
    },
    /// A read failed after the retries (5xx, transport). Provisional.
    #[error("{method} {path}: unavailable: {detail}")]
    Unavailable {
        method: String,
        path: String,
        detail: String,
    },
    /// GitHub asked us to wait longer than the client may block.
    #[error("{path}: rate limited for {wait_secs} s")]
    RateLimited { path: String, wait_secs: u64 },
    /// Refused before sending: the request would leave the API host or use plain HTTP.
    #[error("{0}")]
    Refused(String),
    /// A cosmetic request was not sent because the budget is low.
    #[error("{path}: deferred, the request budget is reserved for landing")]
    Deferred { path: String },
    #[error("authentication: {0}")]
    Auth(String),
    #[error("{path}: cannot decode the response: {detail}")]
    Decode { path: String, detail: String },
}

impl Error {
    /// The status code of a definitive answer.
    pub fn status(&self) -> Option<u16> {
        match self {
            Error::Status { status, .. } => Some(*status),
            Error::NotFound { .. } => Some(404),
            _ => None,
        }
    }

    /// Whether the condition may pass by itself (try again later, never act on it).
    pub fn is_provisional(&self) -> bool {
        matches!(
            self,
            Error::NotFound { .. }
                | Error::Ambiguous { .. }
                | Error::Unavailable { .. }
                | Error::RateLimited { .. }
                | Error::Deferred { .. }
        ) || matches!(self, Error::Status { status, .. } if *status >= 500)
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    /// Attempts for a read on 5xx or a transport error.
    pub attempts: u32,
    /// Attempts for a read that returns 404.
    pub not_found_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    /// The longest the client blocks for a rate limit before it gives up with `RateLimited`.
    pub max_rate_limit_wait: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        RetryPolicy {
            attempts: 4,
            not_found_attempts: 3,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(20),
            max_rate_limit_wait: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    /// Fast retries for tests.
    pub fn fast() -> Self {
        RetryPolicy {
            attempts: 4,
            not_found_attempts: 3,
            base_delay: Duration::from_millis(2),
            max_delay: Duration::from_millis(20),
            max_rate_limit_wait: Duration::from_secs(5),
        }
    }
}

/// The central token bucket.
#[derive(Clone, Debug)]
pub struct BudgetConfig {
    pub capacity: f64,
    /// Tokens per second. GitHub's primary limit for an installation is 5000 requests per
    /// hour (about 1.4/s); content-creating requests have a secondary limit of 80 per minute.
    pub refill_per_sec: f64,
    /// Cosmetic requests only go while more than this many tokens are left.
    pub cosmetic_reserve: f64,
}

impl Default for BudgetConfig {
    fn default() -> Self {
        BudgetConfig {
            capacity: 100.0,
            refill_per_sec: 1.3,
            cosmetic_reserve: 30.0,
        }
    }
}

#[derive(Debug)]
struct Budget {
    cfg: BudgetConfig,
    tokens: f64,
    last: Instant,
    blocked_until: Option<Instant>,
}

enum Admit {
    Go,
    Wait(Duration),
    Deny,
}

impl Budget {
    fn new(cfg: BudgetConfig) -> Self {
        Budget {
            tokens: cfg.capacity,
            cfg,
            last: Instant::now(),
            blocked_until: None,
        }
    }

    fn refill(&mut self, now: Instant) {
        let dt = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + dt * self.cfg.refill_per_sec).min(self.cfg.capacity);
        self.last = now;
    }

    fn admit(&mut self, prio: Priority) -> Admit {
        let now = Instant::now();
        self.refill(now);
        if let Some(until) = self.blocked_until {
            if until > now {
                return match prio {
                    Priority::Cosmetic | Priority::Background => Admit::Deny,
                    _ => Admit::Wait(until - now),
                };
            }
            self.blocked_until = None;
        }
        match prio {
            Priority::Landing => {
                self.tokens -= 1.0;
                Admit::Go
            }
            Priority::Normal if self.tokens >= 1.0 => {
                self.tokens -= 1.0;
                Admit::Go
            }
            Priority::Normal => Admit::Wait(Duration::from_secs_f64(
                (1.0 - self.tokens) / self.cfg.refill_per_sec.max(0.001),
            )),
            Priority::Cosmetic | Priority::Background
                if self.tokens >= self.cfg.cosmetic_reserve + 1.0 =>
            {
                self.tokens -= 1.0;
                Admit::Go
            }
            Priority::Cosmetic | Priority::Background => Admit::Deny,
        }
    }

    fn block_for(&mut self, d: Duration) {
        let until = Instant::now() + d;
        if self.blocked_until.is_none_or(|u| u < until) {
            self.blocked_until = Some(until);
        }
    }
}

/// Counters for doctor and tests.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct Stats {
    /// Writes the write gate refused (shadow mode, a lost lease); never sent.
    pub writes_refused: u64,
    pub requests: u64,
    pub retries: u64,
    pub rate_limited: u64,
    pub not_modified: u64,
    pub deferred: u64,
    pub ambiguous: u64,
    /// Redirects followed (renamed repositories).
    pub redirects: u64,
    /// Writes answered with a rate limit and handed back to the caller unsent.
    pub writes_rate_limited: u64,
    pub tokens_issued: u64,
    /// Last `x-ratelimit-remaining` / `x-ratelimit-limit` GitHub reported.
    pub remaining: Option<u64>,
    pub limit: Option<u64>,
}

#[derive(Clone, Debug)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
    /// Lower-case header names.
    pub headers: Vec<(String, String)>,
    /// Served from the ETag cache after a 304.
    pub cached: bool,
}

impl Response {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    pub fn json<T: DeserializeOwned>(&self, path: &str) -> Result<T, Error> {
        serde_json::from_slice(&self.body).map_err(|e| Error::Decode {
            path: path.to_string(),
            detail: e.to_string(),
        })
    }

    /// The `rel="next"` URL of a `Link` header.
    pub fn next_link(&self) -> Option<String> {
        let link = self.header("link")?;
        link.split(',').find_map(|part| {
            let (url, rel) = part.split_once(';')?;
            rel.contains("rel=\"next\"").then(|| {
                url.trim()
                    .trim_start_matches('<')
                    .trim_end_matches('>')
                    .to_string()
            })
        })
    }
}

#[derive(Clone)]
pub struct ClientConfig {
    /// `https://api.github.com`, or the fake's address in tests.
    pub api_base: String,
    /// Numeric app id (or client id), the JWT's `iss`.
    pub app_id: String,
    pub key: Arc<AppKey>,
    pub retry: RetryPolicy,
    pub budget: BudgetConfig,
    pub timeout: Duration,
    /// Allow a plain `http://` API base (tests against the fake GitHub only).
    pub allow_insecure_http: bool,
}

struct Inner {
    #[cfg(feature = "test-hooks")]
    budget_waits: std::sync::atomic::AtomicUsize,
    cfg: ClientConfig,
    agent: ureq::Agent,
    budget: Mutex<Budget>,
    tokens: Mutex<
        HashMap<
            u64,
            (
                String,
                u64,
                Option<std::collections::BTreeMap<String, String>>,
            ),
        >,
    >,
    /// Body of the token request per installation (repositories, permissions).
    scopes: Mutex<HashMap<u64, serde_json::Value>>,
    etags: Mutex<HashMap<String, (String, Response)>>,
    stats: Mutex<Stats>,
    rng: Mutex<u64>,
    gate: std::sync::RwLock<Option<Arc<dyn WriteGate>>>,
}

/// A GitHub REST client. Cheap to clone; all clones share one budget and one token cache.
#[derive(Clone)]
pub struct Client {
    inner: Arc<Inner>,
    probe_memo: Option<Arc<ProbeMemo>>,
    execution_gates: Vec<Arc<dyn WriteGate>>,
}

/// One observer snapshot only. It never belongs to the worker's read/landing path.
pub(crate) struct ProbeMemo {
    entries: Mutex<BTreeMap<String, Response>>,
    total: Arc<std::sync::atomic::AtomicUsize>,
    bytes: std::sync::atomic::AtomicUsize,
}

const PROBE_MEMO_MAX_BYTES: usize = 2 * 1024 * 1024;
const PROBE_MEMO_MAX_ENTRIES: usize = 512;
const PROBE_MEMO_MAX_TOTAL_BYTES: usize = 16 * 1024 * 1024;

impl ProbeMemo {
    pub(crate) fn new(total: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        Self {
            entries: Mutex::new(BTreeMap::new()),
            total,
            bytes: std::sync::atomic::AtomicUsize::new(0),
        }
    }
    fn get(&self, key: &str) -> Option<Response> {
        self.entries.lock().expect("probe memo").get(key).cloned()
    }

    fn put(&self, key: String, response: &Response) -> Result<(), Error> {
        let mut entries = self.entries.lock().expect("probe memo");
        if entries.contains_key(&key) {
            return Ok(());
        }
        let size = |k: &str, r: &Response| {
            k.len()
                + r.body.len()
                + r.headers
                    .iter()
                    .map(|(a, b)| a.len() + b.len())
                    .sum::<usize>()
        };
        let amount = size(&key, response);
        let local = self.bytes.load(Ordering::SeqCst);
        if entries.len() >= PROBE_MEMO_MAX_ENTRIES
            || local.saturating_add(amount) > PROBE_MEMO_MAX_BYTES
        {
            return Err(Error::Refused("watchdog probe memo limit exceeded".into()));
        }
        self.total
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |v| {
                v.checked_add(amount)
                    .filter(|n| *n <= PROBE_MEMO_MAX_TOTAL_BYTES)
            })
            .map_err(|_| Error::Refused("watchdog total memo limit exceeded".into()))?;
        self.bytes.fetch_add(amount, Ordering::SeqCst);
        entries.insert(key, response.clone());
        Ok(())
    }
}

impl Drop for ProbeMemo {
    fn drop(&mut self) {
        self.total
            .fetch_sub(self.bytes.load(Ordering::SeqCst), Ordering::SeqCst);
    }
}

/// One pre-paid cosmetic write. Dropping an unused permit returns its token.
pub(crate) struct CosmeticPermit {
    inner: Arc<Inner>,
    available: bool,
}

impl Drop for CosmeticPermit {
    fn drop(&mut self) {
        if self.available {
            let mut b = self.inner.budget.lock().unwrap_or_else(|e| e.into_inner());
            b.refill(Instant::now());
            b.tokens = (b.tokens + 1.0).min(b.cfg.capacity);
        }
    }
}

const ETAG_CACHE_MAX: usize = 4096;

/// Percent-encodes a path segment, keeping `/` (branch names contain slashes).
pub fn enc_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~' | b'/') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Percent-encodes a query value.
pub fn enc_query(s: &str) -> String {
    enc_path(s).replace('/', "%2F")
}

fn root_certs() -> Vec<ureq::tls::Certificate<'static>> {
    let candidates = [
        std::env::var("SSL_CERT_FILE").ok(),
        Some("/etc/ssl/certs/ca-certificates.crt".into()),
        Some("/etc/ssl/cert.pem".into()),
        Some("/etc/pki/tls/certs/ca-bundle.crt".into()),
    ];
    for path in candidates.into_iter().flatten() {
        if let Ok(pem) = std::fs::read(&path) {
            let certs: Vec<_> = ureq::tls::parse_pem(&pem)
                .filter_map(|i| match i {
                    Ok(ureq::tls::PemItem::Certificate(c)) => Some(c.to_owned()),
                    _ => None,
                })
                .collect();
            if !certs.is_empty() {
                return certs;
            }
        }
    }
    Vec::new()
}

impl Client {
    /// Reserve a write token while leaving at least one token for the lookup that precedes it.
    pub(crate) fn reserve_cosmetic_write(&self, path: &str) -> Result<CosmeticPermit, Error> {
        let mut b = self.inner.budget.lock().expect("budget");
        let now = Instant::now();
        b.refill(now);
        if b.blocked_until.is_some_and(|until| until > now)
            || b.tokens < b.cfg.cosmetic_reserve + 2.0
        {
            drop(b);
            self.bump(|s| s.deferred += 1);
            return Err(Error::Deferred { path: path.into() });
        }
        b.tokens -= 1.0;
        Ok(CosmeticPermit {
            inner: self.inner.clone(),
            available: true,
        })
    }

    fn use_cosmetic_permit(&self, permit: &mut CosmeticPermit, path: &str) -> Result<(), Error> {
        if !permit.available || !Arc::ptr_eq(&self.inner, &permit.inner) {
            return Err(Error::Refused("invalid cosmetic budget permit".into()));
        }
        let b = self.inner.budget.lock().expect("budget");
        if b.blocked_until.is_some_and(|until| until > Instant::now()) {
            return Err(Error::Deferred { path: path.into() });
        }
        permit.available = false;
        Ok(())
    }

    /// Test support: replace the local budget without changing the remote API.
    #[cfg(feature = "test-hooks")]
    pub fn test_set_budget(&self, cfg: BudgetConfig) {
        *self.inner.budget.lock().expect("budget") = Budget::new(cfg);
    }
    #[cfg(feature = "test-hooks")]
    pub fn test_budget_waits(&self) -> usize {
        self.inner
            .budget_waits
            .load(std::sync::atomic::Ordering::SeqCst)
    }
    pub fn new(cfg: ClientConfig) -> Self {
        let mut config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .timeout_global(Some(cfg.timeout))
            .max_redirects(0)
            .user_agent(concat!("schlange/", env!("CARGO_PKG_VERSION")));
        if cfg.api_base.starts_with("https://") {
            let roots = root_certs();
            config = config.tls_config(
                ureq::tls::TlsConfig::builder()
                    .root_certs(ureq::tls::RootCerts::from(roots))
                    .build(),
            );
        }
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(1)
            | 1;
        Client {
            inner: Arc::new(Inner {
                #[cfg(feature = "test-hooks")]
                budget_waits: std::sync::atomic::AtomicUsize::new(0),
                agent: ureq::Agent::new_with_config(config.build()),
                budget: Mutex::new(Budget::new(cfg.budget.clone())),
                tokens: Mutex::new(HashMap::new()),
                scopes: Mutex::new(HashMap::new()),
                etags: Mutex::new(HashMap::new()),
                stats: Mutex::new(Stats::default()),
                rng: Mutex::new(seed),
                gate: std::sync::RwLock::new(None),
                cfg,
            }),
            probe_memo: None,
            execution_gates: Vec::new(),
        }
    }

    pub(crate) fn with_probe_memo(&self, memo: Arc<ProbeMemo>) -> Self {
        Self {
            inner: self.inner.clone(),
            probe_memo: Some(memo),
            execution_gates: self.execution_gates.clone(),
        }
    }

    fn probe_key(url: &str, authorization: &str) -> String {
        let digest = ring::digest::digest(&ring::digest::SHA256, authorization.as_bytes());
        let hash = digest
            .as_ref()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        format!("GET\0{url}\0accept:application/vnd.github+json\0version:2022-11-28\0{hash}")
    }

    fn probe_cached_key(
        &self,
        url: &str,
        auth: Auth,
        prio: Priority,
        method: &str,
    ) -> Option<String> {
        self.probe_memo.as_ref()?;
        if method != "GET" || prio != Priority::Background {
            return None;
        }
        let Auth::Installation(id) = auth else {
            return None;
        };
        let tokens = self.inner.tokens.lock().expect("tokens");
        let (token, expiry, _) = tokens.get(&id)?;
        if *expiry <= unix_now() + 300 {
            return None;
        }
        Some(Self::probe_key(url, &format!("token {token}")))
    }

    /// Sets (or clears) the gate every write must pass (shadow mode, the lease's fence).
    pub fn set_write_gate(&self, gate: Option<Arc<dyn WriteGate>>) {
        *self.inner.gate.write().unwrap_or_else(|p| p.into_inner()) = gate;
    }

    /// Scope an additional write restriction to this clone and its descendants.
    pub fn with_execution_gate(&self, gate: Arc<dyn WriteGate>) -> Self {
        let mut scoped = self.clone();
        scoped.execution_gates.push(gate);
        scoped
    }

    pub(crate) fn has_execution_gate(&self) -> bool {
        !self.execution_gates.is_empty()
    }
    pub(crate) fn last_execution_gate(&self) -> Option<Arc<dyn WriteGate>> {
        self.execution_gates.last().cloned()
    }

    pub fn config(&self) -> &ClientConfig {
        &self.inner.cfg
    }

    pub fn stats(&self) -> Stats {
        self.inner.stats.lock().expect("stats").clone()
    }

    fn bump(&self, f: impl FnOnce(&mut Stats)) {
        f(&mut self.inner.stats.lock().expect("stats"));
    }

    /// The app's JWT.
    pub fn app_jwt(&self) -> Result<String, Error> {
        self.inner
            .cfg
            .key
            .jwt(&self.inner.cfg.app_id, unix_now())
            .map_err(|e| Error::Auth(e.to_string()))
    }

    /// Limits the tokens of an installation to these repositories (names without owner) and
    /// permissions. A cached token of another scope is dropped.
    pub fn set_token_scope(
        &self,
        installation: u64,
        repositories: &[String],
        permissions: &std::collections::BTreeMap<String, String>,
    ) {
        let body = serde_json::json!({
            "repositories": repositories,
            "permissions": permissions,
        });
        let mut scopes = self.inner.scopes.lock().expect("scopes");
        if scopes.get(&installation) != Some(&body) {
            scopes.insert(installation, body);
            self.forget_token(installation);
        }
    }

    /// An installation token, from the cache while it has more than five minutes left.
    pub fn installation_token(&self, installation: u64) -> Result<String, Error> {
        let now = unix_now();
        if let Some((t, exp, _)) = self.inner.tokens.lock().expect("tokens").get(&installation)
            && *exp > now + 300
        {
            return Ok(t.clone());
        }
        let path = format!("/app/installations/{installation}/access_tokens");
        #[derive(serde::Deserialize)]
        struct Token {
            token: String,
            expires_at: String,
            #[serde(default)]
            permissions: Option<std::collections::BTreeMap<String, String>>,
        }
        let body = self
            .inner
            .scopes
            .lock()
            .expect("scopes")
            .get(&installation)
            .map(|b| b.to_string())
            .unwrap_or_else(|| "{}".into());
        // Creating a token has no side effect worth guarding; repeat it like a read.
        let resp = self.send_inner(
            "POST",
            &path,
            Auth::App,
            Priority::Landing,
            Some(body.into_bytes()),
            true,
            false,
            None,
        )?;
        let tok: Token = resp.json(&path)?;
        let exp = parse_rfc3339(&tok.expires_at).unwrap_or(now + 3000);
        self.bump(|s| s.tokens_issued += 1);
        self.inner
            .tokens
            .lock()
            .expect("tokens")
            .insert(installation, (tok.token.clone(), exp, tok.permissions));
        Ok(tok.token)
    }

    /// Permissions GitHub returned for the actual repository-scoped installation token.
    pub fn installation_token_permissions(
        &self,
        installation: u64,
    ) -> Result<std::collections::BTreeMap<String, String>, Error> {
        self.installation_token(installation)?;
        self.inner
            .tokens
            .lock()
            .expect("tokens")
            .get(&installation)
            .and_then(|(_, _, permissions)| permissions.clone())
            .ok_or_else(|| Error::Auth("installation token permissions unavailable".into()))
    }

    fn forget_token(&self, installation: u64) {
        self.inner
            .tokens
            .lock()
            .expect("tokens")
            .remove(&installation);
    }

    fn jitter(&self, attempt: u32) -> Duration {
        let p = &self.inner.cfg.retry;
        let exp = p
            .base_delay
            .saturating_mul(1u32 << attempt.min(16))
            .min(p.max_delay);
        let mut r = self.inner.rng.lock().expect("rng");
        *r ^= *r << 13;
        *r ^= *r >> 7;
        *r ^= *r << 17;
        // Between half and the full exponential delay ("equal jitter").
        let f = 0.5 + (*r % 1000) as f64 / 2000.0;
        exp.mul_f64(f)
    }

    pub(crate) fn base(&self) -> &str {
        self.inner.cfg.api_base.trim_end_matches('/')
    }

    pub(crate) fn matches_api_path(&self, url: &str, path: &str) -> bool {
        url == format!("{}{path}", self.base())
    }

    /// Whether an absolute URL is on the API host (same scheme, host and port, below the
    /// base path).
    pub fn on_api_host(&self, url: &str) -> bool {
        let base = self.base();
        url.strip_prefix(base)
            .is_some_and(|rest| rest.is_empty() || rest.starts_with('/') || rest.starts_with('?'))
    }

    /// The full URL of a path, or an absolute URL if it is on the API host.
    fn url(&self, path: &str) -> Result<String, Error> {
        let url = if path.starts_with("http://") || path.starts_with("https://") {
            if !self.on_api_host(path) {
                return Err(Error::Refused(format!(
                    "refusing to send a request (and the token) to {path}: not on the API host {}",
                    self.base()
                )));
            }
            path.to_string()
        } else {
            format!("{}{}", self.base(), path)
        };
        if url.starts_with("http://") && !self.inner.cfg.allow_insecure_http {
            return Err(Error::Refused(format!(
                "refusing to send credentials over plain HTTP to {}; use https (tests only: allow_insecure_http)",
                self.base()
            )));
        }
        Ok(url)
    }

    fn admit(&self, prio: Priority, path: &str) -> Result<(), Error> {
        let mut waited = Duration::ZERO;
        loop {
            let a = self.inner.budget.lock().expect("budget").admit(prio);
            match a {
                Admit::Go => return Ok(()),
                Admit::Deny => {
                    self.bump(|s| s.deferred += 1);
                    return Err(Error::Deferred {
                        path: path.to_string(),
                    });
                }
                Admit::Wait(d) => {
                    #[cfg(feature = "test-hooks")]
                    self.inner
                        .budget_waits
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if waited + d > self.inner.cfg.retry.max_rate_limit_wait {
                        return Err(Error::RateLimited {
                            path: path.to_string(),
                            wait_secs: d.as_secs(),
                        });
                    }
                    std::thread::sleep(d.min(Duration::from_secs(5)));
                    waited += d.min(Duration::from_secs(5));
                }
            }
        }
    }

    /// Whether a response is a rate limit, and how long to wait.
    fn rate_limit_wait(resp: &Response) -> Option<Duration> {
        let limited = resp.status == 429
            || (resp.status == 403
                && (resp.header("x-ratelimit-remaining") == Some("0")
                    || resp.header("retry-after").is_some()
                    || String::from_utf8_lossy(&resp.body)
                        .to_ascii_lowercase()
                        .contains("rate limit")));
        if !limited {
            return None;
        }
        if let Some(s) = resp
            .header("retry-after")
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            return Some(Duration::from_secs(s));
        }
        if resp.header("x-ratelimit-remaining") == Some("0")
            && let Some(reset) = resp
                .header("x-ratelimit-reset")
                .and_then(|v| v.parse::<u64>().ok())
        {
            return Some(Duration::from_secs(reset.saturating_sub(unix_now()).max(1)));
        }
        // A secondary limit without a header: GitHub asks for at least a minute.
        Some(Duration::from_secs(60))
    }

    fn raw(
        &self,
        method: &str,
        url: &str,
        headers: &[(String, String)],
        body: Option<&[u8]>,
    ) -> Result<Response, String> {
        macro_rules! with_headers {
            ($b:expr) => {{
                let mut b = $b;
                for (k, v) in headers {
                    b = b.header(k.as_str(), v.as_str());
                }
                b
            }};
        }
        let agent = &self.inner.agent;
        let body = body.unwrap_or(&[]);
        let result = match method {
            "GET" => with_headers!(agent.get(url)).call(),
            "DELETE" => with_headers!(agent.delete(url)).call(),
            "POST" => with_headers!(agent.post(url)).send(body),
            "PUT" => with_headers!(agent.put(url)).send(body),
            "PATCH" => with_headers!(agent.patch(url)).send(body),
            other => return Err(format!("unsupported method {other}")),
        };
        let mut resp = result.map_err(|e| e.to_string())?;
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(k, v)| {
                (
                    k.as_str().to_ascii_lowercase(),
                    v.to_str().unwrap_or("").to_string(),
                )
            })
            .collect();
        let body = resp
            .body_mut()
            .with_config()
            .limit(64 * 1024 * 1024)
            .read_to_vec()
            .map_err(|e| e.to_string())?;
        Ok(Response {
            status,
            body,
            headers,
            cached: false,
        })
    }

    /// Sends one request with the rules in the module documentation.
    pub fn send(
        &self,
        method: &str,
        path: &str,
        auth: Auth,
        prio: Priority,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, Error> {
        let body = body.map(|b| b.to_string().into_bytes());
        let idempotent = matches!(method, "GET" | "DELETE");
        self.send_inner(method, path, auth, prio, body, idempotent, true, None)
    }

    /// A fenced cosmetic write with one previously reserved budget token.
    pub(crate) fn send_reserved_cosmetic(
        &self,
        method: &str,
        path: &str,
        auth: Auth,
        body: &serde_json::Value,
        permit: &mut CosmeticPermit,
    ) -> Result<Response, Error> {
        if !matches!(method, "POST" | "PATCH") {
            return Err(Error::Refused("invalid reserved cosmetic method".into()));
        }
        self.send_inner(
            method,
            path,
            auth,
            Priority::Cosmetic,
            Some(body.to_string().into_bytes()),
            false,
            true,
            Some(permit),
        )
    }

    /// As [`Client::send`], without asking the write gate: only for the lease's own
    /// compare-and-swap on its fence ref (`crate::lease`).
    pub fn send_unfenced(
        &self,
        method: &str,
        path: &str,
        auth: Auth,
        prio: Priority,
        body: Option<&serde_json::Value>,
    ) -> Result<Response, Error> {
        if !self.execution_gates.is_empty() && !matches!(method, "GET" | "HEAD") {
            return Err(Error::Refused(
                "scoped execution cannot send an unfenced write".into(),
            ));
        }
        let body = body.map(|b| b.to_string().into_bytes());
        let idempotent = matches!(method, "GET" | "DELETE");
        self.send_inner(method, path, auth, prio, body, idempotent, false, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn send_inner(
        &self,
        method: &str,
        path: &str,
        auth: Auth,
        prio: Priority,
        body: Option<Vec<u8>>,
        retry_errors: bool,
        fenced: bool,
        mut permit: Option<&mut CosmeticPermit>,
    ) -> Result<Response, Error> {
        let mut url = self.url(path)?;
        let policy = self.inner.cfg.retry.clone();
        let mut cache_key = format!("{auth:?} {url}");
        let mut attempt = 0u32;
        let mut not_found = 0u32;
        let mut auth_retry = true;
        let mut rate_waited = Duration::ZERO;
        let mut redirects = 0u32;
        loop {
            if let Some(key) = self.probe_cached_key(&url, auth, prio, method)
                && let Some(response) = self.probe_memo.as_ref().and_then(|m| m.get(&key))
            {
                return Ok(response);
            }
            if permit.is_none() {
                self.admit(prio, path)?;
            }
            let mut headers = vec![
                (
                    "accept".to_string(),
                    "application/vnd.github+json".to_string(),
                ),
                ("x-github-api-version".to_string(), "2022-11-28".to_string()),
            ];
            match auth {
                Auth::App => headers.push((
                    "authorization".into(),
                    format!("Bearer {}", self.app_jwt()?),
                )),
                Auth::Installation(id) => headers.push((
                    "authorization".into(),
                    format!("token {}", self.installation_token(id)?),
                )),
                Auth::None => {}
            }
            let probe_key =
                if method == "GET" && prio == Priority::Background && self.probe_memo.is_some() {
                    headers
                        .iter()
                        .find(|(k, _)| k == "authorization")
                        .map(|(_, value)| Self::probe_key(&url, value))
                } else {
                    None
                };
            if body.is_some() {
                headers.push(("content-type".into(), "application/json".into()));
            }
            let cached = (method == "GET")
                .then(|| {
                    self.inner
                        .etags
                        .lock()
                        .expect("etags")
                        .get(&cache_key)
                        .cloned()
                })
                .flatten();
            if let Some((etag, _)) = &cached {
                headers.push(("if-none-match".into(), etag.clone()));
            }
            if fenced && method != "GET" && method != "HEAD" {
                let actual_path = url
                    .strip_prefix(self.base())
                    .ok_or_else(|| Error::Refused("write destination escaped API host".into()))?;
                for gate in &self.execution_gates {
                    gate.permit_body(method, actual_path, body.as_deref())
                        .map_err(|why| {
                            self.bump(|s| s.writes_refused += 1);
                            Error::Refused(format!("{method} {actual_path} not sent: {why}"))
                        })?;
                }
                let gate = self
                    .inner
                    .gate
                    .read()
                    .unwrap_or_else(|p| p.into_inner())
                    .clone();
                if let Some(gate) = gate {
                    gate.permit_body(method, actual_path, body.as_deref())
                        .map_err(|why| {
                            self.bump(|s| s.writes_refused += 1);
                            Error::Refused(format!("{method} {actual_path} not sent: {why}"))
                        })?;
                }
            }
            if let Some(p) = permit.take() {
                self.use_cosmetic_permit(p, path)?;
            }
            self.bump(|s| s.requests += 1);
            let resp = match self.raw(method, &url, &headers, body.as_deref()) {
                Ok(r) => r,
                Err(detail) => {
                    if retry_errors && attempt + 1 < policy.attempts {
                        attempt += 1;
                        self.bump(|s| s.retries += 1);
                        std::thread::sleep(self.jitter(attempt));
                        continue;
                    }
                    return Err(if retry_errors {
                        Error::Unavailable {
                            method: method.into(),
                            path: path.into(),
                            detail,
                        }
                    } else {
                        self.bump(|s| s.ambiguous += 1);
                        Error::Ambiguous {
                            method: method.into(),
                            path: path.into(),
                            detail,
                        }
                    });
                }
            };
            if fenced && method != "GET" && method != "HEAD" {
                let actual_path = url
                    .strip_prefix(self.base())
                    .ok_or_else(|| Error::Refused("write destination escaped API host".into()))?;
                for gate in &self.execution_gates {
                    gate.response(method, actual_path, resp.status, &resp.body)
                        .map_err(|why| {
                            Error::Refused(format!(
                                "{method} {actual_path} requires recovery: {why}"
                            ))
                        })?;
                }
            }
            {
                let mut s = self.inner.stats.lock().expect("stats");
                if let Some(r) = resp
                    .header("x-ratelimit-remaining")
                    .and_then(|v| v.parse().ok())
                {
                    s.remaining = Some(r);
                }
                if let Some(l) = resp
                    .header("x-ratelimit-limit")
                    .and_then(|v| v.parse().ok())
                {
                    s.limit = Some(l);
                }
            }
            if let Some(wait) = Self::rate_limit_wait(&resp) {
                self.bump(|s| s.rate_limited += 1);
                self.inner.budget.lock().expect("budget").block_for(wait);
                if prio == Priority::Background {
                    return Err(Error::RateLimited {
                        path: path.into(),
                        wait_secs: wait.as_secs(),
                    });
                }
                if prio == Priority::Cosmetic {
                    return Err(Error::Deferred { path: path.into() });
                }
                if !retry_errors {
                    // A write: GitHub did not carry it out, but the world may change while we
                    // wait. The caller decides again from fresh reads.
                    self.bump(|s| s.writes_rate_limited += 1);
                    return Err(Error::RateLimited {
                        path: path.into(),
                        wait_secs: wait.as_secs(),
                    });
                }
                if rate_waited + wait > policy.max_rate_limit_wait {
                    return Err(Error::RateLimited {
                        path: path.into(),
                        wait_secs: wait.as_secs(),
                    });
                }
                rate_waited += wait;
                // `admit` sleeps until the block ends.
                continue;
            }
            if resp.status == 304
                && let Some((_, mut r)) = cached
            {
                self.bump(|s| s.not_modified += 1);
                r.cached = true;
                if let Some(key) = probe_key
                    && let Some(memo) = &self.probe_memo
                {
                    memo.put(key, &r)?;
                }
                return Ok(r);
            }
            if matches!(resp.status, 301 | 302 | 307 | 308) {
                // A renamed repository. Reads follow any of these; a write only follows 307
                // and 308, which keep the method and body.
                let follow = method == "GET" || matches!(resp.status, 307 | 308);
                if let Some(loc) = resp.header("location").map(str::to_string)
                    && follow
                    && redirects < 3
                {
                    let next = if loc.starts_with('/') {
                        format!("{}{loc}", self.base())
                    } else {
                        loc
                    };
                    url = self.url(&next)?;
                    cache_key = format!("{auth:?} {url}");
                    redirects += 1;
                    self.bump(|s| s.redirects += 1);
                    continue;
                }
                return Err(Error::Status {
                    method: method.into(),
                    path: path.into(),
                    status: resp.status,
                    message: format!(
                        "redirect not followed (to {})",
                        resp.header("location").unwrap_or("?")
                    ),
                });
            }
            if resp.status == 401 && auth_retry {
                if let Auth::Installation(id) = auth {
                    self.forget_token(id);
                }
                auth_retry = false;
                continue;
            }
            if resp.status >= 500 {
                if retry_errors && attempt + 1 < policy.attempts {
                    attempt += 1;
                    self.bump(|s| s.retries += 1);
                    std::thread::sleep(self.jitter(attempt));
                    continue;
                }
                let detail = format!("HTTP {}", resp.status);
                return Err(if retry_errors {
                    Error::Unavailable {
                        method: method.into(),
                        path: path.into(),
                        detail,
                    }
                } else {
                    self.bump(|s| s.ambiguous += 1);
                    Error::Ambiguous {
                        method: method.into(),
                        path: path.into(),
                        detail,
                    }
                });
            }
            if resp.status == 404 {
                if retry_errors && not_found + 1 < policy.not_found_attempts {
                    not_found += 1;
                    self.bump(|s| s.retries += 1);
                    std::thread::sleep(self.jitter(not_found));
                    continue;
                }
                return Err(Error::NotFound {
                    method: method.into(),
                    path: path.into(),
                });
            }
            if resp.status >= 400 {
                let message = serde_json::from_slice::<serde_json::Value>(&resp.body)
                    .ok()
                    .and_then(|v| {
                        v.get("message")
                            .and_then(|m| m.as_str())
                            .map(str::to_string)
                    })
                    .unwrap_or_else(|| {
                        String::from_utf8_lossy(&resp.body)
                            .chars()
                            .take(200)
                            .collect()
                    });
                return Err(Error::Status {
                    method: method.into(),
                    path: path.into(),
                    status: resp.status,
                    message,
                });
            }
            if method == "GET"
                && let Some(etag) = resp.header("etag")
            {
                let mut cache = self.inner.etags.lock().expect("etags");
                if cache.len() >= ETAG_CACHE_MAX {
                    cache.clear();
                }
                cache.insert(cache_key.clone(), (etag.to_string(), resp.clone()));
            }
            if let Some(key) = probe_key
                && let Some(memo) = &self.probe_memo
            {
                memo.put(key, &resp)?;
            }
            return Ok(resp);
        }
    }

    pub fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        auth: Auth,
        prio: Priority,
    ) -> Result<T, Error> {
        self.send("GET", path, auth, prio, None)?.json(path)
    }

    /// Follows `Link: rel="next"` up to `max_pages`; `extract` takes the items of one page.
    pub fn get_paginated<T: DeserializeOwned>(
        &self,
        path: &str,
        auth: Auth,
        prio: Priority,
        max_pages: usize,
        extract: impl Fn(serde_json::Value) -> Option<serde_json::Value>,
    ) -> Result<Vec<T>, Error> {
        let initial = self.url(path)?;
        let initial_key = self
            .pagination_key(&initial)
            .ok_or_else(|| Error::Refused("invalid initial pagination URL".into()))?;
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        while let Some(p) = next.take() {
            let resp = self.send("GET", &p, auth, prio, None)?;
            let v: serde_json::Value = resp.json(&p)?;
            let items = extract(v).ok_or_else(|| Error::Decode {
                path: p.clone(),
                detail: "missing paginated list".into(),
            })?;
            let items: Vec<T> = serde_json::from_value(items).map_err(|e| Error::Decode {
                path: p.clone(),
                detail: e.to_string(),
            })?;
            out.extend(items);
            pages += 1;
            next = resp.next_link();
            if next.is_some() && pages >= max_pages {
                return Err(Error::Decode {
                    path: p,
                    detail: "pagination limit exceeded".into(),
                });
            }
            if let Some(n) = &next
                && !self.on_api_host(n)
            {
                return Err(Error::Refused(format!(
                    "{path}: the next page {n} is not on the API host; not following it"
                )));
            }
            if let Some(n) = &next {
                let key = self
                    .pagination_key(n)
                    .ok_or_else(|| Error::Refused("invalid next-page URL".into()))?;
                if key.0 != initial_key.0
                    || key.1 != initial_key.1
                    || key.2 != initial_key.2 + pages
                {
                    return Err(Error::Refused(
                        "next-page URL changed path, query or page sequence".into(),
                    ));
                }
            }
        }
        Ok(out)
    }

    /// Reads a wrapped GitHub list completely, rejecting malformed or changing counts and
    /// any remaining page beyond the bound. Required check identities must never be partial.
    pub fn get_paginated_checked<T: DeserializeOwned>(
        &self,
        path: &str,
        auth: Auth,
        prio: Priority,
        max_pages: usize,
        key: &str,
    ) -> Result<Vec<T>, Error> {
        let initial = self.url(path)?;
        let initial_key = self
            .pagination_key(&initial)
            .ok_or_else(|| Error::Refused("invalid initial pagination URL".into()))?;
        if initial_key.2 != 1 {
            return Err(Error::Refused("pagination must start at page 1".into()));
        }
        let mut out = Vec::new();
        let mut next = Some(path.to_string());
        let mut pages = 0;
        let mut total = None;
        while let Some(p) = next.take() {
            if pages >= max_pages {
                return Err(Error::Decode {
                    path: p,
                    detail: "pagination limit exceeded".into(),
                });
            }
            let response = self.send("GET", &p, auth, prio, None)?;
            let value: serde_json::Value = response.json(&p)?;
            let count = value
                .get("total_count")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| Error::Decode {
                    path: p.clone(),
                    detail: "missing total_count".into(),
                })?;
            if total.is_some_and(|old| old != count) {
                return Err(Error::Decode {
                    path: p,
                    detail: "total_count changed during pagination".into(),
                });
            }
            total = Some(count);
            let items = value.get(key).cloned().ok_or_else(|| Error::Decode {
                path: p.clone(),
                detail: format!("missing {key} list"),
            })?;
            let items: Vec<T> = serde_json::from_value(items).map_err(|e| Error::Decode {
                path: p.clone(),
                detail: e.to_string(),
            })?;
            out.extend(items);
            pages += 1;
            next = response.next_link();
            if next.as_ref().is_some_and(|n| !self.on_api_host(n)) {
                return Err(Error::Refused(format!(
                    "{path}: next page is not on the API host"
                )));
            }
            if let Some(n) = &next {
                let key = self
                    .pagination_key(n)
                    .ok_or_else(|| Error::Refused("invalid next-page URL".into()))?;
                if key.0 != initial_key.0 || key.1 != initial_key.1 || key.2 != pages + 1 {
                    return Err(Error::Refused(
                        "next-page URL changed path, query or page sequence".into(),
                    ));
                }
            }
        }
        if total != Some(out.len() as u64) {
            return Err(Error::Decode {
                path: path.into(),
                detail: "pagination count mismatch".into(),
            });
        }
        Ok(out)
    }

    fn pagination_key(&self, url: &str) -> Option<(String, Vec<String>, usize)> {
        let relative = url.strip_prefix(self.base())?;
        if !relative.starts_with('/') || relative.contains('#') {
            return None;
        }
        let (path, query) = relative.split_once('?').unwrap_or((relative, ""));
        let mut page = None;
        let mut params = Vec::new();
        for part in query.split('&').filter(|p| !p.is_empty()) {
            if let Some(value) = part.strip_prefix("page=") {
                if page.is_some() {
                    return None;
                }
                page = Some(value.parse::<usize>().ok()?);
            } else {
                params.push(part.to_string());
            }
        }
        params.sort();
        Some((path.to_string(), params, page.unwrap_or(1)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_memo_is_isolated_bounded_and_keeps_pagination_headers() {
        use std::sync::atomic::AtomicUsize;
        let total = Arc::new(AtomicUsize::new(0));
        let a = Arc::new(ProbeMemo::new(total.clone()));
        let b = ProbeMemo::new(total.clone());
        let key = Client::probe_key("https://api.example.test/a?page=1", "token first");
        assert_ne!(
            key,
            Client::probe_key("https://api.example.test/a?page=2", "token first")
        );
        assert_ne!(
            key,
            Client::probe_key("https://api.example.test/a?page=1", "token second")
        );
        assert!(!key.contains("token first"));
        let response = Response {
            status: 200,
            body: b"page one".to_vec(),
            headers: vec![(
                "link".into(),
                "<https://api.example.test/a?page=2>; rel=\"next\"".into(),
            )],
            cached: false,
        };
        a.put(key.clone(), &response).unwrap();
        assert_eq!(a.get(&key).unwrap().headers, response.headers);
        assert!(
            b.get(&key).is_none(),
            "separate probes do not share cached data"
        );
        let shared = a.clone();
        drop(a);
        assert!(
            total.load(Ordering::SeqCst) > 0,
            "live probe retains its budget"
        );
        drop(shared);
        assert_eq!(total.load(Ordering::SeqCst), 0);
        let oversized = Response {
            body: vec![0; PROBE_MEMO_MAX_BYTES],
            ..response.clone()
        };
        assert!(b.put("large".into(), &oversized).is_err());
        for n in 0..PROBE_MEMO_MAX_ENTRIES {
            b.put(format!("page-{n}"), &response).unwrap();
        }
        assert!(b.put("one-too-many".into(), &response).is_err());
    }

    #[test]
    fn probe_memos_share_an_aggregate_memory_limit() {
        use std::sync::atomic::AtomicUsize;
        let total = Arc::new(AtomicUsize::new(0));
        let mut memos = Vec::new();
        for i in 0..8 {
            let memo = ProbeMemo::new(total.clone());
            let response = Response {
                status: 200,
                body: vec![0; PROBE_MEMO_MAX_BYTES - 100],
                headers: vec![],
                cached: false,
            };
            memo.put(format!("key-{i}"), &response).unwrap();
            memos.push(memo);
        }
        let overflow = ProbeMemo::new(total.clone());
        let response = Response {
            status: 200,
            body: vec![0; 1024],
            headers: vec![],
            cached: false,
        };
        assert!(overflow.put("overflow".into(), &response).is_err());
        drop(memos);
        assert_eq!(total.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn concurrent_probe_memos_cannot_exceed_the_shared_limit() {
        use std::sync::atomic::AtomicUsize;
        let total = Arc::new(AtomicUsize::new(0));
        let held = Arc::new(Mutex::new(Vec::new()));
        std::thread::scope(|scope| {
            for i in 0..16 {
                let total = total.clone();
                let held = held.clone();
                scope.spawn(move || {
                    let memo = ProbeMemo::new(total);
                    let response = Response {
                        status: 200,
                        body: vec![0; PROBE_MEMO_MAX_BYTES - 100],
                        headers: vec![],
                        cached: false,
                    };
                    if memo.put(format!("key-{i}"), &response).is_ok() {
                        held.lock().unwrap().push(memo);
                    }
                });
            }
        });
        assert_eq!(held.lock().unwrap().len(), 8);
        assert!(total.load(Ordering::SeqCst) <= PROBE_MEMO_MAX_TOTAL_BYTES);
        held.lock().unwrap().clear();
        assert_eq!(total.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn budget_lets_landing_through_and_defers_cosmetics() {
        let mut b = Budget::new(BudgetConfig {
            capacity: 3.0,
            refill_per_sec: 0.0001,
            cosmetic_reserve: 2.0,
        });
        // 3 tokens: one cosmetic fits (3 >= 2 + 1), then cosmetics are refused.
        assert!(matches!(b.admit(Priority::Cosmetic), Admit::Go));
        assert!(matches!(b.admit(Priority::Cosmetic), Admit::Deny));
        assert!(matches!(b.admit(Priority::Normal), Admit::Go));
        assert!(matches!(b.admit(Priority::Normal), Admit::Go));
        assert!(matches!(b.admit(Priority::Normal), Admit::Wait(_)));
        // Landing goes into debt.
        assert!(matches!(b.admit(Priority::Landing), Admit::Go));
        assert!(b.tokens < 0.0);
        b.block_for(Duration::from_secs(30));
        assert!(matches!(b.admit(Priority::Cosmetic), Admit::Deny));
        assert!(matches!(b.admit(Priority::Landing), Admit::Wait(_)));
    }

    #[test]
    fn watchdog_reads_leave_budget_for_reconciliation() {
        let mut b = Budget::new(BudgetConfig {
            capacity: 3.0,
            refill_per_sec: 0.0001,
            cosmetic_reserve: 2.0,
        });
        assert!(matches!(b.admit(Priority::Background), Admit::Go));
        assert!(matches!(b.admit(Priority::Background), Admit::Deny));
        assert!(matches!(b.admit(Priority::Normal), Admit::Go));
        assert!(matches!(b.admit(Priority::Normal), Admit::Go));
        b.block_for(Duration::from_secs(30));
        assert!(matches!(b.admit(Priority::Background), Admit::Deny));
    }

    #[test]
    fn rate_limit_detection() {
        let r = |status, headers: &[(&str, &str)], body: &str| Response {
            status,
            body: body.as_bytes().to_vec(),
            headers: headers
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            cached: false,
        };
        assert_eq!(
            Client::rate_limit_wait(&r(429, &[("retry-after", "7")], "")),
            Some(Duration::from_secs(7))
        );
        assert_eq!(
            Client::rate_limit_wait(&r(
                403,
                &[],
                r#"{"message":"You have exceeded a secondary rate limit"}"#
            )),
            Some(Duration::from_secs(60))
        );
        assert!(
            Client::rate_limit_wait(&r(
                403,
                &[("x-ratelimit-remaining", "0"), ("x-ratelimit-reset", "1")],
                ""
            ))
            .is_some()
        );
        assert_eq!(
            Client::rate_limit_wait(&r(403, &[], r#"{"message":"Resource not accessible"}"#)),
            None
        );
        assert_eq!(Client::rate_limit_wait(&r(200, &[], "")), None);
    }

    #[test]
    fn link_header_next() {
        let r = Response {
            status: 200,
            body: vec![],
            headers: vec![(
                "link".into(),
                r#"<https://x/a?page=2>; rel="next", <https://x/a?page=5>; rel="last""#.into(),
            )],
            cached: false,
        };
        assert_eq!(r.next_link().as_deref(), Some("https://x/a?page=2"));
    }

    #[test]
    fn path_encoding_keeps_slashes() {
        assert_eq!(
            enc_path("mergify/merge-queue/ab12"),
            "mergify/merge-queue/ab12"
        );
        assert_eq!(enc_path("a b#c"), "a%20b%23c");
        assert_eq!(enc_query("o:mergify/x"), "o%3Amergify%2Fx");
    }

    #[test]
    fn provisional_errors() {
        assert!(
            Error::NotFound {
                method: "GET".into(),
                path: "/x".into()
            }
            .is_provisional()
        );
        assert!(
            !Error::Status {
                method: "PUT".into(),
                path: "/x".into(),
                status: 405,
                message: String::new()
            }
            .is_provisional()
        );
    }
}
