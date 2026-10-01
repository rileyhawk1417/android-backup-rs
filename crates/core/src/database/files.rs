//! Per-file manifest records.
//!
//! Invariants enforced here (plan §4.3, §9):
//! * one row per `(device_id, source_path)`;
//! * a row is only ever *added* or *updated* — deleting a file from the phone
//!   never deletes anything here;
//! * `last_successful_backup` is only advanced when a file was actually verified,
//!   so an interrupted or failed transfer cannot make a stale copy look fresh.

use rusqlite::{params, OptionalExtension, Row};

use crate::clock;

use super::{Db, DbError};

/// Status values stored in `backup_files.status`.
pub mod status {
    pub const VERIFIED: &str = "verified";
    pub const FAILED: &str = "failed_transfer";
}

/// Data recorded for one file after (attempted) transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileRecord {
    pub source_path: String,
    pub destination_path: String,
    pub source_size: Option<u64>,
    pub source_modified_at: Option<i64>,
    pub content_hash: Option<String>,
}

/// A row of `backup_files`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackupFileRow {
    pub id: i64,
    pub device_id: i64,
    pub job_id: i64,
    pub source_path: String,
    pub destination_path: String,
    pub source_size: Option<i64>,
    pub source_modified_at: Option<i64>,
    pub content_hash: Option<String>,
    pub last_successful_backup: Option<String>,
    pub status: String,
}

impl BackupFileRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            id: row.get("id")?,
            device_id: row.get("device_id")?,
            job_id: row.get("job_id")?,
            source_path: row.get("source_path")?,
            destination_path: row.get("destination_path")?,
            source_size: row.get("source_size")?,
            source_modified_at: row.get("source_modified_at")?,
            content_hash: row.get("content_hash")?,
            last_successful_backup: row.get("last_successful_backup")?,
            status: row.get("status")?,
        })
    }

    /// Size as an unsigned value, guarding against a corrupt negative row.
    pub fn size(&self) -> Option<u64> {
        self.source_size.and_then(|v| u64::try_from(v).ok())
    }

    pub fn is_verified(&self) -> bool {
        self.status == status::VERIFIED
    }
}

/// Record a verified file and bump the job's copied/bytes counters atomically.
pub fn record_verified(
    db: &Db,
    device_id: i64,
    job_id: i64,
    record: &FileRecord,
    now: &str,
) -> Result<(), DbError> {
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO backup_files
                 (device_id, job_id, source_path, destination_path, source_size,
                  source_modified_at, content_hash, last_successful_backup, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'verified')
             ON CONFLICT(device_id, source_path) DO UPDATE SET
                 job_id                 = excluded.job_id,
                 destination_path       = excluded.destination_path,
                 source_size            = COALESCE(excluded.source_size, backup_files.source_size),
                 source_modified_at     = COALESCE(excluded.source_modified_at, backup_files.source_modified_at),
                 content_hash           = COALESCE(excluded.content_hash, backup_files.content_hash),
                 last_successful_backup = excluded.last_successful_backup,
                 status                 = 'verified'",
            params![
                device_id,
                job_id,
                record.source_path,
                record.destination_path,
                record.source_size.map(|v| v as i64),
                record.source_modified_at,
                record.content_hash,
                now,
            ],
        )?;
        tx.execute(
            "UPDATE backup_jobs
             SET files_copied = files_copied + 1, bytes_copied = bytes_copied + ?2
             WHERE id = ?1",
            params![job_id, record.source_size.unwrap_or(0) as i64],
        )?;
        Ok(())
    })
}

/// Record a failed transfer. `last_successful_backup` is deliberately left alone,
/// and the previously known destination is kept if this attempt had none.
pub fn record_failed(
    db: &Db,
    device_id: i64,
    job_id: i64,
    record: &FileRecord,
) -> Result<(), DbError> {
    db.transaction(|tx| {
        tx.execute(
            "INSERT INTO backup_files
                 (device_id, job_id, source_path, destination_path, source_size,
                  source_modified_at, content_hash, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'failed_transfer')
             ON CONFLICT(device_id, source_path) DO UPDATE SET
                 job_id             = excluded.job_id,
                 source_size        = COALESCE(excluded.source_size, backup_files.source_size),
                 source_modified_at = COALESCE(excluded.source_modified_at, backup_files.source_modified_at),
                 status             = 'failed_transfer'",
            params![
                device_id,
                job_id,
                record.source_path,
                record.destination_path,
                record.source_size.map(|v| v as i64),
                record.source_modified_at,
                record.content_hash,
            ],
        )?;
        tx.execute(
            "UPDATE backup_jobs SET files_failed = files_failed + 1 WHERE id = ?1",
            params![job_id],
        )?;
        Ok(())
    })
}

/// Downgrade a `verified` row to `failed_transfer` because the local copy is gone
/// or the wrong size.
///
/// Used by [`crate::backup::recovery`]: no job is running, so no job counters are
/// touched — the row simply stops claiming a copy that is not there any more. The
/// historical `last_successful_backup` and `content_hash` are kept: they describe
/// the last good copy, and the next run uses them to decide what to re-fetch.
///
/// Returns `true` when a row was actually changed.
pub fn mark_stale(db: &Db, device_id: i64, source_path: &str) -> Result<bool, DbError> {
    let conn = db.conn();
    let changed = conn.execute(
        "UPDATE backup_files SET status = 'failed_transfer'
         WHERE device_id = ?1 AND source_path = ?2 AND status = 'verified'",
        params![device_id, source_path],
    )?;
    Ok(changed > 0)
}

/// Every known file for a device, in a stable order.
pub fn manifest_for_device(db: &Db, device_id: i64) -> Result<Vec<BackupFileRow>, DbError> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT * FROM backup_files WHERE device_id = ?1 ORDER BY source_path ASC")?;
    let rows = stmt.query_map(params![device_id], BackupFileRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn files_for_job(db: &Db, job_id: i64) -> Result<Vec<BackupFileRow>, DbError> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT * FROM backup_files WHERE job_id = ?1 ORDER BY source_path ASC")?;
    let rows = stmt.query_map(params![job_id], BackupFileRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn status_for_path(
    db: &Db,
    device_id: i64,
    source_path: &str,
) -> Result<Option<String>, DbError> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT status FROM backup_files WHERE device_id = ?1 AND source_path = ?2")?;
    Ok(stmt
        .query_row(params![device_id, source_path], |row| {
            row.get::<_, String>(0)
        })
        .optional()?)
}

/// Counts of verified / failed rows for a device.
pub fn summary_counts(db: &Db, device_id: i64) -> Result<(u64, u64), DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT
             SUM(CASE WHEN status = 'verified' THEN 1 ELSE 0 END),
             SUM(CASE WHEN status = 'failed_transfer' THEN 1 ELSE 0 END)
         FROM backup_files WHERE device_id = ?1",
    )?;
    let (verified, failed): (Option<i64>, Option<i64>) =
        stmt.query_row(params![device_id], |row| Ok((row.get(0)?, row.get(1)?)))?;
    Ok((
        verified.unwrap_or(0).max(0) as u64,
        failed.unwrap_or(0).max(0) as u64,
    ))
}

/// Total bytes recorded as backed up for a device.
pub fn total_bytes(db: &Db, device_id: i64) -> Result<u64, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT COALESCE(SUM(source_size), 0) FROM backup_files
         WHERE device_id = ?1 AND status = 'verified'",
    )?;
    let total: i64 = stmt.query_row(params![device_id], |row| row.get(0))?;
    Ok(u64::try_from(total).unwrap_or(0))
}

/// Timestamp helper used by callers that only need "now".
pub fn record_verified_now(
    db: &Db,
    device_id: i64,
    job_id: i64,
    record: &FileRecord,
) -> Result<(), DbError> {
    record_verified(db, device_id, job_id, record, &clock::now_iso())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::{devices, jobs};

    fn fixture(db: &Db) -> (i64, i64) {
        let device =
            devices::upsert_seen(db, "SER1", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job = jobs::insert_job(db, device.id, "2026-01-01T00:00:00Z").unwrap();
        (device.id, job)
    }

    fn record(source: &str) -> FileRecord {
        FileRecord {
            source_path: source.to_string(),
            destination_path: format!("SER1/DCIM/{source}"),
            source_size: Some(1024),
            source_modified_at: Some(1_700_000_000),
            content_hash: None,
        }
    }

    #[test]
    fn verified_records_are_inserted_with_timestamp() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();
        let rows = manifest_for_device(&db, device).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, status::VERIFIED);
        assert_eq!(
            rows[0].last_successful_backup.as_deref(),
            Some("2026-01-01T00:10:00Z")
        );
    }

    #[test]
    fn re_verifying_does_not_create_duplicate_rows() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-02T00:10:00Z").unwrap();
        let rows = manifest_for_device(&db, device).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(
            rows[0].last_successful_backup.as_deref(),
            Some("2026-01-02T00:10:00Z")
        );
    }

    #[test]
    fn job_counters_track_copies() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();
        record_verified(&db, device, job, &record("b.jpg"), "2026-01-01T00:10:01Z").unwrap();
        let job_row = jobs::get_job(&db, job).unwrap().unwrap();
        assert_eq!(job_row.files_copied, 2);
        assert_eq!(job_row.bytes_copied, 2048);
    }

    #[test]
    fn failed_transfer_never_advances_last_successful_backup() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();
        record_failed(&db, device, job, &record("a.jpg")).unwrap();
        let rows = manifest_for_device(&db, device).unwrap();
        assert_eq!(rows[0].status, status::FAILED);
        assert_eq!(
            rows[0].last_successful_backup.as_deref(),
            Some("2026-01-01T00:10:00Z"),
            "a failed retry must not erase the previous good backup time"
        );
        // ...and the recorded (good) destination survives a failed retry.
        assert_eq!(rows[0].destination_path, "SER1/DCIM/a.jpg");
        assert_eq!(jobs::get_job(&db, job).unwrap().unwrap().files_failed, 1);
    }

    #[test]
    fn manifest_is_scoped_per_device() {
        let db = Db::open_in_memory().unwrap();
        let (device_a, job_a) = fixture(&db);
        let other =
            devices::upsert_seen(&db, "SER2", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job_b = jobs::insert_job(&db, other.id, "2026-01-01T00:00:00Z").unwrap();
        record_verified(
            &db,
            device_a,
            job_a,
            &record("a.jpg"),
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        record_verified(
            &db,
            other.id,
            job_b,
            &record("a.jpg"),
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        assert_eq!(manifest_for_device(&db, device_a).unwrap().len(), 1);
        assert_eq!(manifest_for_device(&db, other.id).unwrap().len(), 1);
    }

    #[test]
    fn same_source_path_on_a_different_device_is_not_a_conflict() {
        let db = Db::open_in_memory().unwrap();
        let (device_a, job_a) = fixture(&db);
        let other =
            devices::upsert_seen(&db, "SER2", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job_b = jobs::insert_job(&db, other.id, "2026-01-01T00:00:00Z").unwrap();
        record_verified(
            &db,
            device_a,
            job_a,
            &record("a.jpg"),
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        record_verified(
            &db,
            other.id,
            job_b,
            &record("a.jpg"),
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        assert_eq!(summary_counts(&db, device_a).unwrap(), (1, 0));
        assert_eq!(summary_counts(&db, other.id).unwrap(), (1, 0));
    }

    #[test]
    fn summary_counts_and_bytes() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();
        record_verified(&db, device, job, &record("b.jpg"), "2026-01-01T00:10:00Z").unwrap();
        record_failed(&db, device, job, &record("c.jpg")).unwrap();
        assert_eq!(summary_counts(&db, device).unwrap(), (2, 1));
        assert_eq!(total_bytes(&db, device).unwrap(), 2048);
    }

    #[test]
    fn status_lookup_by_path() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();
        assert_eq!(
            status_for_path(&db, device, "a.jpg").unwrap().as_deref(),
            Some("verified")
        );
        assert_eq!(status_for_path(&db, device, "missing.jpg").unwrap(), None);
    }

    #[test]
    fn mark_stale_downgrades_a_missing_copy_without_touching_history() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        record_verified(&db, device, job, &record("a.jpg"), "2026-01-01T00:10:00Z").unwrap();

        assert!(mark_stale(&db, device, "a.jpg").unwrap());
        let row = &manifest_for_device(&db, device).unwrap()[0];
        assert_eq!(row.status, status::FAILED);
        assert_eq!(
            row.last_successful_backup.as_deref(),
            Some("2026-01-01T00:10:00Z")
        );
        assert_eq!(row.destination_path, "SER1/DCIM/a.jpg");

        // Idempotent: a second pass changes nothing and must not double-count.
        assert!(!mark_stale(&db, device, "a.jpg").unwrap());
        assert_eq!(jobs::get_job(&db, job).unwrap().unwrap().files_failed, 0);

        // Unknown or other-device paths are not invented into existence.
        assert!(!mark_stale(&db, device, "nope.jpg").unwrap());
        assert!(manifest_for_device(&db, device).unwrap().len() == 1);
    }

    #[test]
    fn empty_manifest_reports_zeroes_not_errors() {
        let db = Db::open_in_memory().unwrap();
        let (device, _job) = fixture(&db);
        assert_eq!(summary_counts(&db, device).unwrap(), (0, 0));
        assert_eq!(total_bytes(&db, device).unwrap(), 0);
    }

    #[test]
    fn sizes_survive_the_round_trip() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        let mut rec = record("big.bin");
        rec.source_size = Some(u64::from(u32::MAX) + 17);
        record_verified(&db, device, job, &rec, "2026-01-01T00:10:00Z").unwrap();
        let rows = manifest_for_device(&db, device).unwrap();
        assert_eq!(rows[0].size(), Some(u64::from(u32::MAX) + 17));
    }
}
