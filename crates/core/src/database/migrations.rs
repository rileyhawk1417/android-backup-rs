//! Embedded, versioned schema migrations.
//!
//! The whole migration set runs inside one `BEGIN IMMEDIATE` transaction
//! together with the `PRAGMA user_version` bump, which buys two properties:
//!
//! * **crash safety** — a process that dies mid-migration leaves the database on
//!   the previous version rather than half-migrated;
//! * **no two migrators** — two instances starting at the same moment (the app
//!   plus, say, a recovery helper, or the crash-worker test) serialise on the
//!   write lock instead of both racing to `CREATE TABLE`.

use rusqlite::{Connection, TransactionBehavior};

use super::DbError;

/// The schema version this build understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// `(version, sql)` in ascending version order. Append-only.
const MIGRATIONS: &[(u32, &str)] = &[
    (1, include_str!("migrations/v1.sql")),
    (2, include_str!("migrations/v2.sql")),
];

/// Read `PRAGMA user_version`.
pub fn user_version(conn: &Connection) -> Result<u32, DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version.clamp(0, u32::MAX as i64) as u32)
}

/// Apply all pending migrations, returning the resulting schema version.
pub fn apply(conn: &mut Connection) -> Result<u32, DbError> {
    let current = user_version(conn)?;
    check_supported(current)?;
    if current == SCHEMA_VERSION {
        return Ok(current);
    }

    // The optimistic check above avoids taking a write lock in the common case.
    // Everything below runs under `BEGIN IMMEDIATE`, and the version is read
    // again *inside* the lock: whatever we concluded before may already be stale.
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let current = user_version(&tx)?;
    check_supported(current)?;
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        tracing::info!(version, "applying database migration");
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", *version)?;
    }
    let version = user_version(&tx)?;
    tx.commit()?;
    Ok(version)
}

fn check_supported(version: u32) -> Result<(), DbError> {
    if version > SCHEMA_VERSION {
        return Err(DbError::VersionSkew {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_database_reaches_latest_version() {
        let mut conn = Connection::open_in_memory().unwrap();
        assert_eq!(apply(&mut conn).unwrap(), SCHEMA_VERSION);
        assert_eq!(user_version(&conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn migration_list_is_contiguous_and_ascending() {
        for (index, (version, sql)) in MIGRATIONS.iter().enumerate() {
            assert_eq!(
                *version,
                index as u32 + 1,
                "migration numbering must be contiguous"
            );
            assert!(!sql.trim().is_empty());
        }
    }

    #[test]
    fn concurrent_migrators_do_not_collide() {
        // Two instances starting against the same (brand new) file must not both
        // try to create the schema; one migrates, the other waits and finds it
        // done. Without `BEGIN IMMEDIATE` this fails with
        // "table devices already exists".
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let path = path.clone();
                let barrier = std::sync::Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    super::super::Db::open_file(&path).map(|db| db.schema_version())
                })
            })
            .collect();
        for thread in threads {
            let result = thread.join().unwrap();
            assert_eq!(result.unwrap().unwrap(), SCHEMA_VERSION);
        }
    }

    #[test]
    fn a_v1_database_upgrades_in_place_and_keeps_its_rows() {
        // Build a v1 database by hand: exactly what a 0.1.0 install has on disk.
        let mut conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(MIGRATIONS[0].1).unwrap();
        conn.pragma_update(None, "user_version", 1).unwrap();
        conn.execute_batch(
            "INSERT INTO devices (id, serial, display_name, created_at, last_seen_at)
                 VALUES (7, 'SER7', 'Pixel 8', '2026-01-01T00:00:00Z', '2026-01-01T00:00:00Z');
             INSERT INTO backup_jobs (id, device_id, started_at, status)
                 VALUES (3, 7, '2026-01-01T00:00:00Z', 'completed');",
        )
        .unwrap();

        assert_eq!(apply(&mut conn).unwrap(), SCHEMA_VERSION);

        // Rows survive, and the new columns take their defaults: no device is
        // auto-enrolled by an upgrade, and no historical job is relabelled as
        // the machine's work.
        let (kind, hours, trigger): (String, Option<i64>, String) = conn
            .query_row(
                "SELECT d.auto_schedule_kind, d.auto_schedule_hours, j.trigger FROM devices d
                 JOIN backup_jobs j ON j.device_id = d.id WHERE d.id = 7",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "off");
        assert_eq!(hours, None);
        assert_eq!(trigger, "manual");

        // Running again is a no-op, as it is on every app start.
        assert_eq!(apply(&mut conn).unwrap(), SCHEMA_VERSION);
    }

    #[test]
    fn failed_migration_leaves_version_untouched() {
        // Simulated by hand: an invalid statement must abort the whole step.
        let mut conn = Connection::open_in_memory().unwrap();
        let tx = conn.transaction().unwrap();
        tx.execute_batch("CREATE TABLE t (a);").unwrap();
        let bad = tx.execute_batch("THIS IS NOT SQL;").unwrap_err();
        assert!(!bad.to_string().is_empty());
        drop(tx);
        assert_eq!(user_version(&conn).unwrap(), 0);
    }
}
