//! One place that turns a manifest row into a [`JobConfig`] (Wave 9, E09.5).
//!
//! There are two callers that want a backup to run: the person pressing Start,
//! and the automation driver noticing a phone that is due. They must agree on
//! *everything* that decides whether a backup may proceed and where it lands —
//! otherwise "why did the automatic one write somewhere else?" has no answer, and
//! the automation path would be a second, quietly different backup engine.
//!
//! So the decision lives here once, in terms of the manifest alone. It deliberately
//! has no view state: automation runs with no window in front of anyone, and the
//! facts it needs (destination, enabled folders) are in the manifest, not on
//! screen.

use std::sync::Arc;

use android_backup_core::backup::destination::{self, FsProbe};
use android_backup_core::backup::job::JobConfig;
use android_backup_core::backup::transfer::VerifyMode;
use android_backup_core::backup::DEFAULT_FREE_SPACE_FLOOR;
use android_backup_core::config::AppConfig;
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::Db;

/// Build the job configuration for `device`, validating the destination here.
///
/// Everything that can be known before a job row exists is checked now: a dead
/// drive, a manifest that cannot be read, a device with no folders selected. The
/// errors are the sentences the panel shows — the same ones for a person and for
/// automation.
pub fn for_device(
    db: &Db,
    cfg: &AppConfig,
    probe: &Arc<dyn FsProbe>,
    device: &DeviceRow,
) -> Result<JobConfig, String> {
    // Every device in the manifest, not just the ones currently on screen: folder
    // names are disambiguated against devices that are unplugged too, and the name
    // must not change depending on what happens to be connected.
    let others =
        devices::list(db).map_err(|err| format!("could not read the device list: {err}"))?;
    let dest = destination::resolve_with_config(device, &others, cfg)
        .map_err(|err| format!("no backup location is configured: {err}"))?;
    check_destination_ready(probe, &dest)?;

    let folders = devices::enabled_folders(db, device.id)
        .map_err(|err| format!("could not read the folder list: {err}"))?;
    if folders.is_empty() {
        return Err("no folders are selected for this device — select at least one".to_string());
    }

    Ok(JobConfig {
        device_id: device.id,
        serial: device.serial.clone(),
        display_name: device.display_name.clone(),
        root: dest.root,
        folders,
        custom_folders: Vec::new(),
        whole_storage: false,
        // Hash mode is the strongest check; a device without `sha256sum` degrades
        // to recording the local digest rather than failing.
        verify: VerifyMode::Hash,
        // Let the engine detect it: it knows the phone better than the UI.
        storage_root: None,
        free_space_floor: DEFAULT_FREE_SPACE_FLOOR,
    })
}

/// A destination folder that does not exist is fine (it will be created); one
/// whose *parent* is missing means the drive is gone, and that must be said
/// before a job row exists rather than after the first file.
pub fn check_destination_ready(
    probe: &Arc<dyn FsProbe>,
    dest: &destination::EffectiveDestination,
) -> Result<(), String> {
    let check = if dest.device_subfolder {
        dest.root.parent().unwrap_or(&dest.root)
    } else {
        &dest.root
    };
    destination::validate(&**probe, check, None)
        .map(|_| ())
        .map_err(|err| format!("{} cannot be used: {err}", check.display()))
}
