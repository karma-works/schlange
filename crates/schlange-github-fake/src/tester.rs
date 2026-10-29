//! A tester DSL in the style of rust-lang/bors' test suite: it starts the fake GitHub and
//! `schlange serve` (the real webhook router, workers and GitHub client) against it, and
//! offers short verbs for what users and CI do.
//!
//! ```ignore
//! let t = Tester::new(ARZTAGENT).start();
//! let pr = t.open_pr("arztagent-bot").file("backend/a.py", "a = 1\n").create();
//! t.pr(pr).check("ci-ok").succeed();
//! t.advance("5m");
//! t.queue_pr().check("ci-ok").succeed();
//! t.pr(pr).wait_merged();
//! t.assert_invariants();
//! ```
//!
//! Time: the service runs on a [`ManualClock`]; `advance` moves it (batch wait times, the
//! reconciliation period). Everything else is real: HTTP, signatures, threads.

use std::sync::Arc;
use std::time::{Duration, Instant};

use schlange_forge::git::Tree;
use schlange_github::RepoName;
use schlange_github::client::{BudgetConfig, Client, ClientConfig, RetryPolicy};
use schlange_github::config::ConfigCheck;
use schlange_github::jwt::AppKey;
use schlange_github::lease::LeaseConfig;
use schlange_github::service::{ConfigSource, CrashPoint, PauseHook, ServeConfig, Service};
use schlange_github::shadow::ShadowConfig;
use schlange_github::time::ManualClock;

use crate::{
    CiRule, FakeConfig, FakeGithub, FakePr, FaultRule, HookFaults, MergeGuard, TEST_APP_KEY,
};

/// The default timeout of `wait_*`.
pub const WAIT: Duration = Duration::from_secs(20);

pub struct Tester {
    config_yaml: String,
    fake_cfg: FakeConfig,
    ci: Vec<CiRule>,
    reconcile_every: u64,
    hook_faults: HookFaults,
    faults: Vec<FaultRule>,
    budget: BudgetConfig,
    /// Serve the configuration from the repository (`.mergify.yml` in the base) instead of
    /// handing it over directly.
    from_repo: bool,
    merge_guard: Option<MergeGuard>,
    persistent: bool,
    crash: Option<CrashPoint>,
    shadow: bool,
    lease: Option<LeaseConfig>,
    pause_before_land: Option<Arc<PauseHook>>,
    watchdog_pause: Option<Arc<PauseHook>>,
    watchdog_reconcile_pause: Option<Arc<PauseHook>>,
}

pub struct Running {
    pub gh: FakeGithub,
    pub service: Option<Service>,
    pub clock: Arc<ManualClock>,
    pub client: Client,
    pub serve_cfg: ServeConfig,
}

/// The validator `serve` uses, with the pinned schema of this repository.
pub fn config_check() -> Arc<ConfigCheck> {
    static CHECK: std::sync::OnceLock<Arc<ConfigCheck>> = std::sync::OnceLock::new();
    CHECK
        .get_or_init(|| {
            let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
            Arc::new(
                ConfigCheck::locate(None, &root)
                    .expect("schema missing: run tools/fetch-mergify-sources"),
            )
        })
        .clone()
}

/// A fresh directory under the system temp dir.
pub fn temp_dir(name: &str) -> std::path::PathBuf {
    static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = N.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let d = std::env::temp_dir().join(format!(
        "schlange-{name}-{}-{n}-{}",
        std::process::id(),
        schlange_github::time::unix_now()
    ));
    let _ = std::fs::remove_dir_all(&d);
    d
}

fn secs(d: &str) -> u64 {
    let (n, unit) = d.split_at(d.find(|c: char| !c.is_ascii_digit()).unwrap_or(d.len()));
    let n: u64 = n.parse().expect("number");
    n * match unit {
        "" | "s" => 1,
        "m" => 60,
        "h" => 3600,
        other => panic!("unit {other}"),
    }
}

impl Tester {
    pub fn new(config_yaml: &str) -> Self {
        Tester {
            config_yaml: config_yaml.to_string(),
            fake_cfg: FakeConfig::default(),
            ci: Vec::new(),
            reconcile_every: 3600,
            hook_faults: HookFaults::default(),
            faults: Vec::new(),
            budget: BudgetConfig {
                capacity: 10_000.0,
                refill_per_sec: 10_000.0,
                cosmetic_reserve: 10.0,
            },
            from_repo: true,
            merge_guard: None,
            persistent: false,
            crash: None,
            shadow: false,
            lease: None,
            pause_before_land: None,
            watchdog_pause: None,
            watchdog_reconcile_pause: None,
        }
    }

    /// `serve --shadow`: decide, never write; a write attempt panics (S-SHADOW).
    pub fn shadow(mut self) -> Self {
        self.shadow = true;
        self
    }

    /// `serve` with a lease (MQ-32) under this holder name and time-to-live.
    pub fn lease(mut self, holder: &str, ttl: u64) -> Self {
        self.lease = Some(LeaseConfig {
            holder: holder.into(),
            ttl,
        });
        self
    }

    /// The first `serve` process pauses before its first landing until the hook is released.
    pub fn pause_before_land(mut self, hook: Arc<PauseHook>) -> Self {
        self.pause_before_land = Some(hook);
        self
    }

    /// Tests a state change while an observer is reading.
    pub fn watchdog_pause(mut self, hook: Arc<PauseHook>) -> Self {
        self.watchdog_pause = Some(hook);
        self
    }

    /// Pause just before an observer requests reconciliation.
    pub fn watchdog_reconcile_pause(mut self, hook: Arc<PauseHook>) -> Self {
        self.watchdog_reconcile_pause = Some(hook);
        self
    }

    /// `serve` with a data directory (SQLite journal, snapshots, dedup, audit).
    pub fn persistent(mut self) -> Self {
        self.persistent = true;
        self
    }

    /// The first `serve` process crashes at this point (restart it with `start_service`).
    pub fn crash_at(mut self, c: CrashPoint) -> Self {
        self.crash = Some(c);
        self.persistent = true;
        self
    }

    /// The fake's merge guard: the conditions a pull request must meet when it lands (I1).
    pub fn merge_guard(mut self, g: MergeGuard) -> Self {
        self.merge_guard = Some(g);
        self
    }

    /// Changes the fake GitHub's configuration (permissions, base files, …).
    pub fn fake(mut self, f: impl FnOnce(&mut FakeConfig)) -> Self {
        f(&mut self.fake_cfg);
        self
    }

    pub fn base_file(mut self, path: &str, content: &str) -> Self {
        self.fake_cfg
            .base_files
            .insert(path.to_string(), content.to_string());
        self
    }

    pub fn ci(mut self, rule: CiRule) -> Self {
        self.ci.push(rule);
        self
    }

    /// Seconds between full reconciliations (on the manual clock).
    pub fn reconcile_every(mut self, d: &str) -> Self {
        self.reconcile_every = secs(d);
        self
    }

    pub fn hook_faults(mut self, f: HookFaults) -> Self {
        self.hook_faults = f;
        self
    }

    pub fn fault(mut self, f: FaultRule) -> Self {
        self.faults.push(f);
        self
    }

    pub fn budget(mut self, b: BudgetConfig) -> Self {
        self.budget = b;
        self
    }

    /// Hands the configuration to `serve` instead of reading it from the repository.
    pub fn config_given(mut self) -> Self {
        self.from_repo = false;
        self
    }

    pub fn start(mut self) -> Running {
        if self.from_repo {
            self.fake_cfg
                .base_files
                .insert(".mergify.yml".into(), self.config_yaml.clone());
        }
        let gh = FakeGithub::start(self.fake_cfg.clone());
        gh.set_ci(self.ci.clone());
        gh.set_merge_guard(self.merge_guard.clone());
        gh.set_hook_faults(self.hook_faults.clone());
        for f in &self.faults {
            gh.add_fault(f.clone());
        }
        let client = Client::new(ClientConfig {
            api_base: gh.url(),
            app_id: self.fake_cfg.app_id.to_string(),
            key: Arc::new(AppKey::from_pem(TEST_APP_KEY).expect("test key")),
            retry: RetryPolicy::fast(),
            budget: self.budget.clone(),
            timeout: Duration::from_secs(10),
            allow_insecure_http: true,
        });
        let clock = Arc::new(ManualClock::new(1_800_000_000));
        let serve_cfg = ServeConfig {
            client: client.clone(),
            repos: vec![RepoName::parse(&self.fake_cfg.full_name()).expect("repo")],
            installation: None,
            webhook_secret: self.fake_cfg.webhook_secret.clone(),
            listen: "127.0.0.1:0".parse().expect("addr"),
            reconcile_every: self.reconcile_every,
            config: if self.from_repo {
                ConfigSource::Repository
            } else {
                ConfigSource::Text(self.config_yaml.clone())
            },
            poll: Duration::from_millis(5),
            check_name: "Schlange merge queue".into(),
            retry_base: Duration::from_millis(20),
            retry_max: Duration::from_millis(300),
            check: config_check(),
            data_dir: self.persistent.then(|| temp_dir("data")),
            admin_token: Some("test-admin-token".into()),
            http: Default::default(),
            crash: self.crash,
            shadow: self.shadow.then(|| ShadowConfig {
                panic_on_write: true,
                ..ShadowConfig::default()
            }),
            lease: self.lease.clone(),
            pause_before_land: self.pause_before_land.clone(),
            heartbeat_test_offset: self
                .pause_before_land
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            watchdog_test_observations: self
                .pause_before_land
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            watchdog_test_samples: self
                .pause_before_land
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            watchdog_test_cycles: self
                .pause_before_land
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            watchdog_test_partial_prs: self
                .pause_before_land
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            watchdog_test_completed: self
                .pause_before_land
                .as_ref()
                .map(|_| Arc::new(std::sync::atomic::AtomicU64::new(0))),
            watchdog_test_pause: self.watchdog_pause.clone(),
            watchdog_test_reconcile_pause: self.watchdog_reconcile_pause.clone(),
        };
        let service = Service::start(serve_cfg.clone(), clock.clone()).expect("serve starts");
        gh.set_webhook_target(Some(service.webhook_url()));
        let mut r = Running {
            gh,
            service: Some(service),
            clock,
            client,
            serve_cfg,
        };
        // A restart does not crash again.
        r.serve_cfg.crash = None;
        r.serve_cfg.pause_before_land = None;
        // The start-up reconciliation has run once the worker published its status.
        r.wait_until("start-up reconciliation", |r| {
            r.status()
                .is_some_and(|s| s.reconciliations > 0 || s.config_error.is_some())
        });
        r
    }
}

pub struct PrBuilder<'a> {
    t: &'a Running,
    author: String,
    title: String,
    files: Vec<(String, String)>,
    draft: bool,
    labels: Vec<String>,
    body: String,
    fork: bool,
}

impl PrBuilder<'_> {
    /// The description (`Depends-On`, `Merge-After`).
    pub fn body(mut self, b: &str) -> Self {
        self.body = b.into();
        self
    }

    /// Opened from a fork.
    pub fn fork(mut self) -> Self {
        self.fork = true;
        self
    }

    pub fn file(mut self, path: &str, content: &str) -> Self {
        self.files.push((path.into(), content.into()));
        self
    }

    pub fn title(mut self, t: &str) -> Self {
        self.title = t.into();
        self
    }

    pub fn draft(mut self) -> Self {
        self.draft = true;
        self
    }

    pub fn label(mut self, l: &str) -> Self {
        self.labels.push(l.into());
        self
    }

    pub fn create(self) -> u64 {
        let files = if self.files.is_empty() {
            vec![(
                format!("change-{}.txt", self.title.len()),
                self.title.clone(),
            )]
        } else {
            self.files
        };
        self.t.gh.open_pr_with(
            &self.author,
            &self.title,
            &files,
            self.draft,
            &self.labels,
            &self.body,
            self.fork,
        )
    }
}

pub struct PrHandle<'a> {
    t: &'a Running,
    pub number: u64,
}

pub struct CheckHandle<'a> {
    t: &'a Running,
    number: u64,
    name: String,
}

impl CheckHandle<'_> {
    pub fn succeed(&self) -> u64 {
        self.t
            .gh
            .report_check(self.number, &self.name, Some("success"))
    }

    pub fn fail(&self) -> u64 {
        self.t
            .gh
            .report_check(self.number, &self.name, Some("failure"))
    }

    pub fn conclude(&self, c: &str) -> u64 {
        self.t.gh.report_check(self.number, &self.name, Some(c))
    }

    pub fn start(&self) -> u64 {
        self.t.gh.report_check(self.number, &self.name, None)
    }
}

impl PrHandle<'_> {
    pub fn check(&self, name: &str) -> CheckHandle<'_> {
        CheckHandle {
            t: self.t,
            number: self.number,
            name: name.into(),
        }
    }

    pub fn get(&self) -> FakePr {
        self.t.gh.pr(self.number).expect("pr exists")
    }

    pub fn push(&self, path: &str, content: &str) {
        self.t
            .gh
            .push_pr(self.number, &[(path.into(), content.into())]);
    }

    pub fn label(&self, l: &str) {
        self.t.gh.label(self.number, l, true);
    }

    /// `user` comments `body`; returns the comment id.
    pub fn comment(&self, user: &str, body: &str) -> u64 {
        self.t.gh.comment(self.number, user, body)
    }

    /// `user` ticks the checkbox in the queue's comment with `marker`
    /// (`schlange_core::commands::CONTROLS_MARKER` or `STATUS_MARKER`); waits for the comment.
    pub fn tick(&self, user: &str, marker: &str) {
        let n = self.number;
        let m = marker.to_string();
        self.t.wait_until(&format!("a checkbox in #{n}"), |t| {
            t.gh.comments_on(n)
                .iter()
                .any(|c| c.body.contains(&m) && c.body.contains("- [ ]"))
        });
        assert!(self.t.gh.tick(n, user, marker), "no checkbox to tick");
    }

    /// `user` pushes a commit.
    pub fn push_as(&self, user: &str, path: &str, content: &str) {
        self.t
            .gh
            .push_pr_as(self.number, user, &[(path.into(), content.into())]);
    }

    pub fn edit_body(&self, body: &str) {
        self.t.gh.edit_body(self.number, body);
    }

    pub fn approve(&self, user: &str) {
        self.t.gh.review(self.number, user, "APPROVED");
    }

    /// The comments on this pull request (bodies).
    pub fn comments(&self) -> Vec<String> {
        self.t
            .gh
            .comments_on(self.number)
            .into_iter()
            .map(|c| c.body)
            .collect()
    }

    /// Waits until a comment contains `text`.
    pub fn wait_comment(&self, text: &str) {
        let n = self.number;
        let text = text.to_string();
        self.t
            .wait_until(&format!("a comment on #{n} with {text:?}"), |t| {
                t.gh.comments_on(n).iter().any(|c| c.body.contains(&text))
            });
    }

    pub fn unlabel(&self, l: &str) {
        self.t.gh.label(self.number, l, false);
    }

    pub fn close(&self) {
        self.t.gh.close_pr(self.number);
    }

    pub fn wait_merged(&self) {
        let n = self.number;
        self.t.wait_until(&format!("#{n} merged"), |t| {
            t.gh.pr(n).is_some_and(|p| p.merged)
        });
    }

    /// Waits until the pull request is queued at its current head.
    pub fn wait_queued(&self) {
        let n = self.number;
        self.t.wait_until(&format!("#{n} queued"), |t| {
            let head = t.gh.pr(n).map(|p| p.head_sha).unwrap_or_default();
            t.status().is_some_and(|s| {
                s.state["queued"][n.to_string()]["head_sha"].as_str() == Some(head.as_str())
            })
        });
    }

    /// The queue's own check run on this pull request.
    pub fn queue_check(&self) -> Option<crate::FakeRun> {
        self.t.gh.queue_check(self.number)
    }
}

impl Running {
    pub fn open_pr(&self, author: &str) -> PrBuilder<'_> {
        PrBuilder {
            t: self,
            author: author.into(),
            title: format!("change by {author}"),
            files: vec![],
            draft: false,
            labels: vec![],
            body: String::new(),
            fork: false,
        }
    }

    /// A repository permission of a login (`collaborators/{u}/permission`).
    pub fn permission(&self, login: &str, permission: &str) {
        self.gh.set_collaborator(login, permission);
    }

    pub fn pr(&self, number: u64) -> PrHandle<'_> {
        PrHandle { t: self, number }
    }

    /// The open queue draft (waits for exactly one).
    pub fn queue_pr(&self) -> PrHandle<'_> {
        self.wait_until("an open queue draft", |t| {
            t.gh.open_queue_drafts().len() == 1
        });
        let d = self.gh.open_queue_drafts().remove(0);
        PrHandle {
            t: self,
            number: d.number,
        }
    }

    /// Moves the service's clock.
    pub fn advance(&self, d: &str) {
        self.clock.advance(secs(d));
    }

    /// Calls `serve`'s queue API (`/repos/{owner}/{repo}/<path>`) with the admin token; the
    /// status code and the body.
    pub fn admin(&self, method: &str, path: &str, body: Option<&str>) -> (u16, String) {
        self.admin_as(method, path, body, Some("test-admin-token"))
    }

    /// [`Running::admin`] with another (or no) bearer token.
    pub fn admin_as(
        &self,
        method: &str,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> (u16, String) {
        let base = self
            .service
            .as_ref()
            .expect("serve runs")
            .webhook_url()
            .trim_end_matches("/webhook")
            .to_string();
        let url = format!("{base}/repos/{}/{path}", self.gh.config().full_name());
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(std::time::Duration::from_secs(40)))
                .build(),
        );
        let auth = token.map(|t| format!("Bearer {t}")).unwrap_or_default();
        let resp = match (method, body) {
            ("GET", _) => agent.get(&url).header("authorization", &auth).call(),
            ("DELETE", _) => agent.delete(&url).header("authorization", &auth).call(),
            ("PUT", b) => agent
                .put(&url)
                .header("authorization", &auth)
                .header("content-type", "application/json")
                .send(b.unwrap_or("")),
            (_, b) => agent
                .post(&url)
                .header("authorization", &auth)
                .header("content-type", "application/json")
                .send(b.unwrap_or("")),
        };
        match resp {
            Ok(mut r) => {
                let status = r.status().as_u16();
                (status, r.body_mut().read_to_string().unwrap_or_default())
            }
            Err(e) => (0, e.to_string()),
        }
    }

    pub fn status(&self) -> Option<schlange_github::service::WorkerStatus> {
        self.service
            .as_ref()?
            .status()
            .get(&self.gh.config().full_name())
            .cloned()
    }

    /// Waits until webhooks are delivered and handled, and the worker has gone around its
    /// loop a few times without new input.
    pub fn settle(&self) {
        let deadline = Instant::now() + WAIT;
        let mut quiet = 0;
        while quiet < 5 {
            assert!(Instant::now() < deadline, "did not settle");
            let idle = self.gh.hooks_idle()
                && self.service.as_ref().is_none_or(|s| s.inflight() == 0)
                && self.status().is_none_or(|s| s.pending.is_empty());
            quiet = if idle { quiet + 1 } else { 0 };
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    pub fn wait_until(&self, what: &str, f: impl Fn(&Running) -> bool) {
        let deadline = Instant::now() + WAIT;
        while !f(self) {
            if Instant::now() > deadline {
                let log = self
                    .status()
                    .map(|s| s.log_tail.join("\n  "))
                    .unwrap_or_default();
                panic!("timed out waiting for: {what}\nservice log:\n  {log}");
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Stops `serve` (as a crash would: in-memory state is lost).
    pub fn stop_service(&mut self) {
        if let Some(s) = self.service.take() {
            s.shutdown();
        }
        self.gh.set_webhook_target(None);
    }

    /// Starts `serve` again; it recovers from GitHub's state by reconciliation.
    pub fn start_service(&mut self) {
        let service =
            Service::start(self.serve_cfg.clone(), self.clock.clone()).expect("serve starts");
        self.gh.set_webhook_target(Some(service.webhook_url()));
        self.service = Some(service);
        self.wait_until("start-up reconciliation", |r| {
            r.status()
                .is_some_and(|s| s.reconciliations > 0 || s.config_error.is_some())
        });
    }

    /// Another `serve` process against the same fake GitHub, with its own client and clock
    /// (another host's clock may run ahead), the same configuration, and `holder` as its lease name (MQ-32). Webhooks keep going to
    /// the first process; the second one reconciles.
    pub fn second_service(
        &self,
        holder: &str,
        data_dir: Option<std::path::PathBuf>,
        clock: Arc<ManualClock>,
    ) -> Service {
        let mut cfg = self.serve_cfg.clone();
        cfg.client = Client::new(self.client.config().clone());
        if let Some(l) = &mut cfg.lease {
            l.holder = holder.into();
        }
        cfg.data_dir = data_dir;
        cfg.pause_before_land = None;
        Service::start(cfg, clock).expect("second serve starts")
    }

    /// I0, I1, MQ-35 and (at quiescence) I3 on the fake's git state, and I1 against the
    /// engine: no pull request the queue dequeued (its last outcome) was landed by the queue.
    pub fn assert_invariants(&self) {
        self.settle();
        let mut v = self.gh.invariant_violations(true);
        v.extend(self.dequeued_but_landed());
        assert!(v.is_empty(), "invariant violations:\n{}", v.join("\n"));
    }

    /// I1 against the engine's outcomes: squashed by the queue, but dequeued.
    pub fn dequeued_but_landed(&self) -> Vec<String> {
        let Some(s) = self.status() else {
            return vec![];
        };
        self.gh
            .squashes()
            .iter()
            .filter(|q| s.state["outcomes"][q.pr.to_string()]["outcome"] == "dequeued")
            .map(|q| {
                format!(
                    "I1: #{} landed after the queue dequeued it ({})",
                    q.pr,
                    s.state["outcomes"][q.pr.to_string()]["reason"]
                )
            })
            .collect()
    }

    /// The code of a pull request's last dequeue, if its last outcome is a dequeue.
    pub fn dequeue_code(&self, n: u64) -> Option<String> {
        let s = self.status()?;
        let o = &s.state["outcomes"][n.to_string()];
        (o["outcome"] == "dequeued").then(|| o["reason"]["code"].as_str().unwrap_or("").to_string())
    }

    /// The engine's decisions so far (JSON lines).
    pub fn decisions(&self) -> Vec<String> {
        self.service
            .as_ref()
            .map(|s| s.decision_log(&self.gh.config().full_name()))
            .unwrap_or_default()
    }

    /// Waits until the worker stopped (an injected crash).
    pub fn wait_dead(&self) {
        self.wait_until("the worker stopped", |t| {
            t.service
                .as_ref()
                .is_some_and(|s| !s.dead_workers().is_empty())
        });
    }

    /// The base branch's tree (path → content).
    pub fn base_tree(&self) -> Tree {
        let tip = self.gh.branch_sha("main").expect("main");
        self.gh.tree_of(&tip)
    }

    pub fn shutdown(mut self) {
        self.stop_service();
        self.gh.shutdown();
        if let Some(d) = &self.serve_cfg.data_dir {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}
