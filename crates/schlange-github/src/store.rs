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
    #[error("database {path} has an invalid schema version")]
    InvalidVersion { path: PathBuf },
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
                 repo TEXT PRIMARY KEY, holder TEXT NOT NULL, expires INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS build_claims (
                 repo TEXT NOT NULL, seq INTEGER NOT NULL, session TEXT NOT NULL,
                 epoch INTEGER NOT NULL, fence TEXT NOT NULL, decision TEXT NOT NULL,
                 checkpoint TEXT NOT NULL, stage TEXT NOT NULL,
                 PRIMARY KEY (repo, seq));
             CREATE TRIGGER IF NOT EXISTS claim_checkpoint_insert BEFORE INSERT ON checkpoints
             WHEN EXISTS(SELECT 1 FROM build_claims WHERE repo = NEW.repo)
             BEGIN SELECT RAISE(ABORT, 'unresolved build claim'); END;
             CREATE TRIGGER IF NOT EXISTS claim_checkpoint_update BEFORE UPDATE ON checkpoints
             WHEN EXISTS(SELECT 1 FROM build_claims WHERE repo = OLD.repo OR repo = NEW.repo)
             BEGIN SELECT RAISE(ABORT, 'unresolved build claim'); END;
             CREATE TRIGGER IF NOT EXISTS claim_checkpoint_delete BEFORE DELETE ON checkpoints
             WHEN EXISTS(SELECT 1 FROM build_claims WHERE repo = OLD.repo)
             BEGIN SELECT RAISE(ABORT, 'unresolved build claim'); END;
             CREATE TRIGGER IF NOT EXISTS claim_intent_insert BEFORE INSERT ON intents
             WHEN EXISTS(SELECT 1 FROM build_claims WHERE repo = NEW.repo)
             BEGIN SELECT RAISE(ABORT, 'unresolved build claim'); END;
             CREATE TRIGGER IF NOT EXISTS claim_intent_update BEFORE UPDATE ON intents
             WHEN EXISTS(SELECT 1 FROM build_claims WHERE repo = OLD.repo OR repo = NEW.repo)
             BEGIN SELECT RAISE(ABORT, 'unresolved build claim'); END;
             CREATE TRIGGER IF NOT EXISTS claim_intent_delete BEFORE DELETE ON intents
             WHEN EXISTS(SELECT 1 FROM build_claims WHERE repo = OLD.repo)
             BEGIN SELECT RAISE(ABORT, 'unresolved build claim'); END;",
        )
        .map_err(&e)?;
        let version: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .optional()
            .map_err(&e)?;
        let version = version
            .map(|v| {
                v.parse::<i64>()
                    .map_err(|_| StoreError::InvalidVersion { path: path.clone() })
            })
            .transpose()?;
        match version {
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
            c.execute(
                "INSERT OR IGNORE INTO deliveries (guid, at) VALUES (?1, ?2)",
                params![guid, at as i64],
            )?;
            c.execute(
                "DELETE FROM deliveries WHERE rowid NOT IN
                   (SELECT rowid FROM deliveries ORDER BY at DESC, rowid DESC LIMIT ?1)",
                params![keep as i64],
            )?;
            Ok(())
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

/// An unresolved BuildCar claim. It is not authorization to send or recover by itself.
#[derive(Debug)]
pub struct BuildClaim {
    sequence: u64,
}
impl BuildClaim {
    pub fn sequence(&self) -> u64 {
        self.sequence
    }
}

fn reject_build_claim(tx: &rusqlite::Transaction<'_>, repo: &str) -> rusqlite::Result<()> {
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM build_claims WHERE repo=?1)",
        [repo],
        |r| r.get(0),
    )?;
    if exists {
        return Err(rusqlite::Error::ToSqlConversionFailure(
            "unresolved build claim".into(),
        ));
    }
    Ok(())
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

impl SqliteJournal {
    #[cfg(test)]
    fn prepare_engine_build_step(
        &self,
        session: &str,
        revision: i64,
        predecessor: &State,
        rules: &schlange_config::rules::Rules,
        event: &schlange_core::Event,
        clock: &dyn crate::time::Clock,
    ) -> Result<Vec<Decision>, JournalError> {
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("a fenced holder is required".into()))?;
        let next_revision = revision
            .checked_add(1)
            .ok_or_else(|| JournalError::Storage("revision overflow".into()))?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = i64::try_from(clock.now()).map_err(|_| rusqlite::Error::ToSqlConversionFailure("time out of range".into()))?;
            check_holder(&tx, &self.repo, Some(holder))?;
            reject_build_claim(&tx, &self.repo)?;
            let expiry: i64 = tx.query_row("SELECT expires FROM lease WHERE repo=?1 AND holder=?2", params![self.repo, holder], |r| r.get(0))?;
            if expiry <= now { return Err(rusqlite::Error::ToSqlConversionFailure("lease expired".into())); }
            let valid: bool = tx.query_row("SELECT EXISTS(SELECT 1 FROM test_build_session WHERE repo=?1 AND session=?2 AND revision=?3) AND NOT EXISTS(SELECT 1 FROM intents WHERE repo=?1)", params![self.repo, session, revision], |r| r.get(0))?;
            if !valid { return Err(rusqlite::Error::ToSqlConversionFailure("session, revision or intents changed".into())); }
            let high_water: i64 = tx.query_row("SELECT high_water FROM test_build_session WHERE repo=?1 AND session=?2 AND revision=?3", params![self.repo, session, revision], |r| r.get(0))?;
            let next_sequence = high_water.checked_add(1).filter(|_| high_water >= 0).ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("invalid or exhausted sequence".into()))?;
            let text: String = tx.query_row("SELECT state FROM checkpoints WHERE repo=?1", [&self.repo], |r| r.get(0))?;
            let expected = serde_json::to_string(predecessor).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if text != expected { return Err(rusqlite::Error::ToSqlConversionFailure("checkpoint changed".into())); }
            let old: State = serde_json::from_str(&text).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let (next, decisions) = schlange_core::step(&old, rules, event, now as u64);
            let mut effects = decisions.iter().filter(|d| schlange_core::journal::is_side_effect(d));
            let build = effects.next().filter(|d| matches!(d, Decision::BuildCar { .. })).ok_or_else(|| rusqlite::Error::ToSqlConversionFailure("expected BuildCar".into()))?;
            if effects.next().is_some() { return Err(rusqlite::Error::ToSqlConversionFailure("multiple side effects".into())); }
            let next = serde_json::to_string(&next).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            let build = serde_json::to_string(build).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            if tx.execute("UPDATE test_build_session SET revision=?1, high_water=?2 WHERE repo=?3 AND session=?4 AND revision=?5 AND high_water=?6", params![next_revision, next_sequence, self.repo, session, revision, high_water])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("revision changed".into())); }
            if tx.execute("UPDATE checkpoints SET state=?1, written=?2 WHERE repo=?3", params![next, now, self.repo])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("checkpoint not updated".into())); }
            if tx.execute("INSERT INTO intents VALUES (?1, ?2, ?3)", params![self.repo, next_sequence, build])? != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("intent not inserted".into())); }
            tx.commit()?;
            Ok(decisions)
        }).map_err(storage)
    }

    /// Persist a single-build step atomically. The caller must pass the complete decisions from
    /// the same engine step. This experimental primitive does not authorize network writes.
    #[cfg(test)]
    fn prepare_build_step(
        &self,
        seq: u64,
        state: &State,
        decisions: &[Decision],
        clock: &dyn crate::time::Clock,
    ) -> Result<(), JournalError> {
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("a fenced holder is required".into()))?;
        let mut effects = decisions
            .iter()
            .filter(|d| schlange_core::journal::is_side_effect(d));
        let build = effects
            .next()
            .filter(|d| matches!(d, Decision::BuildCar { .. }))
            .ok_or_else(|| JournalError::Storage("expected a single BuildCar".into()))?;
        if effects.next().is_some() {
            return Err(JournalError::Storage(
                "other side effects require recovery".into(),
            ));
        }
        let sequence = i64::try_from(seq)
            .map_err(|_| JournalError::Storage("sequence out of range".into()))?;
        let state = serde_json::to_string(state)?;
        let build = serde_json::to_string(build)?;
        self.store
            .with(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                let now = i64::try_from(clock.now()).map_err(|_| {
                    rusqlite::Error::ToSqlConversionFailure("time out of range".into())
                })?;
                check_holder(&tx, &self.repo, Some(holder))?;
                reject_build_claim(&tx, &self.repo)?;
                let expiry: i64 = tx.query_row(
                    "SELECT expires FROM lease WHERE repo=?1 AND holder=?2",
                    params![self.repo, holder],
                    |r| r.get(0),
                )?;
                if expiry <= now {
                    return Err(rusqlite::Error::ToSqlConversionFailure(
                        "lease expired".into(),
                    ));
                }
                let pending: bool = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM intents WHERE repo=?1)",
                    [&self.repo],
                    |r| r.get(0),
                )?;
                if pending {
                    return Err(rusqlite::Error::ToSqlConversionFailure(
                        "other unresolved intents".into(),
                    ));
                }
                tx.execute(
                    "INSERT OR REPLACE INTO checkpoints (repo,state,written) VALUES (?1,?2,?3)",
                    params![self.repo, state, now],
                )?;
                tx.execute(
                    "INSERT INTO intents (repo,seq,decision) VALUES (?1,?2,?3)",
                    params![self.repo, sequence, build],
                )?;
                tx.commit()
            })
            .map_err(storage)
    }

    /// Create one durable, still-unknown BuildCar claim. Remote lease/ingress proof and a
    /// session incarnation must be established by the worker; this method alone authorizes no
    /// network request and no restart recovery.
    pub fn claim_build(
        &self,
        seq: u64,
        session: &str,
        epoch: u64,
        fence: &str,
        decision: &Decision,
        expected: &State,
        clock: &dyn crate::time::Clock,
    ) -> Result<BuildClaim, JournalError> {
        let holder = self
            .holder
            .as_deref()
            .ok_or_else(|| JournalError::Storage("a fenced holder is required".into()))?;
        if session.len() != 32
            || !session.bytes().all(|b| b.is_ascii_hexdigit())
            || fence.len() != 40
            || !fence.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(JournalError::Storage("invalid session or fence".into()));
        }
        let Decision::BuildCar {
            car,
            queue,
            base_branch,
            branch,
            base_sha,
            prs,
            own,
            stack_on,
        } = decision
        else {
            return Err(JournalError::Storage("claim is not BuildCar".into()));
        };
        let matches = expected.trains.get(base_branch).is_some_and(|train| {
            train.cars.iter().any(|c| {
                c.id == *car
                    && c.queue == *queue
                    && c.branch.as_ref() == Some(branch)
                    && c.base_sha == *base_sha
                    && c.prs == *prs
                    && c.own == *own
                    && c.stacked_on == *stack_on
                    && matches!(c.status, schlange_core::queue::CarStatus::Building { .. })
            })
        });
        if !matches {
            return Err(JournalError::Storage(
                "BuildCar does not match checkpoint".into(),
            ));
        }
        let sequence = i64::try_from(seq)
            .map_err(|_| JournalError::Storage("sequence out of range".into()))?;
        let epoch =
            i64::try_from(epoch).map_err(|_| JournalError::Storage("epoch out of range".into()))?;
        let decision_text = serde_json::to_string(decision)?;
        let state_text = serde_json::to_string(expected)?;
        self.store.with(|c| {
            let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
            let now = i64::try_from(clock.now()).map_err(|_| rusqlite::Error::ToSqlConversionFailure("time out of range".into()))?;
            check_holder(&tx, &self.repo, Some(holder))?;
            reject_build_claim(&tx, &self.repo)?;
            let count: i64 = tx.query_row("SELECT COUNT(*) FROM intents WHERE repo=?1", [&self.repo], |r| r.get(0))?;
            if count != 1 { return Err(rusqlite::Error::ToSqlConversionFailure("other unresolved intents".into())); }
            let expiry: i64 = tx.query_row("SELECT expires FROM lease WHERE repo=?1 AND holder=?2", params![self.repo, holder], |r| r.get(0))?;
            if expiry <= now { return Err(rusqlite::Error::ToSqlConversionFailure("lease expired".into())); }
            let stored: String = tx.query_row("SELECT state FROM checkpoints WHERE repo=?1", [&self.repo], |r| r.get(0))?;
            let intent: String = tx.query_row("SELECT decision FROM intents WHERE repo=?1 AND seq=?2", params![self.repo, sequence], |r| r.get(0))?;
            if stored != state_text || intent != decision_text {
                return Err(rusqlite::Error::ToSqlConversionFailure("checkpoint or intent changed".into()));
            }
            tx.execute("INSERT INTO build_claims (repo,seq,session,epoch,fence,decision,checkpoint,stage) VALUES (?1,?2,?3,?4,?5,?6,?7,'unknown')",
                params![self.repo, sequence, session, epoch, fence, decision_text, state_text])?;
            tx.commit()
        }).map_err(storage)?;
        Ok(BuildClaim { sequence: seq })
    }
    /// Writes only while `holder` holds the repository's lease.
    pub fn fenced(mut self, holder: &str) -> Self {
        self.holder = Some(holder.to_string());
        self
    }

    /// The highest intent sequence number on record (to continue numbering after a restart).
    pub fn max_seq(&self) -> Result<u64, JournalError> {
        let (invalid, max): (bool, i64) = self.store
            .with(|c| {
                c.query_row(
                    "SELECT EXISTS(SELECT 1 FROM intents WHERE repo = ?1 AND (typeof(seq) != 'integer' OR seq < 0)), COALESCE(MAX(CASE WHEN typeof(seq) = 'integer' THEN seq END), 0) FROM intents WHERE repo = ?1",
                    params![self.repo],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .map_err(storage)?;
        if invalid {
            return Err(JournalError::Storage("invalid intent sequence".into()));
        }
        u64::try_from(max).map_err(|_| JournalError::Storage("negative intent sequence".into()))
    }
}

impl Journal for SqliteJournal {
    fn checkpoint(&mut self, state: &State) -> Result<(), JournalError> {
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
                reject_build_claim(&tx, &repo)?;
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
                tx.commit()
            })
            .map_err(storage)?;
        if write_history {
            self.history_written = Some(history);
        }
        Ok(())
    }

    fn intent(&mut self, seq: u64, decision: &Decision) -> Result<(), JournalError> {
        let seq = i64::try_from(seq)
            .map_err(|_| JournalError::Storage("intent sequence out of range".into()))?;
        let text = serde_json::to_string(decision)?;
        let holder = self.holder.clone();
        self.store
            .with(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                check_holder(&tx, &self.repo, holder.as_deref())?;
                reject_build_claim(&tx, &self.repo)?;
                tx.execute(
                    "INSERT INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                    params![self.repo, seq, text],
                )?;
                tx.commit()
            })
            .map_err(storage)
    }

    fn done(&mut self, seq: u64) -> Result<(), JournalError> {
        let seq = i64::try_from(seq)
            .map_err(|_| JournalError::Storage("intent sequence out of range".into()))?;
        let holder = self.holder.clone();
        self.store
            .with(|c| {
                let tx = c.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
                check_holder(&tx, &self.repo, holder.as_deref())?;
                reject_build_claim(&tx, &self.repo)?;
                tx.execute(
                    "DELETE FROM intents WHERE repo = ?1 AND seq = ?2",
                    params![self.repo, seq],
                )?;
                tx.commit()
            })
            .map_err(storage)
    }

    fn recover(&self) -> Result<Option<Recovered>, JournalError> {
        let (text, intents, history) = self
            .store
            .with(|c| {
                let tx = c.transaction()?;
                reject_build_claim(&tx, &self.repo)?;
                let text: Option<String> = tx
                    .query_row(
                        "SELECT state FROM checkpoints WHERE repo = ?1",
                        params![self.repo],
                        |r| r.get(0),
                    )
                    .optional()?;
                let intents: Vec<(i64, String)> = {
                    let mut st = tx.prepare(
                        "SELECT seq, decision FROM intents WHERE repo = ?1 ORDER BY seq",
                    )?;
                    let rows = st.query_map(params![self.repo], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?;
                    rows.collect::<rusqlite::Result<_>>()?
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
                tx.commit()?;
                Ok((text, intents, history))
            })
            .map_err(storage)?;
        let state = match text {
            Some(t) => Some(serde_json::from_str::<State>(&t)?),
            None if !intents.is_empty() => {
                return Err(JournalError::Storage(
                    "pending intents have no checkpoint".into(),
                ));
            }
            None if history.is_empty() && intents.is_empty() => None,
            None => Some(State {
                history,
                ..State::default()
            }),
        };
        let Some(state) = state else {
            return Ok(None);
        };
        let in_doubt = intents
            .into_iter()
            .map(|(seq, d)| {
                let seq = u64::try_from(seq)
                    .map_err(|_| JournalError::Storage("negative intent sequence".into()))?;
                Ok((seq, serde_json::from_str(&d)?))
            })
            .collect::<Result<Vec<_>, JournalError>>()?;
        Ok(Some(Recovered { state, in_doubt }))
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
    fn build_engine_step_requires_same_session_and_revision() {
        let d = dir("build-session-cas");
        let store = Store::open(&d).unwrap();
        assert!(store.lease_acquire("o/r", "a", 10, 100).unwrap());
        let journal = store.journal("o/r").fenced("a");
        let initial = State::default();
        store.with(|c| {
            c.execute_batch("CREATE TABLE test_build_session (repo TEXT PRIMARY KEY, session TEXT NOT NULL, revision INTEGER NOT NULL, high_water INTEGER NOT NULL);")?;
            c.execute("INSERT INTO test_build_session VALUES ('o/r', 'session-a', 4, 41)", [])?;
            c.execute("INSERT INTO checkpoints VALUES (?1,?2,10)", params!["o/r", serde_json::to_string(&initial).unwrap()])?;
            Ok(())
        }).unwrap();
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let pr = schlange_core::model::PullRequest::new(1, "a", "main", "p", "head");
        let event = schlange_core::Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".into(), Some("base".into()))].into_iter().collect(),
            token: None,
        };
        let clock = crate::time::ManualClock::new(10);
        let mut legacy = store.journal("o/r").fenced("a");
        let mut newer = State::default();
        newer.pull_requests.insert(
            99,
            schlange_core::model::PullRequest::new(99, "b", "main", "other", "new-head"),
        );
        legacy.checkpoint(&newer).unwrap();
        assert!(
            journal
                .prepare_engine_build_step("session-a", 4, &initial, &rules, &event, &clock)
                .is_err()
        );
        legacy.checkpoint(&initial).unwrap();
        assert!(
            journal
                .prepare_engine_build_step("other", 4, &initial, &rules, &event, &clock)
                .is_err()
        );
        assert!(
            journal
                .prepare_engine_build_step("session-a", 3, &initial, &rules, &event, &clock)
                .is_err()
        );
        store.with(|c| c.execute_batch("CREATE TRIGGER fail_cas_intent BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            journal
                .prepare_engine_build_step("session-a", 4, &initial, &rules, &event, &clock)
                .is_err()
        );
        let revision: i64 = store
            .with(|c| {
                c.query_row(
                    "SELECT revision FROM test_build_session WHERE repo = 'o/r'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(revision, 4);
        assert_eq!(
            journal
                .recover()
                .unwrap()
                .unwrap()
                .state
                .pull_requests
                .len(),
            0
        );
        store
            .with(|c| c.execute_batch("DROP TRIGGER fail_cas_intent"))
            .unwrap();
        store.with(|c| c.execute_batch("CREATE TRIGGER ignore_checkpoint BEFORE UPDATE ON checkpoints BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            journal
                .prepare_engine_build_step("session-a", 4, &initial, &rules, &event, &clock)
                .is_err()
        );
        assert!(journal.recover().unwrap().unwrap().in_doubt.is_empty());
        store
            .with(|c| c.execute_batch("DROP TRIGGER ignore_checkpoint"))
            .unwrap();
        store.with(|c| c.execute_batch("CREATE TRIGGER ignore_intent BEFORE INSERT ON intents BEGIN SELECT RAISE(IGNORE); END;")).unwrap();
        assert!(
            journal
                .prepare_engine_build_step("session-a", 4, &initial, &rules, &event, &clock)
                .is_err()
        );
        assert!(journal.recover().unwrap().unwrap().in_doubt.is_empty());
        store
            .with(|c| c.execute_batch("DROP TRIGGER ignore_intent"))
            .unwrap();
        let (expected, decisions) = schlange_core::step(&initial, &rules, &event, 10);
        let emitted = journal
            .prepare_engine_build_step("session-a", 4, &initial, &rules, &event, &clock)
            .unwrap();
        assert_eq!(emitted, decisions);
        let recovered = journal.recover().unwrap().unwrap();
        assert_eq!(
            serde_json::to_vec(&recovered.state).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        assert_eq!(
            recovered.in_doubt,
            vec![(
                42,
                decisions
                    .into_iter()
                    .find(|d| matches!(d, Decision::BuildCar { .. }))
                    .unwrap()
            )]
        );
        let revision: i64 = store
            .with(|c| {
                c.query_row(
                    "SELECT revision FROM test_build_session WHERE repo = 'o/r'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(revision, 5);
    }

    #[test]
    fn build_step_and_intent_commit_together_or_not_at_all() {
        let d = dir("atomic-build-step");
        let store = Store::open(&d).unwrap();
        assert!(store.lease_acquire("o/r", "a", 10, 100).unwrap());
        let journal = store.journal("o/r").fenced("a");
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let pr = schlange_core::model::PullRequest::new(1, "a", "main", "p", "head");
        let event = schlange_core::Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".into(), Some("base".into()))].into_iter().collect(),
            token: None,
        };
        let (state, decisions) = schlange_core::step(&State::default(), &rules, &event, 10);
        let mut mixed = decisions.clone();
        mixed.push(Decision::Refresh {
            token: 7,
            numbers: vec![1],
            branches: vec!["main".into()],
        });
        let clock = crate::time::ManualClock::new(10);
        assert!(
            journal
                .prepare_build_step(1, &state, &mixed, &clock)
                .is_err()
        );
        assert!(
            store
                .journal("o/r")
                .prepare_build_step(1, &state, &decisions, &clock)
                .is_err()
        );
        assert!(journal.recover().unwrap().is_none());
        store.with(|c| c.execute_batch("CREATE TRIGGER fail_build_intent BEFORE INSERT ON intents BEGIN SELECT RAISE(ABORT, 'injected'); END;")).unwrap();
        assert!(
            journal
                .prepare_build_step(1, &state, &decisions, &clock)
                .is_err()
        );
        assert!(journal.recover().unwrap().is_none());
        store
            .with(|c| c.execute_batch("DROP TRIGGER fail_build_intent"))
            .unwrap();
        let expected = decisions
            .iter()
            .find(|d| matches!(d, Decision::BuildCar { .. }))
            .unwrap();
        journal
            .prepare_build_step(1, &state, &decisions, &clock)
            .unwrap();
        let recovered = journal.recover().unwrap().unwrap();
        assert_eq!(
            serde_json::to_vec(&recovered.state).unwrap(),
            serde_json::to_vec(&state).unwrap()
        );
        assert_eq!(recovered.in_doubt, vec![(1, expected.clone())]);
    }

    #[test]
    fn build_claim_is_bound_to_exact_intent_and_live_holder() {
        struct LockCheckingClock(Mutex<Connection>);
        impl crate::time::Clock for LockCheckingClock {
            fn now(&self) -> u64 {
                let c = self.0.lock().unwrap();
                let error = c.execute_batch("BEGIN IMMEDIATE").unwrap_err();
                assert!(
                    matches!(error, rusqlite::Error::SqliteFailure(ref code, _) if code.code == rusqlite::ErrorCode::DatabaseBusy),
                    "expected the claim's write lock, got {error}"
                );
                10
            }
        }
        let d = dir("build-claim");
        let store = Store::open(&d).unwrap();
        assert!(store.lease_acquire("o/r", "a", 10, 100).unwrap());
        let mut journal = store.journal("o/r").fenced("a");
        let rules = schlange_config::rules::Rules::from_yaml("queue_rules:\n  - name: default\npull_request_rules:\n  - name: q\n    conditions: []\n    actions:\n      queue:\n").unwrap();
        let pr = schlange_core::model::PullRequest::new(1, "a", "main", "p", "head");
        let snapshot = schlange_core::Event::Snapshot {
            pull_requests: vec![pr],
            branches: [("main".into(), Some("base".into()))].into_iter().collect(),
            token: None,
        };
        let (state, decisions) = schlange_core::step(&State::default(), &rules, &snapshot, 10);
        journal.checkpoint(&state).unwrap();
        let decision = decisions
            .into_iter()
            .find(|d| matches!(d, Decision::BuildCar { .. }))
            .unwrap();
        journal.intent(1, &decision).unwrap();
        let mut wrong = decision.clone();
        if let Decision::BuildCar { branch, .. } = &mut wrong {
            *branch = "queue/other".into();
        }
        let session = "a".repeat(32);
        let fence = "f".repeat(40);
        let old_writer = Connection::open(d.join(DB_FILE)).unwrap();
        let probe = Connection::open(d.join(DB_FILE)).unwrap();
        probe.busy_timeout(std::time::Duration::ZERO).unwrap();
        let clock = LockCheckingClock(Mutex::new(probe));
        journal.intent(2, &decision).unwrap();
        assert!(
            journal
                .claim_build(
                    1,
                    &session,
                    3,
                    &fence,
                    &decision,
                    &state,
                    &crate::time::ManualClock::new(10)
                )
                .is_err(),
            "other unresolved operations require recovery first"
        );
        journal.done(2).unwrap();
        assert!(
            journal
                .claim_build(
                    1,
                    &session,
                    3,
                    &fence,
                    &wrong,
                    &state,
                    &crate::time::ManualClock::new(10)
                )
                .is_err()
        );
        assert!(
            journal
                .claim_build(
                    1,
                    &session,
                    3,
                    &fence,
                    &decision,
                    &state,
                    &crate::time::ManualClock::new(100)
                )
                .is_err()
        );
        let claim = journal
            .claim_build(1, &session, 3, &fence, &decision, &state, &clock)
            .unwrap();
        assert!(
            journal
                .claim_build(
                    1,
                    &session,
                    3,
                    &fence,
                    &decision,
                    &state,
                    &crate::time::ManualClock::new(10)
                )
                .is_err()
        );
        assert_eq!(claim.sequence(), 1);
        assert!(journal.checkpoint(&state).is_err());
        assert!(journal.intent(2, &decision).is_err());
        assert!(journal.done(1).is_err());
        assert!(
            old_writer
                .execute("DELETE FROM intents WHERE repo = 'o/r' AND seq = 1", [])
                .is_err(),
            "a previously opened legacy writer must not erase the claim's intent"
        );
        assert!(
            old_writer
                .execute("UPDATE checkpoints SET state = '{}' WHERE repo = 'o/r'", [])
                .is_err(),
            "a legacy writer must not replace the claimed checkpoint"
        );
        for sql in [
            "INSERT OR REPLACE INTO checkpoints VALUES ('o/r', '{}', 0)",
            "DELETE FROM checkpoints WHERE repo = 'o/r'",
            "INSERT OR REPLACE INTO intents VALUES ('o/r', 1, '{}')",
            "UPDATE intents SET seq = 9 WHERE repo = 'o/r'",
            "UPDATE intents SET repo = 'o/other' WHERE repo = 'o/r'",
        ] {
            assert!(old_writer.execute(sql, []).is_err(), "must reject {sql}");
        }
        old_writer
            .execute("INSERT INTO checkpoints VALUES ('o/other', '{}', 0)", [])
            .unwrap();
        assert!(store.lease_acquire("o/r", "b", 101, 200).unwrap());
        assert!(
            journal
                .claim_build(
                    1,
                    &"b".repeat(32),
                    4,
                    &fence,
                    &decision,
                    &state,
                    &crate::time::ManualClock::new(101)
                )
                .is_err()
        );
        assert!(
            journal.recover().is_err(),
            "an unresolved claim cannot enter legacy recovery"
        );
        drop(journal);
        drop(store);
        let reopened = Store::open(&d).unwrap();
        assert!(reopened.journal("o/r").fenced("b").recover().is_err());
        let stored: (String, String) = reopened
            .with(|c| {
                c.query_row(
                    "SELECT stage, decision FROM build_claims WHERE repo = ?1 AND seq = 1",
                    ["o/r"],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
            })
            .unwrap();
        assert_eq!(stored.0, "unknown");
        assert_eq!(
            serde_json::from_str::<Decision>(&stored.1).unwrap(),
            decision
        );
    }

    #[test]
    fn corrupt_journal_records_are_not_silently_discarded() {
        let d = dir("corrupt-records");
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
            "corrupt checkpoint must not become empty state"
        );
        let stored: String = s
            .with(|c| {
                c.query_row(
                    "SELECT state FROM checkpoints WHERE repo = ?1",
                    params!["o/r"],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(stored, "{");
        j.checkpoint(&State::default()).unwrap();
        j.intent(1, &Decision::WakeAt { at: 99 }).unwrap();
        j.intent(2, &Decision::WakeAt { at: 100 }).unwrap();
        s.with(|c| {
            c.execute(
                "UPDATE intents SET decision = ?1 WHERE repo = ?2 AND seq = 2",
                params!["{", "o/r"],
            )
        })
        .unwrap();
        assert!(j.recover().is_err(), "corrupt intent must not disappear");
        let stored: String = s
            .with(|c| {
                c.query_row(
                    "SELECT decision FROM intents WHERE repo = ?1 AND seq = 2",
                    params!["o/r"],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(stored, "{");
        j.done(2).unwrap();
        s.with(|c| c.execute("DELETE FROM checkpoints WHERE repo = ?1", params!["o/r"]))
            .unwrap();
        assert!(
            j.recover().is_err(),
            "intents without their checkpoint must not be replayed against empty state"
        );
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn malformed_schema_version_is_not_rewritten() {
        let d = dir("invalid-schema-version");
        {
            let s = Store::open(&d).unwrap();
            s.with(|c| c.execute("UPDATE meta SET value = 'invalid' WHERE key = 'schema'", []))
                .unwrap();
        }
        assert!(matches!(
            Store::open(&d),
            Err(StoreError::InvalidVersion { .. })
        ));
        let c = Connection::open(d.join("schlange.db")).unwrap();
        let version: String = c
            .query_row("SELECT value FROM meta WHERE key = 'schema'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(version, "invalid");
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn invalid_intent_sequences_fail_without_changing_rows() {
        let d = dir("invalid-intent-seq");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        j.checkpoint(&State::default()).unwrap();
        let original = Decision::WakeAt { at: 99 };
        j.intent(7, &original).unwrap();
        let text = serde_json::to_string(&original).unwrap();
        s.with(|c| {
            c.execute(
                "INSERT INTO intents (repo, seq, decision) VALUES (?1, -1, ?2)",
                params!["o/r", text],
            )
        })
        .unwrap();
        assert!(j.recover().is_err(), "negative sequence must fail recovery");
        assert!(
            j.max_seq().is_err(),
            "a positive maximum must not hide a negative sequence"
        );
        assert!(
            j.done(u64::MAX).is_err(),
            "unsigned overflow must not delete the negative row"
        );
        assert!(
            j.intent(u64::MAX, &original).is_err(),
            "unsigned overflow must not insert"
        );
        let count: i64 = s
            .with(|c| {
                c.query_row(
                    "SELECT COUNT(*) FROM intents WHERE repo = ?1",
                    params!["o/r"],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(count, 2);
        s.with(|c| {
            c.execute(
                "DELETE FROM intents WHERE repo = ?1 AND seq = -1",
                params!["o/r"],
            )
        })
        .unwrap();
        j.intent(i64::MAX as u64, &original).unwrap();
        assert_eq!(j.max_seq().unwrap(), i64::MAX as u64);
        assert_eq!(j.recover().unwrap().unwrap().in_doubt.len(), 2);
        assert!(j.intent(i64::MAX as u64 + 1, &original).is_err());
        s.with(|c| {
            c.execute(
                "INSERT INTO intents (repo, seq, decision) VALUES (?1, 7.5, ?2)",
                params!["o/r", text],
            )
        })
        .unwrap();
        assert!(
            j.max_seq().is_err(),
            "an interior non-integer sequence must not be hidden by valid extrema"
        );
        assert!(j.recover().is_err());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn lost_holder_cannot_complete_successors_intent() {
        let d = dir("intent-holder");
        let s = Store::open(&d).unwrap();
        assert!(s.lease_acquire("o/r", "a", 100, 130).unwrap());
        let mut a = s.journal("o/r").fenced("a");
        a.checkpoint(&State::default()).unwrap();
        assert!(s.lease_acquire("o/r", "b", 130, 160).unwrap());
        let mut b = s.journal("o/r").fenced("b");
        let decision = Decision::WakeAt { at: 99 };
        b.intent(1, &decision).unwrap();
        assert!(
            a.done(1).is_err(),
            "old holder cannot delete successor's intent"
        );
        assert_eq!(b.recover().unwrap().unwrap().in_doubt, vec![(1, decision)]);
        b.done(1).unwrap();
        assert!(b.recover().unwrap().unwrap().in_doubt.is_empty());
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn duplicate_intent_does_not_replace_existing_decision() {
        let d = dir("intent-duplicate");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        j.checkpoint(&State::default()).unwrap();
        let original = Decision::WakeAt { at: 99 };
        j.intent(1, &original).unwrap();
        assert!(j.intent(1, &Decision::WakeAt { at: 100 }).is_err());
        assert_eq!(j.recover().unwrap().unwrap().in_doubt, vec![(1, original)]);
        let _ = std::fs::remove_dir_all(d);
    }

    #[test]
    fn journal_recovers_older_state_with_defaulted_fields() {
        let d = dir("legacy-state");
        let s = Store::open(&d).unwrap();
        let mut j = s.journal("o/r");
        j.checkpoint(&State::default()).unwrap();
        let mut value = serde_json::to_value(State::default()).unwrap();
        value.as_object_mut().unwrap().remove("seen_comments");
        let text = serde_json::to_string(&value).unwrap();
        s.with(|c| {
            c.execute(
                "UPDATE checkpoints SET state = ?1 WHERE repo = ?2",
                params![text, "o/r"],
            )
        })
        .unwrap();
        assert!(j.recover().unwrap().unwrap().state.seen_comments.is_empty());
        let _ = std::fs::remove_dir_all(d);
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
    fn an_unreadable_snapshot_is_rejected_and_history_is_preserved() {
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
