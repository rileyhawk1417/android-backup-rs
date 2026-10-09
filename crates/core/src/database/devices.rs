//! Device profiles and their folder configuration.

use std::path::{Path, PathBuf};

use rusqlite::{params, OptionalExtension, Row};

use crate::adb::props::DeviceProps;
use crate::automation::Schedule;
use crate::clock;

use super::{Db, DbError};

/// Default folders offered for every new device (plan §7).
pub const DEFAULT_FOLDERS: &[&str] = &[
    "DCIM",
    "Pictures",
    "Movies",
    "Music",
    "Documents",
    "Download",
    "Android/media",
    "Recordings",
    "Screenshots",
];

/// A row of `devices`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceRow {
    pub id: i64,
    pub serial: String,
    pub display_name: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub android_version: Option<String>,
    pub destination_path: Option<PathBuf>,
    /// When automatic backups run for this device (Wave 9). `Off` — the
    /// default — means automation never touches it.
    pub auto_schedule: Schedule,
    pub created_at: String,
    pub last_seen_at: String,
}

impl DeviceRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let destination: Option<String> = row.get("destination_path")?;
        Ok(Self {
            id: row.get("id")?,
            serial: row.get("serial")?,
            display_name: row.get("display_name")?,
            manufacturer: row.get("manufacturer")?,
            model: row.get("model")?,
            android_version: row.get("android_version")?,
            destination_path: destination.map(PathBuf::from),
            auto_schedule: schedule_from_row(row)?,
            created_at: row.get("created_at")?,
            last_seen_at: row.get("last_seen_at")?,
        })
    }
}

/// Rebuild a [`Schedule`] from the manifest's two columns.
///
/// A kind we do not recognise, or `every_hours` with no usable number, reads as
/// [`Schedule::Off`]: a stored value that cannot be understood must never
/// *enable* automation.
fn schedule_from_row(row: &Row<'_>) -> rusqlite::Result<Schedule> {
    let kind: String = row.get("auto_schedule_kind")?;
    let hours: Option<i64> = row.get("auto_schedule_hours")?;
    Ok(Schedule::parse(
        &kind,
        hours.and_then(|h| u32::try_from(h).ok()),
    ))
}

/// Compose the automatic display name for a device.
pub fn default_display_name(serial: &str, props: &DeviceProps) -> String {
    if let Some(candidate) = props.display_candidate() {
        return candidate;
    }
    let tail: String = serial
        .chars()
        .rev()
        .take(6)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("Android device {tail}")
}

/// Insert the device if unseen, otherwise refresh `last_seen_at` and any newly
/// learned properties. A user-chosen display name is never overwritten.
pub fn upsert_seen(
    db: &Db,
    serial: &str,
    props: &DeviceProps,
    now: &str,
) -> Result<DeviceRow, DbError> {
    let name = default_display_name(serial, props);
    db.conn().execute(
        "INSERT INTO devices
             (serial, display_name, manufacturer, model, android_version, created_at, last_seen_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
         ON CONFLICT(serial) DO UPDATE SET
             last_seen_at    = excluded.last_seen_at,
             manufacturer    = COALESCE(excluded.manufacturer, devices.manufacturer),
             model           = COALESCE(excluded.model, devices.model),
             android_version = COALESCE(excluded.android_version, devices.android_version)",
        params![
            serial,
            name,
            props.manufacturer,
            props.model,
            props.android_version,
            now
        ],
    )?;
    get_by_serial(db, serial)?.ok_or_else(|| DbError::NotFound {
        what: "device",
        key: serial.to_string(),
    })
}

pub fn get_by_serial(db: &Db, serial: &str) -> Result<Option<DeviceRow>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM devices WHERE serial = ?1")?;
    let row = stmt
        .query_row(params![serial], DeviceRow::from_row)
        .optional()?;
    Ok(row)
}

pub fn get_by_id(db: &Db, id: i64) -> Result<Option<DeviceRow>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM devices WHERE id = ?1")?;
    Ok(stmt
        .query_row(params![id], DeviceRow::from_row)
        .optional()?)
}

/// All known devices, newest sighting first.
pub fn list(db: &Db) -> Result<Vec<DeviceRow>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM devices ORDER BY last_seen_at DESC, id ASC")?;
    let rows = stmt.query_map([], DeviceRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Set when automatic backups run for one device (Wave 9).
///
/// This is the user's standing consent, so it is stored with the profile rather
/// than in the config file: it belongs to the device, not to the machine. The
/// global default only ever *seeds* this value.
pub fn set_auto_schedule(db: &Db, device_id: i64, schedule: Schedule) -> Result<(), DbError> {
    let schedule = schedule.clamped();
    let affected = db.conn().execute(
        "UPDATE devices SET auto_schedule_kind = ?2, auto_schedule_hours = ?3 WHERE id = ?1",
        params![
            device_id,
            schedule.as_str(),
            schedule.hours().map(i64::from)
        ],
    )?;
    if affected == 0 {
        return Err(DbError::NotFound {
            what: "device",
            key: device_id.to_string(),
        });
    }
    Ok(())
}

/// Rename a device. Only `display_name` changes: identity and history stay put.
pub fn rename(db: &Db, device_id: i64, new_name: &str) -> Result<(), DbError> {
    let affected = db.conn().execute(
        "UPDATE devices SET display_name = ?2 WHERE id = ?1",
        params![device_id, new_name],
    )?;
    if affected == 0 {
        return Err(DbError::NotFound {
            what: "device",
            key: device_id.to_string(),
        });
    }
    Ok(())
}

/// Set (or clear) a device's own destination directory.
pub fn set_destination(db: &Db, device_id: i64, path: Option<&Path>) -> Result<(), DbError> {
    let value = path.map(|p| p.to_string_lossy().to_string());
    let affected = db.conn().execute(
        "UPDATE devices SET destination_path = ?2 WHERE id = ?1",
        params![device_id, value],
    )?;
    if affected == 0 {
        return Err(DbError::NotFound {
            what: "device",
            key: device_id.to_string(),
        });
    }
    Ok(())
}

/// Record that an unknown serial belongs to an existing profile (plan §5.2).
///
/// Existing history is never rewritten; only future sightings of `alias_serial`
/// resolve to `device_id`.
pub fn associate_alias(
    db: &Db,
    alias_serial: &str,
    device_id: i64,
    now: &str,
) -> Result<(), DbError> {
    db.conn().execute(
        "INSERT INTO device_aliases (alias_serial, device_id, created_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(alias_serial) DO UPDATE SET device_id = excluded.device_id",
        params![alias_serial, device_id, now],
    )?;
    Ok(())
}

pub fn aliases_for(db: &Db, device_id: i64) -> Result<Vec<String>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT alias_serial FROM device_aliases WHERE device_id = ?1")?;
    let rows = stmt.query_map(params![device_id], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Resolve a serial to a profile, following alias associations.
///
/// Aliases are consulted **first**: once the user says "this new serial is the same
/// phone as that profile", that decision must win over the transient `devices` row
/// the new serial created while it was still unknown. Nothing is rewritten — the
/// orphaned row keeps its own history — it is simply no longer the answer for that
/// serial.
pub fn resolve_active_device(db: &Db, serial: &str) -> Result<Option<DeviceRow>, DbError> {
    let conn = db.conn();
    let aliased = conn
        .prepare(
            "SELECT d.* FROM device_aliases a
             JOIN devices d ON d.id = a.device_id
             WHERE a.alias_serial = ?1",
        )?
        .query_row(params![serial], DeviceRow::from_row)
        .optional()?;
    if aliased.is_some() {
        return Ok(aliased);
    }
    drop(conn);
    get_by_serial(db, serial)
}

/// One `device_folders` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderRow {
    pub id: i64,
    pub device_id: i64,
    pub source_path: String,
    pub enabled: bool,
}

/// Seed the default folder selection for a newly created device. Idempotent.
pub fn seed_default_folders(db: &Db, device_id: i64) -> Result<(), DbError> {
    let conn = db.conn();
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO device_folders (device_id, source_path, enabled)
             VALUES (?1, ?2, 1)
             ON CONFLICT(device_id, source_path) DO NOTHING",
        )?;
        for folder in DEFAULT_FOLDERS {
            stmt.execute(params![device_id, folder])?;
        }
    }
    tx.commit()?;
    Ok(())
}

/// All configured folders for a device (enabled or not), in insertion order.
pub fn folders(db: &Db, device_id: i64) -> Result<Vec<FolderRow>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT id, device_id, source_path, enabled FROM device_folders
         WHERE device_id = ?1 ORDER BY id ASC",
    )?;
    let rows = stmt.query_map(params![device_id], |row| {
        Ok(FolderRow {
            id: row.get(0)?,
            device_id: row.get(1)?,
            source_path: row.get(2)?,
            enabled: row.get::<_, i64>(3)? != 0,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Enabled folder paths for a device.
pub fn enabled_folders(db: &Db, device_id: i64) -> Result<Vec<String>, DbError> {
    Ok(folders(db, device_id)?
        .into_iter()
        .filter(|f| f.enabled)
        .map(|f| f.source_path)
        .collect())
}

/// Add (or re-enable) a folder path.
pub fn set_folder(
    db: &Db,
    device_id: i64,
    source_path: &str,
    enabled: bool,
) -> Result<(), DbError> {
    db.conn().execute(
        "INSERT INTO device_folders (device_id, source_path, enabled)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(device_id, source_path) DO UPDATE SET enabled = excluded.enabled",
        params![device_id, source_path, i64::from(enabled)],
    )?;
    Ok(())
}

/// Remove a custom folder path.
pub fn remove_folder(db: &Db, device_id: i64, source_path: &str) -> Result<(), DbError> {
    db.conn().execute(
        "DELETE FROM device_folders WHERE device_id = ?1 AND source_path = ?2",
        params![device_id, source_path],
    )?;
    Ok(())
}

/// Convenience used by the watcher sink: upsert + seed folders.
pub fn touch_device(db: &Db, serial: &str, props: &DeviceProps) -> Result<DeviceRow, DbError> {
    let now = clock::now_iso();
    let device = upsert_seen(db, serial, props, &now)?;
    seed_default_folders(db, device.id)?;
    Ok(device)
}

/// The database as the device watcher's sink.
///
/// This is where a phone stops being a line of `adb` output and becomes a record
/// with an identity (plan §5.2): the first time the watcher sees a serial, and
/// whenever the device tells us more about itself, the row is written or
/// enriched. A failed write is logged rather than propagated — a full disk must
/// not take down device detection, and the next tick will try again.
impl crate::devices::DeviceSink for Db {
    fn upsert(&self, device: &crate::devices::DeviceInfo) {
        let props = device.effective_props();
        if let Err(err) = touch_device(self, &device.serial, &props) {
            tracing::warn!(
                serial = %device.serial,
                error = %err,
                "could not record a device profile"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_props() -> DeviceProps {
        DeviceProps {
            manufacturer: Some("Samsung".into()),
            model: Some("SM_A536B".into()),
            product: Some("a53x".into()),
            android_version: Some("14".into()),
        }
    }

    #[test]
    fn a_new_device_is_not_auto_enrolled() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(device.auto_schedule, Schedule::Off, "automation is opt-in");
    }

    #[test]
    fn the_auto_schedule_survives_a_reload_and_an_upsert() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        set_auto_schedule(&db, device.id, Schedule::EveryHours(6)).unwrap();

        // Re-seeing the phone (a later poll, a reconnect) must not reset intent.
        let again = upsert_seen(&db, "SER1", &sample_props(), "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(again.auto_schedule, Schedule::EveryHours(6));
        assert_eq!(
            get_by_id(&db, device.id).unwrap().unwrap().auto_schedule,
            Schedule::EveryHours(6)
        );

        // Switching mode drops the stale interval rather than leaving it behind.
        set_auto_schedule(&db, device.id, Schedule::OnConnect).unwrap();
        let switched = get_by_serial(&db, "SER1").unwrap().unwrap();
        assert_eq!(switched.auto_schedule, Schedule::OnConnect);
        assert_eq!(switched.auto_schedule.hours(), None);

        // And an out-of-range interval never reaches the row unclamped.
        set_auto_schedule(&db, device.id, Schedule::EveryHours(100_000)).unwrap();
        assert_eq!(
            get_by_id(&db, device.id).unwrap().unwrap().auto_schedule,
            Schedule::EveryHours(crate::automation::MAX_INTERVAL_HOURS)
        );
    }

    #[test]
    fn a_row_we_cannot_read_does_not_enable_automation() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        db.conn()
            .execute(
                "UPDATE devices SET auto_schedule_kind = 'from_the_future', auto_schedule_hours = 6
                 WHERE id = ?1",
                params![device.id],
            )
            .unwrap();
        assert_eq!(
            get_by_id(&db, device.id).unwrap().unwrap().auto_schedule,
            Schedule::Off
        );
    }

    #[test]
    fn setting_the_auto_schedule_on_a_missing_device_is_an_error() {
        let db = Db::open_in_memory().unwrap();
        assert!(set_auto_schedule(&db, 12345, Schedule::OnConnect).is_err());
    }

    #[test]
    fn upsert_creates_once_and_refreshes_last_seen() {
        let db = Db::open_in_memory().unwrap();
        let first = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        let second = upsert_seen(&db, "SER1", &sample_props(), "2026-02-02T00:00:00Z").unwrap();
        assert_eq!(first.id, second.id);
        assert_eq!(second.created_at, "2026-01-01T00:00:00Z");
        assert_eq!(second.last_seen_at, "2026-02-02T00:00:00Z");
        assert_eq!(list(&db).unwrap().len(), 1);
    }

    #[test]
    fn upsert_never_overwrites_a_user_chosen_name() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        rename(&db, device.id, "Riley Phone").unwrap();
        let again = upsert_seen(&db, "SER1", &sample_props(), "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(again.display_name, "Riley Phone");
    }

    #[test]
    fn properties_are_filled_in_progressively() {
        let db = Db::open_in_memory().unwrap();
        upsert_seen(&db, "SER1", &DeviceProps::default(), "2026-01-01T00:00:00Z").unwrap();
        let enriched = upsert_seen(&db, "SER1", &sample_props(), "2026-01-02T00:00:00Z").unwrap();
        assert_eq!(enriched.model.as_deref(), Some("SM_A536B"));
        // ...and a later empty read does not erase them.
        let after =
            upsert_seen(&db, "SER1", &DeviceProps::default(), "2026-01-03T00:00:00Z").unwrap();
        assert_eq!(after.model.as_deref(), Some("SM_A536B"));
    }

    #[test]
    fn rename_keeps_identity_and_history_linkage() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        let job =
            crate::database::jobs::insert_job(&db, device.id, "2026-01-01T01:00:00Z").unwrap();
        rename(&db, device.id, "Family Phone").unwrap();
        let renamed = get_by_id(&db, device.id).unwrap().unwrap();
        assert_eq!(renamed.serial, "SER1");
        assert_eq!(renamed.display_name, "Family Phone");
        let job_row = crate::database::jobs::get_job(&db, job).unwrap().unwrap();
        assert_eq!(job_row.device_id, device.id);
    }

    #[test]
    fn unknown_serial_creates_a_separate_profile_until_associated() {
        let db = Db::open_in_memory().unwrap();
        let original = upsert_seen(&db, "OLD", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        let other = upsert_seen(&db, "NEW", &sample_props(), "2026-02-01T00:00:00Z").unwrap();
        assert_ne!(original.id, other.id);

        associate_alias(&db, "NEW", original.id, "2026-02-02T00:00:00Z").unwrap();
        let resolved = resolve_active_device(&db, "NEW").unwrap().unwrap();
        assert_eq!(resolved.id, original.id);
        // Both rows still exist; nothing was rewritten.
        assert_eq!(list(&db).unwrap().len(), 2);
        assert_eq!(
            aliases_for(&db, original.id).unwrap(),
            vec!["NEW".to_string()]
        );
    }

    #[test]
    fn resolve_falls_back_to_direct_serial() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            resolve_active_device(&db, "SER1").unwrap().unwrap().id,
            device.id
        );
        assert!(resolve_active_device(&db, "NOPE").unwrap().is_none());
    }

    #[test]
    fn destination_can_be_set_and_cleared() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        set_destination(&db, device.id, Some(Path::new("/mnt/x"))).unwrap();
        assert_eq!(
            get_by_id(&db, device.id).unwrap().unwrap().destination_path,
            Some(PathBuf::from("/mnt/x"))
        );
        set_destination(&db, device.id, None).unwrap();
        assert_eq!(
            get_by_id(&db, device.id).unwrap().unwrap().destination_path,
            None
        );
    }

    #[test]
    fn seeding_defaults_is_idempotent() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        seed_default_folders(&db, device.id).unwrap();
        seed_default_folders(&db, device.id).unwrap();
        let rows = folders(&db, device.id).unwrap();
        assert_eq!(rows.len(), DEFAULT_FOLDERS.len());
        assert!(rows.iter().all(|f| f.enabled));
    }

    #[test]
    fn folders_can_be_toggled_and_extended() {
        let db = Db::open_in_memory().unwrap();
        let device = upsert_seen(&db, "SER1", &sample_props(), "2026-01-01T00:00:00Z").unwrap();
        seed_default_folders(&db, device.id).unwrap();
        set_folder(&db, device.id, "Download", false).unwrap();
        set_folder(&db, device.id, "Custom/Path", true).unwrap();
        let enabled = enabled_folders(&db, device.id).unwrap();
        assert!(!enabled.contains(&"Download".to_string()));
        assert!(enabled.contains(&"Custom/Path".to_string()));
        remove_folder(&db, device.id, "Custom/Path").unwrap();
        assert!(!enabled_folders(&db, device.id)
            .unwrap()
            .contains(&"Custom/Path".to_string()));
    }

    #[test]
    fn default_name_uses_model_then_serial_tail() {
        assert_eq!(
            default_display_name("SER1", &sample_props()),
            "Samsung SM-A536B"
        );
        assert_eq!(
            default_display_name("R58M12ABCDE", &DeviceProps::default()),
            "Android device 2ABCDE"
        );
    }

    #[test]
    fn touch_device_seeds_folders() {
        let db = Db::open_in_memory().unwrap();
        let device = touch_device(&db, "SER1", &sample_props()).unwrap();
        assert_eq!(
            folders(&db, device.id).unwrap().len(),
            DEFAULT_FOLDERS.len()
        );
    }

    #[test]
    fn renaming_a_missing_device_is_an_error() {
        let db = Db::open_in_memory().unwrap();
        assert!(matches!(
            rename(&db, 42, "x"),
            Err(DbError::NotFound { .. })
        ));
    }

    #[test]
    fn the_watcher_sink_records_a_device_and_its_folders() {
        use crate::adb::parse::{DeviceSnapshot, RawState};
        use crate::devices::{DeviceInfo, DeviceSink};

        let db = Db::open_in_memory().unwrap();
        let snapshot = DeviceSnapshot {
            serial: "R58M12ABCDE".into(),
            state: RawState::Device,
            model: Some("SM_A536B".into()),
            product: Some("a53x".into()),
            transport_id: Some("3".into()),
        };
        let info = DeviceInfo::from_snapshot(&snapshot).with_props(sample_props());
        db.upsert(&info);

        let row = get_by_serial(&db, "R58M12ABCDE").unwrap().unwrap();
        assert_eq!(row.display_name, "Samsung SM-A536B");
        assert_eq!(row.android_version.as_deref(), Some("14"));
        assert_eq!(folders(&db, row.id).unwrap().len(), DEFAULT_FOLDERS.len());

        // A second sighting enriches but does not duplicate, and a user's rename
        // survives it (plan §5.2).
        rename(&db, row.id, "My phone").unwrap();
        db.upsert(&info);
        assert_eq!(list(&db).unwrap().len(), 1);
        assert_eq!(
            get_by_id(&db, row.id).unwrap().unwrap().display_name,
            "My phone"
        );
    }

    #[test]
    fn the_sink_uses_the_listing_model_when_getprop_was_silent() {
        use crate::adb::parse::{DeviceSnapshot, RawState};
        use crate::devices::{DeviceInfo, DeviceSink};

        let db = Db::open_in_memory().unwrap();
        let snapshot = DeviceSnapshot {
            serial: "SER1".into(),
            state: RawState::Device,
            model: Some("Pixel_7".into()),
            product: None,
            transport_id: None,
        };
        db.upsert(&DeviceInfo::from_snapshot(&snapshot));
        assert_eq!(
            get_by_serial(&db, "SER1").unwrap().unwrap().display_name,
            "Pixel-7"
        );
    }
}
