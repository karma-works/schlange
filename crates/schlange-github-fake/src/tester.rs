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
use schlange_github::jwt::AppKey;
use schlange_github::service::{ConfigSource, ServeConfig, Service};
use schlange_github::time::ManualClock;

use crate::{CiRule, FakeConfig, FakeGithub, FakePr, FaultRule, HookFaults, TEST_APP_KEY};

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
}

pub struct Running {
    pub gh: FakeGithub,
    pub service: Option<Service>,
    pub clock: Arc<ManualClock>,
    pub client: Client,
    serve_cfg: ServeConfig,
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
        }
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
        };
        let service = Service::start(serve_cfg.clone(), clock.clone()).expect("serve starts");
        gh.set_webhook_target(Some(service.webhook_url()));
        let r = Running {
            gh,
            service: Some(service),
            clock,
            client,
            serve_cfg,
        };
        // The start-up reconciliation has run once the worker published its status.
        r.wait_until("start-up reconciliation", |r| {
            r.status().is_some_and(|s| s.reconciliations > 0)
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
            r.status().is_some_and(|s| s.reconciliations > 0)
        });
    }

    /// I0, MQ-35 and (at quiescence) I3 on the fake's git state.
    pub fn assert_invariants(&self) {
        self.settle();
        let v = self.gh.invariant_violations(true);
        assert!(v.is_empty(), "invariant violations:\n{}", v.join("\n"));
    }

    /// The base branch's tree (path → content).
    pub fn base_tree(&self) -> Tree {
        let tip = self.gh.branch_sha("main").expect("main");
        self.gh.tree_of(&tip)
    }

    pub fn shutdown(mut self) {
        self.stop_service();
        self.gh.shutdown();
    }
}
