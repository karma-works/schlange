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
//! Confirmed queue-draft membership means queued; generic summary and rule-evaluation
//! checks do not. A known literal dequeue code (`queue-dequeue-reason`) in a concluded
//! run's title or summary is the reason it left. Unknown formats remain unobserved.
//!
//! **The report** ([`report`]) compares per pull request: queued, landed, landing order (rank
//! among the pull requests both landed), the batch it landed in, and the dequeue code.

use std::collections::{BTreeMap, BTreeSet};

use schlange_core::Decision;
use schlange_core::engine::{Event, State};
use schlange_core::model::{PullRequest, Time};
use serde::{Deserialize, Deserializer, Serialize};
use serde_json::json;

use crate::client::Priority;
use crate::forge::GithubForge;
use crate::store::{
    SHADOW_DRAFT_PROBES_MAX, SHADOW_EVENTS_PER_STREAM_MAX, SHADOW_EVENTS_REPO_MAX, ShadowFact,
    Store,
};
use crate::webhook::ShadowPull;

/// Virtual draft numbers start here (far above any real pull request number).
pub const VIRTUAL_DRAFTS: u64 = 1_000_000_000;

pub const SCHLANGE: &str = "schlange";
pub const MERGIFY: &str = "mergify";
const DRAFT_REFERENCE_NUMBERS_MAX: usize = 64;
const DRAFT_BODY_LINES_MAX: usize = 512;
const DRAFT_REFERENCE_LINES_MAX: usize = 64;

#[derive(Clone, Debug)]
pub struct ShadowConfig {
    /// Mergify's bot login (`mergify[bot]`).
    pub mergify_login: String,
    /// Mergify's app slug (its check runs).
    pub mergify_app: String,
    /// Prefix of the other queue's draft branches.
    pub mergify_queue_prefix: String,
    /// Panic on an attempted write instead of refusing it (tests).
    pub panic_on_write: bool,
}

impl Default for ShadowConfig {
    fn default() -> Self {
        ShadowConfig {
            mergify_login: "mergify[bot]".into(),
            mergify_app: "mergify".into(),
            mergify_queue_prefix: "mergify/merge-queue/".into(),
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
    /// Durable source identities of append-only queue, batch and dequeue observations.
    event_keys: BTreeSet<String>,
    /// Counts from the complete durable event rows, never from the report's working set.
    event_counts: BTreeMap<(String, u64, String), usize>,
    /// Complete repository event count from the bounded durable startup snapshot.
    event_total: usize,
    draft_probe_count: usize,
    seq: u64,
    /// Decisions answered or dropped here instead of being sent.
    pub suppressed: u64,
}

fn virtual_sha(kind: &str, car: u64) -> String {
    let tag: String = kind.bytes().map(|b| format!("{:02x}", b)).collect();
    let s = format!("{tag}{car:032x}");
    s[s.len() - 40..].to_string()
}

/// Members of the explicit embarkment section, in order. Never scan prose or PR titles:
/// they can refer to unrelated issues, and sections about PRs ahead are not this batch.
/// This format is exercised by our scripted queue; additional real-world formats require
/// sandbox evidence before being accepted here.
pub fn embarked(body: &str, own: u64) -> Vec<u64> {
    let mut out = Vec::new();
    let Some((_, entries)) = body.split_once("Embarked pull requests:\n") else {
        return out;
    };
    for line in entries.lines() {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        let line = line.strip_prefix("- ").unwrap_or(line);
        let Some(rest) = line.strip_prefix('#') else {
            break;
        };
        let len = rest.bytes().take_while(u8::is_ascii_digit).count();
        let (number, tail) = rest.split_at(len);
        if !tail.is_empty() && !tail.starts_with(':') {
            continue;
        }
        if let Ok(n) = number.parse::<u64>()
            && n != 0
            && n != own
            && !out.contains(&n)
        {
            out.push(n);
        }
    }
    out
}

/// The title's final parenthesized PR-number list is structural membership, unlike
/// incidental references in the member title before it (e.g. `Fix #12 (#50)`).
/// A title without that shape falls back to the dedicated body section.
fn draft_members(title: &str, body: &str, own: u64) -> Vec<u64> {
    if let Some(end) = title.trim_end().strip_suffix(')')
        && let Some((_, members)) = end.rsplit_once('(')
    {
        let parsed: Option<Vec<u64>> = members
            .split(',')
            .map(|part| {
                let digits = part.trim().strip_prefix('#')?;
                if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
                    return None;
                }
                digits.parse::<u64>().ok().filter(|n| *n != 0 && *n != own)
            })
            .collect();
        if let Some(members) = parsed {
            let mut unique = Vec::new();
            for n in members {
                if !unique.contains(&n) {
                    unique.push(n);
                }
            }
            return unique;
        }
    }
    embarked(body, own)
}

fn structural_numbers(text: &str, marker: &str) -> (Vec<u64>, bool) {
    let mut out = BTreeSet::new();
    let mut truncated = false;
    let mut rest = text;
    while let Some((_, after)) = rest.split_once(marker) {
        let len = after.bytes().take_while(u8::is_ascii_digit).count();
        if len > 0
            && let Ok(number) = after[..len].parse::<u64>()
            && number > 0
        {
            out.insert(number);
            if out.len() > DRAFT_REFERENCE_NUMBERS_MAX {
                out.pop_last();
                truncated = true;
                break;
            }
        }
        rest = &after[len..];
    }
    (out.into_iter().collect(), truncated)
}

fn draft_structure(number: u64, title: &str, body: &str) -> DraftStructure {
    let mut lines = body.lines();
    let inspected: Vec<&str> = lines.by_ref().take(DRAFT_BODY_LINES_MAX).collect();
    let body_lines_truncated = lines.next().is_some();
    let mut body_reference_lines_truncated = false;
    let mut body_reference_lines: Vec<DraftReferenceLine> = inspected
        .iter()
        .enumerate()
        .filter_map(|(line, text)| {
            let (hash_references, hash_references_truncated) = structural_numbers(text, "#");
            let (pull_path_references, pull_path_references_truncated) =
                structural_numbers(text, "/pull/");
            if hash_references.is_empty() && pull_path_references.is_empty() {
                return None;
            }
            let trimmed = text.trim_start();
            let without_list = ["- ", "* ", "+ "]
                .iter()
                .find_map(|marker| trimmed.strip_prefix(marker))
                .unwrap_or(trimmed)
                .trim_start();
            Some(DraftReferenceLine {
                line,
                hash_references,
                hash_references_truncated,
                pull_path_references,
                pull_path_references_truncated,
                starts_with_list_marker: without_list.len() != trimmed.len(),
                starts_with_hash_reference: without_list.starts_with('#'),
                starts_with_markdown_link: without_list.starts_with('['),
                contains_html_comment_delimiter: text.contains("<!--") || text.contains("-->"),
            })
        })
        .collect();
    if body_reference_lines.len() > DRAFT_REFERENCE_LINES_MAX {
        body_reference_lines.truncate(DRAFT_REFERENCE_LINES_MAX);
        body_reference_lines_truncated = true;
    }
    let (title_hash_references, title_hash_references_truncated) = structural_numbers(title, "#");
    let (title_pull_path_references, title_pull_path_references_truncated) =
        structural_numbers(title, "/pull/");
    let truncated = title_hash_references_truncated
        || title_pull_path_references_truncated
        || body_lines_truncated
        || body_reference_lines_truncated
        || body_reference_lines
            .iter()
            .any(|line| line.hash_references_truncated || line.pull_path_references_truncated);
    DraftStructure {
        number,
        title_hash_references,
        title_hash_references_truncated,
        title_pull_path_references,
        title_pull_path_references_truncated,
        body_line_count: inspected.len(),
        body_lines_truncated,
        body_reference_lines,
        body_reference_lines_truncated,
        truncated,
    }
}

impl Shadow {
    pub fn new(cfg: ShadowConfig, repo: &str, store: Option<Store>) -> Self {
        let facts_result = store.as_ref().map(|s| s.shadow_facts(repo));
        let events_result = store.as_ref().map(|s| s.shadow_events(repo));
        let load_failed = facts_result.as_ref().is_some_and(Result::is_err)
            || events_result.as_ref().is_some_and(Result::is_err);
        let mut facts = facts_result.and_then(Result::ok).unwrap_or_default();
        let events = events_result.and_then(Result::ok).unwrap_or_default();
        if load_failed {
            facts.push(ShadowFact {
                repo: repo.into(),
                side: "observer".into(),
                pr: 0,
                kind: "history_incomplete".into(),
                at: 0,
                data: json!({ "reason": "storage", "durable": false }),
                seq: 0,
            });
        }
        let mut event_keys: BTreeSet<String> = events
            .iter()
            .map(|(f, key)| format!("{}:{}:{}:{key}", f.side, f.pr, f.kind))
            .collect();
        let mut event_counts = BTreeMap::new();
        for (fact, _) in &events {
            *event_counts
                .entry((fact.side.clone(), fact.pr, fact.kind.clone()))
                .or_insert(0) += 1;
        }
        // Version-1 singleton rows may already describe a draft/car event. Treat their source
        // identity as observed so the first reconciliation after upgrading does not duplicate
        // it in the append-only stream.
        for f in &facts {
            let key = match (f.side.as_str(), f.kind.as_str()) {
                (MERGIFY, "queued" | "batch") => {
                    f.data["draft"].as_u64().map(|n| format!("draft:{n}"))
                }
                (SCHLANGE, "batch") => f.data["car"].as_u64().map(|n| format!("car:{n}")),
                _ => None,
            };
            if let Some(key) = key {
                event_keys.insert(format!("{}:{}:{}:{key}", f.side, f.pr, f.kind));
            }
        }
        let seq = facts.iter().map(|f| f.seq).max().unwrap_or(0);
        let draft_probe_count = facts
            .iter()
            .filter(|fact| fact.kind == "draft_probe")
            .count();
        Shadow {
            cfg,
            repo: repo.to_string(),
            store,
            facts,
            cars: BTreeMap::new(),
            drafts: BTreeMap::new(),
            seen_merged: BTreeSet::new(),
            seen_runs: BTreeSet::new(),
            event_keys,
            event_counts,
            event_total: events.len(),
            draft_probe_count,
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
    ) -> bool {
        let exists = self
            .facts
            .iter()
            .position(|f| f.side == side && f.pr == pr && f.kind == kind);
        if first && exists.is_some() {
            return true;
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
        if let Some(s) = &self.store
            && s.shadow_record(&f, first).is_err()
        {
            return false;
        }
        match exists {
            Some(i) => self.facts[i] = f,
            None => self.facts.push(f),
        }
        true
    }

    fn has(&self, side: &str, pr: u64, kind: &str) -> bool {
        self.facts
            .iter()
            .any(|f| f.side == side && f.pr == pr && f.kind == kind)
    }

    /// Appends an occurrence with a source-stable key. Unlike `fact`, later occurrences of
    /// the same kind are retained, while replay of the same webhook/read is ignored.
    fn event(
        &mut self,
        side: &str,
        pr: u64,
        kind: &str,
        key: impl std::fmt::Display,
        at: Time,
        data: serde_json::Value,
    ) -> bool {
        let key = key.to_string();
        let identity = format!("{side}:{pr}:{kind}:{key}");
        if self.event_keys.contains(&identity) {
            return true;
        }
        if self.has("observer", 0, "history_incomplete") {
            return self.mark_history_incomplete(
                "observer",
                0,
                at,
                json!({ "reason": "repository_limit" }),
            );
        }
        if self.event_total >= SHADOW_EVENTS_REPO_MAX {
            return self.mark_history_incomplete(
                "observer",
                0,
                at,
                json!({ "reason": "repository_limit" }),
            );
        }
        let count_key = (side.to_string(), pr, kind.to_string());
        if self.event_counts.get(&count_key).copied().unwrap_or(0) >= SHADOW_EVENTS_PER_STREAM_MAX {
            return self.mark_history_incomplete(
                side,
                pr,
                at,
                json!({ "stream": kind, "reason": "limit" }),
            );
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
            match s.shadow_event(&f, &key) {
                Ok(true) => {}
                Ok(false) | Err(_) => {
                    // Do not claim the source key or occurrence in memory. The durable inbox
                    // may replay it after storage recovers; meanwhile the report fails closed.
                    return self.mark_history_incomplete(
                        side,
                        pr,
                        at,
                        json!({ "stream": kind, "reason": "storage" }),
                    );
                }
            }
        }
        self.event_keys.insert(identity);
        *self.event_counts.entry(count_key).or_insert(0) += 1;
        self.event_total += 1;
        self.facts.push(f);
        true
    }

    /// The marker is part of the correctness boundary: only claim it in memory after the
    /// singleton row is durable. With no store (unit-only shadow), memory is the authority.
    fn mark_history_incomplete(
        &mut self,
        side: &str,
        pr: u64,
        at: Time,
        data: serde_json::Value,
    ) -> bool {
        let existing = self
            .facts
            .iter()
            .position(|f| f.side == side && f.pr == pr && f.kind == "history_incomplete");
        if existing.is_some_and(|i| self.facts[i].data["durable"] != false) {
            return true;
        }
        let mut f = ShadowFact {
            repo: self.repo.clone(),
            side: side.into(),
            pr,
            kind: "history_incomplete".into(),
            at,
            data,
            seq: self.seq + 1,
        };
        if let Some(store) = &self.store
            && store.shadow_record(&f, true).is_err()
        {
            // Visible immediately so a storage outage cannot look like zero-history
            // agreement, but explicitly non-durable so replay still retries it.
            f.data["durable"] = json!(false);
            match existing {
                Some(i) => self.facts[i] = f,
                None => self.facts.push(f),
            }
            return false;
        }
        self.seq += 1;
        match existing {
            Some(i) => self.facts[i] = f,
            None => self.facts.push(f),
        }
        true
    }

    fn last_occurrence_index(&self, side: &str, pr: u64, kind: &str) -> Option<usize> {
        self.facts
            .iter()
            .rposition(|f| f.side == side && f.pr == pr && f.kind == kind)
    }

    fn occurrence_is_current(&self, side: &str, pr: u64, kind: &str, opposite: &str) -> bool {
        let Some(current) = self.last_occurrence_index(side, pr, kind) else {
            return false;
        };
        self.last_occurrence_index(side, pr, opposite)
            .is_none_or(|other| current > other)
    }

    fn note_schlange_entry(&mut self, number: u64, queue: &str, rule: &str, now: Time) {
        // A replayed decision in the same queue cycle is not another entry. A dequeue between
        // the decisions makes the later entry a distinct occurrence.
        if self.occurrence_is_current(SCHLANGE, number, "queued", "dequeued") {
            return;
        }
        let occurrence = self
            .event_counts
            .get(&(SCHLANGE.into(), number, "queued".into()))
            .copied()
            .unwrap_or(0)
            + 1;
        self.event(
            SCHLANGE,
            number,
            "queued",
            format_args!("entry:{occurrence}"),
            now,
            json!({ "queue": queue, "rule": rule }),
        );
    }

    fn note_schlange_dequeue(&mut self, number: u64, code: Option<&str>, text: &str, now: Time) {
        if self.occurrence_is_current(SCHLANGE, number, "dequeued", "queued") {
            return;
        }
        let occurrence = self
            .event_counts
            .get(&(SCHLANGE.into(), number, "dequeued".into()))
            .copied()
            .unwrap_or(0)
            + 1;
        self.event(
            SCHLANGE,
            number,
            "dequeued",
            format_args!("leave:{occurrence}"),
            now,
            json!({ "code": code, "text": text }),
        );
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
                    self.event(
                        SCHLANGE,
                        *n,
                        "batch",
                        format_args!("car:{car}"),
                        now,
                        json!({ "car": car, "prs": list }),
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
                self.event(
                    SCHLANGE,
                    *number,
                    "batch",
                    format_args!("car:{car}"),
                    now,
                    json!({ "car": car, "prs": [number] }),
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
                self.note_schlange_entry(*number, queue, rule, now);
                vec![]
            }
            Decision::Dequeue { number, reason, .. } => {
                self.note_schlange_dequeue(*number, reason.code, &reason.text, now);
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
            // Once per pull request, kept in the database: Schlange saw it open and could
            // decide about it. The report does not compare pull requests it never saw.
            if !p.closed && p.author != self.cfg.mergify_login && !Self::is_virtual(p.number) {
                self.fact(SCHLANGE, p.number, "seen", now, json!({}), true);
            }
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
    /// with an explicit queue-draft membership list.
    fn note_other(&mut self, p: &PullRequest, now: Time) {
        let author_matches = p.author == self.cfg.mergify_login;
        let prefix_matches = p.head_ref.starts_with(&self.cfg.mergify_queue_prefix);
        let prs = if author_matches || prefix_matches {
            draft_members(&p.title, &p.body, p.number)
        } else {
            Vec::new()
        };
        self.note_draft_probe(
            draft_structure(p.number, &p.title, &p.body),
            author_matches,
            prefix_matches,
            !prs.is_empty(),
            now,
        );
        if author_matches && prefix_matches {
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
                self.event(
                    MERGIFY,
                    *n,
                    "queued",
                    format_args!("draft:{}", p.number),
                    now,
                    json!({ "draft": p.number }),
                );
                self.event(
                    MERGIFY,
                    *n,
                    "batch",
                    format_args!("draft:{}", p.number),
                    now,
                    json!({ "draft": p.number, "prs": prs }),
                );
            }
        }
        // Summary and rule-evaluation check runs are published for unqueued PRs too.
        // Until sandbox evidence identifies an unambiguous queue check, draft membership
        // is the only positive queue signal; never guess from an app slug or check title.
    }

    fn note_draft_probe(
        &mut self,
        structure: DraftStructure,
        author_matches: bool,
        prefix_matches: bool,
        membership_parsed: bool,
        now: Time,
    ) -> bool {
        if !(author_matches || prefix_matches) {
            return true;
        }
        let exists = self
            .facts
            .iter()
            .any(|fact| fact.kind == "draft_probe" && fact.pr == structure.number);
        if !exists && self.draft_probe_count >= SHADOW_DRAFT_PROBES_MAX {
            return self.fact(
                "observer",
                0,
                "draft_probe_truncated",
                now,
                json!({ "reason": "candidate_limit" }),
                true,
            );
        }
        let persisted = self.fact(
            "observer",
            structure.number,
            "draft_probe",
            now,
            json!({
                "matching_configured_author": author_matches,
                "matching_queue_prefix": prefix_matches,
                "structured_membership_parsed": membership_parsed,
                "structure": structure,
            }),
            false,
        );
        if persisted && !exists {
            self.draft_probe_count += 1;
        }
        persisted
    }

    /// Records structured membership directly from a signed, durable pull-request webhook.
    /// The payload is evidence only for the observer; the engine still acts exclusively on
    /// fresh forge reads. Prefix, author and structured-member checks are identical to reads.
    pub fn observe_pull_webhook(&mut self, p: &ShadowPull, now: Time) -> bool {
        let author_matches = p.author == self.cfg.mergify_login;
        let prefix_matches = p.head_ref.starts_with(&self.cfg.mergify_queue_prefix);
        let prs = if author_matches || prefix_matches {
            draft_members(&p.title, &p.body, p.number)
        } else {
            Vec::new()
        };
        if !self.note_draft_probe(
            draft_structure(p.number, &p.title, &p.body),
            author_matches,
            prefix_matches,
            !prs.is_empty(),
            now,
        ) {
            return false;
        }
        if !(author_matches && prefix_matches) {
            return true;
        }
        if prs.is_empty() {
            return true;
        }
        self.drafts.insert(
            p.number,
            OtherDraft {
                base: p.base.clone(),
                prs: prs.clone(),
            },
        );
        let mut durable = true;
        for n in prs.iter().copied() {
            durable &= self.event(
                MERGIFY,
                n,
                "queued",
                format_args!("draft:{}", p.number),
                now,
                json!({ "draft": p.number, "source": "webhook" }),
            );
            durable &= self.event(
                MERGIFY,
                n,
                "batch",
                format_args!("draft:{}", p.number),
                now,
                json!({ "draft": p.number, "prs": prs, "source": "webhook" }),
            );
        }
        durable
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
            let author_matches = p.user.login == self.cfg.mergify_login;
            let prefix_matches = p.head.ref_name.starts_with(&self.cfg.mergify_queue_prefix);
            let prs = if author_matches || prefix_matches {
                draft_members(&p.title, p.body.as_deref().unwrap_or_default(), p.number)
            } else {
                Vec::new()
            };
            self.note_draft_probe(
                draft_structure(p.number, &p.title, p.body.as_deref().unwrap_or_default()),
                author_matches,
                prefix_matches,
                !prs.is_empty(),
                now,
            );
            if author_matches {
                if !prefix_matches {
                    continue;
                }
                if !prs.is_empty() {
                    self.drafts.insert(
                        p.number,
                        OtherDraft {
                            base: p.base.ref_name.clone(),
                            prs: prs.clone(),
                        },
                    );
                    for n in &prs {
                        self.event(
                            MERGIFY,
                            *n,
                            "queued",
                            format_args!("draft:{}", p.number),
                            now,
                            json!({ "draft": p.number }),
                        );
                        self.event(
                            MERGIFY,
                            *n,
                            "batch",
                            format_args!("draft:{}", p.number),
                            now,
                            json!({ "draft": p.number, "prs": prs }),
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
                    .rev()
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
        // Closed PRs may have left State already. The recent-pulls read supplies their
        // head so its dequeue reason remains observable after the worker processes the close.
        let mut candidates: BTreeMap<u64, String> = state
            .pull_requests
            .values()
            .filter(|p| !p.merged && !Self::is_virtual(p.number))
            .map(|p| (p.number, p.head_sha.clone()))
            .collect();
        for p in &recent {
            if p.merged_at.is_none() {
                candidates.insert(p.number, p.head.sha.clone());
            }
        }
        for (n, head) in candidates {
            if !self.has(MERGIFY, n, "queued") || self.has(MERGIFY, n, "landed") {
                continue;
            }
            let runs = match forge.api.check_runs(&head, Priority::Normal) {
                Ok(r) => r,
                Err(_) => continue,
            };
            let app = self.cfg.mergify_app.clone();
            for r in runs.iter().filter(|r| {
                r.app.as_ref().and_then(|a| a.slug.as_deref()) == Some(app.as_str())
                    && r.status == "completed"
            }) {
                // Use GitHub IDs at both the membership check and insertion, not the
                // engine's check ordering field. Do not overwrite the observation time on
                // every reconciliation of the same concluded run.
                if self.seen_runs.contains(&r.id) {
                    continue;
                }
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
                let code = schlange_core::reasons::ALL
                    .iter()
                    .copied()
                    // Preserve literal observer evidence even where the engine has not
                    // adopted the code. Never normalize it to manufacture agreement.
                    .chain(std::iter::once("PR_CLOSED"))
                    .find(|c| {
                        text.split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
                            .any(|w| w == *c)
                    });
                let recorded = code.is_none_or(|code| {
                    self.event(
                        MERGIFY,
                        n,
                        "dequeued",
                        format_args!("run:{}", r.id),
                        now,
                        json!({ "code": code, "run_id": r.id }),
                    )
                });
                if recorded {
                    self.seen_runs.insert(r.id);
                }
            }
        }
    }
}

/// One aspect compared for one pull request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct PrComparison {
    pub number: u64,
    /// `agree`, `disagree`, `inconclusive` (Schlange saw Mergify's merge before it decided)
    /// or `pending` (neither side is done with it).
    pub verdict: String,
    pub queued: Compared<bool>,
    /// Number of distinct queue entries observed on each side.
    pub queue_entries: Compared<usize>,
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
    /// Every observed dequeue code, in occurrence order.
    pub dequeue_reasons: Compared<Vec<String>>,
    pub differences: Vec<String>,
    /// It closed while shadow mode was not watching (before its start, or while it was
    /// stopped): `inconclusive`, and left out of the text report unless `--all`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub before_watching: bool,
    /// At least one occurrence stream could not be retained completely.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_incomplete: bool,
}

#[derive(Deserialize)]
struct PrComparisonWire {
    number: u64,
    verdict: String,
    queued: Compared<bool>,
    queue_entries: Option<Compared<usize>>,
    landed: Compared<bool>,
    order: Option<Compared<usize>>,
    batch: Option<Compared<Vec<u64>>>,
    dequeue_reason: Option<Compared<Option<String>>>,
    dequeue_reasons: Option<Compared<Vec<String>>>,
    differences: Vec<String>,
    #[serde(default)]
    before_watching: bool,
    #[serde(default)]
    history_incomplete: bool,
}

impl<'de> Deserialize<'de> for PrComparison {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let wire = PrComparisonWire::deserialize(deserializer)?;
        let queue_entries = wire
            .queue_entries
            .unwrap_or_else(|| cmp(wire.queued.schlange as usize, wire.queued.mergify as usize));
        let dequeue_reasons = wire.dequeue_reasons.unwrap_or_else(|| {
            wire.dequeue_reason.as_ref().map_or_else(
                || cmp(Vec::new(), Vec::new()),
                |reason| Compared {
                    schlange: reason.schlange.clone().into_iter().collect(),
                    mergify: reason.mergify.clone().into_iter().collect(),
                    agree: reason.agree,
                },
            )
        });
        Ok(Self {
            number: wire.number,
            verdict: wire.verdict,
            queued: wire.queued,
            queue_entries,
            landed: wire.landed,
            order: wire.order,
            batch: wire.batch,
            dequeue_reason: wire.dequeue_reason,
            dequeue_reasons,
            differences: wire.differences,
            before_watching: wire.before_watching,
            history_incomplete: wire.history_incomplete,
        })
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftReferenceLine {
    pub line: usize,
    pub hash_references: Vec<u64>,
    #[serde(default)]
    pub hash_references_truncated: bool,
    pub pull_path_references: Vec<u64>,
    #[serde(default)]
    pub pull_path_references_truncated: bool,
    pub starts_with_list_marker: bool,
    pub starts_with_hash_reference: bool,
    pub starts_with_markdown_link: bool,
    pub contains_html_comment_delimiter: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftStructure {
    pub number: u64,
    pub title_hash_references: Vec<u64>,
    #[serde(default)]
    pub title_hash_references_truncated: bool,
    pub title_pull_path_references: Vec<u64>,
    #[serde(default)]
    pub title_pull_path_references_truncated: bool,
    pub body_line_count: usize,
    #[serde(default)]
    pub body_lines_truncated: bool,
    pub body_reference_lines: Vec<DraftReferenceLine>,
    #[serde(default)]
    pub body_reference_lines_truncated: bool,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DraftObserverDiagnostics {
    /// Candidate pull request numbers only; no title, body or author identity is retained.
    #[serde(default)]
    pub matching_queue_prefix: Vec<u64>,
    #[serde(default)]
    pub matching_configured_author: Vec<u64>,
    #[serde(default)]
    pub structured_membership_parsed: Vec<u64>,
    #[serde(default)]
    pub structures: Vec<DraftStructure>,
    #[serde(default)]
    pub truncated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ShadowReport {
    pub repository: String,
    #[serde(default)]
    pub draft_observer: DraftObserverDiagnostics,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub history_incomplete: bool,
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
    let repository_history_incomplete = facts
        .iter()
        .any(|f| f.pr == 0 && f.kind == "history_incomplete");
    let get = |side: &str, pr: u64, kind: &str| {
        facts
            .iter()
            .find(|f| f.side == side && f.pr == pr && f.kind == kind)
    };
    let get_last = |side: &str, pr: u64, kind: &str| {
        facts
            .iter()
            .rev()
            .find(|f| f.side == side && f.pr == pr && f.kind == kind)
    };
    let history = |side: &str, pr: u64, kind: &str| {
        facts
            .iter()
            .filter(|f| f.side == side && f.pr == pr && f.kind == kind)
            .collect::<Vec<_>>()
    };
    let probe_numbers = |field: &str| {
        let mut numbers = BTreeSet::new();
        let mut truncated = false;
        for fact in facts
            .iter()
            .filter(|fact| fact.kind == "draft_probe" && fact.data[field] == true)
        {
            if !numbers.contains(&fact.pr) && numbers.len() == SHADOW_DRAFT_PROBES_MAX {
                truncated = true;
                continue;
            }
            numbers.insert(fact.pr);
        }
        (numbers.into_iter().collect::<Vec<_>>(), truncated)
    };
    let mut structures_by_number = BTreeMap::new();
    let mut structures_over = false;
    for structure in facts
        .iter()
        .filter(|fact| fact.kind == "draft_probe")
        .filter_map(|fact| {
            serde_json::from_value::<DraftStructure>(fact.data["structure"].clone()).ok()
        })
    {
        if !structures_by_number.contains_key(&structure.number)
            && structures_by_number.len() == SHADOW_DRAFT_PROBES_MAX
        {
            structures_over = true;
            continue;
        }
        structures_by_number.insert(structure.number, structure);
    }
    let structures: Vec<_> = structures_by_number
        .values_mut()
        .take(SHADOW_DRAFT_PROBES_MAX)
        .map(std::mem::take)
        .collect();
    let (matching_queue_prefix, prefix_over) = probe_numbers("matching_queue_prefix");
    let (matching_configured_author, author_over) = probe_numbers("matching_configured_author");
    let (structured_membership_parsed, parsed_over) = probe_numbers("structured_membership_parsed");
    let diagnostics_truncated = facts
        .iter()
        .any(|fact| fact.kind == "draft_probe_truncated")
        || prefix_over
        || author_over
        || parsed_over
        || structures_over
        || structures.iter().any(|structure| structure.truncated);
    let draft_observer = DraftObserverDiagnostics {
        matching_queue_prefix,
        matching_configured_author,
        structured_membership_parsed,
        structures,
        truncated: diagnostics_truncated,
    };
    // A watched pull request with no queue event is explicit negative evidence. This is how
    // selected never-enqueued sandbox cases distinguish zero from a missing observation.
    let prs: BTreeSet<u64> = facts
        .iter()
        .filter(|f| f.pr != 0 && f.kind != "draft_probe" && f.kind != "draft_probe_truncated")
        .map(|f| f.pr)
        .collect();
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
        let queue_entries = cmp(
            history(SCHLANGE, pr, "queued").len(),
            history(MERGIFY, pr, "queued").len(),
        );
        let s_land = get(SCHLANGE, pr, "landed");
        let m_land = get(MERGIFY, pr, "landed");
        let landed = cmp(s_land.is_some(), m_land.is_some());
        let reasons = |side: &str| {
            history(side, pr, "dequeued")
                .into_iter()
                .filter_map(|f| f.data["code"].as_str().map(str::to_string))
                .collect::<Vec<_>>()
        };
        let dequeue_reasons = cmp(reasons(SCHLANGE), reasons(MERGIFY));
        let s_deq = get_last(SCHLANGE, pr, "dequeued")
            .and_then(|f| f.data["code"].as_str().map(str::to_string));
        let m_deq = get_last(MERGIFY, pr, "dequeued")
            .and_then(|f| f.data["code"].as_str().map(str::to_string));
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
        let occurrence_position = |side: &str, kind: &str| {
            facts
                .iter()
                .rposition(|f| f.side == side && f.pr == pr && f.kind == kind)
        };
        let cycle_done = |side: &str, landed: bool| {
            if landed {
                return true;
            }
            match (
                occurrence_position(side, "queued"),
                occurrence_position(side, "dequeued"),
            ) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(q), Some(d)) => d > q,
            }
        };
        let s_done = cycle_done(SCHLANGE, s_land.is_some());
        let m_done = elsewhere || cycle_done(MERGIFY, m_land.is_some());
        let mut verdict = "agree";
        if !queue_entries.agree && s_done && m_done {
            differences.push(format!(
                "queue entries: Schlange {}, Mergify {}",
                queue_entries.schlange, queue_entries.mergify
            ));
        }
        if !dequeue_reasons.agree && s_done && m_done {
            differences.push(format!(
                "dequeue history: Schlange {:?}, Mergify {:?}",
                dequeue_reasons.schlange, dequeue_reasons.mergify
            ));
        }
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
            if !d.agree && dequeue_reasons.agree && s_done && m_done {
                differences.push(format!(
                    "dequeue reason: Schlange {}, Mergify {}",
                    s_deq.as_deref().unwrap_or("none"),
                    m_deq.as_deref().unwrap_or("none")
                ));
            } else if !d.agree && dequeue_reasons.agree {
                verdict = "pending";
            }
            dequeue_reason = Some(d);
        } else if !(s_done && m_done) {
            verdict = "pending";
        }
        // A matching prefix is not agreement while either side has a live latest cycle.
        if !(s_done && m_done) && verdict != "inconclusive" {
            verdict = "pending";
        }
        if !differences.is_empty() && verdict == "agree" {
            verdict = "disagree";
        }
        let history_incomplete = get(SCHLANGE, pr, "history_incomplete").is_some()
            || get(MERGIFY, pr, "history_incomplete").is_some();
        if history_incomplete {
            verdict = "inconclusive";
            differences = vec!["shadow occurrence history is incomplete".into()];
        }
        // Merged or closed before shadow mode watched (before its start, or while it was
        // stopped): Schlange could not decide, so there is nothing to compare.
        let before_watching = get(SCHLANGE, pr, "seen").is_none();
        if before_watching {
            verdict = "inconclusive";
            differences =
                vec!["not seen open: it closed while shadow mode was not watching".into()];
        }
        out.push(PrComparison {
            number: pr,
            verdict: verdict.into(),
            queued,
            queue_entries,
            landed,
            order,
            batch,
            dequeue_reason,
            dequeue_reasons,
            differences,
            before_watching,
            history_incomplete,
        });
    }
    let count = |v: &str| out.iter().filter(|p| p.verdict == v).count();
    ShadowReport {
        repository: repo.to_string(),
        draft_observer,
        history_incomplete: repository_history_incomplete,
        agreements: count("agree"),
        disagreements: count("disagree"),
        inconclusive: count("inconclusive"),
        pending: count("pending"),
        pull_requests: out,
    }
}

impl ShadowReport {
    /// The report as text, without the pull requests that closed before shadow mode watched
    /// (their number is in the first line).
    pub fn to_text(&self) -> String {
        self.text(false)
    }

    /// The report as text with every pull request.
    pub fn to_text_all(&self) -> String {
        self.text(true)
    }

    fn text(&self, all: bool) -> String {
        let hidden = if all {
            0
        } else {
            self.pull_requests
                .iter()
                .filter(|p| p.before_watching)
                .count()
        };
        let mut head = format!(
            "shadow report for {}: {} agree, {} disagree, {} inconclusive, {} pending",
            self.repository, self.agreements, self.disagreements, self.inconclusive, self.pending
        );
        if hidden > 0 {
            head.push_str(&format!(
                "; {hidden} closed before shadow mode watched (--all lists them)"
            ));
        }
        let mut out = vec![head];
        if self.history_incomplete {
            out.push(
                "repository occurrence history is incomplete; no acceptance claim is possible"
                    .into(),
            );
        }
        for p in self
            .pull_requests
            .iter()
            .filter(|p| all || !p.before_watching)
        {
            let mut line = format!(
                "#{} {}: queued {}/{}, entries {}/{}, landed {}/{}",
                p.number,
                p.verdict,
                yn(p.queued.schlange),
                yn(p.queued.mergify),
                p.queue_entries.schlange,
                p.queue_entries.mergify,
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
    fn embarked_numbers_are_structured_entries_not_title_mentions() {
        assert_eq!(
            embarked(
                "Embarked pull requests:\n#50: Fix regression from #12 (#50)\n#51\n",
                99
            ),
            vec![50, 51]
        );
        assert_eq!(embarked("Fix #12 (#50)\nSee #51", 99), Vec::<u64>::new());
        assert_eq!(
            embarked(
                "Embarked pull requests:\n#50\n#50 again\n#99\n\nPull requests ahead:\n#12",
                99
            ),
            vec![50]
        );
    }

    #[test]
    fn only_the_terminal_title_member_list_is_authoritative() {
        let body = "Embarked pull requests:\n#42\n";
        assert_eq!(draft_members("Fix #412 (#500)", body, 999), vec![500]);
        assert_eq!(
            draft_members("Fix #412 (#500, #501)", body, 999),
            vec![500, 501]
        );
        assert_eq!(draft_members("Fix #412 (#500, #500)", body, 999), vec![500]);
        assert_eq!(draft_members("Fix #412 (see #500)", body, 999), vec![42]);
        assert!(draft_members("Fix #412 (#500) continued", "", 999).is_empty());
        assert!(draft_members("Fix #412", "", 999).is_empty());
    }

    #[test]
    fn generic_mergify_checks_do_not_mean_queued_and_nonqueue_bot_prs_are_ignored() {
        use schlange_core::model::{CheckRun, Conclusion};
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", None);
        let mut p = PullRequest::new(50, "author", "main", "feature", "head");
        for (name, conclusion) in [
            ("Summary", Conclusion::Success),
            ("Rule: ready (queue)", Conclusion::Neutral),
            ("Rule: ready (queue)", Conclusion::Failure),
        ] {
            p.checks = vec![CheckRun {
                name: name.into(),
                app: Some("mergify".into()),
                head_sha: p.head_sha.clone(),
                conclusion: Some(conclusion),
                ..Default::default()
            }];
            s.note_other(&p, 1);
        }
        assert!(s.facts().is_empty());
        p.number = 99;
        p.author = "mergify[bot]".into();
        p.body = "Embarked pull requests:\n#12".into();
        s.note_other(&p, 2);
        assert!(!s.has(MERGIFY, 12, "queued"));
        assert_eq!(
            report("o/r", s.facts())
                .draft_observer
                .matching_configured_author,
            vec![99]
        );
        p.head_ref = "mergify/merge-queue/a".into();
        p.body = "Embarked pull requests:\n#50: Fixes #12".into();
        s.note_other(&p, 3);
        assert!(s.has(MERGIFY, 50, "queued"));
        assert!(!s.has(MERGIFY, 12, "queued"));
    }

    #[test]
    fn signed_short_lived_drafts_are_durable_and_replay_is_deduplicated() {
        let dir = std::env::temp_dir().join(format!(
            "schlange-shadow-history-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", Some(store.clone()));
        let pull = |number| ShadowPull {
            number,
            author: "mergify[bot]".into(),
            base: "main".into(),
            head_ref: format!("mergify/merge-queue/{number:010x}"),
            title: "queue draft (#50)".into(),
            body: String::new(),
        };
        s.observe_pull_webhook(&pull(80), 1);
        s.observe_pull_webhook(&pull(80), 2); // durable inbox replay
        s.observe_pull_webhook(&pull(81), 3); // a later requeue
        let before = report("o/r", s.facts());
        let row = before
            .pull_requests
            .iter()
            .find(|p| p.number == 50)
            .unwrap();
        assert_eq!(row.queue_entries.mergify, 2);

        drop(s);
        let restarted = Shadow::new(ShadowConfig::default(), "o/r", Some(store));
        let after = report("o/r", restarted.facts());
        let row = after.pull_requests.iter().find(|p| p.number == 50).unwrap();
        assert_eq!(row.queue_entries.mergify, 2);
        assert_eq!(
            restarted
                .facts()
                .iter()
                .filter(|f| f.side == MERGIFY && f.pr == 50 && f.kind == "batch")
                .count(),
            2
        );
        drop(restarted);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn schlange_queue_cycles_append_and_replay_deduplicates_across_restart() {
        let dir = std::env::temp_dir().join(format!(
            "schlange-shadow-cycles-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", Some(store.clone()));
        s.note_schlange_entry(50, "default", "ready", 1);
        s.note_schlange_entry(50, "default", "ready", 2); // replay of this cycle
        s.note_schlange_dequeue(50, Some("PR_MANUALLY_DEQUEUED"), "manual", 3);
        s.note_schlange_dequeue(50, Some("PR_MANUALLY_DEQUEUED"), "manual", 4); // replay
        s.note_schlange_entry(50, "default", "ready", 5);
        drop(s);

        let mut restarted = Shadow::new(ShadowConfig::default(), "o/r", Some(store));
        restarted.note_schlange_entry(50, "default", "ready", 6); // replay after restart
        restarted.note_schlange_dequeue(50, Some("CHECKS_FAILED"), "red", 7);
        let row = report("o/r", restarted.facts()).pull_requests.remove(0);
        assert_eq!(row.queue_entries.schlange, 2);
        assert_eq!(
            row.dequeue_reasons.schlange,
            vec!["PR_MANUALLY_DEQUEUED", "CHECKS_FAILED"]
        );
        drop(restarted);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn the_text_report_hides_what_closed_before_shadow_mode_watched() {
        let facts = vec![
            // #1: seen open, both queued and landed it.
            f(SCHLANGE, 1, "seen", 0, 1, json!({})),
            f(SCHLANGE, 1, "queued", 1, 2, json!({})),
            f(MERGIFY, 1, "queued", 1, 3, json!({})),
            f(SCHLANGE, 1, "landed", 5, 4, json!({ "batch": [1] })),
            f(MERGIFY, 1, "landed", 6, 5, json!({ "batch": [1] })),
            // #2, #3: Mergify landed them before shadow mode watched.
            f(MERGIFY, 2, "landed", 0, 6, json!({ "batch": [2] })),
            f(MERGIFY, 3, "landed", 0, 7, json!({ "batch": [3] })),
        ];
        let r = report("o/r", &facts);
        let before: Vec<u64> = r
            .pull_requests
            .iter()
            .filter(|p| p.before_watching)
            .map(|p| p.number)
            .collect();
        assert_eq!(before, vec![2, 3]);
        let text = r.to_text();
        assert!(text.contains("#1 agree"), "{text}");
        assert!(!text.contains("#2 ") && !text.contains("#3 "), "{text}");
        assert!(
            text.contains("2 closed before shadow mode watched (--all lists them)"),
            "{text}"
        );
        let all = r.to_text_all();
        assert!(
            all.contains("#2 inconclusive") && all.contains("#3 inconclusive"),
            "{all}"
        );
        assert!(!all.contains("--all lists them"), "{all}");
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
            // #6: Mergify landed it while shadow mode was not watching.
            f(MERGIFY, 6, "landed", 0, 17, json!({ "batch": [6] })),
            // #7: seen open, explicitly never queued by either side.
            f(SCHLANGE, 7, "seen", 0, 18, json!({})),
        ];
        // Schlange saw #1 to #5 open.
        let mut facts = facts;
        for pr in 1..=5 {
            facts.push(f(SCHLANGE, pr, "seen", 0, 100 + pr, json!({})));
        }
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
                (5, "pending"),
                (6, "inconclusive"),
                (7, "agree")
            ]
        );
        assert_eq!(
            (r.agreements, r.disagreements, r.pending, r.inconclusive),
            (3, 2, 1, 1)
        );
        assert!(r.to_text().contains(
            "dequeue history: Schlange [\"CHECKS_FAILED\"], Mergify [\"CHECKS_TIMEOUT\"]"
        ));
    }

    #[test]
    fn report_compares_repeated_queue_and_dequeue_history() {
        let facts = vec![
            f(SCHLANGE, 8, "seen", 0, 1, json!({})),
            f(SCHLANGE, 8, "queued", 1, 2, json!({})),
            f(MERGIFY, 8, "queued", 1, 3, json!({ "draft": 80 })),
            f(
                SCHLANGE,
                8,
                "dequeued",
                2,
                4,
                json!({ "code": "PR_MANUALLY_DEQUEUED" }),
            ),
            f(
                MERGIFY,
                8,
                "dequeued",
                2,
                5,
                json!({ "code": "PR_MANUALLY_DEQUEUED" }),
            ),
            f(SCHLANGE, 8, "queued", 3, 6, json!({})),
            f(MERGIFY, 8, "queued", 3, 7, json!({ "draft": 81 })),
            f(SCHLANGE, 8, "landed", 4, 8, json!({ "batch": [8] })),
            f(MERGIFY, 8, "landed", 4, 9, json!({ "batch": [8] })),
        ];
        let row = report("o/r", &facts).pull_requests.remove(0);
        assert_eq!(row.verdict, "agree");
        assert_eq!(row.queue_entries, cmp(2, 2));
        assert_eq!(
            row.dequeue_reasons,
            cmp(
                vec!["PR_MANUALLY_DEQUEUED".to_string()],
                vec!["PR_MANUALLY_DEQUEUED".to_string()]
            )
        );
    }

    #[test]
    fn watched_never_queued_pull_request_is_an_explicit_zero_history_agreement() {
        let row = report("o/r", &[f(SCHLANGE, 16, "seen", 1, 1, json!({}))])
            .pull_requests
            .remove(0);
        assert_eq!(row.verdict, "agree");
        assert_eq!(row.queue_entries, cmp(0, 0));
        assert_eq!(row.dequeue_reasons, cmp(Vec::new(), Vec::new()));
        assert!(!row.before_watching);
    }

    #[test]
    fn unfinished_latest_cycle_is_pending_even_when_prior_history_matches() {
        let facts = vec![
            f(SCHLANGE, 8, "seen", 0, 1, json!({})),
            f(SCHLANGE, 8, "queued", 1, 2, json!({})),
            f(MERGIFY, 8, "queued", 1, 3, json!({})),
            f(SCHLANGE, 8, "dequeued", 2, 4, json!({ "code": "RED" })),
            f(MERGIFY, 8, "dequeued", 2, 5, json!({ "code": "RED" })),
            f(SCHLANGE, 8, "queued", 3, 6, json!({})),
        ];
        let row = report("o/r", &facts).pull_requests.remove(0);
        assert_eq!(row.verdict, "pending");
        assert_eq!(row.queue_entries, cmp(2, 1));
    }

    #[test]
    fn legacy_report_json_deserializes_history_from_legacy_fields() {
        let json = json!({
            "number": 8,
            "verdict": "agree",
            "queued": {"schlange": true, "mergify": true, "agree": true},
            "landed": {"schlange": false, "mergify": false, "agree": true},
            "dequeue_reason": {
                "schlange": "RED", "mergify": "RED", "agree": true
            },
            "differences": []
        });
        let row: PrComparison = serde_json::from_value(json).unwrap();
        assert_eq!(row.queue_entries, cmp(1, 1));
        assert_eq!(
            row.dequeue_reasons,
            cmp(vec!["RED".into()], vec!["RED".into()])
        );
    }

    #[test]
    fn prior_diagnostic_report_without_structures_deserializes() {
        let old = json!({
            "repository": "o/r",
            "draft_observer": {
                "matching_queue_prefix": [80],
                "matching_configured_author": [80],
                "structured_membership_parsed": []
            },
            "agreements": 0,
            "disagreements": 0,
            "inconclusive": 0,
            "pending": 0,
            "pull_requests": []
        });
        let report: ShadowReport = serde_json::from_value(old).unwrap();
        assert!(report.draft_observer.structures.is_empty());
        assert!(!report.draft_observer.truncated);
    }

    #[test]
    fn draft_structure_is_bounded_and_marks_truncation() {
        let references = (1..=DRAFT_REFERENCE_NUMBERS_MAX + 1)
            .map(|number| format!("#{number}"))
            .collect::<Vec<_>>()
            .join(" ");
        let body = std::iter::repeat_n(references.as_str(), DRAFT_BODY_LINES_MAX + 1)
            .collect::<Vec<_>>()
            .join("\n");
        let structure = draft_structure(1, &references, &body);
        assert_eq!(
            structure.title_hash_references.len(),
            DRAFT_REFERENCE_NUMBERS_MAX
        );
        assert_eq!(structure.body_line_count, DRAFT_BODY_LINES_MAX);
        assert_eq!(
            structure.body_reference_lines.len(),
            DRAFT_REFERENCE_LINES_MAX
        );
        assert!(structure.title_hash_references_truncated);
        assert!(structure.body_lines_truncated);
        assert!(structure.body_reference_lines_truncated);
        assert!(structure.body_reference_lines[0].hash_references_truncated);
        assert!(structure.truncated);
    }

    #[test]
    fn candidate_limit_is_explicit_in_the_report() {
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", None);
        for number in 1..=SHADOW_DRAFT_PROBES_MAX as u64 + 1 {
            s.note_draft_probe(draft_structure(number, "", ""), true, false, false, number);
        }
        let report = report("o/r", s.facts());
        assert_eq!(
            report.draft_observer.structures.len(),
            SHADOW_DRAFT_PROBES_MAX
        );
        assert!(report.draft_observer.truncated);
        assert!(s.has("observer", 0, "draft_probe_truncated"));
    }

    #[test]
    fn failed_event_append_does_not_claim_the_source_key_in_memory() {
        let dir = std::env::temp_dir().join(format!(
            "schlange-shadow-store-failure-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        rusqlite::Connection::open(dir.join("schlange.db"))
            .unwrap()
            .execute_batch("DROP TABLE shadow_events")
            .unwrap();
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", Some(store));
        let pull = ShadowPull {
            number: 80,
            author: "mergify[bot]".into(),
            base: "main".into(),
            head_ref: "mergify/merge-queue/0000000050".into(),
            title: "queue draft (#50)".into(),
            body: String::new(),
        };
        s.observe_pull_webhook(&pull, 1);
        assert!(!s.has(MERGIFY, 50, "queued"));
        assert!(s.has("observer", 0, "history_incomplete"));
        assert!(!s.event_keys.contains("mergify:50:queued:draft:80"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn failed_draft_probe_persistence_refuses_webhook_completion() {
        let dir = std::env::temp_dir().join(format!(
            "schlange-shadow-probe-failure-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::open(&dir).unwrap();
        rusqlite::Connection::open(dir.join("schlange.db"))
            .unwrap()
            .execute_batch("DROP TABLE shadow")
            .unwrap();
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", Some(store));
        let pull = ShadowPull {
            number: 80,
            author: "mergify[bot]".into(),
            base: "main".into(),
            head_ref: "mergify/merge-queue/0000000050".into(),
            title: "queue draft (#50)".into(),
            body: String::new(),
        };
        assert!(!s.observe_pull_webhook(&pull, 1));
        assert!(!s.has("observer", 80, "draft_probe"));
        assert!(!s.has(MERGIFY, 50, "queued"));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn occurrence_limit_marks_history_inconclusive_instead_of_false_agreement() {
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", None);
        s.fact(SCHLANGE, 50, "seen", 0, json!({}), true);
        for n in 0..=SHADOW_EVENTS_PER_STREAM_MAX {
            s.event(
                SCHLANGE,
                50,
                "queued",
                format_args!("test:{n}"),
                n as u64,
                json!({}),
            );
        }
        assert_eq!(
            s.facts()
                .iter()
                .filter(|f| f.side == SCHLANGE && f.pr == 50 && f.kind == "queued")
                .count(),
            SHADOW_EVENTS_PER_STREAM_MAX
        );
        let row = report("o/r", s.facts()).pull_requests.remove(0);
        assert_eq!(row.verdict, "inconclusive");
        assert!(row.history_incomplete);
    }

    #[test]
    fn repository_occurrence_limit_marks_the_whole_report_incomplete() {
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", None);
        s.fact(SCHLANGE, 50, "seen", 0, json!({}), true);
        s.event_total = SHADOW_EVENTS_REPO_MAX;
        assert!(s.event(SCHLANGE, 50, "queued", "entry:1", 1, json!({})));
        let report = report("o/r", s.facts());
        assert!(report.history_incomplete);
        assert_eq!(report.pull_requests[0].queue_entries.schlange, 0);
    }

    #[test]
    fn draft_observer_diagnostics_are_structural_and_not_comparison_rows() {
        let mut s = Shadow::new(ShadowConfig::default(), "o/r", None);
        let mut author_only = PullRequest::new(70, "mergify[bot]", "main", "other", "a");
        author_only.title = "private author-only #51 /pull/52 title".into();
        author_only.body = "- #53: private\n[private](/pull/54)\nprivate only".into();
        s.note_other(&author_only, 1);

        let mut prefix_only = PullRequest::new(
            71,
            "different-bot",
            "main",
            "mergify/merge-queue/0000000047",
            "b",
        );
        prefix_only.body = "private prefix-only body".into();
        s.note_other(&prefix_only, 2);

        let mut parsed = PullRequest::new(
            72,
            "mergify[bot]",
            "main",
            "mergify/merge-queue/0000000048",
            "c",
        );
        parsed.title = "queue draft (#50)".into();
        s.note_other(&parsed, 3);

        let report = report("o/r", s.facts());
        assert!(
            report
                .pull_requests
                .iter()
                .all(|row| ![70, 71, 72].contains(&row.number))
        );
        assert_eq!(report.draft_observer.matching_queue_prefix, vec![71, 72]);
        assert_eq!(
            report.draft_observer.matching_configured_author,
            vec![70, 72]
        );
        assert_eq!(report.draft_observer.structured_membership_parsed, vec![72]);
        let structure = report
            .draft_observer
            .structures
            .iter()
            .find(|structure| structure.number == 70)
            .unwrap();
        assert_eq!(structure.title_hash_references, vec![51]);
        assert_eq!(structure.title_pull_path_references, vec![52]);
        assert_eq!(structure.body_line_count, 3);
        assert_eq!(structure.body_reference_lines.len(), 2);
        assert_eq!(structure.body_reference_lines[0].line, 0);
        assert_eq!(structure.body_reference_lines[0].hash_references, vec![53]);
        assert!(structure.body_reference_lines[0].starts_with_list_marker);
        assert!(structure.body_reference_lines[0].starts_with_hash_reference);
        assert_eq!(
            structure.body_reference_lines[1].pull_path_references,
            vec![54]
        );
        assert!(structure.body_reference_lines[1].starts_with_markdown_link);
        let json = serde_json::to_string(&report).unwrap();
        assert!(!json.contains("private author-only"));
        assert!(!json.contains("private prefix-only body"));
        assert!(!json.contains("different-bot"));
        assert!(!json.contains("mergify[bot]"));
    }
}
