//! Where a backup goes, and whether it can be written right now.
//!
//! Destination problems are detected **before** a transfer starts and re-checked
//! while it runs (plan F3.5): a removable drive that was unplugged must surface as
//! a clear message, never as a half-written file that looks fine.

use std::path::{Path, PathBuf};

use crate::config::AppConfig;
use crate::database::devices::DeviceRow;
use crate::paths::sanitize_component;

/// Free space and writability of a destination.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpaceInfo {
    /// `None` when the platform cannot report it — treated as "unknown", never as
    /// "plenty".
    pub free_bytes: Option<u64>,
    pub writable: bool,
}

/// Destination failures.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DestError {
    #[error("no backup location has been chosen yet")]
    NotConfigured,

    #[error("the backup location {0} does not exist (is the drive still connected?)")]
    Missing(PathBuf),

    #[error("could not create {path}: {message}")]
    CannotCreate { path: PathBuf, message: String },

    #[error("{path} is not writable: {message}")]
    NotWritable { path: PathBuf, message: String },

    #[error("not enough free space in {path}: need {needed} bytes, {available} available", available = available.map(|a| a.to_string()).unwrap_or_else(|| "unknown".into()))]
    InsufficientSpace {
        path: PathBuf,
        needed: u64,
        available: Option<u64>,
    },

    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// The filesystem operations the destination logic needs.
///
/// Implemented for real in `android-backup-platform` and faked in
/// [`crate::mock::MockFsProbe`], so every destination rule is testable without
/// touching a disk.
pub trait FsProbe: Send + Sync {
    /// Create the directory (and parents) if needed.
    fn ensure_dir(&self, path: &Path) -> Result<(), DestError>;
    /// Prove the directory accepts writes.
    fn probe_writable(&self, path: &Path) -> Result<(), DestError>;
    /// Free bytes, `Ok(None)` when the platform cannot tell.
    fn free_bytes(&self, path: &Path) -> Result<Option<u64>, DestError>;
    /// Does the path exist right now?
    fn exists(&self, path: &Path) -> bool;
    /// Size of a local file, `None` when it is missing or unreadable.
    ///
    /// Used to notice a truncated copy left behind by a crash or a full disk,
    /// which the manifest alone cannot reveal.
    fn local_size(&self, path: &Path) -> Option<u64>;
}

/// The real filesystem, used by the app and by the end-to-end tests.
///
/// It lives in `core` rather than in the platform crate because the backup engine
/// is only meaningful with a working probe, and keeping it here lets the whole
/// engine be exercised against a temporary directory on any OS (plan Wave 5 gate).
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalFsProbe;

impl LocalFsProbe {
    pub fn new() -> Self {
        Self
    }

    /// Free bytes on the volume holding `path`, `None` when it cannot be read.
    pub fn free_space(path: &Path) -> Option<u64> {
        free_space_of(path)
    }
}

impl FsProbe for LocalFsProbe {
    fn ensure_dir(&self, path: &Path) -> Result<(), DestError> {
        std::fs::create_dir_all(path).map_err(|err| DestError::CannotCreate {
            path: path.to_path_buf(),
            message: err.to_string(),
        })
    }

    fn probe_writable(&self, path: &Path) -> Result<(), DestError> {
        let probe = path.join(".android-backup-write-test");
        match std::fs::write(&probe, b"") {
            Ok(()) => {
                // Leaving the probe behind would be litter; failing to remove it is
                // not a reason to refuse the backup.
                if let Err(err) = std::fs::remove_file(&probe) {
                    tracing::debug!(path = %probe.display(), error = %err, "could not remove the write probe");
                }
                Ok(())
            }
            Err(err) => Err(DestError::NotWritable {
                path: path.to_path_buf(),
                message: err.to_string(),
            }),
        }
    }

    fn free_bytes(&self, path: &Path) -> Result<Option<u64>, DestError> {
        Ok(free_space_of(path))
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn local_size(&self, path: &Path) -> Option<u64> {
        std::fs::metadata(path)
            .ok()
            .filter(|meta| meta.is_file())
            .map(|meta| meta.len())
    }
}

/// Free space of the volume that `path` lives on.
fn free_space_of(path: &Path) -> Option<u64> {
    let canonical = canonical_for_comparison(path);
    let disks = sysinfo::Disks::new_with_refreshed_list();
    disks
        .list()
        .iter()
        // Windows answers `canonicalize` with a verbatim path (`\\?\C:\…`), which
        // is a different prefix component from the `C:\` a mount point is written
        // with — so `starts_with` refused to match the very volume the path was
        // on, and every existing directory read as "free space unknown". Both
        // ends go through the same canonicalization, which puts them in one
        // namespace; a mount point that refuses (a dead network share) keeps its
        // plain form, exactly like the queried path does.
        .map(|disk| {
            (
                canonical_for_comparison(disk.mount_point()),
                disk.available_space(),
            )
        })
        .filter(|(mount, _)| canonical.starts_with(mount))
        // The longest matching mount point is the volume actually in play.
        .max_by_key(|(mount, _)| mount.as_os_str().len())
        .map(|(_, available)| available)
}

/// The path as the OS names volumes, for prefix comparison only: canonical when
/// the path exists (so both ends of the comparison carry the same prefix), the
/// plain form when it does not.
fn canonical_for_comparison(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

/// Verify a destination is present, writable and has room for `needed` bytes.
pub fn validate(
    probe: &dyn FsProbe,
    path: &Path,
    needed: Option<u64>,
) -> Result<SpaceInfo, DestError> {
    if !probe.exists(path) {
        return Err(DestError::Missing(path.to_path_buf()));
    }
    probe.ensure_dir(path)?;
    probe.probe_writable(path)?;
    let free_bytes = probe.free_bytes(path)?;
    if let (Some(needed), Some(available)) = (needed, free_bytes) {
        if needed > available {
            return Err(DestError::InsufficientSpace {
                path: path.to_path_buf(),
                needed,
                available: Some(available),
            });
        }
    }
    Ok(SpaceInfo {
        free_bytes,
        writable: true,
    })
}

/// A resolved destination for one device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveDestination {
    /// Directory the device's backup tree is rooted at.
    pub root: PathBuf,
    /// True when `root` is `global_backup_dir/<device folder>`.
    pub device_subfolder: bool,
}

/// Filesystem-safe folder name for a device.
///
/// A display name is free text typed by a human, not a path: separators are
/// flattened first, so `Pixel/8` stays one folder and `/etc` can never move a
/// backup out of the configured directory.
pub fn device_folder_name(display_name: &str) -> String {
    let flattened = display_name.trim().replace(['/', '\\'], "_");
    let cleaned = sanitize_component(&flattened);
    if cleaned.trim_matches('_').is_empty() {
        // Nothing usable in the name (empty, spaces, separators only).
        "device".to_string()
    } else {
        cleaned
    }
}

/// Resolve the destination for `device`.
///
/// A device-specific path always wins. Otherwise the device gets its own
/// subfolder under the global directory so two phones can never write over each
/// other's files; if two devices would share a name, the serial tail is appended
/// deterministically.
pub fn resolve(
    device: &DeviceRow,
    others: &[DeviceRow],
    global: Option<&Path>,
) -> Result<EffectiveDestination, DestError> {
    if let Some(explicit) = &device.destination_path {
        return Ok(EffectiveDestination {
            root: explicit.clone(),
            device_subfolder: false,
        });
    }
    let global = global.ok_or(DestError::NotConfigured)?;
    let base = device_folder_name(&device.display_name);
    let clash = others
        .iter()
        .filter(|other| other.id != device.id)
        .any(|other| {
            other.destination_path.is_none() && device_folder_name(&other.display_name) == base
        });
    let folder = if clash {
        format!("{base}_{}", serial_tail(&device.serial))
    } else {
        base
    };
    Ok(EffectiveDestination {
        root: global.join(folder),
        device_subfolder: true,
    })
}

/// Convenience wrapper around [`resolve`] using [`AppConfig::global_backup_dir`].
pub fn resolve_with_config(
    device: &DeviceRow,
    others: &[DeviceRow],
    config: &AppConfig,
) -> Result<EffectiveDestination, DestError> {
    resolve(device, others, config.global_backup_dir.as_deref())
}

/// Last 4 (or fewer) characters of a serial, used to disambiguate folder names.
pub fn serial_tail(serial: &str) -> String {
    let cleaned: String = serial
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect();
    let start = cleaned.len().saturating_sub(4);
    cleaned[start..].to_string()
}

/// Decide whether a transfer may proceed given the remaining space.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceVerdict {
    Ok,
    /// The platform could not report free space; proceed, but say so in the log.
    Unknown,
    Low {
        needed: u64,
        available: u64,
    },
}

/// Compare a requirement against a probe reading.
pub fn space_verdict(space: SpaceInfo, needed: u64) -> SpaceVerdict {
    match space.free_bytes {
        None => SpaceVerdict::Unknown,
        Some(available) if needed > available => SpaceVerdict::Low { needed, available },
        Some(_) => SpaceVerdict::Ok,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockFsProbe;

    fn device(id: i64, name: &str, serial: &str, destination: Option<&str>) -> DeviceRow {
        DeviceRow {
            id,
            serial: serial.to_string(),
            display_name: name.to_string(),
            manufacturer: None,
            model: None,
            android_version: None,
            destination_path: destination.map(PathBuf::from),
            auto_schedule: crate::automation::Schedule::Off,
            created_at: "2026-01-01T00:00:00Z".into(),
            last_seen_at: "2026-01-01T00:00:00Z".into(),
        }
    }

    #[test]
    fn a_name_is_never_a_path() {
        let global = Path::new("/home/me/AndroidBackups");
        // A display name with separators stays a single folder inside `global`.
        for name in ["Pixel/8", "/etc", "..", "../../escape", "  ", "///", "a\\b"] {
            let dest = resolve(&device(1, name, "SER1", None), &[], Some(global)).unwrap();
            assert!(
                dest.root.starts_with(global),
                "{name:?} escaped the backup directory: {}",
                dest.root.display()
            );
            assert_eq!(
                dest.root.parent(),
                Some(global),
                "{name:?} produced a nested folder: {}",
                dest.root.display()
            );
        }
        assert_eq!(device_folder_name("/etc"), "_etc");
        assert_eq!(device_folder_name("Pixel/8"), "Pixel_8");
        assert_eq!(
            device_folder_name("Pixel/8/Pro \u{1}"),
            "Pixel_8_Pro_",
            "control characters are dropped, trailing space becomes _"
        );
    }

    #[test]
    fn explicit_device_path_wins() {
        let device = device(1, "Phone", "SER1", Some("/mnt/external/Phone"));
        let dest = resolve(&device, &[], Some(Path::new("/home/me/AndroidBackups"))).unwrap();
        assert_eq!(dest.root, PathBuf::from("/mnt/external/Phone"));
        assert!(!dest.device_subfolder);
    }

    #[test]
    fn global_directory_gets_a_device_subfolder() {
        let device = device(1, "Samsung SM-A536B", "SER1", None);
        let dest = resolve(&device, &[], Some(Path::new("/home/me/AndroidBackups"))).unwrap();
        assert_eq!(
            dest.root,
            PathBuf::from("/home/me/AndroidBackups/Samsung SM-A536B")
        );
        assert!(dest.device_subfolder);
    }

    #[test]
    fn no_location_at_all_is_an_error() {
        let device = device(1, "Phone", "SER1", None);
        assert!(matches!(
            resolve(&device, &[], None),
            Err(DestError::NotConfigured)
        ));
    }

    #[test]
    fn identical_device_names_get_distinct_folders() {
        let a = device(1, "Pixel", "ABCD1234", None);
        let b = device(2, "Pixel", "WXYZ9876", None);
        let dest_a = resolve(&a, &[a.clone(), b.clone()], Some(Path::new("/backups"))).unwrap();
        let dest_b = resolve(&b, &[a.clone(), b.clone()], Some(Path::new("/backups"))).unwrap();
        assert_ne!(dest_a.root, dest_b.root);
        assert_eq!(dest_a.root, PathBuf::from("/backups/Pixel_1234"));
        assert_eq!(dest_b.root, PathBuf::from("/backups/Pixel_9876"));
    }

    #[test]
    fn a_device_with_an_explicit_path_does_not_force_disambiguation() {
        let a = device(1, "Pixel", "ABCD1234", None);
        let b = device(2, "Pixel", "WXYZ9876", Some("/elsewhere"));
        let dest = resolve(&a, &[a.clone(), b.clone()], Some(Path::new("/backups"))).unwrap();
        assert_eq!(dest.root, PathBuf::from("/backups/Pixel"));
    }

    #[test]
    fn folder_names_are_sanitized() {
        assert_eq!(device_folder_name("Samsung SM-A536B"), "Samsung SM-A536B");
        assert_eq!(device_folder_name("Phone: work"), "Phone_ work");
        assert_eq!(device_folder_name("   "), "device");
        assert_eq!(device_folder_name("CON"), "CON_");
    }

    #[test]
    fn missing_destination_is_reported_before_any_write() {
        let probe = MockFsProbe::new();
        let err = validate(&probe, Path::new("/mnt/phone"), Some(10)).unwrap_err();
        assert!(matches!(err, DestError::Missing(_)));
        assert!(
            probe.created.lock().unwrap().is_empty(),
            "nothing was created"
        );
    }

    #[test]
    fn create_failure_is_reported() {
        let mut probe = MockFsProbe::new();
        probe.fail_create = true;
        probe
            .existing
            .lock()
            .unwrap()
            .push(PathBuf::from("/mnt/phone"));
        let err = validate(&probe, Path::new("/mnt/phone"), None).unwrap_err();
        assert!(matches!(err, DestError::CannotCreate { .. }));
    }

    #[test]
    fn insufficient_space_is_a_hard_failure() {
        let probe = MockFsProbe::new()
            .with_existing(&["/mnt/phone"])
            .free_bytes(100);
        let err = validate(&probe, Path::new("/mnt/phone"), Some(101)).unwrap_err();
        match err {
            DestError::InsufficientSpace {
                needed, available, ..
            } => {
                assert_eq!(needed, 101);
                assert_eq!(available, Some(100));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn enough_space_passes_and_reports_the_reading() {
        let probe = MockFsProbe::new()
            .with_existing(&["/mnt/phone"])
            .free_bytes(1_000);
        let info = validate(&probe, Path::new("/mnt/phone"), Some(999)).unwrap();
        assert_eq!(info.free_bytes, Some(1_000));
        assert!(info.writable);
    }

    #[test]
    fn unknown_space_cannot_block_a_transfer_but_is_reported() {
        let mut probe = MockFsProbe::new().with_existing(&["/mnt/phone"]);
        probe.free_bytes = None;
        let info = validate(&probe, Path::new("/mnt/phone"), Some(u64::MAX)).unwrap();
        assert_eq!(
            info.free_bytes, None,
            "unknown space is reported, not invented"
        );
        assert_eq!(
            space_verdict(
                SpaceInfo {
                    free_bytes: None,
                    writable: true
                },
                10
            ),
            SpaceVerdict::Unknown
        );
        assert_eq!(
            space_verdict(
                SpaceInfo {
                    free_bytes: Some(5),
                    writable: true
                },
                10
            ),
            SpaceVerdict::Low {
                needed: 10,
                available: 5
            }
        );
        assert_eq!(
            space_verdict(
                SpaceInfo {
                    free_bytes: Some(10),
                    writable: true
                },
                10
            ),
            SpaceVerdict::Ok
        );
    }

    #[test]
    fn local_probe_reports_the_real_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let probe = LocalFsProbe::new();
        let nested = dir.path().join("a/b/c");

        assert!(!probe.exists(&nested));
        probe.ensure_dir(&nested).unwrap();
        assert!(probe.exists(&nested));
        probe.probe_writable(&nested).unwrap();
        assert!(
            !nested.join(".android-backup-write-test").exists(),
            "the write probe must not be left behind"
        );

        std::fs::write(nested.join("f.bin"), b"12345").unwrap();
        assert_eq!(probe.local_size(&nested.join("f.bin")), Some(5));
        assert_eq!(probe.local_size(&nested.join("missing.bin")), None);
        assert_eq!(probe.local_size(&nested), None, "a directory has no size");
    }

    #[test]
    fn local_probe_can_read_free_space() {
        let dir = tempfile::tempdir().unwrap();
        let probe = LocalFsProbe::new();
        // Not an `if let`: this test once passed on a probe that answered `None`
        // for every existing directory (Windows, where canonicalize returns a
        // `\\?\` path that no mount point matches), and `None` here must fail,
        // because a backup that cannot see its disk's free space has lost a
        // guarantee, not gained flexibility.
        let free = probe
            .free_bytes(dir.path())
            .unwrap()
            .expect("the volume under the temporary directory reports free space");
        assert!(free > 1024 * 1024, "a test machine has some room: {free}");
        assert_eq!(
            LocalFsProbe::free_space(&dir.path().join("not-there")),
            Some(free)
        );
    }

    #[test]
    fn local_probe_reports_an_unwritable_directory() {
        // A path whose parent is a *file* cannot be created, which is the same
        // shape of failure as an unplugged drive.
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("file");
        std::fs::write(&file, b"x").unwrap();
        let probe = LocalFsProbe::new();
        let err = probe.ensure_dir(&file.join("inside")).unwrap_err();
        assert!(matches!(err, DestError::CannotCreate { .. }), "got {err:?}");
    }

    #[test]
    fn serial_tail_is_short_and_alphanumeric() {
        assert_eq!(serial_tail("R58M12ABCDE"), "BCDE");
        assert_eq!(serial_tail("ab"), "ab");
        assert_eq!(serial_tail(""), "");
        assert_eq!(serial_tail("192.168.1.20:5555"), "5555");
    }
}
