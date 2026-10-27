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
                 landed_tree TEXT NOT NULL, landed_parent TEXT NOT NULL, note TEXT NOT NULL);",
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

/// The [`Journal`] of one repository in the database.
pub struct SqliteJournal {
    store: Store,
    repo: String,
    /// The history as last written (rewritten only when it changes).
    history_written: Option<String>,
}

impl SqliteJournal {
    /// The highest intent sequence number on record (to continue numbering after a restart).
    pub fn max_seq(&self) -> Result<u64, JournalError> {
        self.store
            .with(|c| {
                c.query_row(
                    "SELECT COALESCE(MAX(seq), 0) FROM intents WHERE repo = ?1",
                    params![self.repo],
                    |r| r.get::<_, i64>(0),
                )
            })
            .map(|n| n as u64)
            .map_err(storage)
    }
}

impl Journal for SqliteJournal {
    fn checkpoint(&mut self, state: &State) -> Result<(), JournalError> {
        let text = serde_json::to_string(state)?;
        let history = serde_json::to_string(&state.history)?;
        let write_history = self.history_written.as_deref() != Some(history.as_str());
        let now = crate::time::unix_now() as i64;
        let repo = self.repo.clone();
        self.store
            .with(|c| {
                let tx = c.transaction()?;
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
        let text = serde_json::to_string(decision)?;
        self.store
            .with(|c| {
                c.execute(
                    "INSERT OR REPLACE INTO intents (repo, seq, decision) VALUES (?1, ?2, ?3)",
                    params![self.repo, seq as i64, text],
                )
            })
            .map(|_| ())
            .map_err(storage)
    }

    fn done(&mut self, seq: u64) -> Result<(), JournalError> {
        self.store
            .with(|c| {
                c.execute(
                    "DELETE FROM intents WHERE repo = ?1 AND seq = ?2",
                    params![self.repo, seq as i64],
                )
            })
            .map(|_| ())
            .map_err(storage)
    }

    fn recover(&self) -> Result<Option<Recovered>, JournalError> {
        let text: Option<String> = self
            .store
            .with(|c| {
                c.query_row(
                    "SELECT state FROM checkpoints WHERE repo = ?1",
                    params![self.repo],
                    |r| r.get(0),
                )
                .optional()
            })
            .map_err(storage)?;
        let intents: Vec<(u64, String)> = self
            .store
            .with(|c| {
                let mut st =
                    c.prepare("SELECT seq, decision FROM intents WHERE repo = ?1 ORDER BY seq")?;
                let rows = st.query_map(params![self.repo], |r| {
                    Ok((r.get::<_, i64>(0)? as u64, r.get::<_, String>(1)?))
                })?;
                rows.collect()
            })
            .map_err(storage)?;
        let history = self.store.history(&self.repo).map_err(storage)?;
        let state = match text {
            Some(t) => match serde_json::from_str::<State>(&t) {
                Ok(s) => Some(s),
                // A snapshot this binary cannot read (an older format): start from GitHub's
                // state, but keep the runtime history.
                Err(_) => Some(State {
                    history: history.clone(),
                    ..State::default()
                }),
            },
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
            .filter_map(|(seq, d)| serde_json::from_str(&d).ok().map(|d| (seq, d)))
            .collect();
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
        let r = j.recover().unwrap().unwrap();
        assert_eq!(r.state.history, state.history);
        assert!(r.state.queued.is_empty());
        for i in 0..20 {
            s.record_delivery(&format!("g{i}"), i, 5).unwrap();
        }
        assert!(!s.delivery_seen("g0").unwrap());
        assert!(s.delivery_seen("g19").unwrap());
        let _ = std::fs::remove_dir_all(d);
    }
}
