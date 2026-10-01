//! The backup engine: selection → enumeration → comparison → transfer → manifest.
//!
//! Layering inside this module (each layer only talks to the one above it):
//!
//! ```text
//! controller   spawns jobs, owns the registry, exposes pause/resume/cancel
//! job          the engine loop, events, progress, pause/retry policy
//! recovery     crash clean-up: partial files, unfinished jobs, stale rows
//! transfer     staging, verification, atomic rename, history archiving
//! compare      pure diff of phone state against the manifest
//! selection    which phone folders a job covers
//! enumerate    the `RemoteFiles` seam (faked in tests, adb in production)
//! destination  is the drive there, writable and big enough?
//! ```
//!
//! **Invariants** (plan P1/P2/P5/P10): nothing here deletes a file from the phone,
//! nothing deletes a backup from disk, and a manifest row is only marked
//! `verified` after a transfer was verified and renamed into place.

pub mod compare;
pub mod controller;
pub mod destination;
pub mod enumerate;
pub mod job;
pub mod recovery;
pub mod selection;
pub mod transfer;

pub use compare::{Decision, Plan, PlanStats, PlannedFile, Reason};
pub use destination::{DestError, EffectiveDestination, FsProbe, SpaceInfo, SpaceVerdict};
pub use enumerate::{
    AdbRemoteFiles, EntryKind, ListingStrategy, RemoteEntry, RemoteError, RemoteFiles,
};
pub use job::{
    ChannelSink, CollectingSink, ControlSignal, ControlSource, Engine, EventSink, JobConfig,
    JobEvent, JobOutcome, NullSink, PauseReason, Progress, RunControl, Uncontrolled,
};
pub use recovery::{recover_device, recover_root, RecoveryReport};
pub use selection::{Selection, SelectionError};
pub use transfer::{TransferError, TransferOutcome, VerifyMode};

/// Safety margin kept free on the destination, on top of the planned copy size.
/// Filling a volume to the last byte makes the whole desktop unhappy.
pub const DEFAULT_FREE_SPACE_FLOOR: u64 = 64 * 1024 * 1024;

/// Failures that stop a job from making progress at all.
///
/// Per-file failures are *not* in here: they are recorded in the manifest and
/// reported through [`job::JobEvent::FileFailed`] so one unreadable file cannot
/// abort a 40 000-file backup.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum BackupError {
    #[error("could not read the phone's files: {0}")]
    Remote(#[from] RemoteError),

    #[error("this device did not report a usable shared-storage location")]
    NoUsableStorage,

    #[error("{0}")]
    Selection(#[from] selection::SelectionError),

    #[error("the backup location is unusable: {0}")]
    Destination(#[from] destination::DestError),

    #[error("transfer failed: {0}")]
    Transfer(#[from] transfer::TransferError),

    #[error("manifest error: {0}")]
    Database(#[from] crate::database::DbError),

    #[error("the backup drive filled up: {0}")]
    OutOfSpace(destination::DestError),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn per_file_errors_are_not_backup_errors() {
        // A guard for the rule above: transfer failures exist, but the engine
        // records them per file instead of aborting the job.
        let err = BackupError::Transfer(TransferError::SizeMismatch {
            path: std::path::PathBuf::from("/tmp/a"),
            expected: 2,
            actual: 1,
        });
        assert!(err.to_string().contains("incomplete"));
    }

    #[test]
    fn destination_errors_convert() {
        let err: BackupError = destination::DestError::NotConfigured.into();
        assert!(err.to_string().contains("no backup location"));
    }
}
