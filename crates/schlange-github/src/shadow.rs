//! Shadow mode (S-SHADOW, KONZEPT.md sections 8 and 11): `schlange serve --shadow` runs next to
//! Mergify on the same repository, reads everything, decides as usual, and writes nothing to
//! GitHub. It records what it would have done and what Mergify did, and compares the two per
//! pull request.
//!
//! **Never writing** is enforced twice: the worker hands no write decision to the GitHub adapter
//! (this module answers them), and the client's write gate ([`crate::lease::ShadowGate`])
//! refuses (in tests: panics on) every request but a read.
//!
//! **Deciding without acting.** The engine needs replies to go on. This module answers them
//! from a virtual overlay and from what Mergify does:
//!
//! - `BuildCar`: the car exists only here (a virtual branch head, tree and draft number above
//!   [`VIRTUAL_DRAFTS`]). Its checks are **borrowed** from Mergify's draft that embarks the same
//!   pull requests in the same order on the same base, if Mergify built one; otherwise they stay
//!   pending (and the batch composition already differs, which the report shows).
//! - `CloseCar`: answered at once.
//! - `Land`: not answered. After the reply timeout the engine reads GitHub: once Mergify merged
//!   the pull requests the engine counts them as its own landing (the path of a lost landing
//!   reply, MQ-35); when the base moved otherwise it rebuilds; when nothing happened it decides
//!   again. The first `Land` per pull request is what Schlange would have done.
//! - `Refresh` and the reconciliation read GitHub as usual, with the virtual cars added.
//! - Everything else (labels, comments, checks, dequeue notes) is recorded and dropped.
//!
//! **Observing Mergify** (our reading of what is visible on GitHub; to be confirmed on the
//! sandbox, KONZEPT.md section 12): Mergify's draft pull requests (author
//! [`ShadowConfig::mergify_login`], head on the queue prefix) list the pull requests they embark
//! as `#n` in their description; pull requests merged by that login landed through Mergify;
//! Mergify's check run on a pull request (app [`ShadowConfig::mergify_app`]) means it is queued,
//! and a dequeue code (`queue-dequeue-reason`) in a concluded run's title or summary is the
//! reason it left.
//!
//! **The report** ([`report`]) compares per pull request: queued, landed, landing order (rank
//! among the pull requests both landed), the batch it landed in, and the dequeue code.

use std::collections::{BTreeMap, BTreeSet};

use schlange_core::Decision;
use schlange_core::engine::{Event, State};
use schlange_core::model::{PullRequest, Time};
use serde::Serialize;
use serde_json::json;

use crate::client::Priority;
use crate::forge::GithubForge;
use crate::store::{ShadowFact, Store};

/// Virtual draft numbers start here (far above any real pull request number).
pub const VIRTUAL_DRAFTS: u64 = 1_000_000_000;
/// At most this many facts are kept in memory per repository.
const FACTS_MAX: usize = 200_000;

pub const SCHLANGE: &str = "schlange";
pub const MERGIFY: &str = "mergify";

#[derive(Clone, Debug)]
pub struct ShadowConfig {
    /// Mergify's bot login (`mergify[bot]`).
    pub mergify_login: String,
    /// Mergify's app slug (its check runs).
    pub mergify_app: String,
    /// Panic on an attempted write instead of refusing it (tests).
    pub panic_on_write: bool,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        ShadowConfig {
            mergify_login: "mergify[bot]".into(),
            mergify_app: "mergify".into(),
            panic_on_write: false,
        }
    }
}

#[derive(Clone, Debug)]
struct VirtualCar {
    branch: String,
    draft: u64,
    head: String,
    tree: String,
    base: String,
    prs: Vec<u64>,
    /// Mergify's draft with the same pull requests, whose checks the car borrows.
    source: Option<u64>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct OtherDraft {
    base: String,
    prs: Vec<u64>,
}

/// The shadow side of one repository's worker.
pub struct Shadow {
    pub cfg: ShadowConfig,
    repo: String,
    store: Option<Store>,
    facts: Vec<ShadowFact>,
    cars: BTreeMap<u64, VirtualCar>,
    drafts: BTreeMap<u64, OtherDraft>,
    seen_merged: BTreeSet<u64>,
    /// Concluded Mergify check runs already read for a dequeue code.
    seen_runs: BTreeSet<u64>,
    seq: u64,
    /// Decisions answered or dropped here instead of being sent.
    pub suppressed: u64,
}

fn virtual_sha(kind: &str, car: u64) -> String {
    let tag: String = kind.bytes().map(|b| format!("{:02x}", b)).collect();
    let s = format!("{tag}{car:032x}");
    s[s.len() - 40..].to_string()
}

/// Pull request numbers a description or title names as `#n`.
pub fn embarked(body: &str, own: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let b = body.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'#' && (i == 0 || !b[i - 1].is_ascii_alphanumeric()) {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if j > start
                && (j == b.len() || !b[j].is_ascii_alphanumeric())
                && let Ok(n) = body[start..j].parse::<u64>()
                && n != own
                && !out.contains(&n)
            {
                out.push(n);
            }
            i = j.max(i + 1);
        } else {
            i += 1;
        }
    }
    out
}

impl Shadow {
    pub fn new(cfg: ShadowConfig, repo: &str, store: Option<Store>) -> Self {
        let facts = store
            .as_ref()
            .and_then(|s| s.shadow_facts(repo).ok())
            .unwrap_or_default();
        let seq = facts.iter().map(|f| f.seq).max().unwrap_or(0);
        Shadow {
            cfg,
            repo: repo.to_string(),
            store,
            facts,
            cars: BTreeMap::new(),
            drafts: BTreeMap::new(),
            seen_merged: BTreeSet::new(),
            seen_runs: BTreeSet::new(),
            seq,
            suppressed: 0,
        }
    }

    pub fn facts(&self) -> &[ShadowFact] {
        &self.facts
    }

    pub fn is_virtual(n: u64) -> bool {
        n >= VIRTUAL_DRAFTS
    }

    fn is_virtual_branch(&self, b: &str) -> bool {
        self.cars.values().any(|c| c.branch == b)
    }

    /// Records a fact. `first`: an existing fact of this kind stays.
    fn fact(
        &mut self,
        side: &str,
        pr: u64,
        kind: &str,
        at: Time,
        data: serde_json::Value,
        first: bool,
    ) {
        let exists = self
            .facts
            .iter()
            .position(|f| f.side == side && f.pr == pr && f.kind == kind);
        if first && exists.is_some() {
            return;
        }
        self.seq += 1;
        let f = ShadowFact {
            repo: self.repo.clone(),
            side: side.into(),
            pr,
            kind: kind.into(),
            at,
            data,
            seq: self.seq,
        };
        if let Some(s) = &self.store {
            let _ = s.shadow_record(&f, first);
        }
        match exists {
            Some(i) => self.facts[i] = f,
            None => {
                if self.facts.len() >= FACTS_MAX {
                    self.facts.remove(0);
                }
                self.facts.push(f);
            }
        }
    }

    fn has(&self, side: &str, pr: u64, kind: &str) -> bool {
        self.facts
            .iter()
            .any(|f| f.side == side && f.pr == pr && f.kind == kind)
    }

    /// Carries out a decision in shadow: records it, answers what the engine needs, sends
    /// nothing but reads.
    pub fn execute(&mut self, d: &Decision, forge: &mut GithubForge, now: Time) -> Vec<Event> {
        match d {
            Decision::Refresh {
                token,
                numbers,
                branches,
            } => {
                let real: Vec<u64> = numbers
                    .iter()
                    .copied()
                    .filter(|n| !Self::is_virtual(*n))
                    .collect();
                let real_b: Vec<String> = branches
                    .iter()
                    .filter(|b| !self.is_virtual_branch(b))
                    .cloned()
                    .collect();
                match forge.snapshot(&real, &real_b, Some(*token), Priority::Landing) {
                    Ok(ev) => vec![self.patch(ev, forge, now)],
                    Err(e) => vec![Event::Failed {
                        decision: d.clone(),
                        error: e.to_string(),
                    }],
                }
            }
            Decision::BuildCar {
                car,
                queue: _,
                base_branch,
                branch,
                prs,
                own,
                ..
            } => {
                self.suppressed += 1;
                let list: Vec<u64> = prs.iter().map(|p| p.number).collect();
                for n in own {
                    self.fact(
                        SCHLANGE,
                        *n,
                        "batch",
                        now,
                        json!({ "car": car, "prs": list }),
                        false,
                    );
                }
                let v = VirtualCar {
                    branch: branch.clone(),
                    draft: VIRTUAL_DRAFTS + car,
                    head: virtual_sha("head", *car),
                    tree: virtual_sha("tree", *car),
                    base: base_branch.clone(),
                    prs: list,
                    source: None,
                };
                let reply = Event::CarBuilt {
                    car: *car,
                    branch: v.branch.clone(),
                    head_sha: v.head.clone(),
                    tree: v.tree.clone(),
                    draft: v.draft,
                };
                self.cars.insert(*car, v);
                forge.note(format!(
                    "shadow: car {car} not built on GitHub (virtual draft #{})",
                    VIRTUAL_DRAFTS + car
                ));
                vec![reply]
            }
            Decision::CheckInPlace { car, number, .. } => {
                self.suppressed += 1;
                self.fact(
                    SCHLANGE,
                    *number,
                    "batch",
                    now,
                    json!({ "car": car, "prs": [number] }),
                    false,
                );
                vec![]
            }
            Decision::CloseCar { car, .. } => {
                self.suppressed += 1;
                self.cars.remove(car);
                vec![Event::CarClosed { car: *car }]
            }
            Decision::Land { car, prs, .. } => {
                self.suppressed += 1;
                let order: Vec<u64> = prs.iter().map(|p| p.number).collect();
                let batch = self
                    .cars
                    .get(car)
                    .map(|c| c.prs.clone())
                    .unwrap_or_else(|| order.clone());
                for n in &order {
                    if !self.has(SCHLANGE, *n, "landed") {
                        self.fact(
                            SCHLANGE,
                            *n,
                            "landed",
                            now,
                            json!({ "car": car, "batch": batch }),
                            true,
                        );
                    }
                }
                forge.note(format!(
                    "shadow: would land {} (car {car}); waiting for what GitHub shows",
                    order
                        .iter()
                        .map(|n| format!("#{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
                vec![]
            }
            Decision::Enqueue {
                number,
                queue,
                rule,
            } => {
                self.fact(
                    SCHLANGE,
                    *number,
                    "queued",
                    now,
                    json!({ "queue": queue, "rule": rule }),
                    true,
                );
                vec![]
            }
            Decision::Dequeue { number, reason, .. } => {
                self.fact(
                    SCHLANGE,
                    *number,
                    "dequeued",
                    now,
                    json!({ "code": reason.code, "text": reason.text }),
                    false,
                );
                vec![]
            }
            Decision::Merged { number, sha, .. } => {
                self.fact(
                    SCHLANGE,
                    *number,
                    "merged",
                    now,
                    json!({ "sha": sha }),
                    true,
                );
                vec![]
            }
            _ => {
                self.suppressed += 1;
                vec![]
            }
        }
    }

    /// A copy of the state without the virtual cars' drafts and branches, for the adapter's
    /// reconciliation read (it must not ask GitHub for them).
    pub fn real_view(&self, state: &State) -> State {
        let mut s = state.clone();
        s.pull_requests.retain(|n, _| !Self::is_virtual(*n));
        s.branches.retain(|b, _| !self.is_virtual_branch(b));
        for t in s.trains.values_mut() {
            for c in &mut t.cars {
                if c.draft.is_some_and(Self::is_virtual) {
                    c.draft = None;
                }
                if c.branch.as_ref().is_some_and(|b| self.is_virtual_branch(b)) {
                    c.branch = None;
                }
            }
        }
        s
    }

    /// Filters a read request down to what exists on GitHub.
    pub fn real_reads(&self, numbers: &[u64], branches: &[String]) -> (Vec<u64>, Vec<String>) {
        (
            numbers
                .iter()
                .copied()
                .filter(|n| !Self::is_virtual(*n))
                .collect(),
            branches
                .iter()
                .filter(|b| !self.is_virtual_branch(b))
                .cloned()
                .collect(),
        )
    }

    /// Adds the virtual cars to a read of GitHub: their branches, and their drafts with the
    /// checks of Mergify's matching draft. Also notes Mergify's drafts and queue checks seen in
    /// the read.
    pub fn patch(&mut self, ev: Event, forge: &GithubForge, now: Time) -> Event {
        let Event::Snapshot {
            mut pull_requests,
            mut branches,
            token,
        } = ev
        else {
            return ev;
        };
        for p in &pull_requests {
            self.note_other(p, now);
        }
        let ids: Vec<u64> = self.cars.keys().copied().collect();
        for id in ids {
            let mut car = self.cars[&id].clone();
            if car.source.is_none() {
                car.source = self
                    .drafts
                    .iter()
                    .rev()
                    .find(|(_, d)| d.base == car.base && d.prs == car.prs)
                    .map(|(n, _)| *n);
            }
            let source = car.source.and_then(|s| {
                pull_requests
                    .iter()
                    .find(|p| p.number == s)
                    .cloned()
                    .or_else(|| forge.read_pr(s, Priority::Normal).ok())
            });
            let mut pr = PullRequest::new(
                car.draft,
                &forge.opts.bot_login,
                &car.base,
                &car.branch,
                &car.head,
            );
            pr.draft = true;
            pr.title = format!(
                "shadow car {id}: {}",
                car.prs
                    .iter()
                    .map(|n| format!("#{n}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            pr.head_tree = Some(car.tree.clone());
            if let Some(src) = source {
                pr.checks = src
                    .checks
                    .iter()
                    .filter(|c| c.head_sha == src.head_sha)
                    .map(|c| {
                        let mut c = c.clone();
                        c.head_sha = car.head.clone();
                        c
                    })
                    .collect();
                pr.closed = false;
            }
            branches.insert(car.branch.clone(), Some(car.head.clone()));
            pull_requests.retain(|p| p.number != car.draft);
            pull_requests.push(pr);
            self.cars.insert(id, car);
        }
        Event::Snapshot {
            pull_requests,
            branches,
            token,
        }
    }

    /// A pull request read from GitHub: a Mergify draft (what it embarks), or a pull request
    /// with Mergify's queue check.
    fn note_other(&mut self, p: &PullRequest, now: Time) {
        if p.author == self.cfg.mergify_login {
            let text = format!("{}\n{}", p.title, p.body);
            let prs = embarked(&text, p.number);
            if prs.is_empty() {
                return;
            }
            let d = OtherDraft {
                base: p.base.clone(),
                prs: prs.clone(),
            };
            if self.drafts.get(&p.number) != Some(&d) {
                self.drafts.insert(p.number, d);
            }
            for n in &prs {
                self.fact(
                    MERGIFY,
                    *n,
                    "queued",
                    now,
                    json!({ "draft": p.number }),
                    true,
                );
                self.fact(
                    MERGIFY,
                    *n,
                    "batch",
                    now,
                    json!({ "draft": p.number, "prs": prs }),
                    false,
                );
            }
            return;
        }
        if p.closed || p.merged {
            return;
        }
        if p.checks.iter().any(|c| {
            c.head_sha == p.head_sha && c.app.as_deref() == Some(self.cfg.mergify_app.as_str())
        }) {
            self.fact(
                MERGIFY,
                p.number,
                "queued",
                now,
                json!({ "check": true }),
                true,
            );
        }
    }

    /// Reads what Mergify did since the last round: merges (who merged, when), drafts that
    /// left the open list, dequeue codes in its concluded check runs. Reads only.
    pub fn observe(&mut self, forge: &mut GithubForge, state: &State, now: Time) {
        let recent = match forge.api.recent_pulls(Priority::Normal) {
            Ok(r) => r,
            Err(e) => {
                forge.note(format!(
                    "shadow: recent pull requests not read (provisional): {e}"
                ));
                return;
            }
        };
        for p in &recent {
            if p.user.login == self.cfg.mergify_login {
                let text = format!("{}\n{}", p.title, p.body.clone().unwrap_or_default());
                let prs = embarked(&text, p.number);
                if !prs.is_empty() {
                    self.drafts.insert(
                        p.number,
                        OtherDraft {
                            base: p.base.ref_name.clone(),
                            prs: prs.clone(),
                        },
                    );
                    for n in &prs {
                        self.fact(
                            MERGIFY,
                            *n,
                            "queued",
                            now,
                            json!({ "draft": p.number }),
                            true,
                        );
                        self.fact(
                            MERGIFY,
                            *n,
                            "batch",
                            now,
                            json!({ "draft": p.number, "prs": prs }),
                            false,
                        );
                    }
                }
                continue;
            }
            if p.merged_at.is_none() || self.seen_merged.contains(&p.number) {
                continue;
            }
            // Who merged it is in the single read only.
            let full = match forge.api.pull(p.number, Priority::Normal) {
                Ok(f) => f,
                Err(e) => {
                    forge.note(format!("shadow: #{} not read (provisional): {e}", p.number));
                    continue;
                }
            };
            self.seen_merged.insert(p.number);
            let at = p
                .merged_at
                .as_deref()
                .and_then(crate::time::parse_rfc3339)
                .unwrap_or(now);
            let by = full.merged_by.map(|u| u.login).unwrap_or_default();
            if by == self.cfg.mergify_login {
                let batch = self
                    .facts
                    .iter()
                    .find(|f| f.side == MERGIFY && f.pr == p.number && f.kind == "batch")
                    .map(|f| f.data["prs"].clone())
                    .unwrap_or(json!([p.number]));
                self.fact(
                    MERGIFY,
                    p.number,
                    "landed",
                    at,
                    json!({ "sha": p.merge_commit_sha, "batch": batch }),
                    true,
                );
            } else if by != forge.opts.bot_login {
                self.fact(
                    MERGIFY,
                    p.number,
                    "merged_elsewhere",
                    at,
                    json!({ "by": by }),
                    true,
                );
            }
        }
        // Dequeue codes: Mergify's concluded check runs on pull requests it queued.
        let open: Vec<(u64, String)> = state
            .pull_requests
            .values()
            .filter(|p| !p.closed && !p.merged && !Self::is_virtual(p.number))
            .filter(|p| {
                self.has(MERGIFY, p.number, "queued") && !self.has(MERGIFY, p.number, "landed")
            })
            .filter(|p| {
                p.checks.iter().any(|c| {
                    c.head_sha == p.head_sha
                        && c.app.as_deref() == Some(self.cfg.mergify_app.as_str())
                        && c.conclusion.is_some()
                        && !self.seen_runs.contains(&c.seq)
                })
            })
            .map(|p| (p.number, p.head_sha.clone()))
            .collect();
        for (n, head) in open {
            let runs = match forge.api.check_runs(&head, Priority::Normal) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let app = self.cfg.mergify_app.clone();
            for r in runs.iter().filter(|r| {
                r.app.as_ref().and_then(|a| a.slug.as_deref()) == Some(app.as_str())
                    && r.status == "completed"
            }) {
                self.seen_runs.insert(r.id);
                let text = r
                    .output
                    .as_ref()
                    .map(|o| {
                        format!(
                            "{} {}",
                            o.title.clone().unwrap_or_default(),
                            o.summary.clone().unwrap_or_default()
                        )
                    })
                    .unwrap_or_default();
                if let Some(code) = schlange_core::reasons::ALL.iter().find(|c| {
                    text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                        .any(|w| w == **c)
                }) {
                    self.fact(
                        MERGIFY,
                        n,
                        "dequeued",
                        now,
                        json!({ "code": code, "text": text.trim() }),
                        false,
                    );
                }
            }
        }
    }
}

/// One aspect compared for one pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct Compared<T> {
    pub schlange: T,
    pub mergify: T,
    pub agree: bool,
}

fn cmp<T: PartialEq>(schlange: T, mergify: T) -> Compared<T> {
    let agree = schlange == mergify;
    Compared {
        schlange,
        mergify,
        agree,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct PrComparison {
    pub number: u64,
    /// `agree`, `disagree`, `inconclusive` (Schlange saw Mergify's merge before it decided)
    /// or `pending` (neither side is done with it).
    pub verdict: String,
    pub queued: Compared<bool>,
    pub landed: Compared<bool>,
    /// Rank among the pull requests both landed (1 = first).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub order: Option<Compared<usize>>,
    /// The pull requests of the batch it landed in.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub batch: Option<Compared<Vec<u64>>>,
    /// The dequeue code when it did not land.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub dequeue_reason: Option<Compared<Option<String>>>,
    pub differences: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, serde::Deserialize)]
pub struct ShadowReport {
    pub repository: String,
    pub agreements: usize,
    pub disagreements: usize,
    pub inconclusive: usize,
    pub pending: usize,
    pub pull_requests: Vec<PrComparison>,
}

fn list(v: &serde_json::Value) -> Vec<u64> {
    let mut l: Vec<u64> = v
        .as_array()
        .map(|a| a.iter().filter_map(|x| x.as_u64()).collect())
        .unwrap_or_default();
    l.sort();
    l
}

/// Compares the two sides' facts per pull request.
pub fn report(repo: &str, facts: &[ShadowFact]) -> ShadowReport {
    let get = |side: &str, pr: u64, kind: &str| {
        facts
            .iter()
            .find(|f| f.side == side && f.pr == pr && f.kind == kind)
    };
    let prs: BTreeSet<u64> = facts.iter().map(|f| f.pr).collect();
    // Landing order per side: Schlange by decision order, Mergify by merge time.
    let mut ours: Vec<(u64, u64)> = facts
        .iter()
        .filter(|f| f.side == SCHLANGE && f.kind == "landed")
        .map(|f| (f.seq, f.pr))
        .collect();
    ours.sort();
    // Merges in the same second keep the order of the batch that carried them.
    let mut theirs: Vec<((u64, usize, u64), u64)> = facts
        .iter()
        .filter(|f| f.side == MERGIFY && f.kind == "landed")
        .map(|f| {
            let pos = f.data["batch"]
                .as_array()
                .and_then(|a| a.iter().position(|x| x.as_u64() == Some(f.pr)))
                .unwrap_or(0);
            ((f.at, pos, f.seq), f.pr)
        })
        .collect();
    theirs.sort();
    let both: BTreeSet<u64> = ours
        .iter()
        .map(|(_, p)| *p)
        .filter(|p| theirs.iter().any(|(_, q)| q == p))
        .collect();
    let rank = |order: Vec<u64>, pr: u64| {
        order
            .into_iter()
            .filter(|p| both.contains(p))
            .position(|p| p == pr)
            .map(|i| i + 1)
    };
    let mut out = Vec::new();
    for pr in prs {
        let queued = cmp(
            get(SCHLANGE, pr, "queued").is_some(),
            get(MERGIFY, pr, "queued").is_some(),
        );
        let s_land = get(SCHLANGE, pr, "landed");
        let m_land = get(MERGIFY, pr, "landed");
        let landed = cmp(s_land.is_some(), m_land.is_some());
        let s_deq =
            get(SCHLANGE, pr, "dequeued").and_then(|f| f.data["code"].as_str().map(str::to_string));
        let m_deq =
            get(MERGIFY, pr, "dequeued").and_then(|f| f.data["code"].as_str().map(str::to_string));
        let elsewhere = get(MERGIFY, pr, "merged_elsewhere").is_some();
        let mut differences = Vec::new();
        let mut order = None;
        let mut batch = None;
        let mut dequeue_reason = None;
        if !queued.agree {
            differences.push(format!(
                "queued: Schlange {}, Mergify {}",
                if queued.schlange { "yes" } else { "no" },
                if queued.mergify { "yes" } else { "no" }
            ));
        }
        let s_done = s_land.is_some() || s_deq.is_some() || !queued.schlange;
        let m_done = m_land.is_some() || m_deq.is_some() || elsewhere || !queued.mergify;
        let mut verdict = "agree";
        if let (Some(s), Some(m)) = (s_land, m_land) {
            let o = cmp(
                rank(ours.iter().map(|(_, p)| *p).collect(), pr).unwrap_or(0),
                rank(theirs.iter().map(|(_, p)| *p).collect(), pr).unwrap_or(0),
            );
            if !o.agree {
                differences.push(format!(
                    "landing order: Schlange {}, Mergify {}",
                    o.schlange, o.mergify
                ));
            }
            order = Some(o);
            let b = cmp(list(&s.data["batch"]), list(&m.data["batch"]));
            if !b.agree {
                differences.push(format!(
                    "batch: Schlange {:?}, Mergify {:?}",
                    b.schlange, b.mergify
                ));
            }
            batch = Some(b);
        } else if !landed.agree {
            let raced = m_land.is_some()
                && s_deq.as_deref() == Some(schlange_core::reasons::PR_MANUALLY_MERGED);
            if raced {
                verdict = "inconclusive";
                differences.push(
                    "landed: Mergify merged it before Schlange decided (it saw the merge first)"
                        .into(),
                );
            } else if s_done && m_done {
                differences.push(format!(
                    "landed: Schlange {}, Mergify {}",
                    if landed.schlange { "yes" } else { "no" },
                    if landed.mergify { "yes" } else { "no" }
                ));
            } else {
                verdict = "pending";
            }
        } else if s_deq.is_some() || m_deq.is_some() {
            let d = cmp(s_deq.clone(), m_deq.clone());
            if !d.agree && s_done && m_done {
                differences.push(format!(
                    "dequeue reason: Schlange {}, Mergify {}",
                    s_deq.as_deref().unwrap_or("none"),
                    m_deq.as_deref().unwrap_or("none")
                ));
            } else if !d.agree {
                verdict = "pending";
            }
            dequeue_reason = Some(d);
        } else if !(s_done && m_done) {
            verdict = "pending";
        }
        if !differences.is_empty() && verdict == "agree" {
            verdict = "disagree";
        }
        out.push(PrComparison {
            number: pr,
            verdict: verdict.into(),
            queued,
            landed,
            order,
            batch,
            dequeue_reason,
            differences,
        });
    }
    let count = |v: &str| out.iter().filter(|p| p.verdict == v).count();
    ShadowReport {
        repository: repo.to_string(),
        agreements: count("agree"),
        disagreements: count("disagree"),
        inconclusive: count("inconclusive"),
        pending: count("pending"),
        pull_requests: out,
    }
}

impl ShadowReport {
    pub fn to_text(&self) -> String {
        let mut out = vec![format!(
            "shadow report for {}: {} agree, {} disagree, {} inconclusive, {} pending",
            self.repository, self.agreements, self.disagreements, self.inconclusive, self.pending
        )];
        for p in &self.pull_requests {
            let mut line = format!(
                "#{} {}: queued {}/{}, landed {}/{}",
                p.number,
                p.verdict,
                yn(p.queued.schlange),
                yn(p.queued.mergify),
                yn(p.landed.schlange),
                yn(p.landed.mergify)
            );
            if let Some(o) = &p.order {
                line.push_str(&format!(", order {}/{}", o.schlange, o.mergify));
            }
            if let Some(b) = &p.batch {
                line.push_str(&format!(", batch {:?}/{:?}", b.schlange, b.mergify));
            }
            if let Some(d) = &p.dequeue_reason {
                line.push_str(&format!(
                    ", dequeued {}/{}",
                    d.schlange.as_deref().unwrap_or("-"),
                    d.mergify.as_deref().unwrap_or("-")
                ));
            }
            out.push(line);
            for d in &p.differences {
                out.push(format!("    {d}"));
            }
        }
        out.push("(Schlange/Mergify)".into());
        let mut s = out.join("\n");
        s.push('\n');
        s
    }
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(
        side: &str,
        pr: u64,
        kind: &str,
        at: u64,
        seq: u64,
        data: serde_json::Value,
    ) -> ShadowFact {
        ShadowFact {
            repo: "o/r".into(),
            side: side.into(),
            pr,
            kind: kind.into(),
            at,
            data,
            seq,
        }
    }

    #[test]
    fn embarked_numbers() {
        assert_eq!(
            embarked("queue: #3, #12\n#3 again (#4)", 99),
            vec![3, 12, 4]
        );
        assert_eq!(embarked("issue#5 a#b #x #7a #8", 8), Vec::<u64>::new());
    }

    #[test]
    fn report_agreements_and_each_kind_of_difference() {
        let facts = vec![
            // #1, #2: both queued and landed, same batch, same order.
            f(SCHLANGE, 1, "queued", 1, 1, json!({})),
            f(MERGIFY, 1, "queued", 1, 2, json!({})),
            f(SCHLANGE, 2, "queued", 1, 3, json!({})),
            f(MERGIFY, 2, "queued", 1, 4, json!({})),
            f(SCHLANGE, 1, "landed", 5, 5, json!({ "batch": [1, 2] })),
            f(SCHLANGE, 2, "landed", 5, 6, json!({ "batch": [1, 2] })),
            f(MERGIFY, 1, "landed", 6, 7, json!({ "batch": [2, 1] })),
            f(MERGIFY, 2, "landed", 7, 8, json!({ "batch": [1, 2] })),
            // #3: both queued; Schlange dequeues CHECKS_FAILED, Mergify CHECKS_TIMEOUT.
            f(SCHLANGE, 3, "queued", 1, 9, json!({})),
            f(MERGIFY, 3, "queued", 1, 10, json!({})),
            f(
                SCHLANGE,
                3,
                "dequeued",
                9,
                11,
                json!({ "code": "CHECKS_FAILED" }),
            ),
            f(
                MERGIFY,
                3,
                "dequeued",
                9,
                12,
                json!({ "code": "CHECKS_TIMEOUT" }),
            ),
            // #4: only Mergify queued it, and landed it.
            f(MERGIFY, 4, "queued", 1, 13, json!({})),
            f(MERGIFY, 4, "landed", 9, 14, json!({ "batch": [4] })),
            // #5: both queued, nothing finished.
            f(SCHLANGE, 5, "queued", 1, 15, json!({})),
            f(MERGIFY, 5, "queued", 1, 16, json!({})),
        ];
        let r = report("o/r", &facts);
        let v: Vec<(u64, &str)> = r
            .pull_requests
            .iter()
            .map(|p| (p.number, p.verdict.as_str()))
            .collect();
        assert_eq!(
            v,
            vec![
                (1, "agree"),
                (2, "agree"),
                (3, "disagree"),
                (4, "disagree"),
                (5, "pending")
            ]
        );
        assert_eq!((r.agreements, r.disagreements, r.pending), (2, 2, 1));
        assert!(
            r.to_text()
                .contains("dequeue reason: Schlange CHECKS_FAILED, Mergify CHECKS_TIMEOUT")
        );
    }
}
