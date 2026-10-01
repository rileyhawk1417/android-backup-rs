//! Staged, verified, atomic-enough transfers.
//!
//! Every `pull` targets `<destination>/.incomplete/<rel>.part`. Only when the
//! staged file passes verification is it moved into place with a single `rename`
//! on the same volume (plan F4.4, P4/P5):
//!
//! * an interrupted transfer leaves a `.part` file, never a plausible-looking
//!   truncated photo;
//! * a crash between `remove(dest)` and `rename` leaves the destination missing,
//!   which the next run detects through its "is the local copy still there?" check
//!   and re-copies. The manifest is never the reason to believe a file exists.

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::paths::RelPath;

use super::destination::FsProbe;
use super::enumerate::RemoteError;

/// Directory (inside the destination root) that holds in-flight transfers.
pub const STAGING_DIR: &str = ".incomplete";
/// Suffix for partially transferred files.
pub const PART_SUFFIX: &str = ".part";
/// App-owned directory inside a destination root (staging + version history).
/// Never enumerated as user content.
pub const BACKUP_DIR: &str = ".backup";
/// Subdirectory of [`BACKUP_DIR`] holding replaced versions of changed files.
pub const HISTORY_DIR: &str = "history";

/// How thoroughly a staged file is checked before it is accepted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyMode {
    /// Compare the byte count with the phone's reported size (fast, default).
    #[default]
    Size,
    /// Compare byte count and SHA-256 of the whole file (slow, opt-in).
    Hash,
}

impl VerifyMode {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Size => "size check",
            Self::Hash => "size and SHA-256 check",
        }
    }
}

/// Transfer failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum TransferError {
    #[error("could not prepare {path}: {message}")]
    Staging { path: PathBuf, message: String },

    #[error("local i/o error at {path}: {source}")]
    Local {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("the copy of {path} is incomplete: expected {expected} bytes, stored {actual}")]
    SizeMismatch {
        path: PathBuf,
        expected: u64,
        actual: u64,
    },

    #[error("the copy of {path} does not match the phone's bytes (SHA-256 mismatch)")]
    HashMismatch {
        path: PathBuf,
        expected: String,
        actual: String,
    },

    #[error("could not move the finished copy of {path} into place: {source}")]
    Commit {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path} is a directory in the backup, so the file cannot be stored there")]
    DestinationBlocked { path: PathBuf },

    #[error("could not preserve the previous version of {path}: {source}")]
    Archive {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(transparent)]
    Destination(#[from] super::destination::DestError),

    #[error(transparent)]
    Remote(#[from] RemoteError),
}

/// A transfer that has a staging target and a final target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StagedTransfer {
    pub rel: RelPath,
    pub final_path: PathBuf,
    pub staging_path: PathBuf,
}

/// Result of a completed transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransferOutcome {
    pub final_path: PathBuf,
    pub bytes: u64,
    /// Content hash, present when [`VerifyMode::Hash`] was used.
    pub content_hash: Option<String>,
    /// Where the version that was replaced went, if there was one.
    pub archived: Option<PathBuf>,
}

/// Asks [`commit`] to preserve the file it is about to replace.
///
/// Replacing a file the user may already have relied on is never silent: the old
/// copy is moved into `<destination>/.backup/history/<rel>/<stamp>-<hash8>/` and
/// the new copy takes its place (plan E05.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArchiveRequest {
    /// Destination root of the job, i.e. the parent of [`BACKUP_DIR`].
    pub destination_root: PathBuf,
    pub rel: RelPath,
    /// Short stamp for the history folder name (job start, unix seconds).
    pub stamp: String,
    /// Hash recorded for the version on disk, when the manifest knows it.
    pub known_hash: Option<String>,
}

/// `<root>/.backup`
pub fn backup_dir(root: &Path) -> PathBuf {
    root.join(BACKUP_DIR)
}

/// `<root>/.backup/history`
pub fn history_dir(root: &Path) -> PathBuf {
    backup_dir(root).join(HISTORY_DIR)
}

/// `<root>/.incomplete`
pub fn staging_dir(root: &Path) -> PathBuf {
    root.join(STAGING_DIR)
}

/// `<root>/.incomplete/<rel>.part`
pub fn staging_path_for(root: &Path, rel: &RelPath) -> PathBuf {
    let mut path = staging_dir(root);
    for segment in rel.as_str().split('/') {
        path.push(segment);
    }
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".to_string());
    path.set_file_name(format!("{name}{PART_SUFFIX}"));
    path
}

/// True when a path belongs to our own staging area.
pub fn is_staging_path(root: &Path, path: &Path) -> bool {
    path.starts_with(staging_dir(root))
}

/// Create the staging directory and return the transfer targets.
pub fn begin(
    probe: &dyn FsProbe,
    root: &Path,
    rel: &RelPath,
    final_path: &Path,
) -> Result<StagedTransfer, TransferError> {
    let staging = staging_path_for(root, rel);
    if let Some(parent) = staging.parent() {
        probe
            .ensure_dir(parent)
            .map_err(|err| staging_error(parent, err))?;
    }
    if let Some(parent) = final_path.parent() {
        probe
            .ensure_dir(parent)
            .map_err(|err| staging_error(parent, err))?;
    }
    Ok(StagedTransfer {
        rel: rel.clone(),
        final_path: final_path.to_path_buf(),
        staging_path: staging,
    })
}

fn staging_error(path: &Path, err: super::destination::DestError) -> TransferError {
    TransferError::Staging {
        path: path.to_path_buf(),
        message: err.to_string(),
    }
}

/// Stream a file through SHA-256.
pub fn hash_file(path: &Path) -> Result<String, TransferError> {
    let mut file = File::open(path).map_err(|source| TransferError::Local {
        path: path.to_path_buf(),
        source,
    })?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 128 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|source| TransferError::Local {
                path: path.to_path_buf(),
                source,
            })?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Byte size of a local file.
pub fn file_size(path: &Path) -> Result<u64, TransferError> {
    let metadata = std::fs::metadata(path).map_err(|source| TransferError::Local {
        path: path.to_path_buf(),
        source,
    })?;
    Ok(metadata.len())
}

/// Check a staged file against the phone's reported size, and — when a digest of
/// the source is available — against the phone's own bytes.
///
/// `expected_hash` is the digest the *device* computed (`sha256sum` on the remote
/// file), which is the only way to catch a transfer that produced the right
/// number of wrong bytes. `None` means "the phone could not tell us", which is
/// **not** a pass: it is recorded as unknown, and only the size was checked.
pub fn verify_staged(
    path: &Path,
    expected_size: u64,
    mode: VerifyMode,
    expected_hash: Option<&str>,
) -> Result<Option<String>, TransferError> {
    let actual = file_size(path)?;
    if actual != expected_size {
        return Err(TransferError::SizeMismatch {
            path: path.to_path_buf(),
            expected: expected_size,
            actual,
        });
    }
    match mode {
        VerifyMode::Size => Ok(None),
        VerifyMode::Hash => {
            let local = hash_file(path)?;
            if let Some(expected) = expected_hash {
                if !local.eq_ignore_ascii_case(expected.trim()) {
                    return Err(TransferError::HashMismatch {
                        path: path.to_path_buf(),
                        expected: expected.trim().to_string(),
                        actual: local,
                    });
                }
            } else {
                tracing::debug!(
                    path = %path.display(),
                    "the phone reported no digest; recording the local one as evidence only"
                );
            }
            Ok(Some(local))
        }
    }
}

/// Verify and move a staged file to its final location.
///
/// `archive` decides what happens to a file that is already there:
///
/// * `Some(request)` — the previous version is moved into the history tree first;
///   if that move fails, the copy is aborted and **nothing** is deleted. This is
///   what the engine always passes.
/// * `None` — the previous file is removed and replaced in place. Only used by
///   callers that already know the destination is ours and disposable.
///
/// Either way the rename happens last, so a crash can only ever leave a file
/// missing (re-copied on the next run) — never a truncated file that looks fine.
pub fn commit(
    staged: &StagedTransfer,
    expected_size: u64,
    mode: VerifyMode,
    expected_hash: Option<&str>,
    archive: Option<ArchiveRequest>,
) -> Result<TransferOutcome, TransferError> {
    let hash = verify_staged(&staged.staging_path, expected_size, mode, expected_hash)?;

    let mut archived = None;
    if let Ok(metadata) = std::fs::metadata(&staged.final_path) {
        if metadata.is_dir() {
            return Err(TransferError::DestinationBlocked {
                path: staged.final_path.clone(),
            });
        }
        match archive {
            Some(request) => {
                let target = archive_existing(&staged.final_path, &request)?;
                tracing::info!(
                    path = %staged.final_path.display(),
                    archived = %target.display(),
                    "kept the previous version of a changed file"
                );
                archived = Some(target);
            }
            None => {
                tracing::info!(
                    path = %staged.final_path.display(),
                    "replacing the previous copy of a changed file"
                );
                std::fs::remove_file(&staged.final_path).map_err(|source| {
                    TransferError::Commit {
                        path: staged.final_path.clone(),
                        source,
                    }
                })?;
            }
        }
    }

    std::fs::rename(&staged.staging_path, &staged.final_path).map_err(|source| {
        TransferError::Commit {
            path: staged.final_path.clone(),
            source,
        }
    })?;

    Ok(TransferOutcome {
        final_path: staged.final_path.clone(),
        bytes: expected_size,
        content_hash: hash,
        archived,
    })
}

/// Where the previous version of `rel` would be stored.
///
/// `<destination>/.backup/history/<rel directories>/<stamp>-<hash8>/<filename>`
/// A counter is appended when that name is taken, so two jobs in the same second
/// can never overwrite each other's history entry.
pub fn history_target(request: &ArchiveRequest, hash8: &str) -> PathBuf {
    let mut dir = history_dir(&request.destination_root);
    if let Some(parent) = request.rel.parent() {
        for segment in parent.as_str().split('/') {
            dir.push(segment);
        }
    }
    dir.push(format!(
        "{}-{}",
        sanitize_stamp(&request.stamp),
        sanitize_stamp(hash8)
    ));
    unique_dir(dir).join(request.rel.file_name())
}

/// Move an existing file into the version history, returning its new path.
///
/// On failure the original file is left exactly where it was, so the caller can
/// abort the copy without any risk of data loss (plan E05.5.3).
pub fn archive_existing(
    existing: &Path,
    request: &ArchiveRequest,
) -> Result<PathBuf, TransferError> {
    let hash8 = match request.known_hash.as_deref() {
        Some(hash) => short_hash(hash),
        None => match hash_file(existing) {
            Ok(hash) => short_hash(&hash),
            Err(err) => {
                tracing::warn!(
                    path = %existing.display(),
                    error = %err,
                    "could not hash the previous version; filing it without a digest"
                );
                "nohash".to_string()
            }
        },
    };
    let target = history_target(request, &hash8);
    let parent = target.parent().ok_or_else(|| TransferError::Archive {
        path: target.clone(),
        source: std::io::Error::new(std::io::ErrorKind::InvalidInput, "no parent directory"),
    })?;
    std::fs::create_dir_all(parent).map_err(|source| TransferError::Archive {
        path: parent.to_path_buf(),
        source,
    })?;
    std::fs::rename(existing, &target).map_err(|source| TransferError::Archive {
        path: existing.to_path_buf(),
        source,
    })?;
    Ok(target)
}

/// First 8 characters of a hex digest, or the whole thing when it is shorter.
fn short_hash(hash: &str) -> String {
    hash.chars().take(8).collect()
}

/// Keep a stamp usable as a path component.
fn sanitize_stamp(stamp: &str) -> String {
    let cleaned: String = stamp
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .collect();
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned
    }
}

/// Append `-1`, `-2`, … until the path is free.
fn unique_dir(base: PathBuf) -> PathBuf {
    if !base.exists() {
        return base;
    }
    let name = base
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "entry".to_string());
    for suffix in 1..10_000u32 {
        let candidate = base.with_file_name(format!("{name}-{suffix}"));
        if !candidate.exists() {
            return candidate;
        }
    }
    base
}

/// Delete a staged file, ignoring "already gone".
pub fn discard(staged: &StagedTransfer) {
    match std::fs::remove_file(&staged.staging_path) {
        Ok(()) => {
            tracing::debug!(path = %staged.staging_path.display(), "discarded partial transfer")
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => tracing::warn!(
            path = %staged.staging_path.display(),
            error = %err,
            "could not remove a partial transfer"
        ),
    }
}

/// Remove every `*.part` file under `root/.incomplete`, returning what was removed.
///
/// Used at startup (plan F4.4: "leftover partial files are cleaned up on next
/// launch") and never touches anything outside the staging directory.
pub fn sweep_partials(root: &Path) -> Vec<PathBuf> {
    let staging = staging_dir(root);
    let mut removed = Vec::new();
    if !staging.is_dir() {
        return removed;
    }
    let mut stack = vec![staging.clone()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(entries) => entries,
            Err(err) => {
                tracing::debug!(dir = %dir.display(), error = %err, "cannot read staging directory");
                continue;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.to_string_lossy().ends_with(PART_SUFFIX) {
                match std::fs::remove_file(&path) {
                    Ok(()) => removed.push(path),
                    Err(err) => tracing::warn!(
                        path = %path.display(),
                        error = %err,
                        "could not remove a leftover partial file"
                    ),
                }
            }
        }
    }
    if !removed.is_empty() {
        tracing::info!(count = removed.len(), "cleaned up partial transfers");
    }
    removed
}

/// Human-readable byte count for progress messages.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0usize;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockFsProbe;

    fn rel(path: &str) -> RelPath {
        RelPath::parse(path).unwrap()
    }

    fn staged_in(root: &Path, path: &str, content: &[u8]) -> StagedTransfer {
        let rel = rel(path);
        let final_path = root.join(path);
        std::fs::create_dir_all(staging_dir(root).join(rel.parent_str().unwrap_or(""))).unwrap();
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let staged = StagedTransfer {
            final_path,
            staging_path: staging_path_for(root, &rel),
            rel,
        };
        std::fs::write(&staged.staging_path, content).unwrap();
        staged
    }

    fn archive_request(root: &Path, path: &str) -> ArchiveRequest {
        ArchiveRequest {
            destination_root: root.to_path_buf(),
            rel: rel(path),
            stamp: "1700000000".into(),
            known_hash: None,
        }
    }

    #[test]
    fn staging_paths_keep_the_tree_and_add_a_suffix() {
        let root = Path::new("/backups/Phone");
        let path = staging_path_for(root, &rel("DCIM/Camera/a.jpg"));
        assert_eq!(
            path,
            PathBuf::from("/backups/Phone/.incomplete/DCIM/Camera/a.jpg.part")
        );
        assert!(is_staging_path(root, &path));
        assert!(!is_staging_path(
            root,
            Path::new("/backups/Phone/DCIM/a.jpg")
        ));
    }

    #[test]
    fn begin_creates_staging_and_final_directories() {
        let probe = MockFsProbe::new();
        let root = Path::new("/backups/Phone");
        let staged = begin(&probe, root, &rel("DCIM/a.jpg"), &root.join("DCIM/a.jpg")).unwrap();
        let created = probe.created.lock().unwrap().clone();
        assert!(created.contains(&PathBuf::from("/backups/Phone/.incomplete/DCIM")));
        assert!(created.contains(&PathBuf::from("/backups/Phone/DCIM")));
        assert_eq!(
            staged.final_path,
            PathBuf::from("/backups/Phone/DCIM/a.jpg")
        );
    }

    #[test]
    fn size_mismatch_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.part");
        std::fs::write(&file, b"hello").unwrap();
        let err = verify_staged(&file, 10, VerifyMode::Size, None).unwrap_err();
        match err {
            TransferError::SizeMismatch {
                expected, actual, ..
            } => {
                assert_eq!(expected, 10);
                assert_eq!(actual, 5);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn size_check_accepts_matching_files_without_hashing() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.part");
        std::fs::write(&file, b"hello").unwrap();
        assert_eq!(
            verify_staged(&file, 5, VerifyMode::Size, None).unwrap(),
            None
        );
    }

    #[test]
    fn hash_check_produces_a_stable_digest() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.part");
        std::fs::write(&file, b"hello").unwrap();
        let hash = verify_staged(&file, 5, VerifyMode::Hash, None)
            .unwrap()
            .unwrap();
        // Well-known SHA-256 of "hello".
        assert_eq!(
            hash,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn a_wrong_digest_is_a_verification_failure() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.part");
        std::fs::write(&file, b"hello").unwrap();
        let wrong = "0".repeat(64);
        let err = verify_staged(&file, 5, VerifyMode::Hash, Some(&wrong)).unwrap_err();
        assert!(
            matches!(err, TransferError::HashMismatch { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn the_digest_check_is_case_insensitive_and_trimmed() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("a.part");
        std::fs::write(&file, b"hello").unwrap();
        let upper = "2CF24DBA5FB0A30E26E83B2AC5B9E29E1B161E5C1FA7425E73043362938B9824\n";
        let hash = verify_staged(&file, 5, VerifyMode::Hash, Some(upper))
            .unwrap()
            .unwrap();
        assert!(hash.starts_with("2cf24dba"));
    }

    #[test]
    fn commit_moves_the_staged_file_into_place() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let staged = staged_in(root, "DCIM/a.jpg", b"content");

        let outcome = commit(&staged, 7, VerifyMode::Size, None, None).unwrap();
        assert_eq!(outcome.bytes, 7);
        assert_eq!(outcome.archived, None);
        assert_eq!(std::fs::read(root.join("DCIM/a.jpg")).unwrap(), b"content");
        assert!(
            !staged.staging_path.exists(),
            "staging file was moved, not copied"
        );
    }

    #[test]
    fn a_failed_verification_never_touches_the_final_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let staged = staged_in(root, "DCIM/a.jpg", b"truncated");
        std::fs::write(&staged.final_path, b"good old copy").unwrap();

        let err = commit(&staged, 999, VerifyMode::Size, None, None).unwrap_err();
        assert!(matches!(err, TransferError::SizeMismatch { .. }));
        assert_eq!(
            std::fs::read(&staged.final_path).unwrap(),
            b"good old copy",
            "the previous good copy must survive a failed transfer"
        );
    }

    #[test]
    fn commit_replaces_the_previous_copy_of_the_same_source_file() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let staged = staged_in(root, "DCIM/a.jpg", b"new content");
        std::fs::write(&staged.final_path, b"old").unwrap();
        commit(&staged, 11, VerifyMode::Size, None, None).unwrap();
        assert_eq!(std::fs::read(&staged.final_path).unwrap(), b"new content");
    }

    #[test]
    fn a_changed_file_keeps_its_previous_version_in_the_history() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let staged = staged_in(root, "DCIM/Camera/a.jpg", b"new content");
        std::fs::write(&staged.final_path, b"old bytes").unwrap();

        let request = ArchiveRequest {
            known_hash: Some("abcdef0123456789".into()),
            ..archive_request(root, "DCIM/Camera/a.jpg")
        };
        let outcome = commit(&staged, 11, VerifyMode::Size, None, Some(request)).unwrap();

        assert_eq!(std::fs::read(&staged.final_path).unwrap(), b"new content");
        let archived = outcome.archived.expect("the old version was preserved");
        assert_eq!(std::fs::read(&archived).unwrap(), b"old bytes");
        assert_eq!(
            archived,
            root.join(".backup/history/DCIM/Camera/1700000000-abcdef01/a.jpg")
        );
        // The name stays plain: no "-1" version suffix on user-visible files.
        assert_eq!(staged.final_path.file_name().unwrap(), "a.jpg");
    }

    #[test]
    fn history_entries_do_not_collide_within_the_same_second() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let request = archive_request(root, "DCIM/a.jpg");

        std::fs::create_dir_all(root.join("DCIM")).unwrap();
        std::fs::write(root.join("DCIM/a.jpg"), b"first").unwrap();
        let first = archive_existing(&root.join("DCIM/a.jpg"), &request).unwrap();
        std::fs::write(root.join("DCIM/a.jpg"), b"second").unwrap();
        let second = archive_existing(&root.join("DCIM/a.jpg"), &request).unwrap();

        assert_ne!(first, second);
        assert_eq!(std::fs::read(&first).unwrap(), b"first");
        assert_eq!(std::fs::read(&second).unwrap(), b"second");
    }

    #[test]
    fn a_failed_archive_aborts_the_copy_and_keeps_both_files() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let staged = staged_in(root, "DCIM/a.jpg", b"new content");
        std::fs::write(&staged.final_path, b"old bytes").unwrap();
        // A file where the history directory should go makes the archive fail.
        std::fs::create_dir_all(backup_dir(root)).unwrap();
        std::fs::write(history_dir(root), b"not a directory").unwrap();

        let err = commit(
            &staged,
            11,
            VerifyMode::Size,
            None,
            Some(archive_request(root, "DCIM/a.jpg")),
        )
        .unwrap_err();
        assert!(matches!(err, TransferError::Archive { .. }), "got {err:?}");
        assert_eq!(
            std::fs::read(&staged.final_path).unwrap(),
            b"old bytes",
            "the old file is still in place"
        );
        assert!(
            staged.staging_path.exists(),
            "the caller decides what to do with the partial"
        );
    }

    #[test]
    fn a_directory_in_the_way_is_reported_not_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let staged = staged_in(root, "DCIM/a.jpg", b"x");
        std::fs::create_dir_all(&staged.final_path).unwrap();
        let err = commit(&staged, 1, VerifyMode::Size, None, None).unwrap_err();
        assert!(matches!(err, TransferError::DestinationBlocked { .. }));
        assert!(staged.final_path.is_dir(), "nothing was deleted");
    }

    #[test]
    fn discard_removes_the_partial_file_and_shrugs_at_missing_ones() {
        let dir = tempfile::tempdir().unwrap();
        let staged = StagedTransfer {
            rel: rel("a"),
            final_path: dir.path().join("a"),
            staging_path: dir.path().join("a.part"),
        };
        std::fs::write(&staged.staging_path, b"x").unwrap();
        discard(&staged);
        assert!(!staged.staging_path.exists());
        discard(&staged); // must not panic
    }

    #[test]
    fn sweep_removes_partials_recursively_and_nothing_else() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let nested = root.join(".incomplete/DCIM/Camera");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("a.jpg.part"), b"x").unwrap();
        std::fs::write(root.join(".incomplete/b.part"), b"x").unwrap();
        std::fs::write(root.join(".incomplete/keepme.txt"), b"x").unwrap();
        std::fs::write(root.join("DCIM-real.jpg"), b"x").unwrap();

        let removed = sweep_partials(root);
        assert_eq!(removed.len(), 2);
        assert!(root.join(".incomplete/keepme.txt").exists());
        assert!(root.join("DCIM-real.jpg").exists());
        assert!(
            sweep_partials(root).is_empty(),
            "second sweep finds nothing"
        );
    }

    #[test]
    fn sweep_on_a_missing_root_is_harmless() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sweep_partials(&dir.path().join("nope")).is_empty());
    }

    #[test]
    fn human_bytes_is_readable() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(999), "999 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024 * 3 / 2), "1.5 MiB");
        assert_eq!(human_bytes(1024u64.pow(4)), "1.0 TiB");
    }

    #[test]
    fn verify_mode_reports_itself() {
        assert_eq!(VerifyMode::Size.describe(), "size check");
        assert_eq!(VerifyMode::default(), VerifyMode::Size);
    }
}
