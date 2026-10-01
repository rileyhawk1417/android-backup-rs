//! Incremental comparison: phone state versus the recorded manifest.
//!
//! Pure functions over data — no I/O, no clock, no filesystem. That is what makes
//! "did we already back this file up?" answers reproducible and unit-testable
//! (plan F2.2, F7.1).

use std::collections::HashMap;

use crate::database::files::BackupFileRow;
use crate::paths::RelPath;

use super::enumerate::RemoteEntry;

/// What to do with one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Transfer it.
    Copy,
    /// It is already backed up and unchanged.
    Skip,
}

/// Why the decision was made. Recorded in the job log so a user can always see
/// why a file was or was not copied.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// Never seen before.
    New,
    /// Size differs from the recorded copy.
    SizeChanged,
    /// Same size, but the phone reports a different modification time.
    ModifiedTimeChanged,
    /// A previous attempt failed, so it is retried.
    PreviousAttemptFailed,
    /// The recorded local copy is missing from disk; re-copied without deleting
    /// anything from the phone.
    LocalCopyMissing,
    /// The local copy has a different size than the phone's file: a truncated or
    /// tampered copy is re-fetched.
    LocalSizeChanged,
    /// Hash mode only: the local file's bytes no longer match what was recorded.
    LocalContentChanged,
    /// Size and (when known) mtime match the verified copy.
    Unchanged,
    /// Size matches but neither side reports a modification time: treated as
    /// unchanged, and reported so the user can force a full copy if they care.
    UnchangedUnverifiable,
}

impl Reason {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::New => "new file",
            Self::SizeChanged => "size changed on the phone",
            Self::ModifiedTimeChanged => "modified on the phone",
            Self::PreviousAttemptFailed => "older attempt failed",
            Self::LocalCopyMissing => "local copy is missing",
            Self::LocalSizeChanged => "local copy is the wrong size",
            Self::LocalContentChanged => "local copy changed on disk",
            Self::Unchanged => "already backed up",
            Self::UnchangedUnverifiable => "already backed up (size match only)",
        }
    }

    pub fn forces_copy(&self) -> bool {
        matches!(
            self,
            Self::New
                | Self::SizeChanged
                | Self::ModifiedTimeChanged
                | Self::PreviousAttemptFailed
                | Self::LocalCopyMissing
                | Self::LocalSizeChanged
                | Self::LocalContentChanged
        )
    }
}

/// One file with its decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub remote: RemoteEntry,
    pub rel: RelPath,
    pub decision: Decision,
    pub reason: Reason,
    /// Hash recorded for this file by the last successful backup, if any. Used by
    /// [`super::transfer::VerifyMode::Hash`] to re-check files it intends to skip.
    pub known_hash: Option<String>,
}

impl PlannedFile {
    pub fn will_copy(&self) -> bool {
        self.decision == Decision::Copy
    }
}

/// Aggregate view of a plan, used for progress and for the free-space check.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlanStats {
    pub total_files: u64,
    pub total_bytes: u64,
    pub to_copy: u64,
    pub copy_bytes: u64,
    pub to_skip: u64,
    pub skipped_bytes: u64,
}

/// The full plan for a job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    pub stats: PlanStats,
}

impl Plan {
    /// Files that still need transferring.
    pub fn to_transfer(&self) -> impl Iterator<Item = &PlannedFile> {
        self.files.iter().filter(|f| f.will_copy())
    }

    /// Mark a planned file as needing a copy because the local copy is gone,
    /// truncated or changed.
    pub fn force_copy(&mut self, index: usize, reason: Reason) {
        if let Some(file) = self.files.get_mut(index) {
            if file.decision == Decision::Copy {
                return;
            }
            file.decision = Decision::Copy;
            file.reason = reason;
            self.stats.to_copy += 1;
            self.stats.to_skip = self.stats.to_skip.saturating_sub(1);
            self.stats.copy_bytes += file.remote.size;
            self.stats.skipped_bytes = self.stats.skipped_bytes.saturating_sub(file.remote.size);
        }
    }
}

/// Compare one phone file against its manifest row.
pub fn decide(remote: &RemoteEntry, manifest: Option<&BackupFileRow>) -> (Decision, Reason) {
    let Some(row) = manifest else {
        return (Decision::Copy, Reason::New);
    };
    if !row.is_verified() {
        return (Decision::Copy, Reason::PreviousAttemptFailed);
    }
    match row.size() {
        Some(recorded) if recorded != remote.size => (Decision::Copy, Reason::SizeChanged),
        None => (Decision::Copy, Reason::New),
        Some(_) => match (remote.modified_at, row.source_modified_at) {
            (Some(remote_mtime), Some(recorded_mtime)) if remote_mtime != recorded_mtime => {
                (Decision::Copy, Reason::ModifiedTimeChanged)
            }
            (Some(_), Some(_)) => (Decision::Skip, Reason::Unchanged),
            // One side lacks an mtime: the size match is all we have. Copying
            // everything whenever mtime is unavailable would be worse than a
            // documented size-based skip, and nothing is ever deleted.
            _ => (Decision::Skip, Reason::UnchangedUnverifiable),
        },
    }
}

/// Diff the phone's file list against the manifest.
///
/// Manifest rows are never removed here: a file deleted from the phone simply
/// stops appearing in the plan while its row (and its copy on disk) stays put.
pub fn plan(entries: Vec<(RemoteEntry, RelPath)>, manifest: &[BackupFileRow]) -> Plan {
    let index: HashMap<&str, &BackupFileRow> = manifest
        .iter()
        .map(|row| (row.source_path.as_str(), row))
        .collect();

    let mut out = Plan::default();
    out.stats.total_files = entries.len() as u64;
    for (remote, rel) in entries {
        let row = index.get(remote.path.as_str()).copied();
        let (decision, reason) = decide(&remote, row);
        out.stats.total_bytes += remote.size;
        match decision {
            Decision::Copy => {
                out.stats.to_copy += 1;
                out.stats.copy_bytes += remote.size;
            }
            Decision::Skip => {
                out.stats.to_skip += 1;
                out.stats.skipped_bytes += remote.size;
            }
        }
        out.files.push(PlannedFile {
            remote,
            rel,
            decision,
            reason,
            known_hash: row.and_then(|r| r.content_hash.clone()),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::files::{self, FileRecord};
    use crate::database::Db;

    fn entry(path: &str, size: u64, mtime: Option<i64>) -> (RemoteEntry, RelPath) {
        (
            RemoteEntry {
                path: path.to_string(),
                size,
                modified_at: mtime,
                kind: super::super::enumerate::EntryKind::File,
            },
            RelPath::from_absolute(path, "/storage/emulated/0").unwrap(),
        )
    }

    fn manifest_row(db: &Db, path: &str, size: u64, mtime: Option<i64>) -> Vec<BackupFileRow> {
        use crate::database::{devices, jobs};
        let device =
            devices::upsert_seen(db, "S1", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job = jobs::insert_job(db, device.id, "2026-01-01T00:00:00Z").unwrap();
        files::record_verified(
            db,
            device.id,
            job,
            &FileRecord {
                source_path: path.to_string(),
                destination_path: format!("dest/{path}"),
                source_size: Some(size),
                source_modified_at: mtime,
                content_hash: None,
            },
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        files::manifest_for_device(db, device.id).unwrap()
    }

    #[test]
    fn new_file_is_copied() {
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5))],
            &[],
        );
        assert_eq!(plan.stats.to_copy, 1);
        assert_eq!(plan.files[0].reason, Reason::New);
        assert_eq!(plan.files[0].decision, Decision::Copy);
    }

    #[test]
    fn unchanged_file_is_skipped() {
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, Some(5));
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5))],
            &manifest,
        );
        assert_eq!(plan.stats.to_skip, 1);
        assert_eq!(plan.stats.to_copy, 0);
        assert_eq!(plan.files[0].reason, Reason::Unchanged);
    }

    #[test]
    fn size_change_forces_a_copy() {
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, Some(5));
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 11, Some(5))],
            &manifest,
        );
        assert_eq!(plan.files[0].reason, Reason::SizeChanged);
        assert_eq!(plan.stats.copy_bytes, 11);
    }

    #[test]
    fn mtime_change_with_same_size_forces_a_copy() {
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, Some(5));
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(6))],
            &manifest,
        );
        assert_eq!(plan.files[0].reason, Reason::ModifiedTimeChanged);
    }

    #[test]
    fn missing_mtime_falls_back_to_a_size_only_skip() {
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, None);
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, None)],
            &manifest,
        );
        assert_eq!(plan.files[0].reason, Reason::UnchangedUnverifiable);
        assert_eq!(plan.files[0].decision, Decision::Skip);
    }

    #[test]
    fn a_previously_failed_file_is_retried() {
        use crate::database::{devices, jobs};
        let db = Db::open_in_memory().unwrap();
        let device =
            devices::upsert_seen(&db, "S1", &Default::default(), "2026-01-01T00:00:00Z").unwrap();
        let job = jobs::insert_job(&db, device.id, "2026-01-01T00:00:00Z").unwrap();
        files::record_failed(
            &db,
            device.id,
            job,
            &FileRecord {
                source_path: "/storage/emulated/0/DCIM/a.jpg".into(),
                destination_path: "dest/a.jpg".into(),
                source_size: Some(10),
                source_modified_at: Some(5),
                content_hash: None,
            },
        )
        .unwrap();
        let manifest = files::manifest_for_device(&db, device.id).unwrap();
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5))],
            &manifest,
        );
        assert_eq!(plan.files[0].reason, Reason::PreviousAttemptFailed);
        assert!(plan.files[0].will_copy());
    }

    #[test]
    fn a_row_whose_size_was_never_recorded_is_copied() {
        let row = BackupFileRow {
            id: 1,
            device_id: 1,
            job_id: 1,
            source_path: "/storage/emulated/0/a".into(),
            destination_path: "dest/a".into(),
            source_size: None,
            source_modified_at: None,
            content_hash: None,
            last_successful_backup: Some("2026-01-01T00:00:00Z".into()),
            status: files::status::VERIFIED.into(),
        };
        let remote = RemoteEntry {
            path: "/storage/emulated/0/a".into(),
            size: 5,
            modified_at: None,
            kind: super::super::enumerate::EntryKind::File,
        };
        assert_eq!(decide(&remote, Some(&row)), (Decision::Copy, Reason::New));
    }

    #[test]
    fn empty_plan_has_zero_stats() {
        let plan = plan(vec![], &[]);
        assert_eq!(plan.stats, PlanStats::default());
        assert!(plan.to_transfer().next().is_none());
    }

    #[test]
    fn force_copy_moves_a_file_back_into_the_transfer_set() {
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, Some(5));
        let mut plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5))],
            &manifest,
        );
        assert_eq!(plan.stats.to_skip, 1);
        plan.force_copy(0, Reason::LocalCopyMissing);
        assert_eq!(plan.stats.to_copy, 1);
        assert_eq!(plan.stats.to_skip, 0);
        assert_eq!(plan.stats.copy_bytes, 10);
        assert_eq!(plan.stats.skipped_bytes, 0);
        assert_eq!(plan.files[0].reason, Reason::LocalCopyMissing);
        assert_eq!(plan.to_transfer().count(), 1);
        // Forcing twice is a no-op.
        plan.force_copy(0, Reason::LocalSizeChanged);
        assert_eq!(plan.stats.to_copy, 1);
        assert_eq!(plan.files[0].reason, Reason::LocalCopyMissing);
    }

    #[test]
    fn stats_add_up_over_mixed_input() {
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, Some(5));
        let plan = plan(
            vec![
                entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5)),
                entry("/storage/emulated/0/DCIM/b.jpg", 20, Some(7)),
            ],
            &manifest,
        );
        assert_eq!(plan.stats.total_files, 2);
        assert_eq!(plan.stats.total_bytes, 30);
        assert_eq!(plan.stats.to_copy, 1);
        assert_eq!(plan.stats.copy_bytes, 20);
        assert_eq!(plan.stats.to_skip, 1);
        assert_eq!(plan.stats.skipped_bytes, 10);
    }

    #[test]
    fn plan_remembers_the_recorded_hash() {
        let db = Db::open_in_memory().unwrap();
        let mut manifest = manifest_row(&db, "/storage/emulated/0/DCIM/a.jpg", 10, Some(5));
        manifest[0].content_hash = Some("deadbeef".into());
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5))],
            &manifest,
        );
        assert_eq!(plan.files[0].known_hash.as_deref(), Some("deadbeef"));
    }

    #[test]
    fn files_deleted_from_the_phone_never_become_deletions() {
        // The manifest has a row for a file the phone no longer reports.
        let db = Db::open_in_memory().unwrap();
        let manifest = manifest_row(&db, "/storage/emulated/0/DCIM/gone.jpg", 10, Some(5));
        let plan = plan(
            vec![entry("/storage/emulated/0/DCIM/a.jpg", 10, Some(5))],
            &manifest,
        );
        assert_eq!(
            plan.files.len(),
            1,
            "the plan only contains what the phone has"
        );
        assert!(plan.to_transfer().count() == 1);
    }
}
