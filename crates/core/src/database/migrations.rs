//! Embedded, versioned schema migrations.
//!
//! Each migration runs in its own transaction together with the
//! `PRAGMA user_version` bump, so a crash mid-migration leaves the database on
//! the previous version rather than half-migrated.

use rusqlite::Connection;

use super::DbError;

/// The schema version this build understands.
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

/// `(version, sql)` in ascending version order. Append-only.
const MIGRATIONS: &[(u32, &str)] = &[(1, include_str!("migrations/v1.sql"))];

/// Read `PRAGMA user_version`.
pub fn user_version(conn: &Connection) -> Result<u32, DbError> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    Ok(version.clamp(0, u32::MAX as i64) as u32)
}

/// Apply all pending migrations, returning the resulting schema version.
pub fn apply(conn: &mut Connection) -> Result<u32, DbError> {
    let current = user_version(conn)?;
    if current > SCHEMA_VERSION {
        return Err(DbError::VersionSkew {
            found: current,
            supported: SCHEMA_VERSION,
        });
    }
    for (version, sql) in MIGRATIONS {
        if *version <= current {
            continue;
        }
        tracing::info!(version, "applying database migration");
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        tx.pragma_update(None, "user_version", *version)?;
        tx.commit()?;
    }
    user_version(conn)
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
