//! Persistence for `schlange serve`: one SQLite database (WAL mode) in the data directory
//! (`/data/schlange.db` in the container). KONZEPT.md, section 6: "Journal vor jedem
//! Seiteneffekt", restart from journal plus forge state, SQLite in the volume.
//!
//! | Table | What |
//! |---|---|
//! | `checkpoints` | the engine state per repository after each step, before its side effects |
//! | `intents` | side effects about to be carried out and not yet answered (the audit trail of what may have happened) |
//! | `history` | `checks_timeout: auto` runtimes per `queue@base`, also kept when a snapshot cannot be read back |
//! | `cars` | per car: the tree after each pull request (the landing compares against it) |
//! | `deliveries` | webhook delivery ids handled successfully (GitHub redelivers failed ones) |
//! | `audit` | one row per landed pull request: tested head and tree, base before, landed commit and tree (S-AUDIT) |
//! | `events` | the event log: queue and action events per repository, with the pull requests they belong to (`schlange events`) |
//! | `shadow` | shadow mode: what Schlange would have done and what Mergify did, per pull request (`schlange shadow report`) |
//! | `lease` | which process may act for a repository, until when (MQ-32); journal writes check it |
//!
//! Durability: `synchronous = FULL`, so a checkpoint or an intent is on disk before the side
//! effect it guards is sent. One connection behind a mutex: the workers write a few rows per
//! engine step, which SQLite handles in well under a millisecond.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use rusqlite::{Connection, OptionalExtension, params};

use schlange_core::engine::{Decision, Sample, State};
use schlange_core::journal::{Journal, JournalError, Recovered};
use schlange_core::model::Time;

pub const DB_FILE: &str = "schlange.db";
const SCHEMA_VERSION: i64 = 1;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("database {path}: {source}")]
    Sql {
        path: PathBuf,
        source: rusqlite::Error,
    },
    #[error("data directory {path}: {source}")]
    Dir {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("database {path} has schema version {found}, this binary knows {SCHEMA_VERSION}")]
    Version { path: PathBuf, found: i64 },
}

/// A provisional boundary is evidence only: it does not authorize release.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdoptionBoundary {
    pub epoch: String,
    pub fence: String,
    pub cutoff: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdoptionAge {
    BeforeOrEqual,
    TimestampAfter,
    Unknown,
}

impl AdoptionBoundary {
    /// Classifies only canonical whole-second UTC timestamps. Clock relationship is unproven.
    pub fn classify_created_at(&self, raw: Option<&str>) -> AdoptionAge {
        let Some(raw) = raw else {
            return AdoptionAge::Unknown;
        };
        let Some(at) = crate::time::parse_rfc3339(raw) else {
            return AdoptionAge::Unknown;
        };
        if crate::time::format_rfc3339(at) != raw {
            return AdoptionAge::Unknown;
        }
        if at <= self.cutoff {
            AdoptionAge::BeforeOrEqual
        } else {
            AdoptionAge::TimestampAfter
        }
    }
}

/// Durable authenticated-delivery evidence. Signature verification belongs to the router.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxDelivery {
    pub repo: String,
    pub epoch: String,
    pub guid: String,
    pub event: String,
    pub body: Vec<u8>,
    pub received_at: u64,
}

pub struct SessionReply {
    pub state: State,
    pub revision: u64,
    pub decisions: Vec<Decision>,
    pub intents: Vec<(u64, Decision)>,
}

/// Private fields bind an HTTP read to the durable predecessor observed before the read.
pub struct RefreshReadTicket {
    repo: String,
    session: String,
    revision: u64,
    seq: u64,
    request: Decision,
    ingress: u64,
}

/// A non-replayable, process-local handle to a durable started/unknown build.
#[allow(dead_code)]
pub struct BuildWriteTicket {
    pub(crate) repo: String,
    pub(crate) holder: String,
    pub(crate) session: String,
    pub(crate) revision: u64,
    pub(crate) seq: u64,
    pub(crate) ingress: u64,
    pub(crate) fence: String,
    pub(crate) request: Decision,
}

/// Classification is a candidate for a fresh read, never an authorization to replay.
#[derive(Debug, PartialEq, Eq)]
pub enum InboxReplayKind {
    Reconcile,
    Hold,
}

impl InboxDelivery {
    /// Reproduce the live status hint, never the historical check result.
    pub fn replay_hint(&self) -> Option<crate::webhook::Hint> {
        Some(crate::webhook::Hint::Sha(self.replay_status()?.head_sha))
    }

    /// Decode the original status fact; caller must establish membership, order and authority.
    pub fn replay_status(&self) -> Option<schlange_core::model::CheckRun> {
        if self.replay_kind() != InboxReplayKind::Reconcile {
            return None;
        }
        let value: serde_json::Value = serde_json::from_slice(&self.body).ok()?;
        let id = value.get("id")?.as_u64().filter(|id| *id > 0)?;
        let sha = value.get("sha")?.as_str()?;
        if sha.len() != 40
            || !sha
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return None;
        }
        let context = value.get("context")?.as_str().filter(|s| !s.is_empty())?;
        let created = crate::time::parse_rfc3339(value.get("created_at")?.as_str()?)?;
        let updated = crate::time::parse_rfc3339(value.get("updated_at")?.as_str()?)?;
        if updated < created {
            return None;
        }
        let state = value.get("state")?.as_str()?;
        if !matches!(state, "pending" | "success" | "failure" | "error") {
            return None;
        }
        Some(schlange_core::model::CheckRun {
            name: context.into(),
            head_sha: sha.into(),
            seq: id,
            status: true,
            conclusion: crate::webhook::status_conclusion(state),
            ..Default::default()
        })
    }

    pub fn replay_kind(&self) -> InboxReplayKind {
        if self.event != "status" {
            return InboxReplayKind::Hold;
        }
        // The event header is not covered by GitHub's body signature. Refuse
        // payloads that could also describe a command-bearing event.
        let Ok(serde_json::Value::Object(fields)) = serde_json::from_slice(&self.body) else {
            return InboxReplayKind::Hold;
        };
        if [
            "comment",
            "issue",
            "pull_request",
            "check_run",
            "check_suite",
        ]
        .iter()
        .any(|key| fields.contains_key(*key))
        {
            return InboxReplayKind::Hold;
        }
        match crate::webhook::parse(&self.event, &self.body) {
            Ok(parsed)
                if parsed.repo.as_deref() == Some(&self.repo)
                    && parsed
                        .hints
                        .iter()
                        .any(|hint| matches!(hint, crate::webhook::Hint::Sha(_))) =>
            {
                InboxReplayKind::Reconcile
            }
            _ => InboxReplayKind::Hold,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InboxClaim {
    Inserted,
    Duplicate,
    Processed,
}

/// One landed pull request (S-AUDIT).
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditRecord {
    pub repo: String,
    /// Unix seconds.
    pub at: u64,
    pub car: u64,
    pub number: u64,
    /// The head the queue tested and asked GitHub to merge.
    pub tested_head: String,
    /// The tree the queue tested for this pull request (the prefix tree of the car), if known.
    pub tested_tree: Option<String>,
    /// The base the landing expected (the parent the squash commit should have).
    pub base_before: String,
    pub landed_sha: String,
    pub landed_tree: String,
    /// The landed commit's actual parent.
    pub landed_parent: String,
    /// Anything unusual: a parent or tree that differs, a landing found after a lost reply.
    pub note: String,
}

/// A handle on the database. Cheap to clone; all clones share one connection.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
    pub path: PathBuf,
}

impl std::fmt::Debug for Store {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Store({})", self.path.display())
    }
}

fn sql_err(path: &Path) -> impl Fn(rusqlite::Error) -> StoreError + '_ {
    move |source| StoreError::Sql {
        path: path.to_path_buf(),
        source,
    }
}

fn storage(e: impl std::fmt::Display) -> JournalError {
    JournalError::Storage(e.to_string())
}

impl Store {
    /// Opens (creates) `dir/schlange.db`.
    pub fn open(dir: &Path) -> Result<Store, StoreError> {
        std::fs::create_dir_all(dir).map_err(|source| StoreError::Dir {
            path: dir.to_path_buf(),
            source,
        })?;
        let path = dir.join(DB_FILE);
        let e = sql_err(&path);
        let conn = Connection::open(&path).map_err(&e)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(&e)?;
        // WAL: readers never block the writer; FULL: a commit is on disk when it returns.
        let mode: String = conn
            .query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))
            .map_err(&e)?;
        debug_assert_eq!(mode.to_ascii_lowercase(), "wal");
        conn.execute_batch(
            "PRAGMA synchronous = FULL;
             PRAGMA foreign_keys = ON;
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS checkpoints (
                 repo TEXT PRIMARY KEY, state TEXT NOT NULL, written INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS intents (
                 repo TEXT NOT NULL, seq INTEGER NOT NULL, decision TEXT NOT NULL,
                 PRIMARY KEY (repo, seq));
             CREATE TABLE IF NOT EXISTS inbox (
                 guid TEXT PRIMARY KEY, repo TEXT NOT NULL, epoch TEXT NOT NULL,
                 event TEXT NOT NULL, body BLOB NOT NULL, received_at INTEGER NOT NULL,
                 processed INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS inbox_revisions (
                 repo TEXT PRIMARY KEY, revision INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS inbox_redecide (
                 repo TEXT PRIMARY KEY, revision INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS status_sequence (
                 repo TEXT PRIMARY KEY, session TEXT NOT NULL, holder TEXT NOT NULL, high_water INTEGER NOT NULL,
                 confirmed INTEGER NOT NULL DEFAULT 1, fence TEXT NOT NULL DEFAULT '');
             CREATE TABLE IF NOT EXISTS status_session_history (
                 repo TEXT NOT NULL, session TEXT NOT NULL, PRIMARY KEY (repo, session));
             CREATE TABLE IF NOT EXISTS session_build_claims (
                 repo TEXT PRIMARY KEY, holder TEXT NOT NULL, session TEXT NOT NULL,
                 revision INTEGER NOT NULL, seq INTEGER NOT NULL, ingress INTEGER NOT NULL,
                 fence TEXT NOT NULL, decision TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS adoption_boundaries (
                 repo TEXT PRIMARY KEY, epoch TEXT NOT NULL, fence TEXT NOT NULL, cutoff INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS prepared_dispatches (
                 repo TEXT NOT NULL, session TEXT NOT NULL, id INTEGER NOT NULL,
                 decision TEXT NOT NULL, status TEXT NOT NULL,
                 PRIMARY KEY (repo, session, id));
             CREATE TABLE IF NOT EXISTS history (
                 repo TEXT NOT NULL, key TEXT NOT NULL, at INTEGER NOT NULL, secs INTEGER NOT NULL);
             CREATE INDEX IF NOT EXISTS history_repo ON history (repo, key);
             CREATE TABLE IF NOT EXISTS cars (
                 repo TEXT NOT NULL, car INTEGER NOT NULL, prefix TEXT NOT NULL,
                 PRIMARY KEY (repo, car));
             CREATE TABLE IF NOT EXISTS deliveries (guid TEXT PRIMARY KEY, at INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS audit (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, at INTEGER NOT NULL,
                 car INTEGER NOT NULL, number INTEGER NOT NULL, tested_head TEXT NOT NULL,
                 tested_tree TEXT, base_before TEXT NOT NULL, landed_sha TEXT NOT NULL,
                 landed_tree TEXT NOT NULL, landed_parent TEXT NOT NULL, note TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS events (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, at INTEGER NOT NULL,
                 type TEXT NOT NULL, pull_request INTEGER, pulls TEXT NOT NULL,
                 base_ref TEXT, trigger TEXT NOT NULL, metadata TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS events_repo ON events (repo, id);
             CREATE TABLE IF NOT EXISTS shadow (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, repo TEXT NOT NULL, side TEXT NOT NULL,
                 pr INTEGER NOT NULL, kind TEXT NOT NULL, at INTEGER NOT NULL, data TEXT NOT NULL,
                 UNIQUE (repo, side, pr, kind));
             CREATE TABLE IF NOT EXISTS lease (
                 repo TEXT PRIMARY KEY, holder TEXT NOT NULL, expires INTEGER NOT NULL);",
        )
        .map_err(&e)?;
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(&e)?;
        match version.and_then(|v| v.parse::<i64>().ok()) {
            None => {
                conn.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES ('schema', ?1)",
                    params![SCHEMA_VERSION.to_string()],
                )
                .map_err(&e)?;
            }
            Some(SCHEMA_VERSION) => {}
            Some(found) => {
                return Err(StoreError::Version {
                    path: path.clone(),
                    found,
                });
            }
        }
        let path = path.clone();
        Ok(Store {
            conn: Arc::new(Mutex::new(conn)),
            path,
        })
    }

    fn with<R>(
        &self,
        f: impl FnOnce(&mut Connection) -> rusqlite::Result<R>,
    ) -> rusqlite::Result<R> {
        let mut c = self.conn.lock().unwrap_or_else(|p| p.into_inner());
        f(&mut c)
    }

    /// The journal of one repository.
    pub fn journal(&self, repo: &str) -> SqliteJournal {
        SqliteJournal {
            store: self.clone(),
            repo: repo.to_string(),
            history_written: None,
            holder: None,
        }
    }

    /// The journal mode SQLite reports (`wal`).
    pub fn journal_mode(&self) -> rusqlite::Result<String> {
        self.with(|c| c.query_row("PRAGMA journal_mode", [], |r| r.get(0)))
    }

    // ---- webhook deliveries ------------------------------------------------------------

    pub fn delivery_seen(&self, guid: &str) -> rusqlite::Result<bool> {
        self.with(|c| {
            c.query_row(
                "SELECT 1 FROM deliveries WHERE guid = ?1",
                params![guid],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
        })
    }

    /// Records a delivery that was handled; keeps the newest `keep`.
    pub fn record_delivery(&self, guid: &str, at: u64, keep: u64) -> rusqlite::Result<()> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let provisional: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM adoption_boundaries)",
                [],
                |r| r.get(0),
            )?;
            if provisional {
                return Err(rusqlite::Error::ToSqlConversionFailure(
                    "provisional adoption requires a durable scoped inbox".into(),
                ));
            }
            tx.execute(
                "INSERT OR IGNORE INTO deliveries (guid, at) VALUES (?1, ?2)",
                params![guid, at as i64],
            )?;
            tx.execute(
                "DELETE FROM deliveries WHERE rowid NOT IN
                   (SELECT rowid FROM deliveries ORDER BY at DESC, rowid DESC LIMIT ?1)",
                params![keep as i64],
            )?;
            tx.commit()
        })
    }

    // ---- cars --------------------------------------------------------------------------

    pub fn save_car(&self, repo: &str, car: u64, prefix: &[(u64, String)]) -> rusqlite::Result<()> {
        let text = serde_json::to_string(prefix).unwrap_or_default();
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO cars (repo, car, prefix) VALUES (?1, ?2, ?3)",
                params![repo, car as i64, text],
            )
            .map(|_| ())
        })
    }

    pub fn load_car(&self, repo: &str, car: u64) -> rusqlite::Result<Option<Vec<(u64, String)>>> {
        let text: Option<String> = self.with(|c| {
            c.query_row(
                "SELECT prefix FROM cars WHERE repo = ?1 AND car = ?2",
                params![repo, car as i64],
                |r| r.get(0),
            )
            .optional()
        })?;
        Ok(text.and_then(|t| serde_json::from_str(&t).ok()))
    }

    pub fn delete_car(&self, repo: &str, car: u64) -> rusqlite::Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM cars WHERE repo = ?1 AND car = ?2",
                params![repo, car as i64],
            )
            .map(|_| ())
        })
    }

    // ---- audit -------------------------------------------------------------------------

    pub fn audit(&self, r: &AuditRecord) -> rusqlite::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO audit (repo, at, car, number, tested_head, tested_tree, base_before,
                   landed_sha, landed_tree, landed_parent, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    r.repo,
                    r.at as i64,
                    r.car as i64,
                    r.number as i64,
                    r.tested_head,
                    r.tested_tree,
                    r.base_before,
                    r.landed_sha,
                    r.landed_tree,
                    r.landed_parent,
                    r.note
                ],
            )
            .map(|_| ())
        })
    }

    /// The audit records of a repository, oldest first.
    pub fn audits(&self, repo: &str) -> rusqlite::Result<Vec<AuditRecord>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT repo, at, car, number, tested_head, tested_tree, base_before, landed_sha,
                        landed_tree, landed_parent, note
                 FROM audit WHERE repo = ?1 ORDER BY id",
            )?;
            let rows = st.query_map(params![repo], |r| {
                Ok(AuditRecord {
                    repo: r.get(0)?,
                    at: r.get::<_, i64>(1)? as u64,
                    car: r.get::<_, i64>(2)? as u64,
                    number: r.get::<_, i64>(3)? as u64,
                    tested_head: r.get(4)?,
                    tested_tree: r.get(5)?,
                    base_before: r.get(6)?,
                    landed_sha: r.get(7)?,
                    landed_tree: r.get(8)?,
                    landed_parent: r.get(9)?,
                    note: r.get(10)?,
                })
            })?;
            rows.collect()
        })
    }

    // ---- meta --------------------------------------------------------------------------

    pub fn meta(&self, key: &str) -> rusqlite::Result<Option<String>> {
        self.with(|c| {
            c.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |r| {
                r.get(0)
            })
            .optional()
        })
    }

    pub fn set_meta(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![key, value],
            )
            .map(|_| ())
        })
    }

    /// Advance a cooperating fence lineage atomically. Broken or unknown provenance remains
    /// untrusted until a future explicit recovery protocol establishes it.
    pub fn advance_fence_lineage(
        &self,
        repo: &str,
        holder: &str,
        now: impl FnOnce() -> u64,
        built_on: Option<&str>,
        sha: &str,
    ) -> rusqlite::Result<bool> {
        let fence_key = format!("fence:{repo}");
        let trust_key = format!("fence-trust:{repo}");
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx
                .query_row(
                    "SELECT holder, expires FROM lease WHERE repo = ?1",
                    params![repo],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let now = now();
            if !owner.is_some_and(|(h, expiry)| {
                h == holder && u64::try_from(expiry).is_ok_and(|expiry| expiry > now)
            }) {
                return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into()));
            }
            let old: Option<String> = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![fence_key],
                    |r| r.get(0),
                )
                .optional()?;
            let trust: Option<String> = tx
                .query_row(
                    "SELECT value FROM meta WHERE key = ?1",
                    params![trust_key],
                    |r| r.get(0),
                )
                .optional()?;
            let unconfirmed: bool = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1 AND (typeof(confirmed) != 'integer' OR confirmed != 1))",
                params![repo],
                |r| r.get(0),
            )?;
            let current = !unconfirmed
                && trust.as_deref() == Some("verified")
                && old.as_deref().is_some_and(|old| Some(old) == built_on);
            tx.execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)",
                params![fence_key, sha],
            )?;
            if !current {
                tx.execute(
                    "INSERT OR REPLACE INTO meta (key, value) VALUES (?1, 'untrusted')",
                    params![trust_key],
                )?;
            }
            tx.commit()?;
            Ok(current)
        })
    }

    /// Persist a provisional, locally fenced boundary without overwriting legacy state.
    /// The caller must separately verify the remote fence and establish trusted server time.
    pub fn create_provisional_adoption(
        &self,
        repo: &str,
        holder: &str,
        now: impl FnOnce() -> u64,
        boundary: &AdoptionBoundary,
    ) -> rusqlite::Result<()> {
        if boundary.epoch.is_empty() || boundary.fence.is_empty() {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "invalid adoption boundary".into(),
            ));
        }
        let cutoff = i64::try_from(boundary.cutoff)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) {
                return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into()));
            }
            let trust: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", params![format!("fence-trust:{repo}")], |r| r.get(0)).optional()?;
            if trust.as_deref().is_some_and(|value| value != "untrusted") {
                return Err(rusqlite::Error::ToSqlConversionFailure("existing or malformed lineage requires recovery".into()));
            }
            let legacy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM checkpoints WHERE repo = ?1 UNION ALL SELECT 1 FROM intents WHERE repo = ?1 UNION ALL SELECT 1 FROM history WHERE repo = ?1 UNION ALL SELECT 1 FROM prepared_dispatches WHERE repo = ?1 UNION ALL SELECT 1 FROM session_build_claims WHERE repo = ?1 UNION ALL SELECT 1 FROM status_sequence WHERE repo = ?1 UNION ALL SELECT 1 FROM status_session_history WHERE repo = ?1 UNION ALL SELECT 1 FROM cars WHERE repo = ?1 UNION ALL SELECT 1 FROM audit WHERE repo = ?1 UNION ALL SELECT 1 FROM events WHERE repo = ?1 UNION ALL SELECT 1 FROM shadow WHERE repo = ?1 UNION ALL SELECT 1 FROM inbox_revisions WHERE repo = ?1 UNION ALL SELECT 1 FROM inbox WHERE repo = ?1 UNION ALL SELECT 1 FROM deliveries)", params![repo], |r| r.get(0))?;
            if legacy { return Err(rusqlite::Error::ToSqlConversionFailure("existing journal requires recovery".into())); }
            tx.execute("INSERT INTO adoption_boundaries (repo, epoch, fence, cutoff) VALUES (?1, ?2, ?3, ?4)", params![repo, boundary.epoch, boundary.fence, cutoff])?;
            tx.commit()
        })
    }

    pub fn provisional_adoption(&self, repo: &str) -> rusqlite::Result<Option<AdoptionBoundary>> {
        self.with(|c| {
            c.query_row(
                "SELECT epoch, fence, cutoff FROM adoption_boundaries WHERE repo = ?1",
                params![repo],
                |r| {
                    let cutoff: i64 = r.get(2)?;
                    let cutoff = u64::try_from(cutoff).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Integer,
                            Box::new(e),
                        )
                    })?;
                    let epoch: String = r.get(0)?;
                    let fence: String = r.get(1)?;
                    if epoch.is_empty() || fence.is_empty() {
                        return Err(rusqlite::Error::ToSqlConversionFailure(
                            "invalid stored adoption boundary".into(),
                        ));
                    }
                    Ok(AdoptionBoundary {
                        epoch,
                        fence,
                        cutoff,
                    })
                },
            )
            .optional()
        })
    }

    /// Local session rotation only; caller must separately prove remote fence and lineage.
    pub fn take_over_status_session(
        &self,
        repo: &str,
        holder: &str,
        previous: &str,
        next: &str,
        now: impl FnOnce() -> u64,
    ) -> rusqlite::Result<()> {
        if repo.is_empty()
            || holder.is_empty()
            || previous.is_empty()
            || next.is_empty()
            || next.len() > 128
            || previous == next
        {
            return Err(rusqlite::Error::ToSqlConversionFailure(
                "invalid status session".into(),
            ));
        }
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            let unknown: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![repo], |r| r.get(0))?;
            if unknown { return Err(rusqlite::Error::ToSqlConversionFailure("started build requires explicit recovery".into())); }
            let old: Option<(String, i64)> = tx.query_row("SELECT session, high_water FROM status_sequence WHERE repo = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if !old.is_some_and(|(session, high)| session == previous && high >= 0) { return Err(rusqlite::Error::ToSqlConversionFailure("unknown status lineage".into())); }
            let fence: String = tx.query_row("SELECT value FROM meta WHERE key = ?1", params![format!("fence:{repo}")], |r| r.get(0))?;
            if fence.is_empty() { return Err(rusqlite::Error::ToSqlConversionFailure("missing fence".into())); }
            let trust: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", params![format!("fence-trust:{repo}")], |r| r.get(0)).optional()?;
            if trust.as_deref() != Some("verified") { return Err(rusqlite::Error::ToSqlConversionFailure("untrusted status lineage".into())); }
            tx.execute("INSERT OR IGNORE INTO status_session_history (repo, session) VALUES (?1, ?2)", params![repo, previous])?;
            tx.execute("INSERT INTO status_session_history (repo, session) VALUES (?1, ?2)", params![repo, next])?;
            if tx.execute("UPDATE status_sequence SET holder = ?1, session = ?2, confirmed = 0, fence = ?5 WHERE repo = ?3 AND session = ?4 AND confirmed = 1", params![holder, next, repo, previous, fence])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.execute("UPDATE meta SET value = ?2 WHERE key = ?1", params![format!("fence-trust:{repo}"), format!("rotating:{next}")])?;
            tx.commit()
        })
    }

    pub fn confirm_status_session(
        &self,
        repo: &str,
        holder: &str,
        session: &str,
        fence: &str,
        now: impl FnOnce() -> u64,
    ) -> rusqlite::Result<()> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > now())) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            let actual: Option<String> = tx.query_row("SELECT value FROM meta WHERE key = ?1", params![format!("fence:{repo}")], |r| r.get(0)).optional()?;
            if fence.is_empty() || actual.as_deref() != Some(fence) || tx.execute("UPDATE status_sequence SET confirmed = 1 WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND fence = ?4 AND confirmed = 0", params![repo, holder, session, fence])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("stale status session confirmation".into())); }
            if tx.execute("UPDATE meta SET value = 'verified' WHERE key = ?1 AND value = ?2", params![format!("fence-trust:{repo}"), format!("rotating:{session}")])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("invalid status lineage confirmation".into())); }
            tx.commit()
        })
    }

    /// Persist a delivery before acknowledgement. Caller must authenticate signature and authorize repository.
    /// Full inbox refuses new records rather than dropping pending or dedup evidence.
    pub fn accept_inbox(
        &self,
        item: &InboxDelivery,
        capacity: usize,
    ) -> rusqlite::Result<InboxClaim> {
        let invalid = || rusqlite::Error::ToSqlConversionFailure("invalid inbox delivery".into());
        if item.repo.is_empty()
            || item.repo.len() > 256
            || item.epoch.is_empty()
            || item.epoch.len() > 128
            || item.guid.is_empty()
            || item.guid.len() > 128
            || item.event.is_empty()
            || item.event.len() > 128
            || item.body.len() > crate::http::MAX_BODY
            || capacity == 0
        {
            return Err(invalid());
        }
        let parsed = crate::webhook::parse(&item.event, &item.body).map_err(|_| invalid())?;
        if parsed.repo.as_deref() != Some(&item.repo) {
            return Err(invalid());
        }
        let received_at = i64::try_from(item.received_at)
            .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            // This intake has no authenticated adoption-session binding yet.
            reject_provisional_adoption(&tx, &item.repo)?;
            let old: Option<(String, String, String, Vec<u8>, i64)> = tx.query_row("SELECT repo, epoch, event, body, processed FROM inbox WHERE guid = ?1", params![item.guid], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))).optional()?;
            if let Some((repo, epoch, event, body, processed)) = old {
                if repo != item.repo || epoch != item.epoch || event != item.event || body != item.body { return Err(invalid()); }
                let claim = match processed { 0 => InboxClaim::Duplicate, 1 => InboxClaim::Processed, _ => return Err(invalid()) };
                tx.commit()?;
                return Ok(claim);
            }
            let legacy: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM deliveries WHERE guid = ?1)", params![item.guid], |r| r.get(0))?;
            if legacy { return Err(rusqlite::Error::ToSqlConversionFailure("legacy delivery has no durable payload".into())); }
            let (count, bytes): (i64, i64) = tx.query_row("SELECT COUNT(*), COALESCE(SUM(length(body) + length(CAST(repo AS BLOB)) + length(CAST(epoch AS BLOB)) + length(CAST(guid AS BLOB)) + length(CAST(event AS BLOB)) + 8), 0) FROM inbox", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if u64::try_from(count).unwrap_or(u64::MAX) >= capacity as u64 || u64::try_from(bytes).unwrap_or(u64::MAX).saturating_add(item.body.len() as u64).saturating_add(item.repo.len() as u64).saturating_add(item.epoch.len() as u64).saturating_add(item.guid.len() as u64).saturating_add(item.event.len() as u64).saturating_add(8) > 32 * 1024 * 1024 { return Err(rusqlite::Error::ToSqlConversionFailure("inbox capacity exceeded".into())); }
            tx.execute("INSERT INTO inbox (guid, repo, epoch, event, body, received_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)", params![item.guid, item.repo, item.epoch, item.event, item.body, received_at])?;
            tx.commit()?;
            Ok(InboxClaim::Inserted)
        })
    }

    /// Durable processed-state revision, not an authorization proof.
    pub fn inbox_revision(&self, repo: &str) -> rusqlite::Result<u64> {
        self.with(|c| {
            let rev: Option<i64> = c
                .query_row(
                    "SELECT revision FROM inbox_revisions WHERE repo = ?1",
                    params![repo],
                    |r| r.get(0),
                )
                .optional()?;
            u64::try_from(rev.unwrap_or(0)).map_err(|e| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(e),
                )
            })
        })
    }

    /// Whether any epoch has unprocessed input for this repository.
    pub fn has_pending_inbox(&self, repo: &str) -> rusqlite::Result<bool> {
        self.with(|c| {
            let (invalid, pending): (bool, bool) = c.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed NOT IN (0, 1))), EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND processed = 0)", params![repo], |r| Ok((r.get(0)?, r.get(1)?)))?;
            if invalid { return Err(rusqlite::Error::ToSqlConversionFailure("invalid inbox status".into())); }
            Ok(pending)
        })
    }

    /// Pending items in arrival order. This is not permission to replay a command.
    /// The original intake epoch remains evidence, not the successor's authority.
    pub fn pending_inbox_all_epochs(&self, repo: &str) -> rusqlite::Result<Vec<InboxDelivery>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT epoch, guid, event, body, received_at, processed FROM inbox WHERE repo = ?1 ORDER BY rowid")?;
            let rows = st.query_map(params![repo], |r| {
                let at: i64 = r.get(4)?;
                let received_at = u64::try_from(at).map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Integer, Box::new(e)))?;
                let processed: i64 = r.get(5)?;
                if !(0..=1).contains(&processed) { return Err(rusqlite::Error::ToSqlConversionFailure("invalid inbox status".into())); }
                Ok((InboxDelivery { repo: repo.into(), epoch: r.get(0)?, guid: r.get(1)?, event: r.get(2)?, body: r.get(3)?, received_at }, processed))
            })?;
            let mut pending = Vec::new();
            for row in rows {
                let (item, processed) = row?;
                if item.epoch.is_empty() || item.guid.is_empty() || item.event.is_empty() || item.body.len() > crate::http::MAX_BODY || crate::webhook::parse(&item.event, &item.body).ok().and_then(|p| p.repo).as_deref() != Some(repo) {
                    return Err(rusqlite::Error::ToSqlConversionFailure("invalid stored inbox delivery".into()));
                }
                if processed == 0 { pending.push(item); }
            }
            Ok(pending)
        })
    }

    /// Pending items in one intake epoch. This is not permission to replay a command.
    pub fn pending_inbox(&self, repo: &str, epoch: &str) -> rusqlite::Result<Vec<InboxDelivery>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT guid, event, body, received_at FROM inbox WHERE repo = ?1 AND epoch = ?2 AND processed = 0 ORDER BY rowid")?;
            let rows = st.query_map(params![repo, epoch], |r| {
                let at: i64 = r.get(3)?;
                let received_at = u64::try_from(at).map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Integer, Box::new(e)))?;
                Ok(InboxDelivery { repo: repo.into(), epoch: epoch.into(), guid: r.get(0)?, event: r.get(1)?, body: r.get(2)?, received_at })
            })?;
            rows.collect()
        })
    }

    // ---- event log ---------------------------------------------------------------------

    /// Appends an event; returns its id.
    pub fn add_event(&self, e: &EventRecord) -> rusqlite::Result<u64> {
        let pulls = format!(
            ",{},",
            e.pull_requests
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
        let meta = e.metadata.to_string();
        self.with(|c| {
            c.execute(
                "INSERT INTO events (repo, at, type, pull_request, pulls, base_ref, trigger, metadata)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    e.repository,
                    e.at as i64,
                    e.kind,
                    e.pull_request.map(|n| n as i64),
                    pulls,
                    e.base_ref,
                    e.trigger,
                    meta
                ],
            )?;
            Ok(c.last_insert_rowid() as u64)
        })
    }

    /// Events of a repository, newest first: those of pull request `pr` (as its own or as one
    /// of a batch), at or after `since` (Unix seconds), with an id below `before` (paging), at
    /// most `limit`.
    pub fn events(&self, repo: &str, q: &EventQuery) -> rusqlite::Result<Vec<EventRecord>> {
        let like = q.pr.map(|n| format!("%,{n},%"));
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT id, repo, at, type, pull_request, pulls, base_ref, trigger, metadata
                 FROM events
                 WHERE repo = ?1 AND (?2 IS NULL OR pulls LIKE ?2) AND (?3 IS NULL OR at >= ?3)
                   AND (?4 IS NULL OR id < ?4) AND (?5 IS NULL OR type = ?5)
                 ORDER BY id DESC LIMIT ?6",
            )?;
            let rows = st.query_map(
                params![
                    repo,
                    like,
                    q.since.map(|t| t as i64),
                    q.before.map(|t| t as i64),
                    q.kind,
                    q.limit.clamp(1, 10_000) as i64
                ],
                |r| {
                    let pulls: String = r.get(5)?;
                    let meta: String = r.get(8)?;
                    Ok(EventRecord {
                        id: r.get::<_, i64>(0)? as u64,
                        repository: r.get(1)?,
                        at: r.get::<_, i64>(2)? as u64,
                        kind: r.get(3)?,
                        pull_request: r.get::<_, Option<i64>>(4)?.map(|n| n as u64),
                        pull_requests: pulls.split(',').filter_map(|x| x.parse().ok()).collect(),
                        base_ref: r.get(6)?,
                        trigger: r.get(7)?,
                        metadata: serde_json::from_str(&meta).unwrap_or_default(),
                    })
                },
            )?;
            rows.collect()
        })
    }

    // ---- shadow mode -------------------------------------------------------------------

    /// Records an observation of one side (`schlange` or `mergify`) about a pull request.
    /// `first`: only the first of its kind counts (an existing row stays); otherwise the last
    /// one replaces it. Returns whether it was written.
    pub fn shadow_record(&self, o: &ShadowFact, first: bool) -> rusqlite::Result<bool> {
        let data = o.data.to_string();
        self.with(|c| {
            let sql = if first {
                "INSERT OR IGNORE INTO shadow (repo, side, pr, kind, at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            } else {
                "INSERT OR REPLACE INTO shadow (repo, side, pr, kind, at, data) VALUES (?1, ?2, ?3, ?4, ?5, ?6)"
            };
            let n = c.execute(
                sql,
                params![o.repo, o.side, o.pr as i64, o.kind, o.at as i64, data],
            )?;
            Ok(n > 0)
        })
    }

    /// Whether a fact of this kind exists.
    pub fn shadow_has(
        &self,
        repo: &str,
        side: &str,
        pr: u64,
        kind: &str,
    ) -> rusqlite::Result<bool> {
        self.with(|c| {
            c.query_row(
                "SELECT 1 FROM shadow WHERE repo = ?1 AND side = ?2 AND pr = ?3 AND kind = ?4",
                params![repo, side, pr as i64, kind],
                |_| Ok(()),
            )
            .optional()
            .map(|r| r.is_some())
        })
    }

    /// Every shadow fact of a repository, in the order they were first written.
    pub fn shadow_facts(&self, repo: &str) -> rusqlite::Result<Vec<ShadowFact>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT repo, side, pr, kind, at, data, id FROM shadow WHERE repo = ?1 ORDER BY id",
            )?;
            let rows = st.query_map(params![repo], |r| {
                let data: String = r.get(5)?;
                Ok(ShadowFact {
                    repo: r.get(0)?,
                    side: r.get(1)?,
                    pr: r.get::<_, i64>(2)? as u64,
                    kind: r.get(3)?,
                    at: r.get::<_, i64>(4)? as u64,
                    data: serde_json::from_str(&data).unwrap_or_default(),
                    seq: r.get::<_, i64>(6)? as u64,
                })
            })?;
            rows.collect()
        })
    }

    // ---- lease (MQ-32) -----------------------------------------------------------------

    /// Takes or renews the lease of `repo` for `holder` until `expires` when it is free, held
    /// by `holder`, or expired at `now`. Returns whether `holder` holds it now.
    pub fn lease_acquire(
        &self,
        repo: &str,
        holder: &str,
        now: u64,
        expires: u64,
    ) -> rusqlite::Result<bool> {
        self.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let current: Option<(String, i64)> = tx
                .query_row(
                    "SELECT holder, expires FROM lease WHERE repo = ?1",
                    params![repo],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            let free = match &current {
                None => true,
                Some((h, e)) => h == holder || (*e as u64) <= now,
            };
            if free {
                tx.execute(
                    "INSERT OR REPLACE INTO lease (repo, holder, expires) VALUES (?1, ?2, ?3)",
                    params![repo, holder, expires as i64],
                )?;
            }
            tx.commit()?;
            Ok(free)
        })
    }

    /// The holder of `repo`'s lease and when it expires.
    pub fn lease_holder(&self, repo: &str) -> rusqlite::Result<Option<(String, u64)>> {
        self.with(|c| {
            c.query_row(
                "SELECT holder, expires FROM lease WHERE repo = ?1",
                params![repo],
                |r| Ok((r.get(0)?, r.get::<_, i64>(1)? as u64)),
            )
            .optional()
        })
    }

    /// Gives the lease up (a clean shutdown), if `holder` has it.
    pub fn lease_release(&self, repo: &str, holder: &str) -> rusqlite::Result<()> {
        self.with(|c| {
            c.execute(
                "DELETE FROM lease WHERE repo = ?1 AND holder = ?2",
                params![repo, holder],
            )
            .map(|_| ())
        })
    }

    // ---- checks_timeout: auto history --------------------------------------------------

    pub fn history(&self, repo: &str) -> rusqlite::Result<BTreeMap<String, Vec<Sample>>> {
        self.with(|c| {
            let mut st =
                c.prepare("SELECT key, at, secs FROM history WHERE repo = ?1 ORDER BY rowid")?;
            let rows = st.query_map(params![repo], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as Time,
                    r.get::<_, i64>(2)? as Time,
                ))
            })?;
            let mut out: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
            for row in rows {
                let (k, at, secs) = row?;
                out.entry(k).or_default().push(Sample { at, secs });
            }
            Ok(out)
        })
    }
}

/// One entry of the event log (`schlange events`, `GET /repos/{owner}/{repo}/logs`). The
/// shape follows the public event log API (`id`, `received_at`, `type`, `pull_request`,
/// `base_ref`, `trigger`, `metadata`); the types and metadata are Schlange's own.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct EventRecord {
    pub id: u64,
    pub repository: String,
    /// Unix seconds.
    pub at: u64,
    /// `action.queue.enter`, `action.queue.leave`, `action.queue.checks_start`, …
    #[serde(rename = "type")]
    pub kind: String,
    pub pull_request: Option<u64>,
    /// Every pull request the event belongs to (a batch has several).
    pub pull_requests: Vec<u64>,
    pub base_ref: Option<String>,
    /// What caused it (`Schlange engine`, `admin API`, …).
    pub trigger: String,
    pub metadata: serde_json::Value,
}

/// A query of the event log.
#[derive(Clone, Debug, Default)]
pub struct EventQuery {
    pub pr: Option<u64>,
    /// Unix seconds.
    pub since: Option<u64>,
    /// Only ids below this one (the next page).
    pub before: Option<u64>,
    pub kind: Option<String>,
    pub limit: u64,
}

/// What one side did (or would have done) with a pull request, in shadow mode.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ShadowFact {
    pub repo: String,
    /// `schlange` (what it would have done) or `mergify` (what was observed on GitHub).
    pub side: String,
    pub pr: u64,
    /// `queued`, `batch`, `landed`, `dequeued`, `merged_elsewhere`.
    pub kind: String,
    /// Unix seconds.
    pub at: u64,
    pub data: serde_json::Value,
    /// Order of writing (landing order).
    #[serde(default)]
    pub seq: u64,
}

/// The journal refuses to write once another process holds the repository's lease.
pub const LEASE_LOST: &str = "lease lost";

/// Experimental stored evidence for a dispatch. Not sufficient on its own to authorize replay.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PreparedDispatch {
    pub session: String,
    pub id: u64,
    pub decision: Decision,
    pub head: String,
    pub base: String,
    pub config_digest: String,
}

impl PreparedDispatch {
    fn valid(&self) -> bool {
        !self.session.is_empty()
            && !self.head.is_empty()
            && !self.base.is_empty()
            && !self.config_digest.is_empty()
            && matches!(
                self.decision,
                Decision::Act {
                    op: schlange_core::actions::Op::Dispatch { .. },
                    ..
                }
            )
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DispatchStatus {
    Prepared,
    MayHaveSent,
}

/// The [`Journal`] of one repository in the database.
pub struct SqliteJournal {
    store: Store,
    repo: String,
    /// The history as last written (rewritten only when it changes).
    history_written: Option<String>,
    /// With a lease (MQ-32): every write checks in the same transaction that this holder
    /// still holds the repository's lease, so a process that lost it cannot overwrite the
    /// journal of the one that took over (same data directory).
    holder: Option<String>,
}

/// Fails the transaction when `holder` does not hold `repo`'s lease.
fn check_holder(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
    holder: Option<&str>,
) -> rusqlite::Result<()> {
    let Some(h) = holder else { return Ok(()) };
    let current: Option<String> = tx
        .query_row(
            "SELECT holder FROM lease WHERE repo = ?1",
            params![repo],
            |r| r.get(0),
        )
        .optional()?;
    if current.as_deref() == Some(h) {
        Ok(())
    } else {
        Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into()))
    }
}

fn advance_checkpoint_revision(tx: &rusqlite::Transaction<'_>, repo: &str) -> rusqlite::Result<()> {
    let current: Option<i64> = tx
        .query_row(
            "SELECT revision FROM inbox_revisions WHERE repo = ?1",
            params![repo],
            |r| r.get(0),
        )
        .optional()?;
    let current = current.unwrap_or(0);
    if current < 0 {
        return Err(rusqlite::Error::ToSqlConversionFailure(
            "invalid checkpoint revision".into(),
        ));
    }
    let next = current.checked_add(1).ok_or_else(|| {
        rusqlite::Error::ToSqlConversionFailure("checkpoint revision exhausted".into())
    })?;
    tx.execute(
        "INSERT OR REPLACE INTO inbox_revisions (repo, revision) VALUES (?1, ?2)",
        params![repo, next],
    )?;
    Ok(())
}

fn reject_legacy_journal_with_inbox(
    tx: &rusqlite::Transaction<'_>,
    repo: &str,
) -> rusqlite::Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 UNION ALL SELECT 1 FROM status_sequence WHERE repo = ?1)",
        params![repo],
        |r| r.get(0),
    )?;
    if exists {
        Err(rusqlite::Error::ToSqlConversionFailure(
            "durable inbox requires revision-aware journal".into(),
        ))
    } else {
        Ok(())
    }
}

fn reject_provisional_adoption(tx: &rusqlite::Transaction<'_>, repo: &str) -> rusqlite::Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM adoption_boundaries WHERE repo = ?1)",
        params![repo],
        |r| r.get(0),
    )?;
    if exists {
        Err(rusqlite::Error::ToSqlConversionFailure(
            "provisional adoption requires epoch-aware journal".into(),
        ))
    } else {
        Ok(())
    }
}

fn reject_pending_dispatches(tx: &rusqlite::Transaction<'_>, repo: &str) -> rusqlite::Result<()> {
    let pending: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) OR EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)",
        params![repo],
        |r| r.get(0),
    )?;
    if pending {
        Err(rusqlite::Error::ToSqlConversionFailure(
            "unresolved dispatches require explicit recovery".into(),
        ))
    } else {
        Ok(())
    }
}

impl SqliteJournal {
    /// Check local durable ticket evidence; caller must also check live ingress and remote lease.
    #[allow(dead_code)]
    pub(crate) fn build_ticket_current(
        &self,
        ticket: &BuildWriteTicket,
        ingress: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<(), JournalError> {
        if ticket.repo != self.repo
            || self.holder.as_deref() != Some(ticket.holder.as_str())
            || ticket.ingress != ingress
        {
            return Err(JournalError::Storage("build ticket context changed".into()));
        }
        let revision = i64::try_from(ticket.revision).map_err(storage)?;
        let seq = i64::try_from(ticket.seq).map_err(storage)?;
        let ingress = i64::try_from(ticket.ingress).map_err(storage)?;
        let decision = serde_json::to_string(&ticket.request)?;
        self.store.with(|c| {
            let tx = c.transaction()?;
            let now = now();
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if !owner.is_some_and(|(h, expiry)| h == ticket.holder && u64::try_from(expiry).is_ok_and(|e| e > now)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND revision = ?4 AND seq = ?5 AND ingress = ?6 AND fence = ?7 AND decision = ?8) AND EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1 AND fence = ?7 AND high_water >= ?5) AND EXISTS(SELECT 1 FROM inbox_revisions WHERE repo = ?1 AND revision = ?4) AND EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND seq = ?5 AND decision = ?8) AND NOT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed != 1))", params![self.repo, ticket.holder, ticket.session, revision, seq, ingress, ticket.fence, decision], |r| r.get(0))?;
            if !valid { return Err(rusqlite::Error::ToSqlConversionFailure("build ticket no longer current".into())); }
            tx.commit()?;
            Ok(())
        }).map_err(storage)
    }
    /// Claim the oldest build once. The caller must fence remote lease and ingress before use;
    /// an existing row is unknown work and cannot be replayed by this method.
    #[allow(dead_code, clippy::too_many_arguments)]
    pub(crate) fn claim_session_build(
        &self,
        session: &str,
        revision: u64,
        seq: u64,
        request: &Decision,
        ingress: u64,
        fence: &str,
        now: impl FnOnce() -> u64,
    ) -> Result<BuildWriteTicket, JournalError> {
        let Decision::BuildCar {
            car,
            base_branch,
            branch,
            base_sha,
            prs,
            own,
            stack_on,
            ..
        } = request
        else {
            return Err(JournalError::Storage("not a build request".into()));
        };
        if fence.is_empty() || session.is_empty() {
            return Err(JournalError::Storage("missing build fence".into()));
        }
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let rev = i64::try_from(revision).map_err(storage)?;
        let seq_i = i64::try_from(seq).map_err(storage)?;
        let ingress_i = i64::try_from(ingress).map_err(storage)?;
        let encoded = serde_json::to_string(request)?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            reject_provisional_adoption(&tx, &self.repo)?;
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1 AND fence = ?4 AND high_water >= ?5) AND EXISTS(SELECT 1 FROM inbox_revisions WHERE repo = ?1 AND revision = ?6) AND EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND seq = ?5 AND decision = ?7) AND NOT EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND (typeof(seq) != 'integer' OR seq < 0 OR seq < ?5)) AND NOT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed != 1)) AND NOT EXISTS(SELECT 1 FROM inbox_redecide WHERE repo = ?1) AND NOT EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) AND NOT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo, holder, session, fence, seq_i, rev, encoded], |r| r.get(0))?;
            if !valid { return Err(rusqlite::Error::ToSqlConversionFailure("build predecessor unavailable".into())); }
            let raw: String = tx.query_row("SELECT state FROM checkpoints WHERE repo = ?1", params![self.repo], |r| r.get(0))?;
            let old: State = serde_json::from_str(&raw).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
            let cars: Vec<_> = old.trains.values().flat_map(|t| t.cars.iter().map(move |c| (t, c))).filter(|(_, c)| c.id == *car).collect();
            if old.identity_bootstrap_hold || cars.len() != 1 || cars[0].0.base != *base_branch || cars[0].1.branch.as_deref() != Some(branch) || cars[0].1.base_sha != *base_sha || cars[0].1.prs != *prs || cars[0].1.own != *own || cars[0].1.stacked_on != *stack_on || !matches!(cars[0].1.status, schlange_core::queue::CarStatus::Building { .. }) { return Err(rusqlite::Error::ToSqlConversionFailure("build no longer matches current car".into())); }
            tx.execute("INSERT INTO session_build_claims (repo, holder, session, revision, seq, ingress, fence, decision) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)", params![self.repo, holder, session, rev, seq_i, ingress_i, fence, encoded])?;
            tx.commit()?;
            Ok(BuildWriteTicket { repo: self.repo.clone(), holder: holder.into(), session: session.into(), revision, seq, ingress, fence: fence.into(), request: request.clone() })
        }).map_err(storage)
    }
    /// Capture local evidence before an HTTP read. Caller must fence ingress and remote lease.
    pub fn capture_session_refresh(
        &self,
        session: &str,
        revision: u64,
        seq: u64,
        request: &Decision,
        ingress: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<RefreshReadTicket, JournalError> {
        if !matches!(request, Decision::Refresh { .. }) {
            return Err(JournalError::Storage("not a refresh request".into()));
        }
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let revision_i = i64::try_from(revision).map_err(storage)?;
        let seq_i = i64::try_from(seq).map_err(storage)?;
        let expected = serde_json::to_string(request)?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            reject_provisional_adoption(&tx, &self.repo)?;
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1 AND high_water >= ?4) AND EXISTS(SELECT 1 FROM inbox_revisions WHERE repo = ?1 AND revision = ?5) AND EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND seq = ?4 AND decision = ?6) AND NOT EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND (typeof(seq) != 'integer' OR seq < 0 OR seq < ?4)) AND NOT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed != 1)) AND NOT EXISTS(SELECT 1 FROM inbox_redecide WHERE repo = ?1) AND NOT EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) AND NOT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo, holder, session, seq_i, revision_i, expected], |r| r.get(0))?;
            if !valid { return Err(rusqlite::Error::ToSqlConversionFailure("refresh read predecessor unavailable".into())); }
            tx.commit()?; Ok(RefreshReadTicket { repo: self.repo.clone(), session: session.into(), revision, seq, request: request.clone(), ingress })
        }).map_err(storage)
    }

    /// Commit only against the pre-read ticket and caller's current ingress fence.
    pub fn commit_captured_refresh(
        &mut self,
        ticket: &RefreshReadTicket,
        current_ingress: u64,
        rules: &schlange_config::rules::Rules,
        reply: &schlange_core::Event,
        at: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<SessionReply, JournalError> {
        if ticket.repo != self.repo || ticket.ingress != current_ingress {
            return Err(JournalError::Storage("refresh ingress changed".into()));
        }
        self.commit_session_refresh_reply(
            &ticket.session,
            ticket.revision,
            ticket.seq,
            &ticket.request,
            rules,
            reply,
            at,
            now,
        )
    }

    /// Apply a complete Refresh reply and atomically replace its intent with successor intents.
    /// The caller must separately prove read freshness and the remote/ingress fences.
    fn commit_session_refresh_reply(
        &mut self,
        session: &str,
        revision: u64,
        seq: u64,
        request: &Decision,
        rules: &schlange_config::rules::Rules,
        reply: &schlange_core::Event,
        at: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<SessionReply, JournalError> {
        use std::collections::BTreeSet;
        let Decision::Refresh {
            token,
            numbers,
            branches,
        } = request
        else {
            return Err(JournalError::Storage("not a refresh request".into()));
        };
        let schlange_core::Event::Snapshot {
            pull_requests,
            branches: returned,
            token: Some(reply_token),
        } = reply
        else {
            return Err(JournalError::Storage("not a refresh snapshot".into()));
        };
        let expected_numbers: BTreeSet<_> = numbers.iter().copied().collect();
        let received_numbers: BTreeSet<_> = pull_requests.iter().map(|p| p.number).collect();
        let expected_branches: BTreeSet<_> = branches.iter().cloned().collect();
        let received_branches: BTreeSet<_> = returned.keys().cloned().collect();
        if token != reply_token
            || expected_numbers.len() != numbers.len()
            || received_numbers.len() != pull_requests.len()
            || expected_numbers != received_numbers
            || expected_branches.len() != branches.len()
            || expected_branches != received_branches
        {
            return Err(JournalError::Storage("incomplete refresh reply".into()));
        }
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let revision = i64::try_from(revision).map_err(storage)?;
        let seq = i64::try_from(seq).map_err(storage)?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| JournalError::Storage("inbox revision exhausted".into()))?;
        let expected = serde_json::to_string(request)?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            reject_provisional_adoption(&tx, &self.repo)?;
            let blocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed != 1)) OR EXISTS(SELECT 1 FROM inbox_redecide WHERE repo = ?1) OR EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) OR EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo], |r| r.get(0))?;
            if blocked { return Err(rusqlite::Error::ToSqlConversionFailure("unresolved work requires recovery".into())); }
            let actual: Option<i64> = tx.query_row("SELECT revision FROM inbox_revisions WHERE repo = ?1", params![self.repo], |r| r.get(0)).optional()?;
            if actual != Some(revision) { return Err(rusqlite::Error::ToSqlConversionFailure("stale reply revision".into())); }
            let high: i64 = tx.query_row("SELECT high_water FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1", params![self.repo, holder, session], |r| r.get(0))?;
            if high < seq || seq < 0 { return Err(rusqlite::Error::ToSqlConversionFailure("invalid reply sequence".into())); }
            let invalid_seq: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND (typeof(seq) != 'integer' OR seq < 0))", params![self.repo], |r| r.get(0))?;
            if invalid_seq { return Err(rusqlite::Error::ToSqlConversionFailure("invalid intent sequence".into())); }
            let oldest: Option<i64> = tx.query_row("SELECT MIN(seq) FROM intents WHERE repo = ?1", params![self.repo], |r| r.get(0))?;
            if oldest != Some(seq) { return Err(rusqlite::Error::ToSqlConversionFailure("earlier intent requires resolution".into())); }
            let stored: String = tx.query_row("SELECT decision FROM intents WHERE repo = ?1 AND seq = ?2", params![self.repo, seq], |r| r.get(0))?;
            if stored != expected { return Err(rusqlite::Error::ToSqlConversionFailure("reply request mismatch".into())); }
            let raw: String = tx.query_row("SELECT state FROM checkpoints WHERE repo = ?1", params![self.repo], |r| r.get(0))?;
            let old: State = serde_json::from_str(&raw).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
            if old.identity_bootstrap_hold { return Err(rusqlite::Error::ToSqlConversionFailure("identity recovery held".into())); }
            let (mut state, decisions) = schlange_core::step(&old, rules, reply, at);
            if decisions.iter().any(|d| matches!(d, Decision::Act { op: schlange_core::actions::Op::Dispatch { .. }, .. })) { return Err(rusqlite::Error::ToSqlConversionFailure("unsupported reply decision".into())); }
            for d in &decisions { if let Decision::WakeAt { at } = d { state.wakeups.insert(*at); } }
            let mut assigned = Vec::new(); let mut next = high;
            if tx.execute("DELETE FROM intents WHERE repo = ?1 AND seq = ?2 AND decision = ?3", params![self.repo, seq, expected])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            for d in decisions.iter().filter(|d| schlange_core::journal::is_side_effect(d)) {
                next = next.checked_add(1).ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("intent sequence exhausted".into()))?;
                let encoded = serde_json::to_string(d).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                tx.execute("INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)", params![self.repo, next, encoded])?; assigned.push((next as u64, d.clone()));
            }
            let encoded = serde_json::to_string(&state).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if tx.execute("UPDATE checkpoints SET state = ?1, written = ?2 WHERE repo = ?3", params![encoded, crate::time::unix_now() as i64, self.repo])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.execute("DELETE FROM history WHERE repo = ?1", params![self.repo])?;
            for (key, samples) in &state.history { for sample in samples { tx.execute("INSERT INTO history (repo, key, at, secs) VALUES (?1, ?2, ?3, ?4)", params![self.repo, key, sample.at as i64, sample.secs as i64])?; }}
            if tx.execute("UPDATE status_sequence SET high_water = ?1 WHERE repo = ?2 AND holder = ?3 AND session = ?4 AND high_water = ?5 AND confirmed = 1", params![next, self.repo, holder, session, high])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            if tx.execute("UPDATE inbox_revisions SET revision = ?1 WHERE repo = ?2 AND revision = ?3", params![next_revision, self.repo, revision])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.commit()?; Ok(SessionReply { state, revision: next_revision as u64, decisions, intents: assigned })
        }).map_err(storage)
    }

    /// Persist structurally checked BuildCar evidence with its exact pending intent.
    /// The caller must prove HTTP evidence and the live remote/ingress fences separately.
    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn commit_session_build_reply(
        &mut self,
        session: &str,
        revision: u64,
        seq: u64,
        request: &Decision,
        rules: &schlange_config::rules::Rules,
        reply: &schlange_core::Event,
        prefix: &[(u64, String)],
        at: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<SessionReply, JournalError> {
        self.commit_build_reply_inner(
            session, revision, seq, request, rules, reply, prefix, at, None, now,
        )
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    fn commit_claimed_build_for_test(
        &mut self,
        ticket: &BuildWriteTicket,
        ingress: u64,
        rules: &schlange_config::rules::Rules,
        reply: &schlange_core::Event,
        prefix: &[(u64, String)],
        at: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<SessionReply, JournalError> {
        if ticket.ingress != ingress
            || ticket.repo != self.repo
            || self.holder.as_deref() != Some(&ticket.holder)
        {
            return Err(JournalError::Storage("build ticket context changed".into()));
        }
        self.commit_build_reply_inner(
            &ticket.session,
            ticket.revision,
            ticket.seq,
            &ticket.request,
            rules,
            reply,
            prefix,
            at,
            Some(ticket),
            now,
        )
    }

    #[allow(dead_code, clippy::too_many_arguments)]
    pub(crate) fn commit_verified_build(
        &mut self,
        verified: &crate::service::VerifiedBuild,
        ingress: u64,
        rules: &schlange_config::rules::Rules,
        at: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<SessionReply, JournalError> {
        let ticket = verified.ticket();
        if ticket.ingress != ingress
            || ticket.repo != self.repo
            || self.holder.as_deref() != Some(&ticket.holder)
        {
            return Err(JournalError::Storage("build ticket context changed".into()));
        }
        self.commit_build_reply_inner(
            &ticket.session,
            ticket.revision,
            ticket.seq,
            &ticket.request,
            rules,
            verified.event(),
            verified.prefix(),
            at,
            Some(ticket),
            now,
        )
    }

    #[allow(dead_code, clippy::too_many_arguments)]
    fn commit_build_reply_inner(
        &mut self,
        session: &str,
        revision: u64,
        seq: u64,
        request: &Decision,
        rules: &schlange_config::rules::Rules,
        reply: &schlange_core::Event,
        prefix: &[(u64, String)],
        at: u64,
        claim: Option<&BuildWriteTicket>,
        now: impl FnOnce() -> u64,
    ) -> Result<SessionReply, JournalError> {
        let Decision::BuildCar {
            car,
            branch,
            prs,
            own,
            stack_on,
            ..
        } = request
        else {
            return Err(JournalError::Storage("not a build request".into()));
        };
        let schlange_core::Event::CarBuilt {
            car: reply_car,
            branch: reply_branch,
            head_sha,
            tree,
            draft,
        } = reply
        else {
            return Err(JournalError::Storage("not a build reply".into()));
        };
        let expected: Vec<_> = prs
            .iter()
            .filter(|p| stack_on.is_none() || own.contains(&p.number))
            .map(|p| p.number)
            .collect();
        let actual: Vec<_> = prefix.iter().map(|(n, _)| *n).collect();
        let distinct: std::collections::BTreeSet<_> = expected.iter().copied().collect();
        let all: std::collections::BTreeSet<_> = prs.iter().map(|p| p.number).collect();
        let own_set: std::collections::BTreeSet<_> = own.iter().copied().collect();
        if car != reply_car
            || branch != reply_branch
            || head_sha.is_empty()
            || tree.is_empty()
            || *draft == 0
            || expected.is_empty()
            || all.len() != prs.len()
            || own_set.len() != own.len()
            || own_set.is_empty()
            || !own_set.is_subset(&all)
            || distinct.len() != expected.len()
            || expected != actual
            || prefix.iter().any(|(_, t)| t.is_empty())
            || prefix.last().is_none_or(|(_, t)| t != tree)
        {
            return Err(JournalError::Storage("invalid build evidence".into()));
        }
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let revision = i64::try_from(revision).map_err(storage)?;
        let seq = i64::try_from(seq).map_err(storage)?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| JournalError::Storage("inbox revision exhausted".into()))?;
        let expected = serde_json::to_string(request)?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            reject_provisional_adoption(&tx, &self.repo)?;
            let blocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed != 1)) OR EXISTS(SELECT 1 FROM inbox_redecide WHERE repo = ?1) OR EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1)", params![self.repo], |r| r.get(0))?;
            if blocked { return Err(rusqlite::Error::ToSqlConversionFailure("unresolved work requires recovery".into())); }
            if let Some(ticket) = claim {
                let ingress = i64::try_from(ticket.ingress).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                if tx.execute("DELETE FROM session_build_claims WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND revision = ?4 AND seq = ?5 AND ingress = ?6 AND fence = ?7 AND decision = ?8 AND EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND fence = ?7 AND confirmed = 1)", params![self.repo, holder, session, revision, seq, ingress, ticket.fence, expected])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("build claim changed".into())); }
            } else {
                let unknown: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo], |r| r.get(0))?;
                if unknown { return Err(rusqlite::Error::ToSqlConversionFailure("started build requires verified completion".into())); }
            }
            let actual: Option<i64> = tx.query_row("SELECT revision FROM inbox_revisions WHERE repo = ?1", params![self.repo], |r| r.get(0)).optional()?;
            if actual != Some(revision) { return Err(rusqlite::Error::ToSqlConversionFailure("stale reply revision".into())); }
            let high: i64 = tx.query_row("SELECT high_water FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1", params![self.repo, holder, session], |r| r.get(0))?;
            if high < seq || seq < 0 { return Err(rusqlite::Error::ToSqlConversionFailure("invalid reply sequence".into())); }
            let invalid_seq: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND (typeof(seq) != 'integer' OR seq < 0))", params![self.repo], |r| r.get(0))?;
            if invalid_seq { return Err(rusqlite::Error::ToSqlConversionFailure("invalid intent sequence".into())); }
            let oldest: Option<i64> = tx.query_row("SELECT MIN(seq) FROM intents WHERE repo = ?1", params![self.repo], |r| r.get(0))?;
            if oldest != Some(seq) { return Err(rusqlite::Error::ToSqlConversionFailure("earlier intent requires resolution".into())); }
            let stored: String = tx.query_row("SELECT decision FROM intents WHERE repo = ?1 AND seq = ?2", params![self.repo, seq], |r| r.get(0))?;
            if stored != expected { return Err(rusqlite::Error::ToSqlConversionFailure("reply request mismatch".into())); }
            let raw: String = tx.query_row("SELECT state FROM checkpoints WHERE repo = ?1", params![self.repo], |r| r.get(0))?;
            let old: State = serde_json::from_str(&raw).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
            let Decision::BuildCar { car, branch, base_branch, base_sha, prs, own, stack_on, .. } = request else { unreachable!() };
            let matches: Vec<_> = old.trains.values().flat_map(|t| t.cars.iter().map(move |c| (t, c))).filter(|(_, c)| c.id == *car).collect();
            if matches.len() != 1 || matches[0].0.base != *base_branch || matches[0].1.branch.as_deref() != Some(branch) || matches[0].1.base_sha != *base_sha || matches[0].1.prs != *prs || matches[0].1.own != *own || matches[0].1.stacked_on != *stack_on || !matches!(matches[0].1.status, schlange_core::queue::CarStatus::Building { .. }) { return Err(rusqlite::Error::ToSqlConversionFailure("build no longer matches current car".into())); }
            if old.identity_bootstrap_hold { return Err(rusqlite::Error::ToSqlConversionFailure("identity recovery held".into())); }
            let (mut state, decisions) = schlange_core::step(&old, rules, reply, at);
            if decisions.iter().any(|d| matches!(d, Decision::Act { op: schlange_core::actions::Op::Dispatch { .. }, .. })) { return Err(rusqlite::Error::ToSqlConversionFailure("unsupported reply decision".into())); }
            for d in &decisions { if let Decision::WakeAt { at } = d { state.wakeups.insert(*at); } }
            let mut assigned = Vec::new(); let mut next = high;
            let car_i = i64::try_from(*car).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let encoded_prefix = serde_json::to_string(prefix).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tx.execute("INSERT INTO cars (repo, car, prefix) VALUES (?1, ?2, ?3)", params![self.repo, car_i, encoded_prefix])?;
            if tx.execute("DELETE FROM intents WHERE repo = ?1 AND seq = ?2 AND decision = ?3", params![self.repo, seq, expected])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            for d in decisions.iter().filter(|d| schlange_core::journal::is_side_effect(d)) {
                next = next.checked_add(1).ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("intent sequence exhausted".into()))?;
                let encoded = serde_json::to_string(d).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                tx.execute("INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)", params![self.repo, next, encoded])?; assigned.push((next as u64, d.clone()));
            }
            let encoded = serde_json::to_string(&state).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if tx.execute("UPDATE checkpoints SET state = ?1, written = ?2 WHERE repo = ?3", params![encoded, crate::time::unix_now() as i64, self.repo])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.execute("DELETE FROM history WHERE repo = ?1", params![self.repo])?;
            for (key, samples) in &state.history { for sample in samples { tx.execute("INSERT INTO history (repo, key, at, secs) VALUES (?1, ?2, ?3, ?4)", params![self.repo, key, sample.at as i64, sample.secs as i64])?; }}
            if tx.execute("UPDATE status_sequence SET high_water = ?1 WHERE repo = ?2 AND holder = ?3 AND session = ?4 AND high_water = ?5 AND confirmed = 1", params![next, self.repo, holder, session, high])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            if tx.execute("UPDATE inbox_revisions SET revision = ?1 WHERE repo = ?2 AND revision = ?3", params![next_revision, self.repo, revision])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.commit()?; Ok(SessionReply { state, revision: next_revision as u64, decisions, intents: assigned })
        }).map_err(storage)
    }

    /// Persist a safety hold against the exact durable predecessor; does not consume inbox work.
    pub fn commit_session_hold(
        &mut self,
        session: &str,
        revision: u64,
        now: impl FnOnce() -> u64,
    ) -> Result<State, JournalError> {
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let revision = i64::try_from(revision).map_err(storage)?;
        let next = revision
            .checked_add(1)
            .ok_or_else(|| JournalError::Storage("inbox revision exhausted".into()))?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            reject_provisional_adoption(&tx, &self.repo)?;
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1) AND EXISTS(SELECT 1 FROM inbox_revisions WHERE repo = ?1 AND revision = ?4) AND NOT EXISTS(SELECT 1 FROM intents WHERE repo = ?1) AND NOT EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) AND NOT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo, holder, session, revision], |r| r.get(0))?;
            if !valid { return Err(rusqlite::Error::ToSqlConversionFailure("stale or unresolved session hold".into())); }
            let raw: String = tx.query_row("SELECT state FROM checkpoints WHERE repo = ?1", params![self.repo], |r| r.get(0))?;
            let previous: State = serde_json::from_str(&raw).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
            if previous.check_identity_targets.keys().any(|base| previous.check_identity_epochs.get(base) == Some(&u64::MAX)) { return Err(rusqlite::Error::ToSqlConversionFailure("identity epoch exhausted".into())); }
            let (held, decisions) = schlange_core::step(&previous, &schlange_config::rules::Rules::default(), &schlange_core::Event::IdentityBootstrapHeld, sampled);
            if !decisions.is_empty() { return Err(rusqlite::Error::ToSqlConversionFailure("hold unexpectedly emitted decisions".into())); }
            let encoded = serde_json::to_string(&held).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tx.execute("UPDATE checkpoints SET state = ?1, written = ?2 WHERE repo = ?3", params![encoded, crate::time::unix_now() as i64, self.repo])?;
            tx.execute("UPDATE inbox_revisions SET revision = ?1 WHERE repo = ?2", params![next, self.repo])?;
            tx.commit()?; Ok(held)
        }).map_err(storage)
    }

    /// Atomically persist a calculated worker step. Remote lease and ingress fences are caller obligations.
    pub fn commit_session_step(
        &mut self,
        session: &str,
        revision: u64,
        state: &State,
        decisions: &[Decision],
        now: impl FnOnce() -> u64,
    ) -> Result<Vec<(u64, Decision)>, JournalError> {
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        if session.is_empty()
            || state.identity_bootstrap_hold
            || decisions.iter().any(|d| {
                matches!(
                    d,
                    Decision::Act {
                        op: schlange_core::actions::Op::Dispatch { .. },
                        ..
                    }
                )
            })
            || state
                .actions
                .pending
                .values()
                .any(|p| matches!(p.op, schlange_core::actions::Op::Dispatch { .. }))
        {
            return Err(JournalError::Storage("unsupported session step".into()));
        }
        let mut persisted = state.clone();
        for decision in decisions {
            if let Decision::WakeAt { at } = decision {
                persisted.wakeups.insert(*at);
            }
        }
        let text = serde_json::to_string(&persisted)?;
        let revision = i64::try_from(revision).map_err(storage)?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| JournalError::Storage("inbox revision exhausted".into()))?;
        let side = decisions
            .iter()
            .filter(|d| schlange_core::journal::is_side_effect(d))
            .map(|d| Ok((d.clone(), serde_json::to_string(d)?)))
            .collect::<Result<Vec<_>, JournalError>>()?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![self.repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > now())) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            reject_provisional_adoption(&tx, &self.repo)?;
            let blocked: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND (typeof(processed) != 'integer' OR processed != 1)) OR EXISTS(SELECT 1 FROM inbox_redecide WHERE repo = ?1) OR EXISTS(SELECT 1 FROM intents WHERE repo = ?1) OR EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) OR EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo], |r| r.get(0))?;
            if blocked { return Err(rusqlite::Error::ToSqlConversionFailure("unresolved work requires recovery".into())); }
            let actual: Option<i64> = tx.query_row("SELECT revision FROM inbox_revisions WHERE repo = ?1", params![self.repo], |r| r.get(0)).optional()?;
            if actual.unwrap_or(0) != revision { return Err(rusqlite::Error::ToSqlConversionFailure("stale worker revision".into())); }
            let high: i64 = tx.query_row("SELECT high_water FROM status_sequence WHERE repo = ?1 AND holder = ?2 AND session = ?3 AND confirmed = 1", params![self.repo, holder, session], |r| r.get(0))?;
            if high < 0 { return Err(rusqlite::Error::ToSqlConversionFailure("invalid sequence high-water".into())); }
            let mut assigned = Vec::new(); let mut next = high;
            for (decision, encoded) in &side {
                next = next.checked_add(1).ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("intent sequence exhausted".into()))?;
                tx.execute("INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)", params![self.repo, next, encoded])?;
                assigned.push((next as u64, decision.clone()));
            }
            if tx.execute("UPDATE status_sequence SET high_water = ?1 WHERE repo = ?2 AND holder = ?3 AND session = ?4 AND confirmed = 1 AND high_water = ?5", params![next, self.repo, holder, session, high])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            if tx.execute("UPDATE checkpoints SET state = ?1, written = ?2 WHERE repo = ?3", params![text, crate::time::unix_now() as i64, self.repo])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.execute("DELETE FROM history WHERE repo = ?1", params![self.repo])?;
            for (key, samples) in &persisted.history { for sample in samples { tx.execute("INSERT INTO history (repo, key, at, secs) VALUES (?1, ?2, ?3, ?4)", params![self.repo, key, sample.at as i64, sample.secs as i64])?; }}
            tx.execute("INSERT OR REPLACE INTO inbox_revisions (repo, revision) VALUES (?1, ?2)", params![self.repo, next_revision])?;
            tx.commit()?; Ok(assigned)
        }).map_err(storage)
    }

    /// Read pending state without authorizing replay; in-doubt effects remain quarantined.
    pub fn pending_status_snapshot(
        &self,
    ) -> Result<Option<(Recovered, u64, InboxDelivery)>, JournalError> {
        let (recovered, dispatches, revision, pending) = self.recover_all_with_read_hook(|| {})?;
        if !pending || !dispatches.is_empty() {
            return Ok(None);
        }
        let Some(recovered) = recovered else {
            return Ok(None);
        };
        if !recovered.in_doubt.is_empty() {
            return Ok(None);
        }
        let items = self
            .store
            .pending_inbox_all_epochs(&self.repo)
            .map_err(storage)?;
        let Some(item) = items.into_iter().next() else {
            return Ok(None);
        };
        if item.replay_hint().is_none() {
            return Ok(None);
        }
        Ok(Some((recovered, revision, item)))
    }

    pub fn has_inbox_redecide(&self) -> Result<bool, JournalError> {
        self.store
            .with(|c| {
                c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM inbox_redecide WHERE repo = ?1)",
                    params![self.repo],
                    |r| r.get(0),
                )
            })
            .map_err(storage)
    }

    pub fn held_redecide_snapshot(&self) -> Result<Option<Recovered>, JournalError> {
        let (recovered, dispatches, _, _) = self.recover_all_with_read_hook(|| {})?;
        if !dispatches.is_empty() {
            return Err(JournalError::Storage(
                "prepared dispatch requires recovery".into(),
            ));
        }
        Ok(recovered)
    }

    /// Checkpoint a still-held verified view and retain the obligation to re-decide before release.
    pub fn commit_held_status(
        &mut self,
        item: &InboxDelivery,
        revision: u64,
        state: &State,
        session: &str,
        now: impl FnOnce() -> u64,
    ) -> Result<(), JournalError> {
        if !state.identity_bootstrap_hold || item.replay_hint().is_none() {
            return Err(JournalError::Storage("held status required".into()));
        }
        let mark: Option<i64> = self
            .store
            .with(|c| {
                c.query_row(
                    "SELECT high_water FROM status_sequence WHERE repo = ?1 AND session = ?2",
                    params![self.repo, session],
                    |r| r.get(0),
                )
                .optional()
            })
            .map_err(storage)?;
        let Some(mark) = mark else {
            return Err(JournalError::Storage("unknown status session".into()));
        };
        self.commit_inbox_step_inner(item, revision, state, &[], Some((session, mark)), true, now)
    }

    pub fn confirm_rotated_status_session(
        &self,
        session: &str,
        fence: &str,
        now: impl FnOnce() -> u64,
    ) -> Result<(), JournalError> {
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        self.store
            .confirm_status_session(&self.repo, holder, session, fence, now)
            .map_err(storage)
    }

    /// Rotate an already-established session. No marker means unknown legacy lineage.
    pub fn rotate_status_session(
        &self,
        now: impl FnOnce() -> u64,
    ) -> Result<Option<String>, JournalError> {
        use ring::rand::SecureRandom;
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let previous: Option<String> = self
            .store
            .with(|c| {
                c.query_row(
                    "SELECT session FROM status_sequence WHERE repo = ?1",
                    params![self.repo],
                    |r| r.get(0),
                )
                .optional()
            })
            .map_err(storage)?;
        let Some(previous) = previous else {
            return Ok(None);
        };
        let mut nonce = [0_u8; 32];
        ring::rand::SystemRandom::new()
            .fill(&mut nonce)
            .map_err(|_| JournalError::Storage("cannot generate process session".into()))?;
        let session = nonce.iter().map(|b| format!("{b:02x}")).collect::<String>();
        self.store
            .take_over_status_session(&self.repo, holder, &previous, &session, now)
            .map_err(storage)?;
        Ok(Some(session))
    }

    /// Writes only while `holder` holds the repository's lease.
    pub fn fenced(mut self, holder: &str) -> Self {
        self.holder = Some(holder.to_string());
        self
    }

    pub fn has_pending_inbox(&self) -> Result<bool, JournalError> {
        self.store.has_pending_inbox(&self.repo).map_err(storage)
    }

    /// The highest intent sequence number on record (to continue numbering after a restart).
    pub fn max_seq(&self) -> Result<u64, JournalError> {
        self.store
            .with(|c| {
                let (max, invalid): (i64, bool) = c.query_row(
                    "SELECT COALESCE(MAX(seq), 0), EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND (typeof(seq) != 'integer' OR seq < 0)) FROM intents WHERE repo = ?1",
                    params![self.repo], |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                if invalid { return Err(rusqlite::Error::ToSqlConversionFailure("invalid intent sequence".into())); }
                Ok(max)
            })
            .map_err(storage)
            .and_then(|n| u64::try_from(n).map_err(storage))
    }
}

impl SqliteJournal {
    /// Experimental atomic calculation; caller must prove snapshot freshness and external fences.
    pub fn commit_status_snapshot(
        &mut self,
        item: &InboxDelivery,
        revision: u64,
        rules: &schlange_config::rules::Rules,
        snapshot: &schlange_core::engine::Event,
        at: u64,
        session: &str,
        now: impl FnOnce() -> u64,
    ) -> Result<Vec<Decision>, JournalError> {
        use schlange_core::engine::{Event, step};
        if item.replay_hint().is_none() || !matches!(snapshot, Event::Snapshot { .. }) {
            return Err(JournalError::Storage(
                "unsupported inbox event or snapshot".into(),
            ));
        }
        let (recovered, dispatches, actual_revision, pending) =
            self.recover_all_with_read_hook(|| {})?;
        if !pending || !dispatches.is_empty() || revision != actual_revision {
            return Err(JournalError::Storage("inbox recovery not current".into()));
        }
        let recovered =
            recovered.ok_or_else(|| JournalError::Storage("missing checkpoint".into()))?;
        if !recovered.in_doubt.is_empty() {
            return Err(JournalError::Storage(
                "in-doubt intents require recovery".into(),
            ));
        }
        if recovered.state.identity_bootstrap_hold {
            return Err(JournalError::Storage("identity bootstrap held".into()));
        }
        let (state, decisions) = step(&recovered.state, rules, snapshot, at);
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("missing fenced holder".into()))?;
        let mark: Option<(String, String, i64)> = self
            .store
            .with(|c| {
                c.query_row(
                    "SELECT session, holder, high_water FROM status_sequence WHERE repo = ?1",
                    params![self.repo],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()
            })
            .map_err(storage)?;
        let Some((bound_session, bound_holder, high_water)) = mark else {
            return Err(JournalError::Storage("unknown sequence lineage".into()));
        };
        if session.is_empty() || session != bound_session || holder != bound_holder {
            return Err(JournalError::Storage("stale status session".into()));
        }
        let mut seq = u64::try_from(high_water).map_err(storage)?;
        let mut intents = Vec::new();
        for d in decisions
            .iter()
            .filter(|d| schlange_core::journal::is_side_effect(d))
        {
            seq = seq
                .checked_add(1)
                .ok_or_else(|| JournalError::Storage("intent sequence exhausted".into()))?;
            intents.push((seq, d.clone()));
        }
        self.commit_inbox_step_inner(
            item,
            revision,
            &state,
            &intents,
            Some((session, high_water)),
            false,
            now,
        )?;
        Ok(decisions)
    }

    /// Atomically checkpoint one authenticated inbox item's processed state and all supplied
    /// side-effect intents. This primitive does not authorize replay or external writes.
    pub fn commit_inbox_step(
        &mut self,
        item: &InboxDelivery,
        expected_revision: u64,
        state: &State,
        intents: &[(u64, Decision)],
        now: impl FnOnce() -> u64,
    ) -> Result<(), JournalError> {
        self.commit_inbox_step_inner(item, expected_revision, state, intents, None, false, now)
    }

    fn commit_inbox_step_inner(
        &mut self,
        item: &InboxDelivery,
        expected_revision: u64,
        state: &State,
        intents: &[(u64, Decision)],
        status_session: Option<(&str, i64)>,
        held_status: bool,
        now: impl FnOnce() -> u64,
    ) -> Result<(), JournalError> {
        let Some(holder) = self.holder.as_deref() else {
            return Err(JournalError::Storage(
                "inbox commit requires a fenced journal".into(),
            ));
        };
        if item.repo != self.repo
            || state
                .actions
                .pending
                .values()
                .any(|p| matches!(p.op, schlange_core::actions::Op::Dispatch { .. }))
            || intents.iter().any(|(_, d)| {
                matches!(
                    d,
                    Decision::Act {
                        op: schlange_core::actions::Op::Dispatch { .. },
                        ..
                    }
                )
            })
        {
            return Err(JournalError::Storage(
                "inbox step requires matching repository and no unjournaled dispatch".into(),
            ));
        }
        let text = serde_json::to_string(state)?;
        let history = serde_json::to_string(&state.history)?;
        let records = intents
            .iter()
            .map(|(seq, d)| {
                Ok((
                    i64::try_from(*seq).map_err(storage)?,
                    serde_json::to_string(d)?,
                ))
            })
            .collect::<Result<Vec<_>, JournalError>>()?;
        let received = i64::try_from(item.received_at).map_err(storage)?;
        let revision = i64::try_from(expected_revision).map_err(storage)?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| JournalError::Storage("inbox revision exhausted".into()))?;
        let repo = self.repo.clone();
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let owner: Option<(String, i64)> = tx.query_row("SELECT holder, expires FROM lease WHERE repo = ?1", params![repo], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
            let sampled = now();
            if !owner.is_some_and(|(h, expiry)| h == holder && u64::try_from(expiry).is_ok_and(|e| e > sampled)) { return Err(rusqlite::Error::ToSqlConversionFailure(LEASE_LOST.into())); }
            if status_session.is_none() && tx.query_row("SELECT EXISTS(SELECT 1 FROM status_sequence WHERE repo = ?1)", params![repo], |r| r.get::<_, bool>(0))? { return Err(rusqlite::Error::ToSqlConversionFailure("status session requires fenced completion".into())); }
            if let Some((session, old)) = status_session {
                let count = i64::try_from(records.len()).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                let next = old.checked_add(count).ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("status sequence exhausted".into()))?;
                if old < 0 || tx.execute("UPDATE status_sequence SET high_water = ?1 WHERE repo = ?2 AND session = ?3 AND holder = ?4 AND high_water = ?5 AND confirmed = 1", params![next, repo, session, holder, old])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("stale status sequence session".into())); }
            }
            reject_provisional_adoption(&tx, &repo)?;
            reject_pending_dispatches(&tx, &repo)?;
            if held_status && tx.query_row("SELECT EXISTS(SELECT 1 FROM intents WHERE repo = ?1)", params![repo], |r| r.get::<_, bool>(0))? { return Err(rusqlite::Error::ToSqlConversionFailure("in-doubt effects require recovery".into())); }
            let current: Option<i64> = tx.query_row("SELECT revision FROM inbox_revisions WHERE repo = ?1", params![repo], |r| r.get(0)).optional()?;
            if current.unwrap_or(0) != revision { return Err(rusqlite::Error::ToSqlConversionFailure("stale inbox revision".into())); }
            let oldest: Option<String> = tx.query_row("SELECT guid FROM inbox WHERE repo = ?1 AND processed = 0 ORDER BY rowid LIMIT 1", params![repo], |r| r.get(0)).optional()?;
            if oldest.as_deref() != Some(&item.guid) { return Err(rusqlite::Error::ToSqlConversionFailure("inbox item is not oldest pending".into())); }
            tx.execute("INSERT OR REPLACE INTO checkpoints (repo, state, written) VALUES (?1, ?2, ?3)", params![repo, text, crate::time::unix_now() as i64])?;
            tx.execute("DELETE FROM history WHERE repo = ?1", params![repo])?;
            for (key, samples) in &state.history { for sample in samples {
                tx.execute("INSERT INTO history (repo, key, at, secs) VALUES (?1, ?2, ?3, ?4)", params![repo, key, sample.at as i64, sample.secs as i64])?;
            }}
            for (seq, decision) in &records { tx.execute("INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)", params![repo, seq, decision])?; }
            if tx.execute("UPDATE inbox SET processed = 1 WHERE guid = ?1 AND repo = ?2 AND epoch = ?3 AND event = ?4 AND body = ?5 AND received_at = ?6 AND processed = 0", params![item.guid, repo, item.epoch, item.event, item.body, received])? != 1 { return Err(rusqlite::Error::QueryReturnedNoRows); }
            tx.execute("INSERT OR REPLACE INTO inbox_revisions (repo, revision) VALUES (?1, ?2)", params![repo, next_revision])?;
            if held_status { tx.execute("INSERT OR REPLACE INTO inbox_redecide (repo, revision) VALUES (?1, ?2)", params![repo, next_revision])?; }
            tx.commit()
        }).map_err(storage)?;
        self.history_written = Some(history);
        Ok(())
    }

    pub fn checkpoint_with_intents(
        &mut self,
        state: &State,
        intents: &[(u64, Decision)],
    ) -> Result<(), JournalError> {
        self.commit_reply(state, &[], intents)
    }

    /// Atomically checkpoint the state produced by a validated reply, resolve the
    /// associated prior intents, and persist all resulting new intents.
    pub fn commit_reply(
        &mut self,
        state: &State,
        completed: &[u64],
        intents: &[(u64, Decision)],
    ) -> Result<(), JournalError> {
        if state
            .actions
            .pending
            .values()
            .any(|p| matches!(p.op, schlange_core::actions::Op::Dispatch { .. }))
        {
            return Err(JournalError::Storage(
                "pending dispatch requires atomic outbox".into(),
            ));
        }
        self.commit_reply_with_dispatches(state, completed, intents, &[])
    }

    pub fn checkpoint_with_prepared_dispatches(
        &mut self,
        state: &State,
        dispatches: &[PreparedDispatch],
    ) -> Result<(), JournalError> {
        self.checkpoint_with_effects(state, &[], dispatches)
    }

    pub fn checkpoint_with_effects(
        &mut self,
        state: &State,
        intents: &[(u64, Decision)],
        dispatches: &[PreparedDispatch],
    ) -> Result<(), JournalError> {
        if intents.iter().any(|(_, d)| {
            matches!(
                d,
                Decision::Act {
                    op: schlange_core::actions::Op::Dispatch { .. },
                    ..
                }
            )
        }) {
            return Err(JournalError::Storage(
                "dispatch must use prepared outbox".into(),
            ));
        }
        if self.holder.is_none() {
            return Err(JournalError::Storage(
                "dispatch outbox requires a fenced journal".into(),
            ));
        }
        let pending: std::collections::BTreeSet<u64> = state
            .actions
            .pending
            .iter()
            .filter_map(|(id, p)| {
                matches!(p.op, schlange_core::actions::Op::Dispatch { .. }).then_some(*id)
            })
            .collect();
        let recorded: std::collections::BTreeSet<u64> = dispatches
            .iter()
            .filter_map(|d| match d.decision {
                Decision::Act { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        if recorded.len() != dispatches.len() || recorded != pending {
            return Err(JournalError::Storage(
                "dispatch outbox must cover all pending dispatches".into(),
            ));
        }
        self.commit_reply_with_dispatches(state, &[], intents, dispatches)
    }

    fn commit_reply_with_dispatches(
        &mut self,
        state: &State,
        completed: &[u64],
        intents: &[(u64, Decision)],
        dispatches: &[PreparedDispatch],
    ) -> Result<(), JournalError> {
        let dispatches = dispatches
            .iter()
            .map(|d| {
                let matches_pending = match &d.decision {
                    Decision::Act { id, rule, op } => state
                        .actions
                        .pending
                        .get(id)
                        .is_some_and(|p| p.rule == *rule && p.op == *op),
                    _ => false,
                };
                let matches_pull = state
                    .pull_requests
                    .get(&match &d.decision {
                        Decision::Act { op, .. } => op.number(),
                        _ => 0,
                    })
                    .is_some_and(|p| {
                        p.head_sha == d.head && p.base == d.base && !p.closed && !p.merged
                    });
                if !d.valid() || !matches_pending || !matches_pull {
                    return Err(JournalError::Storage("invalid dispatch evidence".into()));
                }
                Ok((
                    d.session.clone(),
                    i64::try_from(d.id).map_err(storage)?,
                    serde_json::to_string(d)?,
                ))
            })
            .collect::<Result<Vec<_>, JournalError>>()?;
        let intents = intents
            .iter()
            .map(|(seq, decision)| Ok((*seq, serde_json::to_string(decision)?)))
            .collect::<Result<Vec<_>, JournalError>>()?;
        let text = serde_json::to_string(state)?;
        let history = serde_json::to_string(&state.history)?;
        let write_history = self.history_written.as_deref() != Some(history.as_str());
        let now = crate::time::unix_now() as i64;
        let repo = self.repo.clone();
        let holder = self.holder.clone();
        self.store
            .with(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                check_holder(&tx, &repo, holder.as_deref())?;
                reject_provisional_adoption(&tx, &repo)?;
                reject_legacy_journal_with_inbox(&tx, &repo)?;
                advance_checkpoint_revision(&tx, &repo)?;
                let pending: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM prepared_dispatches WHERE repo = ?1) OR EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![repo], |r| r.get(0))?;
                if pending { return Err(rusqlite::Error::ToSqlConversionFailure("unresolved dispatches require explicit recovery".into())); }
                tx.execute(
                    "INSERT OR REPLACE INTO checkpoints (repo, state, written) VALUES (?1, ?2, ?3)",
                    params![repo, text, now],
                )?;
                if write_history {
                    tx.execute("DELETE FROM history WHERE repo = ?1", params![repo])?;
                    for (k, samples) in &state.history {
                        for s in samples {
                            tx.execute(
                                "INSERT INTO history (repo, key, at, secs) VALUES (?1, ?2, ?3, ?4)",
                                params![repo, k, s.at as i64, s.secs as i64],
                            )?;
                        }
                    }
                }
                for seq in completed {
                    let seq = i64::try_from(*seq)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    if tx.execute(
                        "DELETE FROM intents WHERE repo = ?1 AND seq = ?2",
                        params![repo, seq],
                    )? != 1
                    {
                        return Err(rusqlite::Error::QueryReturnedNoRows);
                    }
                }
                for (seq, decision) in &intents {
                    let seq = i64::try_from(*seq)
                        .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
                    tx.execute(
                        "INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                        params![repo, seq, decision],
                    )?;
                }
                for (session, id, decision) in &dispatches {
                    tx.execute("INSERT INTO prepared_dispatches (repo, session, id, decision, status) VALUES (?1, ?2, ?3, ?4, 'prepared')", params![repo, session, id, decision])?;
                }
                tx.commit()
            })
            .map_err(storage)?;
        if write_history {
            self.history_written = Some(history);
        }
        Ok(())
    }
}

impl SqliteJournal {
    pub fn dispatch_outbox(&self) -> Result<Vec<(PreparedDispatch, DispatchStatus)>, JournalError> {
        let entries: Vec<(String, i64, String, String)> = self.store.with(|c| {
            let mut st = c.prepare("SELECT session, id, decision, status FROM prepared_dispatches WHERE repo = ?1 ORDER BY session, id")?;
            let rows = st.query_map(params![self.repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
            rows.collect()
        }).map_err(storage)?;
        entries
            .into_iter()
            .map(|(session, id, data, status)| {
                let dispatch: PreparedDispatch = serde_json::from_str(&data)?;
                if !dispatch.valid()
                    || dispatch.session != session
                    || i64::try_from(dispatch.id).ok() != Some(id)
                {
                    return Err(JournalError::Storage(
                        "invalid stored dispatch identity".into(),
                    ));
                }
                let status = match status.as_str() {
                    "prepared" => DispatchStatus::Prepared,
                    "may_have_sent" => DispatchStatus::MayHaveSent,
                    _ => return Err(JournalError::Storage("unknown dispatch status".into())),
                };
                Ok((dispatch, status))
            })
            .collect()
    }

    pub fn mark_dispatch_if_matches(
        &mut self,
        expected: &PreparedDispatch,
    ) -> Result<(), JournalError> {
        if self.holder.is_none() || !expected.valid() {
            return Err(JournalError::Storage(
                "dispatch transition requires fenced, valid evidence".into(),
            ));
        }
        let id = i64::try_from(expected.id).map_err(storage)?;
        let decision = serde_json::to_string(expected)?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            check_holder(&tx, &self.repo, self.holder.as_deref())?;
            reject_provisional_adoption(&tx, &self.repo)?;
            reject_legacy_journal_with_inbox(&tx, &self.repo)?;
            if tx.execute("UPDATE prepared_dispatches SET status = 'may_have_sent' WHERE repo = ?1 AND session = ?2 AND id = ?3 AND decision = ?4 AND status = 'prepared'", params![self.repo, expected.session, id, decision])? != 1 {
                return Err(rusqlite::Error::QueryReturnedNoRows);
            }
            tx.commit()
        }).map_err(storage)
    }
}

impl Journal for SqliteJournal {
    fn checkpoint(&mut self, state: &State) -> Result<(), JournalError> {
        self.checkpoint_with_intents(state, &[])
    }

    fn intent(&mut self, seq: u64, decision: &Decision) -> Result<(), JournalError> {
        let text = serde_json::to_string(decision)?;
        let seq = i64::try_from(seq).map_err(storage)?;
        self.store
            .with(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                check_holder(&tx, &self.repo, self.holder.as_deref())?;
                reject_provisional_adoption(&tx, &self.repo)?;
                reject_legacy_journal_with_inbox(&tx, &self.repo)?;
                reject_pending_dispatches(&tx, &self.repo)?;
                tx.execute(
                    "INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                    params![self.repo, seq, text],
                )?;
                tx.commit()
            })
            .map_err(storage)
    }

    fn done(&mut self, seq: u64) -> Result<(), JournalError> {
        let seq = i64::try_from(seq).map_err(storage)?;
        self.store
            .with(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                check_holder(&tx, &self.repo, self.holder.as_deref())?;
                reject_provisional_adoption(&tx, &self.repo)?;
                reject_legacy_journal_with_inbox(&tx, &self.repo)?;
                reject_pending_dispatches(&tx, &self.repo)?;
                tx.execute(
                    "DELETE FROM intents WHERE repo = ?1 AND seq = ?2",
                    params![self.repo, seq],
                )?;
                tx.commit()
            })
            .map_err(storage)
    }

    fn recover(&self) -> Result<Option<Recovered>, JournalError> {
        self.recover_with_read_hook(|| {})
    }
}

impl SqliteJournal {
    fn recover_with_read_hook(
        &self,
        after_checkpoint: impl FnOnce(),
    ) -> Result<Option<Recovered>, JournalError> {
        let (recovery, dispatches, _, pending_inbox) =
            self.recover_all_with_read_hook(after_checkpoint)?;
        if pending_inbox {
            return Err(JournalError::Storage(
                "pending inbox requires explicit recovery".into(),
            ));
        }
        if !dispatches.is_empty() {
            return Err(JournalError::Storage(
                "prepared dispatches require explicit recovery".into(),
            ));
        }
        Ok(recovery)
    }

    /// Read state, intents and dispatch evidence from one SQLite snapshot. Does not authorize sending.
    pub fn recover_with_dispatches(
        &self,
    ) -> Result<Option<(Recovered, Vec<(PreparedDispatch, DispatchStatus)>)>, JournalError> {
        let (recovery, dispatches, _, pending_inbox) = self.recover_all_with_read_hook(|| {})?;
        if pending_inbox {
            return Err(JournalError::Storage(
                "pending inbox requires explicit recovery".into(),
            ));
        }
        Ok(recovery.map(|r| (r, dispatches)))
    }

    /// Read state and revision from the same SQLite snapshot; not an authorization proof.
    #[cfg(test)]
    fn recover_with_revision_hook(
        &self,
        after_checkpoint: impl FnOnce(),
    ) -> Result<(Option<Recovered>, u64), JournalError> {
        let (recovery, dispatches, revision, _) =
            self.recover_all_with_read_hook(after_checkpoint)?;
        if !dispatches.is_empty() {
            return Err(JournalError::Storage(
                "prepared dispatches require explicit recovery".into(),
            ));
        }
        Ok((recovery, revision))
    }

    fn recover_all_with_read_hook(
        &self,
        after_checkpoint: impl FnOnce(),
    ) -> Result<
        (
            Option<Recovered>,
            Vec<(PreparedDispatch, DispatchStatus)>,
            u64,
            bool,
        ),
        JournalError,
    > {
        let (text, intents, history, entries, revision, pending_inbox) = self
            .store
            .with(|c| {
                let tx = c.transaction()?;
                reject_provisional_adoption(&tx, &self.repo)?;
                let build_unknown: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM session_build_claims WHERE repo = ?1)", params![self.repo], |r| r.get(0))?;
                if build_unknown { return Err(rusqlite::Error::ToSqlConversionFailure("started build requires explicit recovery".into())); }
                let text: Option<String> = tx
                    .query_row(
                        "SELECT state FROM checkpoints WHERE repo = ?1",
                        params![self.repo],
                        |r| r.get(0),
                    )
                    .optional()?;
                after_checkpoint();
                let intents: Vec<(u64, String)> = {
                    let mut st = tx.prepare(
                        "SELECT seq, decision FROM intents WHERE repo = ?1 ORDER BY seq",
                    )?;
                    let rows = st.query_map(params![self.repo], |r| {
                        let seq = r.get::<_, i64>(0)?;
                        let seq = u64::try_from(seq).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(e)))?;
                        Ok((seq, r.get::<_, String>(1)?))
                    })?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let history = {
                    let mut st = tx.prepare(
                        "SELECT key, at, secs FROM history WHERE repo = ?1 ORDER BY rowid",
                    )?;
                    let rows = st.query_map(params![self.repo], |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)? as Time,
                            r.get::<_, i64>(2)? as Time,
                        ))
                    })?;
                    let mut out: BTreeMap<String, Vec<Sample>> = BTreeMap::new();
                    for row in rows {
                        let (key, at, secs) = row?;
                        out.entry(key).or_default().push(Sample { at, secs });
                    }
                    out
                };
                let entries: Vec<(String, i64, String, String)> = {
                    let mut st = tx.prepare("SELECT session, id, decision, status FROM prepared_dispatches WHERE repo = ?1 ORDER BY session, id")?;
                    let rows = st.query_map(params![self.repo], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
                    rows.collect::<rusqlite::Result<Vec<_>>>()?
                };
                let revision: Option<i64> = tx.query_row("SELECT revision FROM inbox_revisions WHERE repo = ?1", params![self.repo], |r| r.get(0)).optional()?;
                let revision = u64::try_from(revision.unwrap_or(0)).map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Integer, Box::new(e)))?;
                let pending_inbox: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM inbox WHERE repo = ?1 AND processed = 0)", params![self.repo], |r| r.get(0))?;
                tx.commit()?;
                Ok((text, intents, history, entries, revision, pending_inbox))
            })
            .map_err(storage)?;
        let mut dispatches = Vec::new();
        for (session, id, data, status) in entries {
            let d: PreparedDispatch = serde_json::from_str(&data)?;
            if !d.valid() || d.session != session || i64::try_from(d.id).ok() != Some(id) {
                return Err(JournalError::Storage(
                    "invalid stored dispatch identity".into(),
                ));
            }
            let status = match status.as_str() {
                "prepared" => DispatchStatus::Prepared,
                "may_have_sent" => DispatchStatus::MayHaveSent,
                _ => return Err(JournalError::Storage("unknown dispatch status".into())),
            };
            dispatches.push((d, status));
        }
        let state = match text {
            Some(t) => Some(serde_json::from_str::<State>(&t)?),
            None if !intents.is_empty() || !dispatches.is_empty() => {
                return Err(JournalError::Storage(
                    "journal intents exist without a checkpoint".into(),
                ));
            }
            None if history.is_empty() => None,
            None => Some(State {
                history,
                ..State::default()
            }),
        };
        let Some(state) = state else {
            return Ok((None, dispatches, revision, pending_inbox));
        };
        let in_doubt = intents
            .into_iter()
            .map(|(seq, d)| Ok((seq, serde_json::from_str(&d)?)))
            .collect::<Result<Vec<_>, JournalError>>()?;
        let pending_dispatches: std::collections::BTreeSet<u64> = state
            .actions
            .pending
            .iter()
            .filter_map(|(id, p)| {
                matches!(p.op, schlange_core::actions::Op::Dispatch { .. }).then_some(*id)
            })
            .collect();
        let recorded: std::collections::BTreeSet<u64> = dispatches
            .iter()
            .filter_map(|(d, _)| match d.decision {
                Decision::Act { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        if recorded.len() != dispatches.len() || recorded != pending_dispatches {
            return Err(JournalError::Storage(
                "dispatch outbox does not match pending actions".into(),
            ));
        }
        for (d, _) in &dispatches {
            let (id, rule, op) = match &d.decision {
                Decision::Act { id, rule, op } => (*id, rule, op),
                _ => return Err(JournalError::Storage("invalid dispatch decision".into())),
            };
            let pending = state
                .actions
                .pending
                .get(&id)
                .is_some_and(|p| &p.rule == rule && &p.op == op);
            let pull = state.pull_requests.get(&op.number()).is_some_and(|p| {
                !p.closed && !p.merged && p.head_sha == d.head && p.base == d.base
            });
            if !pending || !pull {
                return Err(JournalError::Storage(
                    "dispatch disagrees with checkpoint".into(),
                ));
            }
        }
        Ok((
            Some(Recovered { state, in_doubt }),
            dispatches,
            revision,
            pending_inbox,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "schlange-store-{name}-{}-{}",
            std::process::id(),
            crate::time::unix_now()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn journal_survives_reopening_and_reports_intents_in_doubt() {
        let d = dir("journal");
        let mut state = State {
            counter: 41,
            ..State::default()
        };
        state
            .history
            .insert("default@main".into(), vec![Sample { at: 10, secs: 300 }]);
        let land = Decision::WakeAt { at: 99 };
        {
            let s = Store::open(&d).unwrap();
            assert_eq!(s.journal_mode().unwrap().to_ascii_lowercase(), "wal");
            let mut j = s.journal("o/r");
            assert!(j.recover().unwrap().is_none());
            j.checkpoint(&state).unwrap();
            j.intent(1, &land).unwrap();
            j.intent(2, &land).unwrap();
            j.done(1).unwrap();
            s.record_delivery("abc", 5, 10).unwrap();
            s.save_car("o/r", 7, &[(1, "t1".into()), (2, "t2".into())])
                .unwrap();
        }
        let s = Store::open(&d).unwrap();
        let j = s.journal("o/r");
        let r = j.recover().unwrap().unwrap();
        assert_eq!(r.state.counter, 41);
        assert_eq!(r.state.history, state.history);
        assert_eq!(r.in_doubt, vec![(2, land)]);
        assert_eq!(j.max_seq().unwrap(), 2);
        assert!(s.delivery_seen("abc").unwrap());
        assert!(!s.delivery_seen("abd").unwrap());
        assert_eq!(
            s.load_car("o/r", 7).unwrap().unwrap(),
            vec![(1, "t1".to_string()), (2, "t2".to_string())]
        );
        assert!(s.journal("o/other").recover().unwrap().is_none());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn checkpoint_and_decisions_commit_atomically() {
        let d = dir("atomic-intents");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        let old = State {
            counter: 1,
            ..State::default()
        };
        let new = State {
            counter: 2,
            ..State::default()
        };
        j.checkpoint(&old).unwrap();
        let one = Decision::WakeAt { at: 10 };
        let two = Decision::WakeAt { at: 20 };
        j.checkpoint_with_intents(&new, &[(1, one.clone()), (2, two.clone())])
            .unwrap();
        let r = s.journal("o/r").recover().unwrap().unwrap();
        assert_eq!(r.state.counter, 2);
        assert_eq!(r.in_doubt, vec![(1, one.clone()), (2, two)]);
        assert!(
            j.checkpoint_with_intents(&old, &[(3, one.clone()), (1, one)])
                .is_err()
        );
        let r = s.journal("o/r").recover().unwrap().unwrap();
        assert_eq!(
            r.state.counter, 2,
            "failed batch must not write the checkpoint"
        );
        assert_eq!(
            r.in_doubt.len(),
            2,
            "failed batch must not retain partial intents"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn prepared_dispatch_is_atomic_and_transition_is_fenced_and_irreversible() {
        let d = dir("dispatch-outbox");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut ja = a.journal("o/r").fenced("a");
        let old = State {
            counter: 1,
            ..State::default()
        };
        let new = State {
            counter: 2,
            ..State::default()
        };
        ja.checkpoint(&old).unwrap();
        ja.intent(20, &Decision::WakeAt { at: 20 }).unwrap();
        let prepared = PreparedDispatch {
            session: "session-a".into(),
            id: 7,
            decision: Decision::Act {
                id: 9,
                rule: "preview".into(),
                op: schlange_core::actions::Op::Dispatch {
                    number: 1,
                    workflow: "preview.yml".into(),
                    reference: Some("main".into()),
                    inputs: BTreeMap::new(),
                },
            },
            head: "head".into(),
            base: "main".into(),
            config_digest: "config".into(),
        };
        assert!(
            a.journal("o/r")
                .checkpoint_with_prepared_dispatches(&new, &[prepared.clone()])
                .is_err(),
            "unfenced outbox writes are forbidden"
        );
        assert!(
            ja.checkpoint_with_prepared_dispatches(&new, &[prepared.clone()])
                .is_err(),
            "dispatch must match a pending action in checkpoint"
        );
        let mut new = new;
        if let Decision::Act { id, rule, op } = &prepared.decision {
            new.actions.pending.insert(
                *id,
                schlange_core::actions::PendingAction {
                    rule: rule.clone(),
                    op: op.clone(),
                    level: None,
                    reply: None,
                    report: None,
                    sent: 0,
                    attempts: 1,
                    edge: None,
                },
            );
        }
        assert!(
            ja.checkpoint(&new).is_err(),
            "legacy checkpoint must not silently persist dispatch without outbox"
        );
        assert!(
            ja.checkpoint_with_prepared_dispatches(&new, &[prepared.clone()])
                .is_err(),
            "dispatch evidence must match current PR head/base"
        );
        new.pull_requests.insert(
            1,
            schlange_core::model::PullRequest::new(1, "dev", "main", "branch", "head"),
        );
        let mut incomplete = new.clone();
        let extra = incomplete.actions.pending.get(&9).unwrap().clone();
        incomplete.actions.pending.insert(10, extra);
        assert!(
            ja.checkpoint_with_prepared_dispatches(&incomplete, &[prepared.clone()])
                .is_err(),
            "every pending dispatch needs an outbox entry"
        );
        ja.checkpoint_with_effects(
            &new,
            &[(31, Decision::WakeAt { at: 31 })],
            &[prepared.clone()],
        )
        .unwrap();
        assert!(
            a.journal("o/r")
                .mark_dispatch_if_matches(&prepared)
                .is_err(),
            "unfenced transition is forbidden"
        );
        assert!(
            a.journal("o/r").recover().is_err(),
            "legacy recovery must not ignore prepared dispatches"
        );
        assert!(
            ja.checkpoint(&old).is_err(),
            "generic checkpoint must not overwrite state with unresolved dispatch"
        );
        assert!(
            ja.intent(21, &Decision::WakeAt { at: 21 }).is_err(),
            "legacy intent must not bypass unresolved outbox"
        );
        assert!(
            ja.done(20).is_err(),
            "legacy done must not bypass unresolved outbox"
        );
        let (snapshot, entries) = a.journal("o/r").recover_with_dispatches().unwrap().unwrap();
        assert_eq!(snapshot.state.counter, 2);
        assert!(
            snapshot
                .in_doubt
                .contains(&(31, Decision::WakeAt { at: 31 })),
            "other same-step intents are committed with dispatch"
        );
        assert_eq!(entries, vec![(prepared.clone(), DispatchStatus::Prepared)]);
        let recovered = a.journal("o/r").dispatch_outbox().unwrap();
        assert_eq!(
            recovered,
            vec![(prepared.clone(), DispatchStatus::Prepared)]
        );
        assert!(
            ja.checkpoint_with_prepared_dispatches(&old, &[prepared.clone()])
                .is_err()
        );
        let stored: String = a
            .with(|c| {
                c.query_row(
                    "SELECT state FROM checkpoints WHERE repo = ?1",
                    params!["o/r"],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(serde_json::from_str::<State>(&stored).unwrap().counter, 2);
        assert!(b.lease_acquire("o/r", "b", 131, 190).unwrap());
        let mut jb = b.journal("o/r").fenced("b");
        assert!(ja.mark_dispatch_if_matches(&prepared).is_err());
        let mut changed = prepared.clone();
        changed.head = "other-head".into();
        b.with(|c| {
            c.execute(
                "UPDATE prepared_dispatches SET decision = ?1 WHERE repo = 'o/r'",
                params![serde_json::to_string(&changed).unwrap()],
            )
        })
        .unwrap();
        assert!(
            jb.mark_dispatch_if_matches(&prepared).is_err(),
            "changed payload must not be marked for sending"
        );
        b.with(|c| {
            c.execute(
                "UPDATE prepared_dispatches SET decision = ?1 WHERE repo = 'o/r'",
                params![serde_json::to_string(&prepared).unwrap()],
            )
        })
        .unwrap();
        let incoming = InboxDelivery { repo: "o/r".into(), epoch: "epoch".into(), guid: "incoming".into(), event: "push".into(), body: br#"{"repository":{"full_name":"o/r"},"ref":"refs/heads/main","after":"abc","deleted":false}"#.to_vec(), received_at: 1 };
        b.accept_inbox(&incoming, 10).unwrap();
        assert!(
            jb.mark_dispatch_if_matches(&prepared).is_err(),
            "new inbox event must fence prepared dispatch"
        );
        // Test fixture cleanup only: real protocol must process/resolve input, never delete it.
        b.with(|c| c.execute("DELETE FROM inbox WHERE guid = 'incoming'", []))
            .unwrap();
        jb.mark_dispatch_if_matches(&prepared).unwrap();
        assert!(
            jb.mark_dispatch_if_matches(&prepared).is_err(),
            "transition must be single use"
        );
        let (_, entries) = b.journal("o/r").recover_with_dispatches().unwrap().unwrap();
        assert_eq!(
            entries,
            vec![(prepared.clone(), DispatchStatus::MayHaveSent)]
        );
        assert_eq!(
            b.journal("o/r").dispatch_outbox().unwrap(),
            vec![(prepared.clone(), DispatchStatus::MayHaveSent)]
        );
        b.with(|c| {
            c.execute(
                "UPDATE prepared_dispatches SET status = 'invalid' WHERE repo = 'o/r'",
                [],
            )
        })
        .unwrap();
        assert!(
            b.journal("o/r").recover_with_dispatches().is_err(),
            "invalid outbox status must fail closed"
        );
        b.with(|c| c.execute("UPDATE prepared_dispatches SET status = 'may_have_sent', decision = '{' WHERE repo = 'o/r'", [])).unwrap();
        assert!(
            b.journal("o/r").recover_with_dispatches().is_err(),
            "invalid outbox payload must fail closed"
        );
        b.with(|c| {
            c.execute(
                "UPDATE prepared_dispatches SET decision = ?1 WHERE repo = 'o/r'",
                params![serde_json::to_string(&prepared).unwrap()],
            )
        })
        .unwrap();
        b.with(|c| c.execute("DELETE FROM prepared_dispatches WHERE repo = 'o/r'", []))
            .unwrap();
        assert!(
            b.journal("o/r").recover_with_dispatches().is_err(),
            "legacy pending dispatch with missing outbox must fail closed"
        );
        b.with(|c| c.execute("INSERT INTO prepared_dispatches (repo, session, id, decision, status) VALUES (?1, ?2, ?3, ?4, 'may_have_sent')", params!["o/r", prepared.session, prepared.id as i64, serde_json::to_string(&prepared).unwrap()])).unwrap();
        let mut duplicate = prepared.clone();
        duplicate.session = "session-b".into();
        b.with(|c| c.execute("INSERT INTO prepared_dispatches (repo, session, id, decision, status) VALUES (?1, ?2, ?3, ?4, 'prepared')", params!["o/r", duplicate.session, duplicate.id as i64, serde_json::to_string(&duplicate).unwrap()])).unwrap();
        assert!(
            b.journal("o/r").recover_with_dispatches().is_err(),
            "one pending action cannot authorize multiple outbox entries"
        );
        b.with(|c| {
            c.execute(
                "DELETE FROM prepared_dispatches WHERE repo = 'o/r' AND session = 'session-b'",
                [],
            )
        })
        .unwrap();
        b.with(|c| {
            c.execute(
                "UPDATE prepared_dispatches SET session = 'different' WHERE repo = 'o/r'",
                [],
            )
        })
        .unwrap();
        assert!(
            b.journal("o/r").dispatch_outbox().is_err(),
            "row identity must match stored payload"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn recovery_reads_checkpoint_and_intents_from_one_snapshot() {
        let d = dir("recovery-snapshot");
        let reader = Store::open(&d).unwrap();
        let writer = Store::open(&d).unwrap();
        let mut j = reader.journal("o/r");
        j.checkpoint_with_intents(
            &State {
                counter: 1,
                ..State::default()
            },
            &[(1, Decision::WakeAt { at: 10 })],
        )
        .unwrap();
        let r = j
            .recover_with_read_hook(|| {
                let mut w = writer.journal("o/r");
                w.commit_reply(
                    &State {
                        counter: 2,
                        ..State::default()
                    },
                    &[1],
                    &[(2, Decision::WakeAt { at: 20 })],
                )
                .unwrap();
            })
            .unwrap()
            .unwrap();
        assert_eq!(r.state.counter, 1);
        assert_eq!(r.in_doubt, vec![(1, Decision::WakeAt { at: 10 })]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn recovery_snapshot_never_mixes_old_state_and_new_dispatch_outbox() {
        let d = dir("dispatch-snapshot");
        let reader = Store::open(&d).unwrap();
        let writer = Store::open(&d).unwrap();
        assert!(writer.lease_acquire("o/r", "writer", 100, 130).unwrap());
        let mut j = reader.journal("o/r");
        j.checkpoint(&State {
            counter: 1,
            ..State::default()
        })
        .unwrap();
        let op = schlange_core::actions::Op::Dispatch {
            number: 1,
            workflow: "preview.yml".into(),
            reference: Some("main".into()),
            inputs: BTreeMap::new(),
        };
        let mut next = State {
            counter: 2,
            ..State::default()
        };
        next.pull_requests.insert(
            1,
            schlange_core::model::PullRequest::new(1, "dev", "main", "branch", "head"),
        );
        next.actions.pending.insert(
            9,
            schlange_core::actions::PendingAction {
                rule: "preview".into(),
                op: op.clone(),
                level: None,
                reply: None,
                report: None,
                sent: 0,
                attempts: 1,
                edge: None,
            },
        );
        let entry = PreparedDispatch {
            session: "s".into(),
            id: 7,
            decision: Decision::Act {
                id: 9,
                rule: "preview".into(),
                op,
            },
            head: "head".into(),
            base: "main".into(),
            config_digest: "config".into(),
        };
        let (snapshot, entries, _, _) = j
            .recover_all_with_read_hook(|| {
                writer
                    .journal("o/r")
                    .fenced("writer")
                    .checkpoint_with_prepared_dispatches(&next, &[entry.clone()])
                    .unwrap();
            })
            .unwrap();
        assert_eq!(snapshot.unwrap().state.counter, 1);
        assert!(
            entries.is_empty(),
            "reader must not combine old checkpoint with new outbox"
        );
        let (snapshot, entries) = j.recover_with_dispatches().unwrap().unwrap();
        assert_eq!(snapshot.state.counter, 2);
        assert_eq!(entries, vec![(entry, DispatchStatus::Prepared)]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn reply_checkpoint_and_intent_resolution_are_atomic() {
        let d = dir("reply-resolution");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        let before = State {
            counter: 1,
            ..State::default()
        };
        let after = State {
            counter: 2,
            ..State::default()
        };
        let old = Decision::WakeAt { at: 10 };
        let new = Decision::WakeAt { at: 20 };
        j.checkpoint_with_intents(&before, &[(1, old.clone())])
            .unwrap();
        assert!(
            j.commit_reply(&after, &[1, 99], &[(2, new.clone())])
                .is_err()
        );
        let r = j.recover().unwrap().unwrap();
        assert_eq!(r.state.counter, 1);
        assert_eq!(r.in_doubt, vec![(1, old)]);
        j.commit_reply(&after, &[1], &[(2, new.clone())]).unwrap();
        let r = s.journal("o/r").recover().unwrap().unwrap();
        assert_eq!(r.state.counter, 2);
        assert_eq!(r.in_doubt, vec![(2, new)]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn recovery_rejects_negative_intent_sequence() {
        let d = dir("negative-intent");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        j.checkpoint(&State::default()).unwrap();
        let decision = serde_json::to_string(&Decision::WakeAt { at: 1 }).unwrap();
        s.with(|c| {
            c.execute(
                "INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                params!["o/r", -1, decision],
            )
        })
        .unwrap();
        j.intent(1, &Decision::WakeAt { at: 2 }).unwrap();
        assert!(
            j.recover().is_err(),
            "negative sequence must not be reinterpreted as an unsigned ID"
        );
        assert!(
            j.max_seq().is_err(),
            "a valid maximum must not hide a negative sequence"
        );
        assert!(j.intent(u64::MAX, &Decision::WakeAt { at: 3 }).is_err());
        assert!(j.done(u64::MAX).is_err());
        s.with(|c| c.execute("DELETE FROM intents WHERE repo = 'o/r'", []))
            .unwrap();
        j.intent(7, &Decision::WakeAt { at: 7 }).unwrap();
        j.intent(8, &Decision::WakeAt { at: 8 }).unwrap();
        let decision = serde_json::to_string(&Decision::WakeAt { at: 75 }).unwrap();
        s.with(|c| {
            c.execute(
                "INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                params!["o/r", 7.5_f64, decision],
            )
        })
        .unwrap();
        assert!(
            j.max_seq().is_err(),
            "interior noninteger sequence must be rejected"
        );
        assert!(
            j.recover().is_err(),
            "interior noninteger sequence must fail recovery"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn orphan_intent_does_not_recover_as_default_state() {
        let d = dir("orphan-intent");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        j.intent(1, &Decision::WakeAt { at: 1 }).unwrap();
        assert!(
            j.recover().is_err(),
            "intent without checkpoint is not an empty state"
        );
        assert_eq!(
            j.max_seq().unwrap(),
            1,
            "failed recovery preserves the intent"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn corrupt_checkpoint_and_intent_fail_closed() {
        let d = dir("corrupt-journal");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        j.checkpoint(&State::default()).unwrap();
        s.with(|c| {
            c.execute(
                "UPDATE checkpoints SET state = ?1 WHERE repo = ?2",
                params!["{", "o/r"],
            )
        })
        .unwrap();
        assert!(
            j.recover().is_err(),
            "an unreadable checkpoint must not become empty state"
        );
        j.checkpoint(&State::default()).unwrap();
        s.with(|c| {
            c.execute(
                "INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                params!["o/r", 1, "{"],
            )
        })
        .unwrap();
        assert!(
            j.recover().is_err(),
            "an unreadable intent must not disappear"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn inbox_retains_edited_checkbox_and_atomically_deduplicates_concurrent_claims() {
        use std::sync::{Arc, Barrier};
        let d = dir("durable-inbox");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"schlange[bot]"},"body":"- [x] queue","updated_at":"2027-01-15T08:00:01Z"},"sender":{"login":"alice"}}"#.to_vec();
        let item = InboxDelivery {
            repo: "o/r".into(),
            epoch: "epoch-a".into(),
            guid: "guid".into(),
            event: "issue_comment".into(),
            body,
            received_at: 1,
        };
        let gate = Arc::new(Barrier::new(2));
        let gate_a = gate.clone();
        let one = item.clone();
        let first = std::thread::spawn(move || {
            gate_a.wait();
            a.accept_inbox(&one, 10).unwrap()
        });
        let gate_b = gate.clone();
        let two = item.clone();
        let second = std::thread::spawn(move || {
            gate_b.wait();
            b.accept_inbox(&two, 10).unwrap()
        });
        let results = [first.join().unwrap(), second.join().unwrap()];
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == InboxClaim::Inserted)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|r| **r == InboxClaim::Duplicate)
                .count(),
            1
        );
        let reopened = Store::open(&d).unwrap();
        let pending = reopened.pending_inbox("o/r", "epoch-a").unwrap();
        assert_eq!(pending, vec![item.clone()]);
        let parsed = crate::webhook::parse(&item.event, &pending[0].body).unwrap();
        assert!(parsed.hints.iter().any(|hint| matches!(hint, crate::webhook::Hint::Comment(c) if c.id == 77 && c.sender == "alice" && c.author == "schlange[bot]" && c.edited && c.version == "2027-01-15T08:00:01Z")));
        assert_eq!(
            reopened.accept_inbox(&item, 1).unwrap(),
            InboxClaim::Duplicate,
            "duplicate remains retriable at capacity"
        );
        let mut full = item.clone();
        full.guid = "another".into();
        assert!(
            reopened.accept_inbox(&full, 1).is_err(),
            "capacity must preserve pending records"
        );
        let mut oversized = item.clone();
        oversized.guid = "oversized".into();
        oversized.epoch = "x".repeat(129);
        assert!(
            reopened.accept_inbox(&oversized, 10).is_err(),
            "inbox metadata must be bounded"
        );
        let mut changed_epoch = item.clone();
        changed_epoch.epoch = "epoch-b".into();
        assert!(
            reopened.accept_inbox(&changed_epoch, 10).is_err(),
            "same GUID in another epoch requires explicit transfer"
        );
        let mut changed_event = item.clone();
        changed_event.event = "ping".into();
        assert!(
            reopened.accept_inbox(&changed_event, 10).is_err(),
            "event collision must be rejected"
        );
        let mut changed_repo = item.clone();
        changed_repo.repo = "o/other".into();
        assert!(
            reopened.accept_inbox(&changed_repo, 10).is_err(),
            "body repository must match envelope"
        );
        let mut collision = item;
        collision.body.push(b' ');
        assert!(
            reopened.accept_inbox(&collision, 10).is_err(),
            "same GUID with different payload is not a duplicate"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn inbox_completion_checkpoint_and_new_intents_commit_atomically() {
        let d = dir("inbox-completion");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        let before = State {
            counter: 1,
            ..State::default()
        };
        let after = State {
            counter: 2,
            ..State::default()
        };
        j.checkpoint(&before).unwrap();
        let old = Decision::WakeAt { at: 1 };
        let new = Decision::WakeAt { at: 2 };
        j.intent(1, &old).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        let item = InboxDelivery { repo: "o/r".into(), epoch: "epoch".into(), guid: "guid".into(), event: "push".into(), body: br#"{"repository":{"full_name":"o/r"},"ref":"refs/heads/main","after":"abc","deleted":false}"#.to_vec(), received_at: 1 };
        assert_eq!(s.accept_inbox(&item, 10).unwrap(), InboxClaim::Inserted);
        assert!(
            j.recover().is_err(),
            "legacy recovery must not ignore unprocessed inbox delivery"
        );
        assert!(
            j.commit_inbox_step(&item, rev, &after, &[(1, new.clone())], || 101)
                .is_err(),
            "duplicate intent must roll back the whole step"
        );
        assert_eq!(s.pending_inbox("o/r", "epoch").unwrap(), vec![item.clone()]);
        assert_eq!(
            j.recover_with_revision_hook(|| {})
                .unwrap()
                .0
                .unwrap()
                .state
                .counter,
            1
        );
        j.commit_inbox_step(&item, rev, &after, &[(2, new.clone())], || 101)
            .unwrap();
        assert!(s.pending_inbox("o/r", "epoch").unwrap().is_empty());
        assert_eq!(
            s.accept_inbox(&item, 10).unwrap(),
            InboxClaim::Processed,
            "completed redelivery must not wake replay"
        );
        let recovered = j.recover().unwrap().unwrap();
        assert_eq!(recovered.state.counter, 2);
        assert_eq!(recovered.in_doubt, vec![(1, old), (2, new)]);
        assert!(
            j.commit_inbox_step(&item, rev + 1, &before, &[], || 101)
                .is_err(),
            "processed event cannot be committed twice"
        );
        assert_eq!(j.recover().unwrap().unwrap().state.counter, 2);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn inbox_completion_requires_oldest_pending_and_current_revision_across_epochs() {
        let d = dir("inbox-order-revision");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        let body = br#"{"repository":{"full_name":"o/r"},"ref":"refs/heads/main","after":"abc","deleted":false}"#.to_vec();
        let first = InboxDelivery {
            repo: "o/r".into(),
            epoch: "old".into(),
            guid: "one".into(),
            event: "push".into(),
            body: body.clone(),
            received_at: 1,
        };
        let second = InboxDelivery {
            repo: "o/r".into(),
            epoch: "new".into(),
            guid: "two".into(),
            event: "push".into(),
            body,
            received_at: 2,
        };
        s.accept_inbox(&first, 10).unwrap();
        s.accept_inbox(&second, 10).unwrap();
        let state1 = State {
            counter: 1,
            ..State::default()
        };
        let state2 = State {
            counter: 2,
            ..State::default()
        };
        assert!(
            j.commit_inbox_step(&second, rev, &state2, &[], || 101)
                .is_err(),
            "new epoch cannot skip old pending event"
        );
        j.commit_inbox_step(&first, rev, &state1, &[], || 101)
            .unwrap();
        assert_eq!(s.inbox_revision("o/r").unwrap(), rev + 1);
        assert!(
            j.commit_inbox_step(&second, rev, &state2, &[], || 101)
                .is_err(),
            "stale checkpoint revision cannot commit"
        );
        assert_eq!(
            j.recover_with_revision_hook(|| {})
                .unwrap()
                .0
                .unwrap()
                .state
                .counter,
            1
        );
        j.commit_inbox_step(&second, rev + 1, &state2, &[], || 101)
            .unwrap();
        assert_eq!(s.inbox_revision("o/r").unwrap(), rev + 2);
        assert_eq!(j.recover().unwrap().unwrap().state.counter, 2);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn inbox_recovery_reads_state_and_revision_from_same_snapshot() {
        let d = dir("inbox-revision-snapshot");
        let reader = Store::open(&d).unwrap();
        let writer = Store::open(&d).unwrap();
        let mut j = reader.journal("o/r");
        j.checkpoint(&State {
            counter: 1,
            ..State::default()
        })
        .unwrap();
        let expected = reader.inbox_revision("o/r").unwrap();
        let (snapshot, revision) = j
            .recover_with_revision_hook(|| {
                writer
                    .journal("o/r")
                    .checkpoint(&State {
                        counter: 2,
                        ..State::default()
                    })
                    .unwrap();
            })
            .unwrap();
        assert_eq!(snapshot.unwrap().state.counter, 1);
        assert_eq!(
            revision, expected,
            "old state must not carry a newer revision"
        );
        let (snapshot, revision) = j.recover_with_revision_hook(|| {}).unwrap();
        assert_eq!(snapshot.unwrap().state.counter, 2);
        assert_eq!(revision, expected + 1);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn ordinary_checkpoint_invalidates_stale_inbox_step_revision() {
        let d = dir("inbox-stale-checkpoint");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State {
            counter: 1,
            ..State::default()
        })
        .unwrap();
        let observed = s.inbox_revision("o/r").unwrap();
        let item = InboxDelivery { repo: "o/r".into(), epoch: "epoch".into(), guid: "one".into(), event: "push".into(), body: br#"{"repository":{"full_name":"o/r"},"ref":"refs/heads/main","after":"abc","deleted":false}"#.to_vec(), received_at: 1 };
        s.accept_inbox(&item, 10).unwrap();
        assert!(
            j.checkpoint(&State {
                counter: 2,
                ..State::default()
            })
            .is_err(),
            "legacy checkpoint cannot race a durable inbox"
        );
        assert!(
            j.intent(8, &Decision::WakeAt { at: 8 }).is_err(),
            "legacy intent cannot bypass inbox ordering"
        );
        assert!(
            j.done(8).is_err(),
            "legacy completion cannot bypass inbox ordering"
        );
        assert_eq!(
            j.recover_with_revision_hook(|| {})
                .unwrap()
                .0
                .unwrap()
                .state
                .counter,
            1
        );
        assert_eq!(s.inbox_revision("o/r").unwrap(), observed);
        assert_eq!(s.pending_inbox("o/r", "epoch").unwrap(), vec![item.clone()]);
        j.commit_inbox_step(
            &item,
            observed,
            &State {
                counter: 3,
                ..State::default()
            },
            &[],
            || 101,
        )
        .unwrap();
        assert!(
            j.checkpoint(&State {
                counter: 2,
                ..State::default()
            })
            .is_err(),
            "stale legacy writer cannot overwrite a processed inbox step"
        );
        assert_eq!(j.recover().unwrap().unwrap().state.counter, 3);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn session_build_reply_and_prefix_are_one_fenced_transaction() {
        use schlange_core::Event;
        use schlange_core::model::{CheckRun, Conclusion, PullRequest};
        let d = dir("session-build-reply");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 7)", [])).unwrap();
        let request = Decision::Refresh {
            token: 9,
            numbers: vec![1],
            branches: vec!["main".into()],
        };
        let assigned = j
            .commit_session_step(
                "session-a",
                rev,
                &State::default(),
                &[request.clone()],
                || 101,
            )
            .unwrap();
        let ticket = j
            .capture_session_refresh("session-a", rev + 1, assigned[0].0, &request, 17, || 101)
            .unwrap();
        let mut pr = PullRequest::new(1, "alice", "main", "pr-1", "h1");
        pr.checks = vec![CheckRun {
            name: "ci".into(),
            head_sha: "h1".into(),
            conclusion: Some(Conclusion::Success),
            seq: 1,
            ..Default::default()
        }];
        let snapshot = Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".into(), Some("b0".into()))].into(),
            token: Some(9),
        };
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let committed = j
            .commit_captured_refresh(&ticket, 17, &rules, &snapshot, 5, || 101)
            .unwrap();
        let (seq, build) = committed
            .intents
            .iter()
            .find(|(_, d)| matches!(d, Decision::BuildCar { .. }))
            .unwrap();
        let Decision::BuildCar { car, branch, .. } = build else {
            unreachable!()
        };
        let reply = Event::CarBuilt {
            car: *car,
            branch: branch.clone(),
            head_sha: "head".into(),
            tree: "tree".into(),
            draft: 42,
        };
        let prefix = vec![(1, "tree".to_string())];
        // A separate journal models a worker dying after the durable start but before a write.
        let claim_dir = dir("session-build-claim");
        let claims = Store::open(&claim_dir).unwrap();
        assert!(claims.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut claimant = claims.journal("o/r").fenced("a");
        claimant.checkpoint(&committed.state).unwrap();
        let claim_rev = claims.inbox_revision("o/r").unwrap();
        let encoded = serde_json::to_string(build).unwrap();
        claims.with(|c| {
            c.execute("INSERT INTO status_sequence (repo, session, holder, high_water, fence) VALUES ('o/r', 'session-a', 'a', ?1, 'fence-a')", [*seq as i64])?;
            c.execute("INSERT INTO intents (repo, seq, decision) VALUES ('o/r', ?1, ?2)", params![*seq as i64, encoded])?;
            Ok(())
        }).unwrap();
        assert!(
            claimant
                .claim_session_build("wrong", claim_rev, *seq, build, 17, "fence-a", || 101)
                .is_err()
        );
        assert!(
            claimant
                .claim_session_build("session-a", claim_rev, *seq, build, 17, "wrong", || 101)
                .is_err()
        );
        assert!(
            claimant
                .claim_session_build(
                    "session-a",
                    claim_rev - 1,
                    *seq,
                    build,
                    17,
                    "fence-a",
                    || 101
                )
                .is_err()
        );
        let mut wrong_build = build.clone();
        if let Decision::BuildCar { queue, .. } = &mut wrong_build {
            *queue = "different".into();
        }
        assert!(
            claimant
                .claim_session_build(
                    "session-a",
                    claim_rev,
                    *seq,
                    &wrong_build,
                    17,
                    "fence-a",
                    || 101
                )
                .is_err()
        );
        claims.with(|c| c.execute_batch("CREATE TRIGGER fail_claim BEFORE INSERT ON session_build_claims BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            claimant
                .claim_session_build("session-a", claim_rev, *seq, build, 17, "fence-a", || 101)
                .is_err()
        );
        claims
            .with(|c| c.execute_batch("DROP TRIGGER fail_claim;"))
            .unwrap();
        let build_ticket = claimant
            .claim_session_build("session-a", claim_rev, *seq, build, 17, "fence-a", || 101)
            .unwrap();
        assert!(
            claimant
                .build_ticket_current(&build_ticket, 17, || 101)
                .is_ok()
        );
        claims
            .with(|c| {
                c.execute(
                    "UPDATE session_build_claims SET seq = seq + 1 WHERE repo = 'o/r'",
                    [],
                )
            })
            .unwrap();
        assert!(
            claimant
                .build_ticket_current(&build_ticket, 17, || 101)
                .is_err(),
            "changed durable claim invalidates the ticket"
        );
        claims
            .with(|c| {
                c.execute(
                    "UPDATE session_build_claims SET seq = seq - 1 WHERE repo = 'o/r'",
                    [],
                )
            })
            .unwrap();
        assert!(
            claimant
                .build_ticket_current(&build_ticket, 18, || 101)
                .is_err(),
            "new ingress invalidates the ticket"
        );
        claims
            .with(|c| {
                c.execute(
                    "UPDATE status_sequence SET fence = 'moved' WHERE repo = 'o/r'",
                    [],
                )
            })
            .unwrap();
        assert!(
            claimant
                .build_ticket_current(&build_ticket, 17, || 101)
                .is_err(),
            "changed session fence invalidates the ticket"
        );
        claims
            .with(|c| {
                c.execute(
                    "UPDATE status_sequence SET fence = 'fence-a' WHERE repo = 'o/r'",
                    [],
                )
            })
            .unwrap();
        assert!(
            claimant
                .build_ticket_current(&build_ticket, 17, || 130)
                .is_err(),
            "expired lease invalidates the ticket"
        );
        assert!(
            claimant
                .commit_claimed_build_for_test(
                    &build_ticket,
                    18,
                    &rules,
                    &reply,
                    &prefix,
                    6,
                    || 101
                )
                .is_err(),
            "stale ingress cannot resolve a claim"
        );
        claims.with(|c| c.execute_batch("CREATE TRIGGER fail_claim_completion BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        let failed = claimant
            .commit_claimed_build_for_test(&build_ticket, 17, &rules, &reply, &prefix, 6, || 101)
            .err()
            .unwrap();
        assert!(failed.to_string().contains("injected"), "{failed}");
        let remaining: i64 = claims
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM session_build_claims WHERE repo = 'o/r'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(
            remaining, 1,
            "failed completion must roll back claim deletion"
        );
        assert!(claims.load_car("o/r", *car).unwrap().is_none());
        claims
            .with(|c| c.execute_batch("DROP TRIGGER fail_claim_completion;"))
            .unwrap();
        assert!(claims.lease_acquire("o/complete", "a", 100, 130).unwrap());
        let mut completed = claims.journal("o/complete").fenced("a");
        completed.checkpoint(&committed.state).unwrap();
        let complete_rev = claims.inbox_revision("o/complete").unwrap();
        claims.with(|c| {
            c.execute("INSERT INTO status_sequence (repo, session, holder, high_water, fence) VALUES ('o/complete', 'session-a', 'a', ?1, 'fence-a')", [*seq as i64])?;
            c.execute("INSERT INTO intents (repo, seq, decision) VALUES ('o/complete', ?1, ?2)", params![*seq as i64, serde_json::to_string(build).unwrap()])?;
            Ok(())
        }).unwrap();
        let completion_ticket = completed
            .claim_session_build(
                "session-a",
                complete_rev,
                *seq,
                build,
                17,
                "fence-a",
                || 101,
            )
            .unwrap();
        let completed_reply = completed
            .commit_claimed_build_for_test(
                &completion_ticket,
                17,
                &rules,
                &reply,
                &prefix,
                6,
                || 101,
            )
            .unwrap();
        assert_eq!(
            claims.load_car("o/complete", *car).unwrap(),
            Some(prefix.clone())
        );
        assert_eq!(
            claims.inbox_revision("o/complete").unwrap(),
            completed_reply.revision
        );
        assert!(
            !completed
                .recover()
                .unwrap()
                .unwrap()
                .in_doubt
                .iter()
                .any(|(n, _)| n == seq)
        );
        assert!(
            completed
                .commit_claimed_build_for_test(
                    &completion_ticket,
                    17,
                    &rules,
                    &reply,
                    &prefix,
                    6,
                    || 101
                )
                .is_err(),
            "completion is one-use"
        );
        assert!(
            claimant
                .commit_session_build_reply(
                    "session-a",
                    claim_rev,
                    *seq,
                    build,
                    &rules,
                    &reply,
                    &prefix,
                    6,
                    || 101
                )
                .is_err(),
            "unverified reply cannot clear a started build"
        );
        assert!(
            claimant
                .commit_session_step("session-a", claim_rev, &committed.state, &[], || 101)
                .is_err(),
            "ordinary steps cannot bypass an unknown build"
        );
        assert!(
            claimant
                .claim_session_build("session-a", claim_rev, *seq, build, 17, "fence-a", || 101)
                .is_err(),
            "a started build must not be claimed twice"
        );
        drop(claimant);
        drop(claims);
        let reopened = Store::open(&claim_dir).unwrap();
        let claimant = reopened.journal("o/r").fenced("a");
        assert!(
            claimant
                .claim_session_build("session-a", claim_rev, *seq, build, 17, "fence-a", || 101)
                .is_err(),
            "reopen must not authorize replay"
        );
        assert!(
            claimant.recover().is_err(),
            "ordinary recovery cannot replay an unknown build"
        );
        assert!(claimant.recover_with_dispatches().is_err());
        let unchanged: String = reopened
            .with(|c| {
                c.query_row(
                    "SELECT decision FROM intents WHERE repo = 'o/r' AND seq = ?1",
                    [*seq as i64],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(unchanged, serde_json::to_string(build).unwrap());
        assert!(reopened.lease_acquire("o/r", "b", 131, 190).unwrap());
        reopened.set_meta("fence:o/r", "fence-b").unwrap();
        reopened.set_meta("fence-trust:o/r", "verified").unwrap();
        assert!(
            reopened
                .take_over_status_session("o/r", "b", "session-a", "session-b", || 132)
                .is_err(),
            "session rotation cannot launder an unknown build"
        );
        let successor = reopened.journal("o/r").fenced("b");
        assert!(
            successor
                .claim_session_build("session-a", claim_rev, *seq, build, 17, "fence-a", || 132)
                .is_err(),
            "new holder cannot replay unknown work"
        );
        assert!(
            claimant
                .claim_session_build("session-a", claim_rev, *seq, build, 17, "fence-a", || 132)
                .is_err(),
            "old holder cannot claim after takeover"
        );
        drop(claimant);
        drop(reopened);
        let _ = std::fs::remove_dir_all(claim_dir);
        assert!(
            j.commit_session_build_reply(
                "wrong",
                committed.revision,
                *seq,
                build,
                &rules,
                &reply,
                &prefix,
                6,
                || 101
            )
            .is_err()
        );
        assert!(
            j.commit_session_build_reply(
                "session-a",
                committed.revision - 1,
                *seq,
                build,
                &rules,
                &reply,
                &prefix,
                6,
                || 101
            )
            .is_err()
        );
        assert!(
            j.commit_session_build_reply(
                "session-a",
                committed.revision,
                *seq,
                build,
                &rules,
                &reply,
                &[(1, "wrong-tree".into())],
                6,
                || 101
            )
            .is_err()
        );
        s.with(|c| c.execute_batch("CREATE TRIGGER fail_car BEFORE INSERT ON cars BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            j.commit_session_build_reply(
                "session-a",
                committed.revision,
                *seq,
                build,
                &rules,
                &reply,
                &prefix,
                6,
                || 101
            )
            .is_err()
        );
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, committed.intents);
        assert_eq!(s.inbox_revision("o/r").unwrap(), committed.revision);
        assert!(s.load_car("o/r", *car).unwrap().is_none());
        s.with(|c| c.execute_batch("DROP TRIGGER fail_car;"))
            .unwrap();
        s.with(|c| c.execute_batch("CREATE TRIGGER fail_checkpoint BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(ABORT, 'injected after evidence'); END;")).unwrap();
        assert!(
            j.commit_session_build_reply(
                "session-a",
                committed.revision,
                *seq,
                build,
                &rules,
                &reply,
                &prefix,
                6,
                || 101
            )
            .is_err()
        );
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, committed.intents);
        assert_eq!(s.inbox_revision("o/r").unwrap(), committed.revision);
        assert!(
            s.load_car("o/r", *car).unwrap().is_none(),
            "prefix insertion rolled back"
        );
        s.with(|c| c.execute_batch("DROP TRIGGER fail_checkpoint;"))
            .unwrap();
        let out = j
            .commit_session_build_reply(
                "session-a",
                committed.revision,
                *seq,
                build,
                &rules,
                &reply,
                &prefix,
                6,
                || 101,
            )
            .unwrap();
        assert_eq!(s.load_car("o/r", *car).unwrap(), Some(prefix));
        assert_eq!(s.inbox_revision("o/r").unwrap(), out.revision);
        assert!(
            !j.recover()
                .unwrap()
                .unwrap()
                .in_doubt
                .iter()
                .any(|(n, _)| n == seq)
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn session_refresh_reply_persists_successor_build_before_exposing_it() {
        use schlange_core::Event;
        use schlange_core::model::{CheckRun, Conclusion, PullRequest};
        let d = dir("session-refresh-build");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 7)", [])).unwrap();
        let request = Decision::Refresh {
            token: 9,
            numbers: vec![1],
            branches: vec!["main".into()],
        };
        let assigned = j
            .commit_session_step(
                "session-a",
                rev,
                &State::default(),
                &[request.clone()],
                || 101,
            )
            .unwrap();
        let ticket = j
            .capture_session_refresh("session-a", rev + 1, assigned[0].0, &request, 17, || 101)
            .unwrap();
        let mut pr = PullRequest::new(1, "alice", "main", "pr-1", "h1");
        pr.checks = vec![CheckRun {
            name: "ci".into(),
            head_sha: "h1".into(),
            conclusion: Some(Conclusion::Success),
            seq: 1,
            ..Default::default()
        }];
        let snapshot = Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".into(), Some("b0".into()))].into(),
            token: Some(9),
        };
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let committed = j
            .commit_captured_refresh(&ticket, 17, &rules, &snapshot, 5, || 101)
            .unwrap();
        assert!(
            committed
                .decisions
                .iter()
                .any(|d| matches!(d, Decision::Cosmetic { .. })),
            "mixed cosmetic output must be returned rather than gate the build"
        );
        assert!(
            committed
                .intents
                .iter()
                .any(|(_, d)| matches!(d, Decision::BuildCar { .. }))
        );
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, committed.intents);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn session_refresh_reply_is_atomic_and_requires_exact_request() {
        use schlange_core::Event;
        let d = dir("session-refresh-reply");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 7)", [])).unwrap();
        let request = Decision::Refresh {
            token: 9,
            numbers: vec![],
            branches: vec!["main".into()],
        };
        let second = Decision::Refresh {
            token: 10,
            numbers: vec![],
            branches: vec!["main".into()],
        };
        let assigned = j
            .commit_session_step(
                "session-a",
                rev,
                &State::default(),
                &[request.clone(), second.clone()],
                || 101,
            )
            .unwrap();
        let rules = schlange_config::rules::Rules::default();
        let ticket = j
            .capture_session_refresh("session-a", rev + 1, assigned[0].0, &request, 17, || 101)
            .unwrap();
        let second_reply = Event::Snapshot {
            pull_requests: vec![],
            branches: [("main".to_string(), Some("newer".into()))].into(),
            token: Some(10),
        };
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[1].0,
                &second,
                &rules,
                &second_reply,
                5,
                || 101
            )
            .is_err(),
            "later in-flight request cannot overtake older reply"
        );
        let wrong = Event::Snapshot {
            pull_requests: vec![],
            branches: [("main".to_string(), Some("base".into()))].into(),
            token: Some(10),
        };
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &wrong,
                5,
                || 101
            )
            .is_err()
        );
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, assigned);
        let good = Event::Snapshot {
            pull_requests: vec![],
            branches: [("main".to_string(), Some("base".into()))].into(),
            token: Some(9),
        };
        assert!(
            j.commit_captured_refresh(&ticket, 18, &rules, &good, 5, || 101)
                .is_err(),
            "new ingress must invalidate read ticket"
        );
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, assigned);
        s.with(|c| {
            c.execute(
                "INSERT INTO intents (repo, seq, decision) VALUES ('o/r', 8.5, '{}')",
                [],
            )
        })
        .unwrap();
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 101
            )
            .is_err(),
            "corrupt interior sequence cannot be ignored"
        );
        s.with(|c| {
            c.execute(
                "DELETE FROM intents WHERE repo = 'o/r' AND typeof(seq) = 'real'",
                [],
            )
        })
        .unwrap();
        let missing_branch = Event::Snapshot {
            pull_requests: vec![],
            branches: BTreeMap::new(),
            token: Some(9),
        };
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &missing_branch,
                5,
                || 101
            )
            .is_err()
        );
        assert!(
            j.commit_session_refresh_reply(
                "stale-session",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 101
            )
            .is_err()
        );
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 130
            )
            .is_err()
        );
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, assigned);
        s.with(|c| c.execute_batch("CREATE TRIGGER fail_reply_checkpoint BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 101
            )
            .is_err()
        );
        s.with(|c| c.execute_batch("DROP TRIGGER fail_reply_checkpoint;"))
            .unwrap();
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, assigned);
        s.with(|c| c.execute_batch("CREATE TRIGGER ignore_reply_checkpoint BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 101
            )
            .is_err()
        );
        s.with(|c| c.execute_batch("DROP TRIGGER ignore_reply_checkpoint;"))
            .unwrap();
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, assigned);
        let committed = j
            .commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 101,
            )
            .unwrap();
        assert_eq!(committed.revision, rev + 2);
        assert_eq!(committed.state.branches["main"], "base");
        assert!(committed.intents.is_empty());
        assert!(committed.decisions.is_empty());
        let recovered = j.recover().unwrap().unwrap();
        assert_eq!(recovered.in_doubt, vec![assigned[1].clone()]);
        assert_eq!(recovered.state.branches["main"], "base");
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[1].0,
                &second,
                &rules,
                &second_reply,
                5,
                || 101
            )
            .is_err(),
            "later read captured before predecessor must be discarded"
        );
        let reread = Event::Snapshot {
            pull_requests: vec![],
            branches: [("main".to_string(), Some("fresh".into()))].into(),
            token: Some(10),
        };
        let second_committed = j
            .commit_session_refresh_reply(
                "session-a",
                rev + 2,
                assigned[1].0,
                &second,
                &rules,
                &reread,
                6,
                || 101,
            )
            .unwrap();
        assert_eq!(second_committed.state.branches["main"], "fresh");
        assert!(j.recover().unwrap().unwrap().in_doubt.is_empty());
        assert!(
            j.commit_session_refresh_reply(
                "session-a",
                rev + 1,
                assigned[0].0,
                &request,
                &rules,
                &good,
                5,
                || 101
            )
            .is_err()
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn session_hold_checkpoints_before_reprocessing_inbox_without_acknowledging() {
        let d = dir("session-hold");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        let mut before = State {
            counter: 11,
            ..State::default()
        };
        before.pull_requests.insert(
            1,
            schlange_core::model::PullRequest::new(1, "alice", "main", "pr-1", "h1"),
        );
        before
            .check_identity_targets
            .insert("main".into(), [1].into());
        before.check_identity_epochs.insert("main".into(), 3);
        j.checkpoint(&before).unwrap();
        let revision = s.inbox_revision("o/r").unwrap();
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 7)", [])).unwrap();
        let item = InboxDelivery { repo: "o/r".into(), epoch: "new".into(), guid: "new-status".into(), event: "status".into(), body: br#"{"repository":{"full_name":"o/r"},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#.to_vec(), received_at: 1 };
        s.accept_inbox(&item, 10).unwrap();
        assert!(
            j.commit_session_hold("old-session", revision, || 101)
                .is_err()
        );
        assert!(
            j.commit_session_hold("session-a", revision, || 130)
                .is_err()
        );
        s.with(|c| c.execute_batch("CREATE TRIGGER fail_session_hold BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            j.commit_session_hold("session-a", revision, || 101)
                .is_err()
        );
        s.with(|c| c.execute_batch("DROP TRIGGER fail_session_hold;"))
            .unwrap();
        assert_eq!(s.inbox_revision("o/r").unwrap(), revision);
        assert!(
            !j.pending_status_snapshot()
                .unwrap()
                .unwrap()
                .0
                .state
                .identity_bootstrap_hold
        );
        assert_eq!(
            s.pending_inbox_all_epochs("o/r").unwrap(),
            vec![item.clone()]
        );
        j.commit_session_hold("session-a", revision, || 101)
            .unwrap();
        assert_eq!(s.pending_inbox_all_epochs("o/r").unwrap(), vec![item]);
        let (recovered, next, _) = j.pending_status_snapshot().unwrap().unwrap();
        assert_eq!(next, revision + 1);
        assert!(recovered.state.identity_bootstrap_hold);
        assert_eq!(recovered.state.counter, 11);
        assert_eq!(recovered.state.check_identity_epochs["main"], 4);
        assert!(
            recovered.state.holds[&1]
                .iter()
                .any(|h| h.kind == "identity-bootstrap")
        );
        assert!(
            j.commit_session_hold("session-a", revision, || 101)
                .is_err()
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn session_worker_persists_non_effect_step_and_fences_stale_state() {
        let d = dir("session-worker-read-only");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 7)", [])).unwrap();
        let state = State {
            counter: 4,
            ..State::default()
        };
        assert!(
            j.commit_session_step(
                "session-a",
                rev,
                &state,
                &[Decision::WakeAt { at: 12 }],
                || 101
            )
            .unwrap()
            .is_empty()
        );
        let recovered = j.recover().unwrap().unwrap();
        assert_eq!(recovered.state.counter, 4);
        assert!(recovered.state.wakeups.contains(&12));
        assert!(recovered.in_doubt.is_empty());
        assert!(
            j.commit_session_step("session-a", rev, &State::default(), &[], || 101)
                .is_err()
        );
        assert_eq!(j.recover().unwrap().unwrap().state.counter, 4);
        s.with(|c| {
            c.execute(
                "UPDATE status_sequence SET high_water = ?1 WHERE repo = 'o/r'",
                params![i64::MAX],
            )
        })
        .unwrap();
        let refresh = Decision::Refresh {
            token: 2,
            numbers: vec![1],
            branches: vec![],
        };
        assert!(
            j.commit_session_step("session-a", rev + 1, &state, &[refresh], || 101)
                .is_err(),
            "sequence overflow must not checkpoint"
        );
        let dispatch = Decision::Act {
            id: 1,
            rule: "dispatch".into(),
            op: schlange_core::actions::Op::Dispatch {
                number: 1,
                workflow: "ci.yml".into(),
                reference: None,
                inputs: BTreeMap::new(),
            },
        };
        assert!(
            j.commit_session_step("session-a", rev + 1, &state, &[dispatch], || 101)
                .is_err(),
            "dispatch needs its own outbox protocol"
        );
        assert!(
            j.commit_session_step("session-a", rev + 1, &state, &[], || 130)
                .is_err(),
            "expired holder must not checkpoint"
        );
        assert_eq!(s.inbox_revision("o/r").unwrap(), rev + 1);
        assert_eq!(j.recover().unwrap().unwrap().state.counter, 4);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn session_worker_step_atomically_checkpoints_all_intents_and_revision() {
        let d = dir("session-worker-step");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 7)", [])).unwrap();
        assert!(
            j.checkpoint(&State::default()).is_err(),
            "legacy checkpoint cannot bypass session fencing"
        );
        assert!(
            j.intent(99, &Decision::WakeAt { at: 1 }).is_err(),
            "legacy intent cannot bypass session fencing"
        );
        assert!(
            j.done(99).is_err(),
            "legacy completion cannot bypass session fencing"
        );
        let state = State {
            counter: 9,
            ..State::default()
        };
        let decisions = vec![
            Decision::WakeAt { at: 12 },
            Decision::Refresh {
                token: 8,
                numbers: vec![1],
                branches: vec!["main".into()],
            },
        ];
        assert!(
            j.commit_session_step("stale-session", rev, &state, &decisions, || 101)
                .is_err()
        );
        assert_eq!(s.inbox_revision("o/r").unwrap(), rev);
        s.with(|c| c.execute_batch("CREATE TRIGGER fail_session_intent BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            j.commit_session_step("session-a", rev, &state, &decisions, || 101)
                .is_err()
        );
        s.with(|c| c.execute_batch("DROP TRIGGER fail_session_intent;"))
            .unwrap();
        assert_eq!(s.inbox_revision("o/r").unwrap(), rev);
        assert_eq!(j.recover().unwrap().unwrap().state.counter, 0);
        let assigned = j
            .commit_session_step("session-a", rev, &state, &decisions, || 101)
            .unwrap();
        assert_eq!(assigned, vec![(8, decisions[1].clone())]);
        assert_eq!(s.inbox_revision("o/r").unwrap(), rev + 1);
        let recovered = j.recover().unwrap().unwrap();
        assert_eq!(recovered.state.counter, 9);
        assert!(recovered.state.wakeups.contains(&12));
        assert_eq!(recovered.in_doubt, assigned);
        assert!(
            j.commit_session_step("session-a", rev, &state, &[], || 101)
                .is_err()
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn status_snapshot_commits_all_decisions_before_acknowledging() {
        use schlange_core::engine::Event;
        use schlange_core::model::{CheckRun, Conclusion, PullRequest};
        let d = dir("status-atomic-step");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut j = s.journal("o/r").fenced("a");
        j.checkpoint(&State::default()).unwrap();
        j.intent(1, &Decision::WakeAt { at: 1 }).unwrap();
        let rev = s.inbox_revision("o/r").unwrap();
        let item = InboxDelivery { repo: "o/r".into(), epoch: "old".into(), guid: "status".into(), event: "status".into(), body: br#"{"repository":{"full_name":"o/r"},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#.to_vec(), received_at: 1 };
        s.accept_inbox(&item, 10).unwrap();
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\n    queue_conditions:\n      - check-success = ci\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let head = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let mut pr = PullRequest::new(1, "a", "main", "pr-1", head);
        pr.checks = vec![CheckRun {
            name: "ci".into(),
            head_sha: head.into(),
            conclusion: Some(Conclusion::Success),
            seq: 1,
            ..Default::default()
        }];
        let snapshot = Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".to_string(), Some("b0".to_string()))].into(),
            token: None,
        };
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 101)
                .is_err()
        );
        s.with(|c| c.execute("DELETE FROM intents WHERE repo = 'o/r'", []))
            .unwrap();
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 101)
                .is_err(),
            "missing durable sequence session must hold"
        );
        s.with(|c| c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/r', 'session-a', 'a', 0)", [])).unwrap();
        assert!(
            j.commit_inbox_step(&item, rev, &State::default(), &[], || 101)
                .is_err(),
            "legacy completion must not bypass session allocation"
        );
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "stale-session", || 101)
                .is_err()
        );
        s.with(|c| {
            c.execute(
                "UPDATE status_sequence SET high_water = ?1 WHERE repo = 'o/r'",
                params![i64::MAX],
            )
        })
        .unwrap();
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 101)
                .is_err(),
            "sequence overflow must hold"
        );
        assert_eq!(
            s.pending_inbox_all_epochs("o/r").unwrap(),
            vec![item.clone()]
        );
        s.with(|c| {
            c.execute(
                "UPDATE status_sequence SET high_water = 0 WHERE repo = 'o/r'",
                [],
            )
        })
        .unwrap();
        s.with(|c| c.execute_batch("CREATE TRIGGER fail_status_intent BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 101)
                .is_err()
        );
        s.with(|c| c.execute_batch("DROP TRIGGER fail_status_intent;"))
            .unwrap();
        let before: i64 = s
            .with(|c| {
                c.query_row(
                    "SELECT high_water FROM status_sequence WHERE repo = 'o/r'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(
            before, 0,
            "failed intent insertion rolls back sequence allocation"
        );
        let successor = Store::open(&d).unwrap();
        assert!(successor.lease_acquire("o/r", "b", 131, 160).unwrap());
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 131)
                .is_err(),
            "displaced holder cannot commit"
        );
        let mut successor_journal = successor.journal("o/r").fenced("b");
        assert!(
            successor_journal
                .commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 131)
                .is_err(),
            "successor cannot reuse old holder's session"
        );
        assert!(
            successor
                .take_over_status_session("o/r", "a", "session-a", "stale-attempt", || 131)
                .is_err()
        );
        assert!(
            successor
                .take_over_status_session("o/r", "b", "session-a", "expired-attempt", || 160)
                .is_err()
        );
        assert_eq!(
            successor.pending_inbox_all_epochs("o/r").unwrap(),
            vec![item.clone()]
        );
        successor.set_meta("fence:o/r", "fence-b").unwrap();
        successor.set_meta("fence-trust:o/r", "verified").unwrap();
        assert!(
            successor
                .take_over_status_session("o/r", "b", "wrong-old", "session-b", || 131)
                .is_err()
        );
        successor
            .take_over_status_session("o/r", "b", "session-a", "session-b", || 131)
            .unwrap();
        assert!(
            successor
                .confirm_status_session("o/r", "b", "session-b", "wrong-fence", || 131)
                .is_err()
        );
        successor
            .confirm_status_session("o/r", "b", "session-b", "fence-b", || 131)
            .unwrap();
        assert!(
            successor
                .take_over_status_session("o/r", "b", "session-b", "session-a", || 131)
                .is_err(),
            "old sessions cannot be reused"
        );
        assert!(
            j.commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-a", || 131)
                .is_err()
        );
        assert_eq!(
            s.pending_inbox_all_epochs("o/r").unwrap(),
            vec![item.clone()]
        );
        assert_eq!(
            serde_json::to_value(
                j.recover_with_revision_hook(|| {})
                    .unwrap()
                    .0
                    .unwrap()
                    .state
            )
            .unwrap(),
            serde_json::to_value(State::default()).unwrap()
        );
        let decisions = successor_journal
            .commit_status_snapshot(&item, rev, &rules, &snapshot, 5, "session-b", || 131)
            .unwrap();
        let side: Vec<_> = decisions
            .into_iter()
            .filter(schlange_core::journal::is_side_effect)
            .collect();
        assert!(side.iter().any(|d| matches!(d, Decision::BuildCar { .. })));
        let after: i64 = s
            .with(|c| {
                c.query_row(
                    "SELECT high_water FROM status_sequence WHERE repo = 'o/r'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(after as usize, side.len());
        assert!(s.pending_inbox_all_epochs("o/r").unwrap().is_empty());
        let recovered = j.recover().unwrap().unwrap();
        assert_eq!(
            recovered
                .in_doubt
                .iter()
                .map(|(_, d)| d.clone())
                .collect::<Vec<_>>(),
            side
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn replay_status_produces_live_sha_hint_only() {
        let item = InboxDelivery { repo: "o/r".into(), epoch: "old".into(), guid: "x".into(), event: "status".into(), body: br#"{"repository":{"full_name":"o/r"},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"failure","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#.to_vec(), received_at: 1 };
        let parsed = crate::webhook::parse(&item.event, &item.body).unwrap();
        assert!(parsed.event.is_none());
        assert_eq!(
            item.replay_hint(),
            Some(crate::webhook::Hint::Sha(
                "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".into()
            ))
        );
        assert_eq!(parsed.hints, vec![item.replay_hint().unwrap()]);
        let mut edited = item.clone();
        edited.event = "issue_comment".into();
        assert!(edited.replay_hint().is_none());
    }

    #[test]
    fn replay_status_preserves_exact_signed_identity_and_conclusion() {
        let item = InboxDelivery { repo: "o/r".into(), epoch: "old".into(), guid: "x".into(), event: "status".into(), body: br#"{"repository":{"full_name":"o/r"},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"failure","created_at":"2026-01-01T00:00:00Z","updated_at":"2026-01-01T00:00:00Z"}"#.to_vec(), received_at: 1 };
        let run = item.replay_status().unwrap();
        assert_eq!(run.seq, 42);
        assert_eq!(run.head_sha, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(run.name, "ci");
        assert!(run.status);
        assert_eq!(
            run.conclusion,
            Some(schlange_core::model::Conclusion::Failure)
        );
        let mut invalid = item.clone();
        invalid.body = br#"{"repository":{"full_name":"o/r"},"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success"}"#.to_vec();
        assert!(invalid.replay_status().is_none());
        invalid.body = br#"{"repository":{"full_name":"o/r"},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"unknown"}"#.to_vec();
        assert!(invalid.replay_status().is_none());
        invalid.body = br#"{"repository":{"full_name":"o/r"},"id":42,"sha":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","context":"ci","state":"success"}"#.to_vec();
        assert!(invalid.replay_status().is_none());
    }

    #[test]
    fn inbox_replay_classification_quarantines_commands_and_unknown_types() {
        let item = InboxDelivery { repo: "o/r".into(), epoch: "old".into(), guid: "x".into(), event: "issue_comment".into(), body: br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"bot"},"body":"- [x] queue"},"sender":{"login":"alice"}}"#.to_vec(), received_at: 1 };
        assert_eq!(item.replay_kind(), InboxReplayKind::Hold);
        let mut unknown = item.clone();
        unknown.event = "future_event".into();
        assert_eq!(unknown.replay_kind(), InboxReplayKind::Hold);
        let mut status = item.clone();
        status.event = "status".into();
        status.body =
            br#"{"repository":{"full_name":"o/r"},"sha":"abc","context":"ci","state":"success"}"#
                .to_vec();
        assert_eq!(status.replay_kind(), InboxReplayKind::Reconcile);
        status.body = br#"{"repository":{"full_name":"o/r"},"sha":"abc","context":"ci","state":"success","issue":{"number":5},"comment":{"body":"- [x] queue"}}"#.to_vec();
        assert_eq!(status.replay_kind(), InboxReplayKind::Hold);
        status.body = b"{}".to_vec();
        assert_eq!(status.replay_kind(), InboxReplayKind::Hold);
    }

    #[test]
    fn successor_reads_pending_across_epochs_without_relabeling() {
        let d = dir("inbox-successor-epochs");
        let s = Store::open(&d).unwrap();
        let body = br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"bot"},"body":"- [x] queue"},"sender":{"login":"alice"}}"#.to_vec();
        let old = InboxDelivery {
            repo: "o/r".into(),
            epoch: "old".into(),
            guid: "old-guid".into(),
            event: "issue_comment".into(),
            body: body.clone(),
            received_at: 10,
        };
        let new = InboxDelivery {
            repo: "o/r".into(),
            epoch: "new".into(),
            guid: "new-guid".into(),
            event: "issue_comment".into(),
            body,
            received_at: 11,
        };
        s.accept_inbox(&old, 10).unwrap();
        s.accept_inbox(&new, 10).unwrap();
        drop(s);
        let successor = Store::open(&d).unwrap();
        assert_eq!(
            successor.pending_inbox_all_epochs("o/r").unwrap(),
            vec![old, new]
        );
        successor
            .with(|c| c.execute("UPDATE inbox SET processed = 2 WHERE guid = 'old-guid'", []))
            .unwrap();
        assert!(successor.pending_inbox_all_epochs("o/r").is_err());
        successor
            .with(|c| {
                c.execute(
                    "UPDATE inbox SET processed = 0, body = x'7b' WHERE guid = 'old-guid'",
                    [],
                )
            })
            .unwrap();
        assert!(successor.pending_inbox_all_epochs("o/r").is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn concurrent_boundary_and_intake_have_one_winner() {
        use std::sync::{Arc, Barrier};
        let d = dir("adoption-inbox-race");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "h", 1, 100).unwrap());
        let boundary = AdoptionBoundary {
            epoch: "e".into(),
            fence: "f".into(),
            cutoff: 1,
        };
        let item = InboxDelivery {
            repo: "o/r".into(), epoch: "initial".into(), guid: "race".into(), event: "issue_comment".into(),
            body: br#"{"action":"edited","repository":{"full_name":"o/r"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"bot"},"body":"- [x] queue"},"sender":{"login":"alice"}}"#.to_vec(), received_at: 1,
        };
        let gate = Arc::new(Barrier::new(2));
        let other_gate = gate.clone();
        let delivery = item.clone();
        let intake = std::thread::spawn(move || {
            other_gate.wait();
            b.accept_inbox(&delivery, 10).is_ok()
        });
        gate.wait();
        let adopted = a
            .create_provisional_adoption("o/r", "h", || 2, &boundary)
            .is_ok();
        let accepted = intake.join().unwrap();
        assert_ne!(adopted, accepted);
        assert_eq!(a.provisional_adoption("o/r").unwrap().is_some(), adopted);
        assert_eq!(
            a.pending_inbox("o/r", "initial").unwrap(),
            if accepted { vec![item] } else { vec![] }
        );
        let unrelated = InboxDelivery { repo: "o/other".into(), guid: "other".into(), body: br#"{"action":"edited","repository":{"full_name":"o/other"},"issue":{"number":5,"pull_request":{}},"comment":{"id":77,"user":{"login":"bot"},"body":"- [x] queue"},"sender":{"login":"alice"}}"#.to_vec(), epoch: "initial".into(), event: "issue_comment".into(), received_at: 1 };
        assert_eq!(
            a.accept_inbox(&unrelated, 10).unwrap(),
            InboxClaim::Inserted
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn provisional_boundary_and_unbound_inbox_cannot_coexist() {
        let d = dir("adoption-inbox-exclusion");
        let s = Store::open(&d).unwrap();
        let boundary = AdoptionBoundary {
            epoch: "e".into(),
            fence: "f".into(),
            cutoff: 1,
        };
        let item = |repo: &str, guid: &str| {
            InboxDelivery {
            repo: repo.into(), epoch: "initial".into(), guid: guid.into(), event: "issue_comment".into(),
            body: format!(r#"{{"action":"edited","repository":{{"full_name":"{repo}"}},"issue":{{"number":5,"pull_request":{{}}}},"comment":{{"id":77,"user":{{"login":"bot"}},"body":"- [x] queue"}},"sender":{{"login":"alice"}}}}"#).into_bytes(), received_at: 1,
        }
        };
        assert!(s.lease_acquire("o/a", "h", 1, 100).unwrap());
        s.accept_inbox(&item("o/a", "a"), 10).unwrap();
        assert!(
            s.create_provisional_adoption("o/a", "h", || 2, &boundary)
                .is_err()
        );
        assert!(s.lease_acquire("o/b", "h", 1, 100).unwrap());
        s.create_provisional_adoption("o/b", "h", || 2, &boundary)
            .unwrap();
        assert!(s.accept_inbox(&item("o/b", "b"), 10).is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn provisional_adoption_boundary_is_fenced_durable_and_conservative() {
        let d = dir("provisional-adoption");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        let boundary = AdoptionBoundary {
            epoch: "epoch-a".into(),
            fence: "fence-a".into(),
            cutoff: 1_800_000_010,
        };
        a.create_provisional_adoption("o/r", "a", || 101, &boundary)
            .unwrap();
        assert_eq!(
            b.provisional_adoption("o/r").unwrap(),
            Some(boundary.clone())
        );
        assert!(
            b.record_delivery("after-boundary", 1, 100).is_err(),
            "legacy unscoped dedup must not persist during provisional adoption"
        );
        let mut legacy_writer = a.journal("o/r").fenced("a");
        assert!(
            legacy_writer.recover().is_err(),
            "legacy recovery must not interpret provisional epoch as fresh"
        );
        assert!(
            legacy_writer.checkpoint(&State::default()).is_err(),
            "legacy checkpoint cannot write after boundary"
        );
        assert!(
            legacy_writer
                .intent(1, &Decision::WakeAt { at: 1 })
                .is_err(),
            "legacy intent cannot write after boundary"
        );
        assert!(
            legacy_writer.done(1).is_err(),
            "legacy completion cannot write after boundary"
        );
        assert!(
            a.create_provisional_adoption("o/r", "a", || 102, &boundary)
                .is_err(),
            "boundary cannot be reset"
        );
        assert_eq!(
            boundary.classify_created_at(Some("2027-01-15T08:00:00Z")),
            AdoptionAge::BeforeOrEqual
        );
        assert_eq!(
            boundary.classify_created_at(Some("2027-01-15T08:00:10Z")),
            AdoptionAge::BeforeOrEqual
        );
        assert_eq!(
            boundary.classify_created_at(Some("2027-01-15T08:00:11Z")),
            AdoptionAge::TimestampAfter
        );
        assert_eq!(boundary.classify_created_at(None), AdoptionAge::Unknown);
        assert_eq!(
            boundary.classify_created_at(Some("2027-02-31T08:00:11Z")),
            AdoptionAge::Unknown
        );
        assert_eq!(
            boundary.classify_created_at(Some("2027-01-15T08:00:11+00:00")),
            AdoptionAge::Unknown
        );
        assert!(b.lease_acquire("o/r", "b", 131, 190).unwrap());
        assert!(
            a.create_provisional_adoption("o/other", "a", || 132, &boundary)
                .is_err(),
            "lease must cover repository"
        );
        assert_eq!(
            b.provisional_adoption("o/r").unwrap(),
            Some(boundary.clone())
        );
        assert!(b.lease_acquire("o/expired", "b", 100, 130).unwrap());
        assert!(
            b.create_provisional_adoption("o/expired", "b", || 130, &boundary)
                .is_err(),
            "expired lease cannot persist a boundary"
        );
        assert!(b.lease_acquire("o/legacy", "b", 100, 130).unwrap());
        let mut old = b.journal("o/legacy");
        old.checkpoint(&State {
            counter: 7,
            ..State::default()
        })
        .unwrap();
        assert!(
            b.create_provisional_adoption("o/legacy", "b", || 101, &boundary)
                .is_err(),
            "legacy checkpoint is never overwritten"
        );
        assert_eq!(old.recover().unwrap().unwrap().state.counter, 7);
        assert!(b.lease_acquire("o/prior-lineage", "b", 100, 130).unwrap());
        b.set_meta("fence-trust:o/prior-lineage", "verified")
            .unwrap();
        assert!(
            b.create_provisional_adoption("o/prior-lineage", "b", || 101, &boundary)
                .is_err(),
            "existing trusted lineage cannot be treated as a fresh epoch"
        );
        assert!(a.lease_acquire("o/displaced", "a", 100, 130).unwrap());
        assert!(b.lease_acquire("o/displaced", "b", 131, 190).unwrap());
        let error = a
            .create_provisional_adoption("o/displaced", "a", || 132, &boundary)
            .unwrap_err();
        assert!(error.to_string().contains(LEASE_LOST), "{error}");
        assert!(b.provisional_adoption("o/displaced").unwrap().is_none());
        for repo in ["o/orphan-build", "o/orphan-session", "o/orphan-history"] {
            assert!(b.lease_acquire(repo, "b", 100, 130).unwrap());
        }
        b.with(|c| {
            c.execute("INSERT INTO session_build_claims VALUES ('o/orphan-build', 'b', 's', 0, 1, 0, 'f', '{}')", [])?;
            c.execute("INSERT INTO status_sequence (repo, session, holder, high_water) VALUES ('o/orphan-session', 's', 'b', 0)", [])?;
            c.execute("INSERT INTO status_session_history VALUES ('o/orphan-history', 's')", [])?;
            Ok(())
        }).unwrap();
        for repo in ["o/orphan-build", "o/orphan-session", "o/orphan-history"] {
            assert!(
                b.create_provisional_adoption(repo, "b", || 101, &boundary)
                    .is_err(),
                "orphan evidence must block adoption: {repo}"
            );
        }
        assert!(b.lease_acquire("o/deliveries", "b", 100, 130).unwrap());
        b.with(|c| {
            c.execute(
                "INSERT INTO deliveries (guid, at) VALUES ('previous-delivery', 1)",
                [],
            )
        })
        .unwrap();
        assert!(
            b.create_provisional_adoption("o/deliveries", "b", || 101, &boundary)
                .is_err(),
            "unscoped legacy delivery history cannot be assumed empty"
        );
        b.with(|c| {
            c.execute(
                "UPDATE adoption_boundaries SET epoch = '' WHERE repo = 'o/r'",
                [],
            )
        })
        .unwrap();
        assert!(
            b.provisional_adoption("o/r").is_err(),
            "corrupt boundary is not usable"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn adoption_boundary_and_legacy_dedup_cannot_both_win_race() {
        use std::sync::{Arc, Barrier};
        let d = dir("adoption-dedup-race");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        let gate = Arc::new(Barrier::new(2));
        let first_gate = gate.clone();
        let first = std::thread::spawn(move || {
            let boundary = AdoptionBoundary {
                epoch: "epoch".into(),
                fence: "fence".into(),
                cutoff: 1_800_000_000,
            };
            first_gate.wait();
            a.create_provisional_adoption("o/r", "a", || 101, &boundary)
                .is_ok()
        });
        let second_gate = gate.clone();
        let second = std::thread::spawn(move || {
            second_gate.wait();
            b.record_delivery("guid", 1, 100).is_ok()
        });
        let won_boundary = first.join().unwrap();
        let won_delivery = second.join().unwrap();
        assert_ne!(won_boundary, won_delivery, "one must fail closed");
        let s = Store::open(&d).unwrap();
        assert_eq!(
            s.provisional_adoption("o/r").unwrap().is_some(),
            won_boundary
        );
        assert_eq!(s.delivery_seen("guid").unwrap(), won_delivery);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn adoption_boundary_and_legacy_checkpoint_cannot_both_win_race() {
        use std::sync::{Arc, Barrier};
        let d = dir("adoption-writer-race");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        let barrier = Arc::new(Barrier::new(2));
        let gate = barrier.clone();
        let first = std::thread::spawn(move || {
            let boundary = AdoptionBoundary {
                epoch: "epoch".into(),
                fence: "fence".into(),
                cutoff: 1_800_000_000,
            };
            gate.wait();
            a.create_provisional_adoption("o/r", "a", || 101, &boundary)
                .is_ok()
        });
        let gate = barrier.clone();
        let second = std::thread::spawn(move || {
            gate.wait();
            b.journal("o/r")
                .fenced("a")
                .checkpoint(&State::default())
                .is_ok()
        });
        let won_boundary = first.join().unwrap();
        let won_checkpoint = second.join().unwrap();
        assert_ne!(
            won_boundary, won_checkpoint,
            "exactly one generation must win"
        );
        let s = Store::open(&d).unwrap();
        assert_eq!(
            s.provisional_adoption("o/r").unwrap().is_some(),
            won_boundary
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn stale_holder_cannot_advance_trusted_lineage_metadata() {
        let d = dir("stale-lineage-writer");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        a.set_meta("fence:o/r", "old").unwrap();
        a.set_meta("fence-trust:o/r", "verified").unwrap();
        assert!(b.lease_acquire("o/r", "b", 131, 190).unwrap());
        assert!(
            a.advance_fence_lineage("o/r", "a", || 132, Some("old"), "stale")
                .is_err(),
            "old holder must not advance lineage"
        );
        assert_eq!(b.meta("fence:o/r").unwrap().as_deref(), Some("old"));
        assert!(
            b.advance_fence_lineage(
                "o/r",
                "b",
                || {
                    let probe = rusqlite::Connection::open(d.join(DB_FILE)).unwrap();
                    probe.busy_timeout(std::time::Duration::ZERO).unwrap();
                    assert!(
                        probe.execute("BEGIN IMMEDIATE", []).is_err(),
                        "clock must be sampled after acquiring the write transaction"
                    );
                    191
                },
                Some("old"),
                "expired"
            )
            .is_err(),
            "expired holder must not advance lineage"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn lost_holder_cannot_delete_successor_intent_or_replace_it() {
        let d = dir("lost-holder-intent");
        let a = Store::open(&d).unwrap();
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut ja = a.journal("o/r").fenced("a");
        ja.checkpoint(&State::default()).unwrap();
        assert!(b.lease_acquire("o/r", "b", 131, 190).unwrap());
        let mut jb = b.journal("o/r").fenced("b");
        let successor = Decision::WakeAt { at: 22 };
        jb.intent(1, &successor).unwrap();
        assert!(
            jb.intent(1, &Decision::WakeAt { at: 99 }).is_err(),
            "duplicate intent must not replace original"
        );
        assert!(
            ja.done(1).is_err(),
            "old holder cannot delete successor intent"
        );
        assert_eq!(
            jb.recover().unwrap().unwrap().in_doubt,
            vec![(1, successor)]
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn one_lease_per_repository_and_the_journal_of_a_lost_lease_refuses_writes() {
        let d = dir("lease");
        let a = Store::open(&d).unwrap();
        // A second process on the same data directory.
        let b = Store::open(&d).unwrap();
        assert!(a.lease_acquire("o/r", "a", 100, 130).unwrap());
        assert!(!b.lease_acquire("o/r", "b", 110, 140).unwrap(), "held by a");
        assert!(a.lease_acquire("o/r", "a", 120, 150).unwrap(), "renewal");
        assert!(
            b.lease_acquire("o/other", "b", 120, 150).unwrap(),
            "per repository"
        );
        let mut ja = a.journal("o/r").fenced("a");
        ja.checkpoint(&State::default()).unwrap();
        // a stops renewing; b takes over once it expired.
        assert!(!b.lease_acquire("o/r", "b", 149, 179).unwrap());
        assert!(b.lease_acquire("o/r", "b", 150, 180).unwrap());
        assert_eq!(b.lease_holder("o/r").unwrap(), Some(("b".into(), 180)));
        let e = ja.checkpoint(&State::default()).unwrap_err();
        assert!(e.to_string().contains(LEASE_LOST), "{e}");
        let e = ja.intent(1, &Decision::WakeAt { at: 1 }).unwrap_err();
        assert!(e.to_string().contains(LEASE_LOST), "{e}");
        let mut jb = b.journal("o/r").fenced("b");
        jb.checkpoint(&State::default()).unwrap();
        b.lease_release("o/r", "b").unwrap();
        assert!(a.lease_acquire("o/r", "a", 151, 181).unwrap());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn events_filter_by_pull_request_time_and_page() {
        let d = dir("events");
        let s = Store::open(&d).unwrap();
        for (i, prs) in [vec![1], vec![2, 3], vec![1, 3], vec![]]
            .into_iter()
            .enumerate()
        {
            s.add_event(&EventRecord {
                id: 0,
                repository: "o/r".into(),
                at: 100 + i as u64,
                kind: if prs.is_empty() {
                    "queue.pause.create".into()
                } else {
                    "action.queue.enter".into()
                },
                pull_request: prs.first().copied(),
                pull_requests: prs,
                base_ref: Some("main".into()),
                trigger: "test".into(),
                metadata: serde_json::json!({ "i": i }),
            })
            .unwrap();
        }
        let q = |pr, since, before| EventQuery {
            pr,
            since,
            before,
            kind: None,
            limit: 100,
        };
        let ids = |v: Vec<EventRecord>| v.into_iter().map(|e| e.id).collect::<Vec<_>>();
        assert_eq!(
            ids(s.events("o/r", &q(None, None, None)).unwrap()),
            vec![4, 3, 2, 1]
        );
        assert_eq!(
            ids(s.events("o/r", &q(Some(3), None, None)).unwrap()),
            vec![3, 2]
        );
        assert_eq!(
            ids(s.events("o/r", &q(Some(1), Some(102), None)).unwrap()),
            vec![3]
        );
        assert_eq!(
            ids(s.events("o/r", &q(None, None, Some(3))).unwrap()),
            vec![2, 1]
        );
        assert!(s.events("o/x", &q(None, None, None)).unwrap().is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn an_unreadable_snapshot_keeps_the_history_and_deliveries_are_bounded() {
        let d = dir("compat");
        let s = Store::open(&d).unwrap();
        let mut state = State::default();
        state
            .history
            .insert("q@main".into(), vec![Sample { at: 1, secs: 61 }]);
        let mut j = s.journal("o/r");
        j.checkpoint(&state).unwrap();
        s.with(|c| {
            c.execute(
                "UPDATE checkpoints SET state = '{\"not\": \"a state\"' WHERE repo = 'o/r'",
                [],
            )
        })
        .unwrap();
        assert!(j.recover().is_err());
        assert_eq!(s.history("o/r").unwrap(), state.history);
        for i in 0..20 {
            s.record_delivery(&format!("g{i}"), i, 5).unwrap();
        }
        assert!(!s.delivery_seen("g0").unwrap());
        assert!(s.delivery_seen("g19").unwrap());
        let _ = std::fs::remove_dir_all(d);
    }
}
