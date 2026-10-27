//! A fake GitHub for tests, in the spirit of rust-lang/bors' simulated GitHub: an HTTP server
//! with in-memory state that serves the REST endpoints Schlange's client uses, backed by the
//! simulated git of `schlange-forge` (`git::Repo`), so merges, squashes and trees are real
//! model operations. It sends **signed webhooks over HTTP** to Schlange's real webhook router.
//!
//! Faults: API responses with 5xx, 404 storms, rate limits with `Retry-After`, secondary
//! limits, transient 405/409 answers, and "lying" responses (the effect happened, an error is
//! returned); a fault can trigger a side action when it fires (a push to the base, a label), so
//! races are deterministic. Webhooks are lost, duplicated, delayed and therefore reordered.
//!
//! Fidelity beyond the happy path (each off by default, switched on per test):
//! - `mergeable: null` for a number of reads after a push (GitHub computes it lazily);
//! - read-after-write lag: reads of a pull request or a ref return the state before an API
//!   write for a number of reads;
//! - a 403 for merges that change `.github/workflows/` when the token lacks `workflows: write`;
//! - paginated combined statuses (30 per page, `Link` header);
//! - repository renames (301 for reads, 307 for writes to the old name);
//! - installation tokens scoped to repositories and permissions on request.
//!
//! Test actions (opening pull requests, pushing, labelling, reporting check runs) happen
//! directly on the state, as a user or CI would do them on github.com, and emit the webhooks
//! GitHub would send. A CI responder ([`CiRule`]) can report check runs automatically.
//!
//! [`tester`] has a small DSL that starts the fake and `schlange serve` against it.

pub mod http;
pub mod tester;

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use schlange_forge::git::{Repo, Sha, Tree};
use schlange_github::jwt::AppKey;
use schlange_github::time::{format_rfc3339, unix_now};

pub use tester::Tester;

/// The test app's private key (generated for these tests only).
pub const TEST_APP_KEY: &[u8] = include_bytes!("../fixtures/test-app-key.pem");

#[derive(Clone, Debug)]
pub struct FakeConfig {
    pub owner: String,
    pub name: String,
    pub default_branch: String,
    pub base_files: Tree,
    pub app_id: u64,
    pub app_slug: String,
    pub installation_id: u64,
    pub permissions: BTreeMap<String, String>,
    pub webhook_secret: Vec<u8>,
    /// Head branches with this prefix are the queue's (CI rules `on: queue`).
    pub queue_prefix: String,
}

impl Default for FakeConfig {
    fn default() -> Self {
        FakeConfig {
            owner: "karma-works".into(),
            name: "arzt-agent".into(),
            default_branch: "main".into(),
            base_files: [("README.md".to_string(), "hello\n".to_string())].into(),
            app_id: 4242,
            app_slug: "schlange".into(),
            installation_id: 77,
            permissions: schlange_github::permissions::PERMISSIONS
                .iter()
                .filter(|(_, _, req)| *req)
                .map(|(n, l, _)| (n.to_string(), l.to_string()))
                .collect(),
            webhook_secret: b"fake-webhook-secret".to_vec(),
            queue_prefix: "mergify/merge-queue/".into(),
        }
    }
}

impl FakeConfig {
    pub fn full_name(&self) -> String {
        format!("{}/{}", self.owner, self.name)
    }
}

#[derive(Clone, Debug)]
pub struct FakePr {
    pub number: u64,
    pub title: String,
    pub body: String,
    pub user: String,
    pub head_ref: String,
    pub head_sha: Sha,
    pub base: String,
    pub draft: bool,
    pub open: bool,
    pub merged: bool,
    pub merge_commit_sha: Option<Sha>,
    pub labels: BTreeSet<String>,
}

#[derive(Clone, Debug)]
pub struct FakeRun {
    pub id: u64,
    pub name: String,
    pub head_sha: Sha,
    pub status: String,
    pub conclusion: Option<String>,
    pub app_slug: String,
    pub output_title: String,
}

#[derive(Clone, Debug)]
pub struct FakeStatus {
    pub id: u64,
    pub sha: Sha,
    pub context: String,
    pub state: String,
}

/// A squash landing through `PUT /pulls/{n}/merge`.
#[derive(Clone, Debug)]
pub struct Squash {
    pub pr: u64,
    pub sha: Sha,
    pub parent: Sha,
    pub tree: String,
    /// The tree was tested: a queue branch commit with this tree whose draft was green.
    pub tested_green: bool,
    /// The pull request's head when it landed.
    pub head: Sha,
    /// That head was merged into a queue branch (`POST /merges`) whose draft was still open:
    /// the head that landed is a head the queue tested (I1).
    pub head_tested: bool,
    /// What the merge guard said at landing time (`None`: the conditions held).
    pub guard: Option<String>,
}

/// Decides whether a pull request may land right now (I1 at landing time): `Some(why)` when
/// its conditions are false. Tests set it from the configuration they use.
pub type MergeGuard = Arc<dyn Fn(&FakePr) -> Option<String> + Send + Sync>;

/// A merge guard that refuses pull requests carrying any of these labels.
pub fn guard_labels(labels: &[&str]) -> MergeGuard {
    let labels: Vec<String> = labels.iter().map(|l| l.to_string()).collect();
    Arc::new(move |p: &FakePr| {
        labels
            .iter()
            .find(|l| p.labels.contains(*l))
            .map(|l| format!("label `{l}` is set"))
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FaultKind {
    /// Answer with this status, do nothing.
    Status(u16),
    /// 429 with `Retry-After`.
    RateLimit { retry_after: u64 },
    /// 403 "secondary rate limit" with `Retry-After`.
    SecondaryLimit { retry_after: u64 },
    /// Do the request, then answer with this status anyway (MQ-35).
    Lie(u16),
    /// Answer with this status and GitHub's message, do nothing (transient 405/409, 403, 422).
    StatusMessage(u16, String),
}

/// Something that happens on the fake right when a fault fires (on its first hit), before the
/// faulty answer goes out: a race made deterministic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Then {
    /// A direct push to the default branch with these files.
    PushBase(Vec<(String, String)>),
    /// A label is added to a pull request.
    Label(u64, String),
}

#[derive(Clone, Debug)]
pub struct FaultRule {
    /// `GET`, `POST`, … or `*`.
    pub method: String,
    /// Substring of the path (without the query).
    pub path: String,
    pub kind: FaultKind,
    /// How many requests it hits.
    pub times: u32,
    /// Side actions on the first hit.
    pub then: Vec<Then>,
    /// Whether it has fired.
    pub fired: bool,
}

impl FaultRule {
    pub fn new(method: &str, path: &str, kind: FaultKind, times: u32) -> Self {
        FaultRule {
            method: method.into(),
            path: path.into(),
            kind,
            times,
            then: vec![],
            fired: false,
        }
    }

    /// Adds a side action that happens when the fault first fires.
    pub fn then(mut self, t: Then) -> Self {
        self.then.push(t);
        self
    }
}

/// An installation token and what it may do.
#[derive(Clone, Debug)]
pub struct FakeToken {
    pub expires: u64,
    /// Repository names it is limited to (`None`: all of the installation's).
    pub repositories: Option<Vec<String>>,
    /// Its permissions (the installation's, or the subset requested).
    pub permissions: BTreeMap<String, String>,
}

/// Webhook delivery faults (probabilities per delivery).
#[derive(Clone, Debug, Default)]
pub struct HookFaults {
    pub drop: f64,
    pub duplicate: f64,
    /// Each delivery is delayed by up to this much (so deliveries reorder).
    pub max_delay: Duration,
    pub seed: u64,
}

/// Where CI rules run.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CiOn {
    PullRequest,
    Queue,
    Both,
}

pub struct CiCtx<'a> {
    pub number: u64,
    pub author: &'a str,
    pub tree: &'a Tree,
    pub queue: bool,
}

/// Decides a check run's conclusion: `Some("success")` completes it at once, `None` leaves it
/// in progress (complete it with [`FakeGithub::complete_check`]).
pub type CiDecide = Arc<dyn Fn(&CiCtx) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct CiRule {
    pub check: String,
    pub on: CiOn,
    pub decide: CiDecide,
}

impl CiRule {
    /// A check that succeeds, unless a file in the tested tree contains `marker`.
    pub fn fails_on(check: &str, on: CiOn, marker: &str) -> Self {
        let marker = marker.to_string();
        CiRule {
            check: check.into(),
            on,
            decide: Arc::new(move |c: &CiCtx| {
                Some(
                    if !marker.is_empty() && c.tree.values().any(|v| v.contains(&marker)) {
                        "failure"
                    } else {
                        "success"
                    }
                    .to_string(),
                )
            }),
        }
    }

    pub fn success(check: &str, on: CiOn) -> Self {
        Self::fails_on(check, on, "")
    }
}

struct Outgoing {
    at: Instant,
    seq: u64,
    event: String,
    guid: String,
    body: Vec<u8>,
}

#[derive(Clone, Debug)]
pub struct Delivery {
    pub id: u64,
    pub guid: String,
    pub event: String,
    pub delivered_at: u64,
    pub status_code: u16,
}

pub(crate) struct St {
    pub cfg: FakeConfig,
    pub repo: Repo,
    pub initial: Sha,
    pub prs: BTreeMap<u64, FakePr>,
    pub next_issue: u64,
    pub runs: Vec<FakeRun>,
    pub statuses: Vec<FakeStatus>,
    pub next_id: u64,
    pub comments: Vec<(u64, u64, String)>,
    pub reactions: Vec<(u64, String)>,
    pub tokens: BTreeMap<String, FakeToken>,
    /// Bodies of `POST /app/installations/{id}/access_tokens`, in order.
    pub token_requests: Vec<Value>,
    /// The token of the request being handled.
    pub caller_token: Option<String>,
    pub ci: Vec<CiRule>,
    pub faults: Vec<FaultRule>,
    pub hook_faults: HookFaults,
    pub rng: u64,
    pub outbox: Vec<Outgoing>,
    pub sending: usize,
    pub deliveries: Vec<Delivery>,
    pub hooks_dropped: u64,
    pub hooks_duplicated: u64,
    pub hook_seq: u64,
    pub webhook_target: Option<String>,
    pub requests: Vec<String>,
    pub squashes: Vec<Squash>,
    pub external: BTreeSet<Sha>,
    /// Commits made by `POST /merges`, with their branch.
    pub queue_commits: BTreeMap<Sha, String>,
    /// Pull request heads merged into queue branches, with the queue branches.
    pub queue_heads: BTreeMap<Sha, BTreeSet<String>>,
    pub merge_guard: Option<MergeGuard>,
    /// Reads of `mergeable` that answer `null` after a push (0: never).
    pub mergeable_lag: u32,
    pub mergeable_unknown: BTreeMap<u64, u32>,
    /// Reads that return the state before an API write (0: none).
    pub read_lag: u32,
    /// Stale answers: key (`pull:N`, `ref:BRANCH`) → (body, reads left).
    pub stale: BTreeMap<String, (Value, u32)>,
    /// Former repository names (renames answer with redirects).
    pub old_names: Vec<String>,
    pub rulesets: Vec<Value>,
    pub collaborators: BTreeMap<String, String>,
    pub violations: Vec<String>,
}

impl St {
    pub fn id(&mut self) -> u64 {
        self.next_id += 1;
        self.next_id
    }

    fn unit(&mut self) -> f64 {
        let r = &mut self.rng;
        *r ^= *r << 13;
        *r ^= *r >> 7;
        *r ^= *r << 17;
        (*r % 1_000_000) as f64 / 1_000_000.0
    }

    pub fn tip(&self, branch: &str) -> Option<Sha> {
        self.repo.ref_sha(branch).cloned()
    }

    /// A push to a branch: pull requests on it get `mergeable: null` for a while.
    pub fn touched(&mut self, branch: &str) {
        if self.mergeable_lag == 0 {
            return;
        }
        let lag = self.mergeable_lag;
        let affected: Vec<u64> = self
            .prs
            .values()
            .filter(|p| p.open && (p.base == branch || p.head_ref == branch))
            .map(|p| p.number)
            .collect();
        for n in affected {
            self.mergeable_unknown.insert(n, lag);
        }
    }

    /// Remembers the current answer for `key` before an API write changes it (read lag).
    pub fn before_write(&mut self, key: &str, current: Option<Value>) {
        if self.read_lag == 0 {
            return;
        }
        if let Some(v) = current {
            let lag = self.read_lag;
            self.stale.entry(key.to_string()).or_insert((v, lag));
        }
    }

    pub fn ref_json(&self, branch: &str) -> Option<Value> {
        self.tip(branch).map(|s| {
            json!({ "ref": format!("refs/heads/{branch}"), "object": { "sha": s, "type": "commit" } })
        })
    }

    /// A stale answer for `key`, while one is due.
    pub fn stale_read(&mut self, key: &str) -> Option<Value> {
        let (v, left) = self.stale.get_mut(key)?;
        *left = left.saturating_sub(1);
        let (v, done) = (v.clone(), *left == 0);
        if done {
            self.stale.remove(key);
        }
        Some(v)
    }

    /// The permissions of the token of the request being handled.
    pub fn caller_permissions(&self) -> BTreeMap<String, String> {
        self.caller_token
            .as_ref()
            .and_then(|t| self.tokens.get(t))
            .map(|t| t.permissions.clone())
            .unwrap_or_else(|| self.cfg.permissions.clone())
    }

    /// GitHub refuses commits that change workflow files from an app without
    /// `workflows: write`: the message for the first such path, if any.
    pub fn workflow_refusal(&self, paths: &[String]) -> Option<String> {
        let ok = self
            .caller_permissions()
            .get("workflows")
            .is_some_and(|l| l == "write");
        if ok {
            return None;
        }
        paths
            .iter()
            .find(|p| p.starts_with(".github/workflows/"))
            .map(|p| {
                format!(
                    "refusing to allow a GitHub App to create or update workflow `{p}` without `workflows` permission"
                )
            })
    }

    /// A direct push to the default branch (outside the queue).
    pub fn push_base(&mut self, files: &[(String, String)]) -> Sha {
        let base = self.cfg.default_branch.clone();
        let tip = self.tip(&base).expect("base");
        let mut tree = self.repo.tree(&tip).clone();
        for (p, c) in files {
            tree.insert(p.clone(), c.clone());
        }
        let c = self.repo.commit(tree, vec![tip.clone()], "direct push");
        self.repo.update_ref(&base, &tip, &c).expect("cas");
        self.external.insert(c.clone());
        self.touched(&base);
        self.emit_push(&base, &tip, Some(&c));
        c
    }

    pub fn set_label(&mut self, n: u64, label: &str, add: bool) {
        if let Some(p) = self.prs.get_mut(&n) {
            if add {
                p.labels.insert(label.into());
            } else {
                p.labels.remove(label);
            }
        }
        self.emit_pr(
            if add { "labeled" } else { "unlabeled" },
            n,
            json!({ "label": { "name": label } }),
        );
    }

    pub fn run_then(&mut self, then: &[Then]) {
        for t in then {
            match t {
                Then::PushBase(files) => {
                    self.push_base(files);
                }
                Then::Label(n, l) => self.set_label(*n, l, true),
            }
        }
    }

    /// I1 at landing time: was `head` merged into a queue branch whose draft is still open?
    pub fn head_tested(&self, head: &str) -> bool {
        let bot = format!("{}[bot]", self.cfg.app_slug);
        self.queue_heads.get(head).is_some_and(|branches| {
            branches.iter().any(|b| {
                self.prs
                    .values()
                    .any(|p| p.open && p.user == bot && p.head_ref == *b)
            })
        })
    }

    pub fn repo_json(&self) -> Value {
        json!({ "full_name": self.cfg.full_name(), "name": self.cfg.name,
                "owner": { "login": self.cfg.owner }, "default_branch": self.cfg.default_branch })
    }

    pub fn mergeable(&mut self, pr: &FakePr) -> Option<bool> {
        let tip = self.tip(&pr.base)?;
        Some(self.repo.squash(&tip, &pr.head_sha, "probe").is_ok())
    }

    pub fn pr_json(&mut self, n: u64) -> Option<Value> {
        let pr = self.prs.get(&n)?.clone();
        let mergeable = if pr.open { self.mergeable(&pr) } else { None };
        let base_sha = self.tip(&pr.base).unwrap_or_default();
        Some(json!({
            "number": pr.number,
            "title": pr.title,
            "body": pr.body,
            "state": if pr.open { "open" } else { "closed" },
            "draft": pr.draft,
            "merged": pr.merged,
            "merge_commit_sha": pr.merge_commit_sha,
            "mergeable": mergeable,
            "user": { "login": pr.user },
            "head": { "ref": pr.head_ref, "sha": pr.head_sha },
            "base": { "ref": pr.base, "sha": base_sha },
            "labels": pr.labels.iter().map(|l| json!({ "name": l })).collect::<Vec<_>>(),
        }))
    }

    pub fn run_json(&self, r: &FakeRun) -> Value {
        let prs: Vec<Value> = self
            .prs
            .values()
            .filter(|p| p.open && p.head_sha == r.head_sha)
            .map(|p| json!({ "number": p.number }))
            .collect();
        json!({
            "id": r.id, "name": r.name, "head_sha": r.head_sha, "status": r.status,
            "conclusion": r.conclusion, "app": { "slug": r.app_slug },
            "output": { "title": r.output_title },
            "pull_requests": prs,
        })
    }

    /// Queues a webhook (subject to the delivery faults).
    pub fn emit(&mut self, event: &str, mut payload: Value) {
        if let Some(o) = payload.as_object_mut() {
            o.insert("repository".into(), self.repo_json());
            o.insert(
                "installation".into(),
                json!({ "id": self.cfg.installation_id }),
            );
        }
        let body = payload.to_string().into_bytes();
        let now = Instant::now();
        let f = self.hook_faults.clone();
        if f.drop > 0.0 && self.unit() < f.drop {
            self.hooks_dropped += 1;
            return;
        }
        let d1 = f.max_delay.mul_f64(self.unit());
        let dup = f.duplicate > 0.0 && self.unit() < f.duplicate;
        let d2 = f.max_delay.mul_f64(self.unit());
        self.hook_seq += 1;
        let guid = format!("fake-delivery-{}", self.hook_seq);
        self.outbox.push(Outgoing {
            at: now + d1,
            seq: self.hook_seq,
            event: event.into(),
            guid: guid.clone(),
            body: body.clone(),
        });
        if dup {
            self.hooks_duplicated += 1;
            self.hook_seq += 1;
            // A redelivery: same delivery id.
            self.outbox.push(Outgoing {
                at: now + d1 + d2,
                seq: self.hook_seq,
                event: event.into(),
                guid,
                body,
            });
        }
    }

    pub fn emit_pr(&mut self, action: &str, n: u64, extra: Value) {
        let Some(pr) = self.pr_json(n) else { return };
        let mut payload = json!({ "action": action, "number": n, "pull_request": pr });
        if let (Some(o), Some(e)) = (payload.as_object_mut(), extra.as_object()) {
            for (k, v) in e {
                o.insert(k.clone(), v.clone());
            }
        }
        self.emit("pull_request", payload);
    }

    pub fn emit_run(&mut self, action: &str, id: u64) {
        let Some(r) = self.runs.iter().find(|r| r.id == id).cloned() else {
            return;
        };
        let j = self.run_json(&r);
        self.emit("check_run", json!({ "action": action, "check_run": j }));
        if r.status == "completed" {
            let prs = j["pull_requests"].clone();
            self.emit(
                "check_suite",
                json!({ "action": "completed", "check_suite": { "head_sha": r.head_sha, "pull_requests": prs } }),
            );
        }
    }

    pub fn emit_push(&mut self, branch: &str, before: &str, after: Option<&str>) {
        self.emit(
            "push",
            json!({ "ref": format!("refs/heads/{branch}"), "before": before,
                    "after": after.unwrap_or("0000000000000000000000000000000000000000"),
                    "deleted": after.is_none() }),
        );
    }

    /// Adds a check run and emits its webhooks.
    pub fn add_run(&mut self, sha: &str, name: &str, conclusion: Option<String>, app: &str) -> u64 {
        let id = self.id();
        let completed = conclusion.is_some();
        self.runs.push(FakeRun {
            id,
            name: name.into(),
            head_sha: sha.into(),
            status: if completed {
                "completed"
            } else {
                "in_progress"
            }
            .into(),
            conclusion,
            app_slug: app.into(),
            output_title: String::new(),
        });
        self.emit_run("created", id);
        if completed {
            self.emit_run("completed", id);
        }
        id
    }

    /// Runs the CI rules on pull request `n`'s current head.
    pub fn run_ci(&mut self, n: u64) {
        let Some(pr) = self.prs.get(&n).cloned() else {
            return;
        };
        let queue = pr.head_ref.starts_with(&self.cfg.queue_prefix);
        let tree = self.repo.tree(&pr.head_sha).clone();
        let rules = self.ci.clone();
        for rule in rules {
            let applies = match rule.on {
                CiOn::Both => true,
                CiOn::PullRequest => !queue,
                CiOn::Queue => queue,
            };
            if !applies {
                continue;
            }
            let ctx = CiCtx {
                number: n,
                author: &pr.user,
                tree: &tree,
                queue,
            };
            let c = (rule.decide)(&ctx);
            self.add_run(&pr.head_sha, &rule.check, c, "github-actions");
        }
    }

    /// The newest run per name on a commit, from other apps than the queue's.
    pub fn latest_runs(&self, sha: &str) -> BTreeMap<String, FakeRun> {
        let mut out: BTreeMap<String, FakeRun> = BTreeMap::new();
        for r in self
            .runs
            .iter()
            .filter(|r| r.head_sha == sha && r.app_slug != self.cfg.app_slug)
        {
            out.insert(r.name.clone(), r.clone());
        }
        out
    }

    /// Whether `tree` was built by the queue (via `POST /merges`) on a branch whose open
    /// draft pull request has only green checks (I0 on the fake).
    pub fn tree_tested_green(&self, tree: &str) -> bool {
        self.queue_commits.iter().any(|(sha, branch)| {
            self.repo.tree_id_of(sha) == tree
                && self.prs.values().any(|p| {
                    p.open
                        && p.head_ref == *branch
                        && p.user == format!("{}[bot]", self.cfg.app_slug)
                        && self.repo.is_ancestor(sha, &p.head_sha)
                        && {
                            let runs = self.latest_runs(&p.head_sha);
                            !runs.is_empty()
                                && runs
                                    .values()
                                    .all(|r| r.conclusion.as_deref() == Some("success"))
                        }
                })
        })
    }
}

/// The fake GitHub server.
pub struct FakeGithub {
    pub addr: SocketAddr,
    pub(crate) inner: Arc<Inner>,
    threads: Vec<JoinHandle<()>>,
    server: Arc<tiny_http::Server>,
}

pub(crate) struct Inner {
    pub st: Mutex<St>,
    pub outbox_cv: Condvar,
    pub stop: AtomicBool,
    pub public_key: Vec<u8>,
    pub agent: ureq::Agent,
}

impl FakeGithub {
    pub fn start(cfg: FakeConfig) -> FakeGithub {
        let mut repo = Repo::new();
        let root = repo.commit(cfg.base_files.clone(), vec![], "initial commit");
        repo.create_ref(&cfg.default_branch, &root)
            .expect("fresh repo");
        let key = AppKey::from_pem(TEST_APP_KEY).expect("test key");
        let st = St {
            cfg,
            repo,
            initial: root,
            prs: BTreeMap::new(),
            next_issue: 0,
            runs: Vec::new(),
            statuses: Vec::new(),
            next_id: 100_000,
            comments: Vec::new(),
            reactions: Vec::new(),
            tokens: BTreeMap::new(),
            token_requests: Vec::new(),
            caller_token: None,
            ci: Vec::new(),
            faults: Vec::new(),
            hook_faults: HookFaults::default(),
            rng: 0x9e37_79b9_7f4a_7c15,
            outbox: Vec::new(),
            sending: 0,
            deliveries: Vec::new(),
            hooks_dropped: 0,
            hooks_duplicated: 0,
            hook_seq: 0,
            webhook_target: None,
            requests: Vec::new(),
            squashes: Vec::new(),
            external: BTreeSet::new(),
            queue_commits: BTreeMap::new(),
            queue_heads: BTreeMap::new(),
            merge_guard: None,
            mergeable_lag: 0,
            mergeable_unknown: BTreeMap::new(),
            read_lag: 0,
            stale: BTreeMap::new(),
            old_names: Vec::new(),
            rulesets: Vec::new(),
            collaborators: BTreeMap::new(),
            violations: Vec::new(),
        };
        let agent = ureq::Agent::new_with_config(
            ureq::Agent::config_builder()
                .http_status_as_error(false)
                .timeout_global(Some(Duration::from_secs(10)))
                .build(),
        );
        let inner = Arc::new(Inner {
            st: Mutex::new(st),
            outbox_cv: Condvar::new(),
            stop: AtomicBool::new(false),
            public_key: key.public_key_der(),
            agent,
        });
        let server = Arc::new(tiny_http::Server::http("127.0.0.1:0").expect("listen"));
        let addr = server.server_addr().to_ip().expect("ip");
        let mut threads = Vec::new();
        for i in 0..4 {
            let (srv, inn) = (server.clone(), inner.clone());
            threads.push(
                std::thread::Builder::new()
                    .name(format!("fake-github-{i}"))
                    .spawn(move || http::serve(&srv, &inn))
                    .expect("thread"),
            );
        }
        let inn = inner.clone();
        threads.push(
            std::thread::Builder::new()
                .name("fake-github-hooks".into())
                .spawn(move || deliver_loop(&inn))
                .expect("thread"),
        );
        FakeGithub {
            addr,
            inner,
            threads,
            server,
        }
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub(crate) fn with<R>(&self, f: impl FnOnce(&mut St) -> R) -> R {
        let r = f(&mut self.inner.st.lock().expect("fake state"));
        self.inner.outbox_cv.notify_all();
        r
    }

    pub fn config(&self) -> FakeConfig {
        self.with(|s| s.cfg.clone())
    }

    pub fn set_webhook_target(&self, url: Option<String>) {
        self.with(|s| s.webhook_target = url);
    }

    pub fn set_ci(&self, rules: Vec<CiRule>) {
        self.with(|s| s.ci = rules);
    }

    pub fn add_fault(&self, rule: FaultRule) {
        self.with(|s| s.faults.push(rule));
    }

    pub fn clear_faults(&self) {
        self.with(|s| s.faults.clear());
    }

    pub fn set_hook_faults(&self, f: HookFaults) {
        self.with(|s| {
            s.rng = f.seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
            s.hook_faults = f;
        });
    }

    /// The merge guard (I1 at landing time), e.g. [`guard_labels`].
    pub fn set_merge_guard(&self, g: Option<MergeGuard>) {
        self.with(|s| s.merge_guard = g);
    }

    /// `mergeable` reads `null` this many times after each push to a pull request or its base.
    pub fn set_mergeable_lag(&self, reads: u32) {
        self.with(|s| s.mergeable_lag = reads);
    }

    /// Reads of a pull request or ref return the state before an API write this many times.
    pub fn set_read_lag(&self, reads: u32) {
        self.with(|s| s.read_lag = reads);
    }

    /// Renames the repository: the old name answers with redirects.
    pub fn rename_repo(&self, new_name: &str) {
        self.with(|s| {
            let old = std::mem::replace(&mut s.cfg.name, new_name.to_string());
            s.old_names.push(old);
        });
    }

    /// Bodies of the installation token requests, in order.
    pub fn token_requests(&self) -> Vec<Value> {
        self.with(|s| s.token_requests.clone())
    }

    pub fn set_permissions(&self, p: BTreeMap<String, String>) {
        self.with(|s| s.cfg.permissions = p);
    }

    pub fn add_ruleset(&self, ruleset: Value) {
        self.with(|s| s.rulesets.push(ruleset));
    }

    pub fn set_collaborator(&self, login: &str, permission: &str) {
        self.with(|s| {
            s.collaborators
                .insert(login.to_string(), permission.to_string())
        });
    }

    /// Opens a pull request from a new branch with these files changed on top of the base.
    pub fn open_pr(
        &self,
        author: &str,
        title: &str,
        files: &[(String, String)],
        draft: bool,
        labels: &[String],
    ) -> u64 {
        self.with(|s| {
            s.next_issue += 1;
            let n = s.next_issue;
            let base = s.cfg.default_branch.clone();
            let tip = s.tip(&base).expect("base");
            let mut tree = s.repo.tree(&tip).clone();
            for (p, c) in files {
                tree.insert(p.clone(), c.clone());
            }
            let head = s.repo.commit(tree, vec![tip], &format!("{title} (#{n})"));
            let head_ref = format!("pr-{n}");
            s.repo.create_ref(&head_ref, &head).expect("new branch");
            s.prs.insert(
                n,
                FakePr {
                    number: n,
                    title: title.into(),
                    body: String::new(),
                    user: author.into(),
                    head_ref,
                    head_sha: head,
                    base,
                    draft,
                    open: true,
                    merged: false,
                    merge_commit_sha: None,
                    labels: labels.iter().cloned().collect(),
                },
            );
            s.emit_pr("opened", n, json!({}));
            s.run_ci(n);
            n
        })
    }

    /// A new commit on the pull request's branch.
    pub fn push_pr(&self, n: u64, files: &[(String, String)]) -> Sha {
        self.with(|s| {
            let pr = s.prs.get(&n).cloned().expect("pr");
            let mut tree = s.repo.tree(&pr.head_sha).clone();
            for (p, c) in files {
                tree.insert(p.clone(), c.clone());
            }
            let head = s
                .repo
                .commit(tree, vec![pr.head_sha.clone()], &format!("push to #{n}"));
            s.repo
                .update_ref(&pr.head_ref, &pr.head_sha, &head)
                .expect("cas");
            s.prs.get_mut(&n).expect("pr").head_sha = head.clone();
            s.touched(&pr.head_ref);
            s.emit_pr("synchronize", n, json!({}));
            s.run_ci(n);
            head
        })
    }

    pub fn label(&self, n: u64, label: &str, add: bool) {
        self.with(|s| s.set_label(n, label, add));
    }

    pub fn set_draft(&self, n: u64, draft: bool) {
        self.with(|s| {
            if let Some(p) = s.prs.get_mut(&n) {
                p.draft = draft;
            }
            s.emit_pr(
                if draft {
                    "converted_to_draft"
                } else {
                    "ready_for_review"
                },
                n,
                json!({}),
            );
        });
    }

    pub fn close_pr(&self, n: u64) {
        self.with(|s| {
            if let Some(p) = s.prs.get_mut(&n) {
                p.open = false;
            }
            s.emit_pr("closed", n, json!({}));
        });
    }

    /// Reports a check run on pull request `n`'s head; `None` leaves it in progress.
    pub fn report_check(&self, n: u64, name: &str, conclusion: Option<&str>) -> u64 {
        self.with(|s| {
            let sha = s.prs.get(&n).expect("pr").head_sha.clone();
            s.add_run(&sha, name, conclusion.map(str::to_string), "github-actions")
        })
    }

    /// Completes a check run that is in progress.
    pub fn complete_check(&self, id: u64, conclusion: &str) {
        self.with(|s| {
            if let Some(r) = s.runs.iter_mut().find(|r| r.id == id) {
                r.status = "completed".into();
                r.conclusion = Some(conclusion.into());
            }
            s.emit_run("completed", id);
        });
    }

    /// Sets a commit status on pull request `n`'s head.
    pub fn report_status(&self, n: u64, context: &str, state: &str) {
        self.with(|s| {
            let sha = s.prs.get(&n).expect("pr").head_sha.clone();
            let id = s.id();
            s.statuses.push(FakeStatus {
                id,
                sha: sha.clone(),
                context: context.into(),
                state: state.into(),
            });
            s.emit(
                "status",
                json!({ "sha": sha, "context": context, "state": state }),
            );
        });
    }

    /// A commit pushed straight to the default branch (outside the queue).
    pub fn push_base(&self, files: &[(String, String)]) -> Sha {
        self.with(|s| s.push_base(files))
    }

    // ---- observations ------------------------------------------------------------------

    pub fn pr(&self, n: u64) -> Option<FakePr> {
        self.with(|s| s.prs.get(&n).cloned())
    }

    pub fn prs(&self) -> Vec<FakePr> {
        self.with(|s| s.prs.values().cloned().collect())
    }

    /// Open draft pull requests of the queue's bot.
    pub fn open_queue_drafts(&self) -> Vec<FakePr> {
        self.with(|s| {
            let bot = format!("{}[bot]", s.cfg.app_slug);
            s.prs
                .values()
                .filter(|p| p.open && p.user == bot)
                .cloned()
                .collect()
        })
    }

    pub fn branches(&self) -> Vec<String> {
        self.with(|s| s.repo.refs().keys().cloned().collect())
    }

    pub fn branch_sha(&self, b: &str) -> Option<Sha> {
        self.with(|s| s.tip(b))
    }

    pub fn tree_of(&self, sha: &str) -> Tree {
        self.with(|s| s.repo.tree(sha).clone())
    }

    pub fn tree_id_of(&self, sha: &str) -> String {
        self.with(|s| s.repo.tree_id_of(sha))
    }

    pub fn parents_of(&self, sha: &str) -> Vec<Sha> {
        self.with(|s| s.repo.get(sha).parents.clone())
    }

    /// First-parent history of the default branch, newest first, without the initial commit.
    pub fn base_history(&self) -> Vec<Sha> {
        self.with(|s| {
            let tip = s.tip(&s.cfg.default_branch).expect("base");
            let mut h = s.repo.first_parent_history(&tip);
            h.retain(|c| *c != s.initial);
            h
        })
    }

    pub fn squashes(&self) -> Vec<Squash> {
        self.with(|s| s.squashes.clone())
    }

    pub fn runs(&self) -> Vec<FakeRun> {
        self.with(|s| s.runs.clone())
    }

    pub fn requests(&self) -> Vec<String> {
        self.with(|s| s.requests.clone())
    }

    pub fn deliveries(&self) -> Vec<Delivery> {
        self.with(|s| s.deliveries.clone())
    }

    /// `(dropped, duplicated)` webhooks.
    pub fn hook_fault_counts(&self) -> (u64, u64) {
        self.with(|s| (s.hooks_dropped, s.hooks_duplicated))
    }

    /// No webhook waiting or being sent.
    pub fn hooks_idle(&self) -> bool {
        self.with(|s| s.outbox.is_empty() && s.sending == 0)
    }

    /// The queue's own check run on a pull request's head (newest), if any.
    pub fn queue_check(&self, n: u64) -> Option<FakeRun> {
        self.with(|s| {
            let sha = s.prs.get(&n)?.head_sha.clone();
            let app = s.cfg.app_slug.clone();
            s.runs
                .iter()
                .rev()
                .find(|r| r.head_sha == sha && r.app_slug == app)
                .cloned()
        })
    }

    /// Invariant violations on the fake's git state:
    /// - I0: every commit on the default branch is the initial commit, a direct push, or a
    ///   squash landing whose tree the queue built and saw green on a draft;
    /// - I1: every landing happened at a head the queue merged into a queue branch whose draft
    ///   was still open (not after its car was closed, not at another head), with the merge
    ///   guard's conditions true at that moment; nothing landed twice;
    /// - MQ-35: no pull request was squashed twice; no change landed twice on the base;
    /// - I3 (call at quiescence with `quiescent`): no queue branch and no open queue draft.
    pub fn invariant_violations(&self, quiescent: bool) -> Vec<String> {
        self.with(|s| {
            let mut v = s.violations.clone();
            for q in &s.squashes {
                if !q.head_tested {
                    v.push(format!(
                        "I1: #{} landed at head {} that no open queue draft tested",
                        q.pr, q.head
                    ));
                }
                if let Some(why) = &q.guard {
                    v.push(format!(
                        "I1: #{} landed while its conditions were false ({why})",
                        q.pr
                    ));
                }
            }
            let tip = s.tip(&s.cfg.default_branch).expect("base");
            let mut seen_prs = BTreeMap::new();
            for c in s.repo.first_parent_history(&tip) {
                if c == s.initial || s.external.contains(&c) {
                    continue;
                }
                match s.squashes.iter().find(|q| q.sha == c) {
                    None => v.push(format!("I0: {c} on the base was not made by a landing")),
                    Some(q) => {
                        if !q.tested_green {
                            v.push(format!(
                                "I0: #{} landed as {c} with a tree the queue never saw green",
                                q.pr
                            ));
                        }
                        *seen_prs.entry(q.pr).or_insert(0) += 1;
                    }
                }
            }
            for (pr, k) in seen_prs {
                if k > 1 {
                    v.push(format!("MQ-35: #{pr} landed {k} times on the base"));
                }
            }
            let mut per_pr: BTreeMap<u64, u32> = BTreeMap::new();
            for q in &s.squashes {
                *per_pr.entry(q.pr).or_default() += 1;
            }
            for (pr, k) in per_pr {
                if k > 1 {
                    v.push(format!("MQ-35: #{pr} was squash-merged {k} times"));
                }
            }
            if quiescent {
                let bot = format!("{}[bot]", s.cfg.app_slug);
                for p in s.prs.values().filter(|p| p.open && p.user == bot) {
                    v.push(format!("I3: queue draft #{} is still open", p.number));
                }
                for r in s.repo.refs().keys() {
                    if r.starts_with(&s.cfg.queue_prefix) {
                        v.push(format!("I3: queue branch {r} is left over"));
                    }
                }
            }
            v
        })
    }

    pub fn shutdown(self) {
        self.inner.stop.store(true, Ordering::SeqCst);
        self.inner.outbox_cv.notify_all();
        for _ in 0..8 {
            self.server.unblock();
        }
        for t in self.threads {
            let _ = t.join();
        }
    }
}

fn deliver_loop(inner: &Inner) {
    loop {
        let item = {
            let mut st = inner.st.lock().expect("fake state");
            loop {
                if inner.stop.load(Ordering::SeqCst) {
                    return;
                }
                let now = Instant::now();
                let due = st
                    .outbox
                    .iter()
                    .enumerate()
                    .filter(|(_, o)| o.at <= now)
                    .min_by_key(|(_, o)| (o.at, o.seq))
                    .map(|(i, _)| i);
                if let (Some(i), Some(target)) = (due, st.webhook_target.clone()) {
                    let o = st.outbox.remove(i);
                    st.sending += 1;
                    break (o, target, st.cfg.webhook_secret.clone());
                }
                let wait = st
                    .outbox
                    .iter()
                    .map(|o| o.at.saturating_duration_since(now))
                    .min()
                    .unwrap_or(Duration::from_millis(50))
                    .clamp(Duration::from_millis(1), Duration::from_millis(50));
                st = inner.outbox_cv.wait_timeout(st, wait).expect("cv").0;
            }
        };
        let (o, target, secret) = item;
        let sig = schlange_github::webhook::sign(&secret, &o.body);
        let status = inner
            .agent
            .post(&target)
            .header("content-type", "application/json")
            .header("x-github-event", &o.event)
            .header("x-github-delivery", &o.guid)
            .header("x-hub-signature-256", &sig)
            .send(&o.body[..])
            .map(|r| r.status().as_u16())
            .unwrap_or(0);
        let mut st = inner.st.lock().expect("fake state");
        st.sending -= 1;
        let id = st.deliveries.len() as u64 + 1;
        st.deliveries.push(Delivery {
            id,
            guid: o.guid,
            event: o.event,
            delivered_at: unix_now(),
            status_code: status,
        });
    }
}

/// RFC 3339 for the fake's timestamps.
pub(crate) fn ts(t: u64) -> String {
    format_rfc3339(t)
}
