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

use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::Connection;

pub use migrations::SCHEMA_VERSION;

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
    pub fn open_file(path: &Path) -> Result<Self, DbError> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent).map_err(|source| DbError::Io {
                    path: parent.to_path_buf(),
                    source,
                })?;
            }
        }
        let conn = Connection::open(path)?;
        let db = Self {
            conn: Mutex::new(conn),
        };
        db.apply_pragmas(true)?;
        db.migrate()?;
        Ok(db)
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
        if file_backed {
            // WAL keeps a reader from blocking the writer and survives a crash
            // without corrupting committed transactions.
            conn.pragma_update(None, "journal_mode", "WAL")?;
        }
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_millis(5000))?;
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
}
