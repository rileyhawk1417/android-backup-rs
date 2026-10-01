//! Crash recovery: making the destination and the manifest agree again.
//!
//! A backup can be interrupted at any moment (laptop lid, unplugged drive, killed
//! process). Three things can be left behind, and all three are repaired here —
//! without ever deleting a backup or a manifest row:
//!
//! | Left behind | Repair |
//! |---|---|
//! | `.incomplete/**.part` | removed (it is app-created junk, never user data) |
//! | a job stuck in `scanning`/`copying`/`paused` | closed as `failed`, with an explanation |
//! | a `verified` row whose local file is missing or truncated | re-marked `failed_transfer` |
//!
//! The last one matters most: it is what stops the manifest from claiming a file
//! is backed up when the only remaining copy was half-written. Combined with the
//! engine's "is the local copy really there?" check during planning, an
//! interrupted run can only ever cause an extra copy — never a silent gap.
//!
//! [`recover_root`] touches the filesystem only; [`recover_device`] also repairs
//! the manifest and is called at startup and at the start of every job.

use std::path::{Path, PathBuf};

use crate::clock;
use crate::database::files;
use crate::database::jobs::{self, JobCounters, JobStatus};
use crate::database::Db;

use super::destination::FsProbe;
use super::transfer;

/// What a recovery pass repaired.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Partial transfers that were removed.
    pub partials_removed: Vec<PathBuf>,
    /// Jobs that were still open and have now been closed.
    pub unfinished_jobs_closed: Vec<i64>,
    /// Manifest rows that claimed `verified` but whose file was gone or wrong.
    pub stale_rows: Vec<String>,
}

impl RecoveryReport {
    pub fn is_empty(&self) -> bool {
        self.partials_removed.is_empty()
            && self.unfinished_jobs_closed.is_empty()
            && self.stale_rows.is_empty()
    }

    /// One-line description for a log or a status bar.
    pub fn summary(&self) -> String {
        format!(
            "{} partial file(s) removed, {} interrupted job(s) closed, {} record(s) needing a re-copy",
            self.partials_removed.len(),
            self.unfinished_jobs_closed.len(),
            self.stale_rows.len()
        )
    }

    /// `true` when a previous run had actually started writing data.
    pub fn had_interrupted_work(&self) -> bool {
        !self.partials_removed.is_empty() || !self.unfinished_jobs_closed.is_empty()
    }
}

/// Remove leftover partial transfers under `root`.
///
/// Only `<root>/.incomplete/**.part` is touched; every other byte of the
/// destination, and the version history in `.backup/history`, is left alone.
pub fn recover_root(root: &Path) -> RecoveryReport {
    RecoveryReport {
        partials_removed: transfer::sweep_partials(root),
        ..Default::default()
    }
}

/// Filesystem + manifest recovery for one device.
///
/// `root` is the device's destination root. Safe to call before any job; safe to
/// call twice.
pub fn recover_device(
    db: &Db,
    probe: &dyn FsProbe,
    root: &Path,
    device_id: i64,
) -> Result<RecoveryReport, crate::database::DbError> {
    let mut report = recover_root(root);

    // 1. Close jobs that a crash left open, so history never lies about a job
    //    still "copying" from three weeks ago.
    let now = clock::now_iso();
    for job in jobs::unfinished_jobs(db)? {
        if job.device_id != device_id {
            continue;
        }
        let counters = JobCounters {
            scanned: job.files_scanned.max(0) as u64,
            copied: job.files_copied.max(0) as u64,
            skipped: job.files_skipped.max(0) as u64,
            failed: job.files_failed.max(0) as u64,
            bytes_copied: job.bytes_copied.max(0) as u64,
        };
        jobs::finalize(
            db,
            job.id,
            JobStatus::Failed,
            &counters,
            Some(
                "the app stopped while this backup was running; the files it had finished are safe \
                 and will be skipped next time",
            ),
            &now,
        )?;
        report.unfinished_jobs_closed.push(job.id);
    }

    // 2. Stop claiming files are backed up when the local copy is gone, truncated
    //    or otherwise the wrong size. The engine re-copies anything that is not
    //    `verified` here (plan E05.7, "record less when in doubt").
    for row in files::manifest_for_device(db, device_id)? {
        if !row.is_verified() {
            continue;
        }
        let Some(expected) = row.size() else {
            continue;
        };
        if row.destination_path.is_empty() {
            continue;
        }
        let path = PathBuf::from(&row.destination_path);
        let local = probe.local_size(&path);
        if local == Some(expected) {
            continue;
        }
        match local {
            None => tracing::warn!(
                path = %row.destination_path,
                "a recorded backup file is missing; it will be copied again"
            ),
            Some(size) => tracing::warn!(
                path = %row.destination_path,
                expected,
                size,
                "a recorded backup file has the wrong size; it will be copied again"
            ),
        }
        if files::mark_stale(db, device_id, &row.source_path)? {
            report.stale_rows.push(row.source_path.clone());
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::devices;
    use crate::database::files::{status, FileRecord};
    use crate::mock::MockFsProbe;

    fn fixture(db: &Db) -> (i64, i64) {
        let device =
            devices::upsert_seen(db, "SER1", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job = jobs::insert_job(db, device.id, "2026-01-01T00:00:00Z").unwrap();
        (device.id, job)
    }

    fn verify(db: &Db, device: i64, job: i64, source: &str, dest: &str, size: u64) {
        files::record_verified(
            db,
            device,
            job,
            &FileRecord {
                source_path: source.to_string(),
                destination_path: dest.to_string(),
                source_size: Some(size),
                source_modified_at: Some(5),
                content_hash: None,
            },
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
    }

    #[test]
    fn partial_files_are_removed_and_nothing_else_is() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(transfer::staging_dir(root).join("DCIM")).unwrap();
        std::fs::write(root.join(".incomplete/DCIM/a.jpg.part"), b"half").unwrap();
        std::fs::write(root.join("DCIM-real.jpg"), b"whole").unwrap();

        let report = recover_root(root);
        assert_eq!(report.partials_removed.len(), 1);
        assert!(report.had_interrupted_work());
        assert!(root.join("DCIM-real.jpg").exists());
        assert!(!report.is_empty());
        assert!(
            recover_root(root).is_empty(),
            "the second pass finds nothing"
        );
    }

    #[test]
    fn a_clean_destination_reports_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(recover_root(dir.path()).is_empty());
        assert_eq!(
            RecoveryReport::default().summary(),
            "0 partial file(s) removed, 0 interrupted job(s) closed, 0 record(s) needing a re-copy"
        );
    }

    #[test]
    fn an_interrupted_job_is_closed_as_failed_without_touching_its_counters() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        verify(
            &db,
            device,
            job,
            "/storage/emulated/0/DCIM/a.jpg",
            "/backups/a.jpg",
            10,
        );
        jobs::transition(&db, job, JobStatus::Scanning).unwrap();
        jobs::transition(&db, job, JobStatus::Copying).unwrap();

        let probe = MockFsProbe::new().with_file("/backups/a.jpg", 10);
        let dir = tempfile::tempdir().unwrap();
        let report = recover_device(&db, &probe, dir.path(), device).unwrap();

        assert_eq!(report.unfinished_jobs_closed, vec![job]);
        let row = jobs::get_job(&db, job).unwrap().unwrap();
        assert_eq!(row.status, JobStatus::Failed);
        assert!(row.completed_at.is_some());
        assert_eq!(
            row.files_copied, 1,
            "counters are preserved, not recomputed away"
        );
        assert!(row.error_summary.unwrap().contains("app stopped"));
    }

    #[test]
    fn a_recorded_file_that_vanished_is_downgraded_to_failed() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        verify(
            &db,
            device,
            job,
            "/storage/emulated/0/DCIM/gone.jpg",
            "/backups/gone.jpg",
            10,
        );

        let probe = MockFsProbe::new(); // nothing on disk
        let dir = tempfile::tempdir().unwrap();
        let report = recover_device(&db, &probe, dir.path(), device).unwrap();

        assert_eq!(report.stale_rows, vec!["/storage/emulated/0/DCIM/gone.jpg"]);
        let rows = files::manifest_for_device(&db, device).unwrap();
        assert_eq!(rows[0].status, status::FAILED);
        assert_eq!(
            rows[0].last_successful_backup.as_deref(),
            Some("2026-01-01T00:10:00Z"),
            "the historical timestamp is evidence, not a claim of a current copy"
        );
    }

    #[test]
    fn a_truncated_local_copy_is_downgraded_too() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        verify(
            &db,
            device,
            job,
            "/storage/emulated/0/DCIM/a.jpg",
            "/backups/a.jpg",
            10,
        );

        let probe = MockFsProbe::new().with_file("/backups/a.jpg", 4);
        let dir = tempfile::tempdir().unwrap();
        let report = recover_device(&db, &probe, dir.path(), device).unwrap();
        assert_eq!(report.stale_rows.len(), 1);
        assert_eq!(
            files::manifest_for_device(&db, device).unwrap()[0].status,
            status::FAILED
        );
    }

    #[test]
    fn a_correct_local_copy_is_left_verified() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        verify(
            &db,
            device,
            job,
            "/storage/emulated/0/DCIM/a.jpg",
            "/backups/a.jpg",
            10,
        );
        jobs::transition(&db, job, JobStatus::Scanning).unwrap();
        jobs::transition(&db, job, JobStatus::Copying).unwrap();
        jobs::finalize(
            &db,
            job,
            JobStatus::Completed,
            &JobCounters::default(),
            None,
            "2026-01-01T00:20:00Z",
        )
        .unwrap();

        let probe = MockFsProbe::new().with_file("/backups/a.jpg", 10);
        let dir = tempfile::tempdir().unwrap();
        let report = recover_device(&db, &probe, dir.path(), device).unwrap();
        assert!(report.is_empty());
        assert_eq!(
            files::manifest_for_device(&db, device).unwrap()[0].status,
            status::VERIFIED
        );
    }

    #[test]
    fn recovery_is_scoped_to_one_device() {
        let db = Db::open_in_memory().unwrap();
        let (device_a, job_a) = fixture(&db);
        let other =
            devices::upsert_seen(&db, "SER2", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job_b = jobs::insert_job(&db, other.id, "2026-01-01T00:00:00Z").unwrap();
        jobs::transition(&db, job_b, JobStatus::Scanning).unwrap();
        verify(
            &db,
            device_a,
            job_a,
            "/storage/emulated/0/a.jpg",
            "/backups/a.jpg",
            10,
        );

        let probe = MockFsProbe::new().with_file("/backups/a.jpg", 10);
        let dir = tempfile::tempdir().unwrap();
        let report = recover_device(&db, &probe, dir.path(), device_a).unwrap();
        // Device A's own interrupted job is closed; device B's is left alone.
        assert_eq!(report.unfinished_jobs_closed, vec![job_a]);
        assert_eq!(
            jobs::get_job(&db, job_b).unwrap().unwrap().status,
            JobStatus::Scanning,
            "device B is untouched"
        );
    }

    #[test]
    fn double_recovery_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let (device, job) = fixture(&db);
        verify(
            &db,
            device,
            job,
            "/storage/emulated/0/a.jpg",
            "/backups/a.jpg",
            10,
        );
        let probe = MockFsProbe::new();
        let dir = tempfile::tempdir().unwrap();
        let first = recover_device(&db, &probe, dir.path(), device).unwrap();
        let second = recover_device(&db, &probe, dir.path(), device).unwrap();
        assert_eq!(first.stale_rows.len(), 1);
        assert!(
            second.stale_rows.is_empty(),
            "already-downgraded rows are skipped"
        );
    }
}
