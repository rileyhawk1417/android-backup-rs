//! Device profile operations: the edits a user can make to one device.
//!
//! The repositories in [`crate::database::devices`] stay dumb: they write what
//! they are told to write. This module is the service layer that validates first
//! (E04.1, E04.2, E04.6), so the UI can show an inline error instead of writing a
//! name that would later become a hostile folder name.

use std::path::Path;
use std::sync::Arc;

use crate::automation::Schedule;
use crate::database::devices::{self, DeviceRow, FolderRow};
use crate::database::{Db, DbError};
use crate::paths::{PathError, RelPath};

/// Longest display name we accept. The name becomes a path component, and every
/// mainstream filesystem is unhappy well before this.
pub const MAX_DISPLAY_NAME_LEN: usize = 256;

/// Characters that cannot appear in a name that may become a folder name.
const HOSTILE_CHARS: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Why a profile edit was rejected.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ProfileError {
    #[error("the name cannot be empty")]
    EmptyName,

    #[error("the name is too long ({len} characters; at most {max} are allowed)")]
    TooLong { len: usize, max: usize },

    #[error("the name cannot contain {character:?} — it becomes a folder name on this computer")]
    HostileCharacter { character: char },

    #[error("the name cannot start or end with a dot or a space")]
    HostileEdge,

    #[error("a folder path cannot be empty")]
    EmptyFolder,

    #[error("a folder path cannot contain \"..\"")]
    FolderParentSegment,

    #[error("that folder path is not valid: {0}")]
    InvalidFolder(String),

    #[error(transparent)]
    Db(#[from] DbError),
}

/// Equality is used by the tests and by the UI's "did this error change?" checks.
/// The database variant compares by message because [`DbError`] carries sources
/// (I/O errors) that are not `PartialEq`.
impl PartialEq for ProfileError {
    fn eq(&self, other: &Self) -> bool {
        use ProfileError::{
            Db, EmptyFolder, EmptyName, FolderParentSegment, HostileCharacter, HostileEdge,
            InvalidFolder, TooLong,
        };
        match (self, other) {
            (EmptyName, EmptyName)
            | (HostileEdge, HostileEdge)
            | (EmptyFolder, EmptyFolder)
            | (FolderParentSegment, FolderParentSegment) => true,
            (TooLong { len: a, max: b }, TooLong { len: c, max: d }) => a == c && b == d,
            (HostileCharacter { character: a }, HostileCharacter { character: b }) => a == b,
            (InvalidFolder(a), InvalidFolder(b)) => a == b,
            (Db(a), Db(b)) => a.to_string() == b.to_string(),
            _ => false,
        }
    }
}

/// Validate a display name, returning the trimmed form that will be stored.
///
/// Deliberately stricter than the filesystem: the name decides the device's
/// backup folder, so a name that only *sometimes* works (on Linux but not on
/// Windows) is rejected with an explanation instead of creating a folder that
/// cannot be created later.
pub fn validate_display_name(name: &str) -> Result<String, ProfileError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(ProfileError::EmptyName);
    }
    if trimmed.chars().count() > MAX_DISPLAY_NAME_LEN {
        return Err(ProfileError::TooLong {
            len: trimmed.chars().count(),
            max: MAX_DISPLAY_NAME_LEN,
        });
    }
    if let Some(character) = trimmed
        .chars()
        .find(|c| HOSTILE_CHARS.contains(c) || c.is_control())
    {
        return Err(ProfileError::HostileCharacter { character });
    }
    if trimmed.starts_with(['.', ' ']) || trimmed.ends_with(['.', ' ']) {
        return Err(ProfileError::HostileEdge);
    }
    Ok(trimmed.to_string())
}

/// Validate a folder path typed by the user.
///
/// Accepts both an absolute phone path (`/storage/emulated/0/Podcasts`) and a
/// path relative to the storage root (`Podcasts`); both are resolved against the
/// root by [`crate::backup::selection`].
pub fn validate_folder_path(path: &str) -> Result<String, ProfileError> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        return Err(ProfileError::EmptyFolder);
    }
    if let Some(character) = trimmed.chars().find(|c| c.is_control()) {
        return Err(ProfileError::InvalidFolder(format!(
            "control character {character:?}"
        )));
    }
    if trimmed.split('/').any(|segment| segment == "..") {
        return Err(ProfileError::FolderParentSegment);
    }
    // Absolute phone paths and root-relative paths are both accepted; the leading
    // slash is the only difference between them. Characters that are merely
    // awkward on a desktop are *not* rejected here: the destination layout
    // sanitizes them (see `paths::sanitize_component`), which is what keeps a
    // folder named `Podcasts:2026` usable instead of impossible.
    let cleaned = trimmed.trim_start_matches('/');
    if cleaned.is_empty() {
        return Err(ProfileError::EmptyFolder);
    }
    if let Err(err) = RelPath::parse(cleaned) {
        return Err(match err {
            PathError::Empty => ProfileError::EmptyFolder,
            other => ProfileError::InvalidFolder(other.to_string()),
        });
    }
    Ok(trimmed.to_string())
}

/// Edits a UI performs on device profiles.
///
/// Holds the manifest handle and applies the validation above, so every caller
/// gets the same answers.
pub struct ProfileService {
    db: Arc<Db>,
}

impl ProfileService {
    pub fn new(db: Arc<Db>) -> Self {
        Self { db }
    }

    /// The device record, if the manifest knows it.
    pub fn device(&self, device_id: i64) -> Result<Option<DeviceRow>, ProfileError> {
        Ok(devices::get_by_id(&self.db, device_id)?)
    }

    /// Rename a device. History is keyed by `device_id`, so nothing else moves.
    pub fn rename(&self, device_id: i64, new_name: &str) -> Result<String, ProfileError> {
        let validated = validate_display_name(new_name)?;
        devices::rename(&self.db, device_id, &validated)?;
        Ok(validated)
    }

    /// Record that `alias_serial` is the same phone as `device_id` (plan §5.2).
    ///
    /// Never rewrites existing rows: the alias only changes how the serial
    /// resolves from now on.
    pub fn associate_device(&self, alias_serial: &str, device_id: i64) -> Result<(), ProfileError> {
        let trimmed = alias_serial.trim();
        if trimmed.is_empty() {
            return Err(ProfileError::EmptyName);
        }
        devices::associate_alias(&self.db, trimmed, device_id, &crate::clock::now_iso())?;
        Ok(())
    }

    /// Aliases associated with a profile.
    pub fn aliases(&self, device_id: i64) -> Result<Vec<String>, ProfileError> {
        Ok(devices::aliases_for(&self.db, device_id)?)
    }

    /// All configured folders, enabled or not.
    pub fn folders(&self, device_id: i64) -> Result<Vec<FolderRow>, ProfileError> {
        Ok(devices::folders(&self.db, device_id)?)
    }

    /// Enabled folder paths, ready for a [`crate::backup::JobConfig`].
    pub fn enabled_folders(&self, device_id: i64) -> Result<Vec<String>, ProfileError> {
        Ok(devices::enabled_folders(&self.db, device_id)?)
    }

    /// Enable or disable one folder.
    pub fn set_folder(
        &self,
        device_id: i64,
        source_path: &str,
        enabled: bool,
    ) -> Result<(), ProfileError> {
        devices::set_folder(&self.db, device_id, source_path, enabled)?;
        Ok(())
    }

    /// Add a custom folder, after validating it.
    pub fn add_folder(&self, device_id: i64, source_path: &str) -> Result<String, ProfileError> {
        let validated = validate_folder_path(source_path)?;
        devices::set_folder(&self.db, device_id, &validated, true)?;
        Ok(validated)
    }

    /// Remove a folder from the profile (files already backed up are untouched).
    pub fn remove_folder(&self, device_id: i64, source_path: &str) -> Result<(), ProfileError> {
        devices::remove_folder(&self.db, device_id, source_path)?;
        Ok(())
    }

    /// Give this device its own destination directory.
    pub fn set_destination(&self, device_id: i64, path: Option<&Path>) -> Result<(), ProfileError> {
        devices::set_destination(&self.db, device_id, path)?;
        Ok(())
    }

    /// When this device backs itself up (Wave 9, E09.6).
    ///
    /// The interval is clamped here rather than trusted from the panel: the rule
    /// lives with the model, so a UI slip cannot store a schedule the engine's own
    /// bounds would reject.
    pub fn set_auto_schedule(
        &self,
        device_id: i64,
        schedule: Schedule,
    ) -> Result<Schedule, ProfileError> {
        let clamped = schedule.clamped();
        devices::set_auto_schedule(&self.db, device_id, clamped)?;
        Ok(clamped)
    }

    /// All profiles, for destination disambiguation.
    pub fn devices(&self) -> Result<Vec<DeviceRow>, ProfileError> {
        Ok(devices::list(&self.db)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::props::DeviceProps;
    use crate::automation::Schedule;
    use crate::database::devices::DEFAULT_FOLDERS;

    fn props() -> DeviceProps {
        DeviceProps {
            manufacturer: Some("Samsung".to_string()),
            model: Some("SM-A536B".to_string()),
            product: Some("a53x".to_string()),
            android_version: Some("14".to_string()),
        }
    }

    fn service() -> (ProfileService, i64) {
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let device = devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z")
            .expect("device row");
        devices::seed_default_folders(&db, device.id).expect("folders");
        (ProfileService::new(db), device.id)
    }

    #[test]
    fn the_schedule_is_stored_readable_and_in_range() {
        let (service, id) = service();
        for wanted in [
            Schedule::OnConnect,
            Schedule::EveryHours(6),
            // Out of range on the way in, stored in range: the panel cannot
            // write an interval its own control cannot display.
            Schedule::EveryHours(0),
            Schedule::EveryHours(10_000),
        ] {
            let stored = service.set_auto_schedule(id, wanted).expect("write");
            assert_eq!(stored, wanted.clamped());
            let row = service.device(id).expect("read").expect("row");
            assert_eq!(row.auto_schedule, wanted.clamped());
        }
        // Turning it off is a real edit, not a missing value.
        service.set_auto_schedule(id, Schedule::Off).expect("write");
        assert_eq!(
            service.device(id).unwrap().unwrap().auto_schedule,
            Schedule::Off
        );
    }

    #[test]
    fn names_are_trimmed_and_stored() {
        assert_eq!(validate_display_name("  Pixel 8  ").unwrap(), "Pixel 8");
        assert_eq!(validate_display_name("Пиксель 8").unwrap(), "Пиксель 8");
    }

    #[test]
    fn hostile_names_are_rejected_with_a_reason() {
        assert_eq!(
            validate_display_name("   ").unwrap_err(),
            ProfileError::EmptyName
        );
        assert_eq!(
            validate_display_name("a/b").unwrap_err(),
            ProfileError::HostileCharacter { character: '/' }
        );
        assert_eq!(
            validate_display_name("phone\\name").unwrap_err(),
            ProfileError::HostileCharacter { character: '\\' }
        );
        assert_eq!(
            validate_display_name("c:*?").unwrap_err(),
            ProfileError::HostileCharacter { character: ':' }
        );
        assert_eq!(
            validate_display_name("...").unwrap_err(),
            ProfileError::HostileEdge
        );
        assert_eq!(
            validate_display_name("Phone.").unwrap_err(),
            ProfileError::HostileEdge
        );
        assert_eq!(
            validate_display_name("Phone\u{7}").unwrap_err(),
            ProfileError::HostileCharacter { character: '\u{7}' }
        );
        let long = "x".repeat(MAX_DISPLAY_NAME_LEN + 1);
        assert!(matches!(
            validate_display_name(&long).unwrap_err(),
            ProfileError::TooLong { .. }
        ));
        // Every rejected name still has a message the UI can show verbatim.
        for name in ["", "a/b", "..", "x".repeat(300).as_str()] {
            if let Err(err) = validate_display_name(name) {
                assert!(!err.to_string().is_empty());
            }
        }
    }

    #[test]
    fn rename_round_trip_keeps_identity_and_history() {
        let (service, device_id) = service();
        let renamed = service
            .rename(device_id, "  Kitchen phone ")
            .expect("rename");
        assert_eq!(renamed, "Kitchen phone");
        let device = service.device(device_id).expect("query").expect("row");
        assert_eq!(device.display_name, "Kitchen phone");
        assert_eq!(device.serial, "SER1", "identity never changes on rename");
        assert_eq!(device.id, device_id);
        assert!(service.rename(device_id, "a/b").is_err());
        // The failed rename left the good name alone.
        let device = service.device(device_id).expect("query").expect("row");
        assert_eq!(device.display_name, "Kitchen phone");
    }

    #[test]
    fn a_completed_job_keeps_its_device_id_across_a_rename() {
        let (service, device_id) = service();
        let db = service.db.clone();
        let job = crate::database::jobs::insert_job(&db, device_id, "2026-01-01T00:00:00Z")
            .expect("job row");
        crate::database::jobs::finalize(
            &db,
            job,
            crate::database::jobs::JobStatus::Completed,
            &crate::database::jobs::JobCounters::default(),
            None,
            "2026-01-01T00:01:00Z",
        )
        .expect("finalize");
        service.rename(device_id, "Renamed").expect("rename");
        let row = crate::database::jobs::get_job(&db, job)
            .expect("query")
            .expect("job");
        assert_eq!(
            row.device_id, device_id,
            "history stays attached to the profile"
        );
    }

    #[test]
    fn folder_paths_are_validated_before_they_are_stored() {
        assert_eq!(validate_folder_path(" Podcasts ").unwrap(), "Podcasts");
        assert_eq!(
            validate_folder_path("/storage/emulated/0/Podcasts").unwrap(),
            "/storage/emulated/0/Podcasts"
        );
        assert_eq!(
            validate_folder_path("").unwrap_err(),
            ProfileError::EmptyFolder
        );
        assert_eq!(
            validate_folder_path("/storage/emulated/0/../sdcard").unwrap_err(),
            ProfileError::FolderParentSegment
        );
        assert!(validate_folder_path("bad\u{0}name").is_err());
        assert!(validate_folder_path("/").is_err());
    }

    #[test]
    fn custom_folders_persist_and_can_be_removed() {
        let (service, device_id) = service();
        let stored = service
            .add_folder(device_id, " Podcasts ")
            .expect("add folder");
        assert_eq!(stored, "Podcasts");
        let folders = service.folders(device_id).expect("folders");
        assert!(folders.iter().any(|f| f.source_path == "Podcasts"));
        assert_eq!(folders.len(), DEFAULT_FOLDERS.len() + 1);

        service
            .set_folder(device_id, "DCIM", false)
            .expect("disable");
        let enabled = service.enabled_folders(device_id).expect("enabled");
        assert!(!enabled.contains(&"DCIM".to_string()));
        assert!(enabled.contains(&"Podcasts".to_string()));

        service
            .remove_folder(device_id, "Podcasts")
            .expect("remove");
        let folders = service.folders(device_id).expect("folders");
        assert_eq!(folders.len(), DEFAULT_FOLDERS.len());
    }

    #[test]
    fn a_device_can_be_given_its_own_destination() {
        let (service, device_id) = service();
        service
            .set_destination(device_id, Some(Path::new("/mnt/backups/kitchen")))
            .expect("set destination");
        let device = service.device(device_id).expect("query").expect("row");
        assert_eq!(
            device.destination_path.as_deref(),
            Some(Path::new("/mnt/backups/kitchen"))
        );
    }

    #[test]
    fn aliases_associate_without_touching_the_original_row() {
        let (service, device_id) = service();
        service
            .associate_device("NEW-SERIAL", device_id)
            .expect("associate");
        assert_eq!(
            service.aliases(device_id).expect("aliases"),
            vec!["NEW-SERIAL"]
        );
        let resolved = devices::resolve_active_device(&service.db, "NEW-SERIAL")
            .expect("resolve")
            .expect("device");
        assert_eq!(resolved.id, device_id);
        assert!(service.associate_device("  ", device_id).is_err());
    }
}
