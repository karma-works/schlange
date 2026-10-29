//! One writer per repository across processes and hosts (MQ-32, KONZEPT.md section 6: "Ein
//! Schreiber pro Repo und Branch", lease plus fencing).
//!
//! Two layers:
//!
//! - **Local lease** (one data directory, [`crate::store`]): a row per repository with holder
//!   and expiry, taken and renewed in an immediate SQLite transaction. Two processes on the same
//!   data directory never both hold it; the journal checks the holder in the same transaction
//!   as each checkpoint and intent, so a process that lost the lease cannot overwrite the
//!   journal of the one that took over.
//! - **Fence on GitHub** (across hosts): the reference `refs/schlange/lease` points to a commit
//!   whose message names the holder and the expiry. Taking or renewing the lease creates a
//!   child of the commit read and moves the reference to it with `force: false`; GitHub accepts
//!   only a fast-forward, so of two processes that build on the same commit only the first
//!   wins (a real compare-and-swap). Before **every** write the [`LeaseGate`] reads the
//!   reference and lets the write through only while it still points to this process's own
//!   commit and the lease has not expired by this process's clock.
//!
//! What remains (documented, as for landing in `api.rs`): the read of the fence and the write
//! are two requests. A process paused for longer than the lease between them could still send
//! one write after another process took over. The lease's time-to-live bounds that window; the
//! engine's own compare-and-swap reads before landing (expected base, expected head) still apply
//! to that write. Clocks of different hosts must agree to well within the time-to-live.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use schlange_core::model::Time;

use crate::api::RepoApi;
use crate::client::{Priority, WriteGate};
use crate::store::Store;
use crate::time::Clock;

/// The fence reference, without `refs/`.
pub const FENCE_REF: &str = "schlange/lease";

#[derive(Clone, Debug)]
pub struct LeaseConfig {
    /// This process's name in the lease (unique per process: host, pid, random).
    pub holder: String,
    /// Seconds a lease lasts without renewal; renewed after half of it.
    pub ttl: Time,
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct LeaseStatus {
    pub holder: String,
    pub held: bool,
    pub expires: Option<Time>,
    /// The fence commit this process wrote last.
    pub fence: Option<String>,
    /// Who holds it, as last read, when not this process.
    pub other: Option<String>,
    pub last_error: Option<String>,
    /// Writes the gate refused because the lease was not held.
    pub refused: u64,
}

#[derive(Default)]
struct Inner {
    held: bool,
    expires: Time,
    mine: Option<String>,
    next_try: Time,
    other: Option<String>,
    last_error: Option<String>,
    refused: u64,
    tree: Option<String>,
    /// The last acquisition built on the fence this data directory's leader wrote last.
    journal_current: bool,
}

/// The lease of one repository.
pub struct Lease {
    api: RepoApi,
    store: Option<Store>,
    cfg: LeaseConfig,
    clock: Arc<dyn Clock>,
    inner: Mutex<Inner>,
}

/// The record in a fence commit's message.
fn record(holder: &str, expires: Time) -> String {
    format!("schlange lease\n\nholder: {holder}\nexpires: {expires}\n")
}

fn parse_record(message: &str) -> Option<(String, Time)> {
    let mut holder = None;
    let mut expires = None;
    for l in message.lines() {
        if let Some(h) = l.strip_prefix("holder: ") {
            holder = Some(h.trim().to_string());
        }
        if let Some(e) = l.strip_prefix("expires: ") {
            expires = e.trim().parse().ok();
        }
    }
    Some((holder?, expires?))
}

impl Lease {
    pub fn new(
        api: RepoApi,
        store: Option<Store>,
        cfg: LeaseConfig,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Lease {
            api,
            store,
            cfg,
            clock,
            inner: Mutex::new(Inner::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn holder(&self) -> &str {
        &self.cfg.holder
    }

    pub fn repo_full(&self) -> String {
        self.api.repo.full()
    }

    /// Whether the last acquisition took over from the leader whose journal is in this data
    /// directory (this process before, or another one on the same directory), so nobody else
    /// acted since that journal was written.
    pub fn journal_is_current(&self) -> bool {
        self.lock().journal_current
    }

    fn meta_key(&self) -> String {
        format!("fence:{}", self.api.repo.full())
    }

    /// Records the fence commit this data directory's leader wrote, and whether it built on the
    /// one recorded before.
    fn won(&self, built_on: Option<&str>, sha: &str) {
        let Some(s) = &self.store else { return };
        let current = match s.meta(&self.meta_key()) {
            Ok(before) if before.as_deref() == built_on => {
                s.set_meta(&self.meta_key(), sha).is_ok()
            }
            // Do not record an untrusted successor: a later renewal could mistake it for a
            // trusted predecessor. Read/write errors also leave the lineage unproven.
            _ => false,
        };
        self.lock().journal_current = current;
    }

    pub fn status(&self) -> LeaseStatus {
        let i = self.lock();
        LeaseStatus {
            holder: self.cfg.holder.clone(),
            held: i.held,
            expires: i.held.then_some(i.expires),
            fence: i.mine.clone(),
            other: i.other.clone(),
            last_error: i.last_error.clone(),
            refused: i.refused,
        }
    }

    /// Whether this process holds the lease now; takes or renews it when due. A follower
    /// tries again after a quarter of the time-to-live.
    pub fn ensure(&self) -> bool {
        let now = self.clock.now();
        {
            let i = self.lock();
            if i.held && now + self.cfg.ttl / 2 < i.expires {
                return true;
            }
            if !i.held && now < i.next_try {
                return false;
            }
        }
        let r = self.acquire(now);
        let mut i = self.lock();
        match r {
            Ok(Some(sha)) => {
                i.held = true;
                i.expires = now + self.cfg.ttl;
                i.mine = Some(sha);
                i.other = None;
                i.last_error = None;
            }
            Ok(None) => {
                i.held = false;
                i.next_try = now + (self.cfg.ttl / 4).max(1);
            }
            Err(e) => {
                // A failed renewal keeps the lease until it expires; the gate refuses writes
                // once it has.
                i.last_error = Some(e);
                if i.held && now >= i.expires {
                    i.held = false;
                }
                if !i.held {
                    i.next_try = now + (self.cfg.ttl / 4).max(1);
                }
            }
        }
        i.held
    }

    /// One attempt: the local lease, then the fence. `Ok(Some(commit))` when held.
    fn acquire(&self, now: Time) -> Result<Option<String>, String> {
        let repo = self.api.repo.full();
        let expires = now + self.cfg.ttl;
        if let Some(s) = &self.store
            && !s
                .lease_acquire(&repo, &self.cfg.holder, now, expires)
                .map_err(|e| e.to_string())?
        {
            self.lock().other = s.lease_holder(&repo).ok().flatten().map(|(h, _)| h);
            return Ok(None);
        }
        let prio = Priority::Landing;
        let tip = self
            .api
            .fence_ref(FENCE_REF, prio)
            .map_err(|e| e.to_string())?;
        let tree = self.tree(prio)?;
        let message = record(&self.cfg.holder, expires);
        match tip {
            None => {
                let c = self
                    .api
                    .fence_commit(&tree, &[], &message, prio)
                    .map_err(|e| e.to_string())?;
                if self
                    .api
                    .fence_create(FENCE_REF, &c, prio)
                    .map_err(|e| e.to_string())?
                {
                    self.won(None, &c);
                    Ok(Some(c))
                } else {
                    self.step_back(&repo);
                    Ok(None)
                }
            }
            Some(t) => {
                let current = self.api.git_commit(&t, prio).map_err(|e| e.to_string())?;
                if let Some((h, e)) = parse_record(&current.message)
                    && h != self.cfg.holder
                    && e > now
                {
                    self.lock().other = Some(h);
                    self.step_back(&repo);
                    return Ok(None);
                }
                // Our own lease, an expired one, or an unreadable record: build on it.
                let c = self
                    .api
                    .fence_commit(&tree, std::slice::from_ref(&t), &message, prio)
                    .map_err(|e| e.to_string())?;
                if self
                    .api
                    .fence_advance(FENCE_REF, &c, prio)
                    .map_err(|e| e.to_string())?
                {
                    self.won(Some(&t), &c);
                    Ok(Some(c))
                } else {
                    self.step_back(&repo);
                    Ok(None)
                }
            }
        }
    }

    /// Lost on GitHub: give the local lease back so a process on this data directory that
    /// does hold the fence is not blocked.
    fn step_back(&self, repo: &str) {
        if let Some(s) = &self.store {
            let _ = s.lease_release(repo, &self.cfg.holder);
        }
    }

    /// A tree for the fence commits: the default branch's (any existing tree will do).
    fn tree(&self, prio: Priority) -> Result<String, String> {
        if let Some(t) = self.lock().tree.clone() {
            return Ok(t);
        }
        let default = self
            .api
            .repository(prio)
            .map_err(|e| e.to_string())?
            .default_branch;
        let tip = self
            .api
            .branch_sha(&default, prio)
            .map_err(|e| e.to_string())?;
        let tree = self
            .api
            .git_commit(&tip, prio)
            .map_err(|e| e.to_string())?
            .tree
            .sha;
        self.lock().tree = Some(tree.clone());
        Ok(tree)
    }

    /// Whether a write may go out now: the lease is held and not expired, the local lease
    /// names this process, and the fence on GitHub is still this process's commit (read now).
    pub fn check(&self) -> Result<(), String> {
        let now = self.clock.now();
        let mine = {
            let i = self.lock();
            if !i.held {
                return Err("this process does not hold the lease".into());
            }
            if now >= i.expires {
                return Err("the lease expired before it was renewed".into());
            }
            i.mine.clone()
        };
        let repo = self.api.repo.full();
        if let Some(s) = &self.store {
            let h = s.lease_holder(&repo).map_err(|e| e.to_string())?;
            if h.as_ref().map(|(h, _)| h.as_str()) != Some(self.cfg.holder.as_str()) {
                return Err(self.lost(format!(
                    "the local lease is held by {}",
                    h.map(|(h, _)| h).unwrap_or_else(|| "nobody".into())
                )));
            }
        }
        let tip = self
            .api
            .fence_ref(FENCE_REF, Priority::Landing)
            .map_err(|e| format!("the fence cannot be read: {e}"))?;
        if tip != mine {
            return Err(self.lost(format!(
                "the fence moved to {} (another process took the lease)",
                tip.as_deref().unwrap_or("nothing")
            )));
        }
        Ok(())
    }

    fn lost(&self, why: String) -> String {
        let mut i = self.lock();
        i.held = false;
        i.next_try = 0;
        i.last_error = Some(why.clone());
        why
    }

    /// Gives the lease up (clean shutdown): the local row. The fence expires by itself.
    pub fn release(&self) {
        if let Some(s) = &self.store {
            let _ = s.lease_release(&self.api.repo.full(), &self.cfg.holder);
        }
        self.lock().held = false;
    }
}

/// The write gate of a process with leases: a write to a repository goes out only through
/// [`Lease::check`]. Writes outside every served repository are refused.
pub struct LeaseGate {
    /// `/repos/{owner}/{name}/` → its lease.
    leases: BTreeMap<String, Arc<Lease>>,
}

impl LeaseGate {
    pub fn new(leases: Vec<Arc<Lease>>) -> Self {
        LeaseGate {
            leases: leases
                .into_iter()
                .map(|l| {
                    (
                        format!(
                            "/repos/{}/{}/",
                            crate::client::enc_path(&l.api.repo.owner),
                            crate::client::enc_path(&l.api.repo.name)
                        ),
                        l,
                    )
                })
                .collect(),
        }
    }
}

impl WriteGate for LeaseGate {
    fn permit(&self, _method: &str, path: &str) -> Result<(), String> {
        let lease = self
            .leases
            .iter()
            .find(|(prefix, _)| path.starts_with(prefix.as_str()))
            .map(|(_, l)| l)
            .ok_or_else(|| "no lease covers this path".to_string())?;
        lease.check().inspect_err(|_| {
            lease.lock().refused += 1;
        })
    }
}

/// The write gate of shadow mode: refuses every write. With `panic_on_write` (tests) an
/// attempted write is a bug and panics; every attempt is counted either way.
pub struct ShadowGate {
    pub panic_on_write: bool,
    attempts: Mutex<Vec<String>>,
}

impl ShadowGate {
    pub fn new(panic_on_write: bool) -> Self {
        ShadowGate {
            panic_on_write,
            attempts: Mutex::new(Vec::new()),
        }
    }

    /// The writes something tried to send (none, unless there is a bug).
    pub fn attempts(&self) -> Vec<String> {
        self.attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}

impl WriteGate for ShadowGate {
    fn permit(&self, method: &str, path: &str) -> Result<(), String> {
        self.attempts
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(format!("{method} {path}"));
        if self.panic_on_write {
            panic!("shadow mode tried to write to GitHub: {method} {path}");
        }
        Err("shadow mode never writes to GitHub".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_round_trip() {
        assert_eq!(
            parse_record(&record("host-1/42", 1234)),
            Some(("host-1/42".into(), 1234))
        );
        assert_eq!(parse_record("something else"), None);
    }

    #[test]
    fn the_shadow_gate_refuses_and_counts() {
        let g = ShadowGate::new(false);
        assert!(g.permit("POST", "/repos/o/r/issues/1/comments").is_err());
        assert_eq!(g.attempts(), vec!["POST /repos/o/r/issues/1/comments"]);
        let p = ShadowGate::new(true);
        let r = std::panic::catch_unwind(|| p.permit("PUT", "/repos/o/r/pulls/1/merge"));
        assert!(r.is_err(), "panics in tests");
    }
}
