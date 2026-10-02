//! SQLite manifest: devices, device folders, backup jobs and per-file records.
//!
//! The manifest is an **index of known backup state**, never the only copy of the
//! user's files (plan §9). Rows are therefore only ever added or updated — a file
//! that disappears from the phone keeps its row.
//!
//! Concurrency: `rusqlite::Connection` is `Send` but not `Sync`, so it lives
//! behind a `Mutex` inside [`Db`]. One process, one writer, WAL mode: repository
//! functions take `&Db` and lock per operation.

pub mod devices;
pub mod files;
pub mod jobs;
mod migrations;
mod quarantine;

pub use quarantine::{open_or_quarantine, QuarantineReport};

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};
use std::time::Duration;

use rusqlite::Connection;

pub use migrations::SCHEMA_VERSION;

/// How long [`Db::open_file`] keeps starting over while another instance holds
/// the write lock.
///
/// The contention it exists for is a *migration* (milliseconds), so this is
/// generous; the point of the bound is that a genuinely stuck lock surfaces as an
/// error instead of hanging the app at startup forever.
const OPEN_DEADLINE: Duration = Duration::from_secs(15);

/// Sleep between open attempts. Short, because the other side is one DDL batch.
const OPEN_BACKOFF: Duration = Duration::from_millis(25);

/// Errors from manifest storage.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DbError {
    #[error("database error: {0}")]
    Sql(#[from] rusqlite::Error),

    #[error("i/o error accessing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "this database was written by a newer version of the app (schema {found}, supported {supported}); \
         refusing to touch it"
    )]
    VersionSkew { found: u32, supported: u32 },

    #[error("illegal job status transition from {from} to {to}")]
    IllegalTransition { from: String, to: String },

    #[error("no {what} found for {key}")]
    NotFound { what: &'static str, key: String },
}

impl DbError {
    /// True when SQLite is telling us the file is not a database at all, or is
    /// damaged beyond reading.
    ///
    /// This is deliberately narrow: a schema that is *newer* than this build
    /// ([`DbError::VersionSkew`]) is not corruption, and neither is a permission
    /// problem. Only these two codes mean "the bytes on disk are not a SQLite
    /// database", which is the one case where starting over is better than
    /// refusing to start.
    pub fn is_corrupt(&self) -> bool {
        match self {
            Self::Sql(err) => matches!(
                err.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseCorrupt)
                    | Some(rusqlite::ErrorCode::NotADatabase)
            ),
            _ => false,
        }
    }

    /// True when SQLite refused because *another* connection holds a lock.
    ///
    /// `SQLITE_BUSY` and `SQLITE_LOCKED` are the two ways that happens, and both
    /// are temporary by definition — the other side is a second instance of this
    /// app migrating the same fresh manifest, and it will be done in
    /// milliseconds. [`Db::open_file`] retries on exactly this.
    pub fn is_busy(&self) -> bool {
        match self {
            Self::Sql(err) => matches!(
                err.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy) | Some(rusqlite::ErrorCode::DatabaseLocked)
            ),
            _ => false,
        }
    }

    /// What to tell the user when [`DbError::is_corrupt`] is true.
    /// What to tell the user when `original` could not be read and has been
    /// moved to `stash`.
    ///
    /// Deliberately concrete: the file name is the one thing the user needs in
    /// order to decide whether to keep, repair or delete it (§13).
    pub fn repair_guidance(original: &Path, stash: &Path) -> String {
        format!(
            "The backup manifest at {} could not be read, so the app has started \
             with an empty one.\n\n\
             The unreadable file has been kept as {} — nothing was thrown away, \
             and it can be repaired or deleted whenever you like.\n\n\
             Your backed-up files are not affected: the manifest only records \
             what has already been copied. The next backup will re-copy the \
             files it no longer has a record of.",
            original.display(),
            stash.display()
        )
    }
}

/// Handle to the manifest database.
pub struct Db {
    conn: Mutex<Connection>,
}

impl std::fmt::Debug for Db {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Db").finish_non_exhaustive()
    }
}

impl Db {
    /// Open an in-memory database (tests).
    pub fn open_in_memory() -> Result<Self, DbError> {
        let conn = Connection::open_in_memory()?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.apply_pragmas(false)?;
        db.migrate()?;
        Ok(db)
    }

    /// Open (creating if needed) a database file.
    ///
    /// Two instances starting at the same moment both want the write lock: the
    /// app and, say, a second copy of it, or a recovery helper. `busy_timeout`
    /// covers most of that, but not the paths where SQLite deliberately does
    /// *not* consult the busy handler — switching the journal mode to WAL needs
    /// exclusive access, and a lock upgrade aborts rather than waits. Those come
    /// back as `SQLITE_BUSY` immediately, so the answer is to throw the
    /// connection away and start over; the whole attempt is bounded by
    /// [`OPEN_DEADLINE`].
    pub fn open_file(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| DbError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        let deadline = std::time::Instant::now() + OPEN_DEADLINE;
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            let db = Self {
                conn: Mutex::new(Connection::open(path)?),
            };
            match db.apply_pragmas(true).and_then(|()| db.migrate()) {
                Ok(_) => return Ok(db),
                Err(err) if err.is_busy() && std::time::Instant::now() < deadline => {
                    tracing::debug!(
                        attempt,
                        path = %path.display(),
                        "manifest is locked by another instance; starting over"
                    );
                    // Dropping `db` closes the connection, releasing any lock
                    // it still holds, before the next attempt opens a new one.
                    drop(db);
                    std::thread::sleep(OPEN_BACKOFF);
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// Lock the underlying connection.
    pub fn conn(&self) -> MutexGuard<'_, Connection> {
        match self.conn.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                tracing::error!("database mutex was poisoned; recovering");
                poisoned.into_inner()
            }
        }
    }

    /// Apply connection-level pragmas.
    fn apply_pragmas(&self, file_backed: bool) -> Result<(), DbError> {
        let conn = self.conn();
        // First, so that every later statement (including the journal-mode
        // switch and the migrations) waits instead of failing while another
        // instance holds the write lock.
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
        if file_backed {
            // WAL keeps a reader from blocking the writer and survives a crash
            // without corrupting committed transactions. Only switch when it is
            // not already WAL: this pragma needs exclusive access, and it is the
            // one call that reports `SQLITE_BUSY` *without* consulting the busy
            // handler, so asking for the mode we are already in is a needless
            // way to lose a race with another instance.
            let mode: String = conn.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
            if !mode.eq_ignore_ascii_case("wal") {
                conn.pragma_update(None, "journal_mode", "WAL")?;
            }
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(())
    }

    /// Bring the schema up to [`SCHEMA_VERSION`].
    pub fn migrate(&self) -> Result<u32, DbError> {
        let mut conn = self.conn();
        migrations::apply(&mut conn)
    }

    /// Current schema version.
    pub fn schema_version(&self) -> Result<u32, DbError> {
        migrations::user_version(&self.conn())
    }

    /// Run `f` inside a transaction, committing on `Ok`.
    pub fn transaction<T>(
        &self,
        f: impl FnOnce(&rusqlite::Transaction<'_>) -> Result<T, DbError>,
    ) -> Result<T, DbError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let value = f(&tx)?;
        tx.commit()?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_memory_database_migrates() {
        let db = Db::open_in_memory().unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migration_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let v = db.migrate().unwrap();
        assert_eq!(v, SCHEMA_VERSION);
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn all_expected_tables_exist() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn();
        let mut stmt = conn
            .prepare("SELECT name FROM sqlite_master WHERE type = 'table'")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        for expected in [
            "devices",
            "device_aliases",
            "device_folders",
            "backup_jobs",
            "backup_files",
        ] {
            assert!(
                names.contains(&expected.to_string()),
                "missing {expected}: {names:?}"
            );
        }
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Db::open_in_memory().unwrap();
        let conn = db.conn();
        let err = conn.execute(
            "INSERT INTO device_folders (device_id, source_path, enabled) VALUES (999, '/x', 1)",
            [],
        );
        assert!(err.is_err(), "foreign key violation should fail");
    }

    #[test]
    fn newer_schema_version_is_refused() {
        let db = Db::open_in_memory().unwrap();
        db.conn()
            .pragma_update(None, "user_version", SCHEMA_VERSION + 5)
            .unwrap();
        let err = db.migrate().unwrap_err();
        assert!(matches!(err, DbError::VersionSkew { .. }));
    }

    #[test]
    fn file_backed_database_uses_wal_and_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/dir/manifest.db");
        let db = Db::open_file(&path).unwrap();
        let mode: String = db
            .conn()
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mode.to_lowercase(), "wal");
        assert!(path.exists());
    }

    #[test]
    fn reopening_an_existing_database_keeps_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        {
            let db = Db::open_file(&path).unwrap();
            db.conn()
                .execute(
                    "INSERT INTO devices (serial, display_name, created_at, last_seen_at) \
                     VALUES ('S1', 'Phone', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                    [],
                )
                .unwrap();
        }
        let db = Db::open_file(&path).unwrap();
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn transaction_rolls_back_on_error() {
        let db = Db::open_in_memory().unwrap();
        let result: Result<(), DbError> = db.transaction(|tx| {
            tx.execute(
                "INSERT INTO devices (serial, display_name, created_at, last_seen_at) \
                 VALUES ('S1', 'Phone', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z')",
                [],
            )?;
            Err(DbError::NotFound {
                what: "simulated failure",
                key: "x".into(),
            })
        });
        assert!(result.is_err());
        let count: i64 = db
            .conn()
            .query_row("SELECT COUNT(*) FROM devices", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "rolled back");
    }

    #[test]
    fn a_lock_held_by_someone_else_is_recognised_as_temporary() {
        // The classification [`Db::open_file`] retries on. Without a short busy
        // timeout this would take 5 s, so the test sets its own.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        drop(Db::open_file(&path).unwrap());

        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();

        let other = Connection::open(&path).unwrap();
        other.busy_timeout(Duration::from_millis(10)).unwrap();
        let err = other.execute_batch("BEGIN IMMEDIATE").unwrap_err();
        let err = DbError::Sql(err);
        assert!(err.is_busy(), "{err:?} should be temporary");
        assert!(!err.is_corrupt(), "a lock is not a damaged file");
        assert!(
            !DbError::VersionSkew {
                found: 2,
                supported: 1
            }
            .is_busy(),
            "a newer schema must not be retried"
        );
    }

    #[test]
    fn a_second_instance_reopens_a_migrated_manifest_without_waiting() {
        // The happy path for "two copies of the app started at once": once the
        // schema is in place, opening reads `user_version`, finds it current and
        // never asks for a write lock — so it cannot lose a race, however long
        // the other instance holds the lock.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        drop(Db::open_file(&path).unwrap());

        let holder = Connection::open(&path).unwrap();
        holder.execute_batch("BEGIN IMMEDIATE").unwrap();

        let started = std::time::Instant::now();
        let db = Db::open_file(&path).expect("a busy writer must not block a reader");
        assert_eq!(db.schema_version().unwrap(), SCHEMA_VERSION);
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "opening waited {:?} on a lock it does not need",
            started.elapsed()
        );
        holder.execute_batch("ROLLBACK").unwrap();
    }
}
