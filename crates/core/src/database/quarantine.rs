//! Starting over when the manifest cannot be read (plan §9, §13).
//!
//! The manifest is an index, never the only copy of the user's data (plan §9),
//! so an unreadable manifest must never be a dead end: the app quarantines the
//! file — moving it aside, keeping every byte — and starts with an empty one.
//! The next backup re-copies what the manifest no longer knows about, which is
//! the same work a first run does.
//!
//! A repair tool for the damaged file is explicitly out of scope (§13); what we
//! owe the user is the file itself plus an honest explanation, which is what
//! [`DbError::repair_guidance`] is for.

use std::path::{Path, PathBuf};

use crate::clock;

use super::{Db, DbError};

/// The manifest that was moved aside, and where it went.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuarantineReport {
    /// The path the app is using now (`…/manifest.db`).
    pub original: PathBuf,
    /// Where the unreadable file was moved (`…/manifest.corrupt-<unix>.db`).
    pub stash: PathBuf,
    /// Sidecars that were moved with it (`-wal`, `-shm`), in that order.
    pub sidecars: Vec<PathBuf>,
    /// Ready-to-display explanation for the user.
    pub guidance: String,
}

impl QuarantineReport {
    /// One-paragraph summary for a log line.
    pub fn summary(&self) -> String {
        format!(
            "the manifest at {} was unreadable and has been kept at {}",
            self.original.display(),
            self.stash.display()
        )
    }
}

/// Open the manifest, replacing an unreadable one with a fresh, empty database.
///
/// * Opens normally — the common case — with no side effects at all.
/// * [`DbError::is_corrupt`] — moves the file (and its `-wal`/`-shm` sidecars)
///   to `<name>.corrupt-<unix>.<ext>` and opens a new database. The old bytes are
///   untouched, just renamed.
/// * Anything else — permission problems, a newer schema, a full disk — is
///   returned as-is. Silently rotating a database because of a permissions error
///   would be destructive nonsense.
pub fn open_or_quarantine(path: &Path) -> Result<(Db, Option<QuarantineReport>), DbError> {
    match Db::open_file(path) {
        Ok(db) => Ok((db, None)),
        Err(err) if err.is_corrupt() => {
            let (stash, sidecars) = stash_aside(path, clock::now_unix())?;
            // A fresh database in the same place; the app continues from here.
            let db = Db::open_file(path)?;
            let guidance = DbError::repair_guidance(path, &stash);
            Ok((
                db,
                Some(QuarantineReport {
                    original: path.to_path_buf(),
                    stash,
                    sidecars,
                    guidance,
                }),
            ))
        }
        Err(err) => Err(err),
    }
}

/// Move `path` and its SQLite sidecars to `<stem>.corrupt-<stamp><ext>`.
///
/// Returns the new path of the database plus the sidecar paths that moved. A
/// sidecar that does not exist is not an error; a sidecar that cannot be moved
/// *is* (we would be leaving half a database behind).
fn stash_aside(path: &Path, stamp: i64) -> Result<(PathBuf, Vec<PathBuf>), DbError> {
    let stash = corrupt_path(path, stamp);
    std::fs::rename(path, &stash).map_err(|source| DbError::Io {
        path: path.to_path_buf(),
        source,
    })?;

    let mut moved = Vec::new();
    for suffix in ["-wal", "-shm"] {
        let sidecar = sidecar_path(path, suffix);
        if !sidecar.exists() {
            continue;
        }
        let target = sidecar_path(&stash, suffix);
        std::fs::rename(&sidecar, &target).map_err(|source| DbError::Io {
            path: sidecar.clone(),
            source,
        })?;
        moved.push(target);
    }
    Ok((stash, moved))
}

/// `<dir>/<stem>.corrupt-<stamp><ext>` — `manifest.db` →
/// `manifest.corrupt-1767225600.db`.
fn corrupt_path(path: &Path, stamp: i64) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "manifest".to_string());
    let extension = path.extension().map(|e| e.to_string_lossy().to_string());
    let name = match extension {
        Some(ext) => format!("{stem}.corrupt-{stamp}.{ext}"),
        None => format!("{stem}.corrupt-{stamp}"),
    };
    path.with_file_name(name)
}

/// `manifest.db` + `-wal` → `manifest.db-wal`, which is how SQLite names them.
fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    path.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_names_keep_the_extension() {
        let path = Path::new("/data/manifest.db");
        assert_eq!(
            corrupt_path(path, 42),
            PathBuf::from("/data/manifest.corrupt-42.db")
        );
        assert_eq!(
            corrupt_path(Path::new("/data/manifest"), 42),
            PathBuf::from("/data/manifest.corrupt-42")
        );
        assert_eq!(
            sidecar_path(path, "-wal"),
            PathBuf::from("/data/manifest.db-wal")
        );
    }

    #[test]
    fn a_healthy_database_is_opened_as_is() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        let (db, report) = open_or_quarantine(&path).unwrap();
        assert!(report.is_none());
        assert_eq!(
            super::super::files::manifest_for_device(&db, 1)
                .unwrap()
                .len(),
            0
        );
        let leftovers = corrupt_leftovers(dir.path());
        assert!(
            leftovers.is_empty(),
            "nothing was moved aside: {leftovers:?}"
        );
    }

    #[test]
    fn a_truncated_database_is_quarantined_and_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        drop(Db::open_file(&path).unwrap());
        // Chop the file down to the first page marker: not a database any more.
        let data = std::fs::read(&path).unwrap();
        std::fs::write(&path, &data[..data.len() / 2]).unwrap();

        let (db, report) = open_or_quarantine(&path).unwrap();
        let report = report.expect("a truncated database must be quarantined");
        assert_eq!(report.original, path);
        assert!(report.stash.exists(), "the old bytes are kept");
        assert!(report.stash.to_string_lossy().contains(".corrupt-"));
        assert!(report.guidance.contains("manifest only records"));
        assert!(report.summary().contains("unreadable"));

        // The replacement really works.
        super::super::files::manifest_for_device(&db, 1).unwrap();
        let version: u32 = db
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(
            version,
            super::super::SCHEMA_VERSION,
            "the fresh database is migrated"
        );
    }

    #[test]
    fn a_garbage_file_is_quarantined_too() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        std::fs::write(&path, b"this is not a sqlite database, it is a poem").unwrap();

        let (_, report) = open_or_quarantine(&path).unwrap();
        assert!(report.is_some());
    }

    #[test]
    fn sidecars_move_with_their_database() {
        // Exercised against `stash_aside` directly: SQLite edits its own `-wal`
        // while failing to open a damaged file, so driving this through a real
        // open would be testing SQLite, not this code.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        std::fs::write(&path, b"not a database").unwrap();
        std::fs::write(sidecar_path(&path, "-wal"), b"leftover wal").unwrap();
        std::fs::write(sidecar_path(&path, "-shm"), b"leftover shm").unwrap();

        let (stash, sidecars) = stash_aside(&path, 42).unwrap();
        assert_eq!(stash, dir.path().join("manifest.corrupt-42.db"));
        assert_eq!(sidecars.len(), 2, "{sidecars:?}");
        assert!(std::fs::read(&stash).is_ok(), "the database bytes moved");
        assert!(stash.with_file_name("manifest.corrupt-42.db-wal").exists());
        assert!(stash.with_file_name("manifest.corrupt-42.db-shm").exists());
        assert!(!path.exists());
        assert!(!sidecar_path(&path, "-wal").exists());
    }

    #[test]
    fn a_missing_sidecar_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        std::fs::write(&path, b"not a database").unwrap();
        let (_, sidecars) = stash_aside(&path, 7).unwrap();
        assert!(sidecars.is_empty());
    }

    fn corrupt_leftovers(dir: &Path) -> Vec<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.file_name().to_string_lossy().to_string())
            .filter(|name| name.contains(".corrupt-"))
            .collect()
    }

    #[test]
    fn a_version_skew_is_never_quarantined() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.db");
        {
            let db = Db::open_file(&path).unwrap();
            let conn = db.conn();
            conn.pragma_update(None, "user_version", 9999u32).unwrap();
        }
        let err = open_or_quarantine(&path).unwrap_err();
        assert!(
            matches!(err, DbError::VersionSkew { found: 9999, .. }),
            "{err}"
        );
        assert!(!err.is_corrupt());
        // The user's database is still exactly where it was.
        assert!(path.exists());
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .map(|e| e.file_name().to_string_lossy().contains(".corrupt-"))
                    .unwrap_or(false))
                .count(),
            0
        );
    }
}
