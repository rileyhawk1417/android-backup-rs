//! Device discovery and identity (Wave 3, plan §5).
//!
//! ```text
//! adb devices -l ──▶ parse_devices ──▶ DeviceSnapshot ─┐
//!                                                      ├─▶ DeviceInfo ──▶ DeviceEvent
//! shell getprop  ──▶ fetch_device_props ──▶ DeviceProps┘        │
//!                                                               ▼
//!                                                     DeviceSink (the manifest)
//!
//! (`DeviceSnapshot` above is `adb`'s parse type — one listing entry. The
//! watcher's live device list is [`DeviceSnapshotHandle`], a different thing
//! with an unfortunately similar name.)
//! ```
//!
//! Two seams keep this layer honest:
//!
//! * [`crate::adb::Adb`] is the only process/I-O dependency — the watcher is
//!   driven by a mock in tests.
//! * [`DeviceSink`] is the only persistence dependency. The watcher *emits*; the
//!   caller decides where devices become records. That keeps `core::devices`
//!   usable without a database (a future CLI or diagnostics tool) while the
//!   desktop app simply passes `Arc<Db>` (see `database::devices`).

pub mod backup_gate;
pub mod profile;
pub mod state;
pub mod watcher;

pub use backup_gate::{check, check_watcher, GateError};
pub use profile::{validate_display_name, validate_folder_path, ProfileError, ProfileService};
pub use state::DeviceState;
pub use watcher::{DeviceEvent, DeviceSnapshotHandle, DeviceWatcher, WatcherConfig};

use std::sync::Arc;

use crate::adb::parse::{DeviceSnapshot, RawState};
use crate::adb::props::DeviceProps;

/// Everything currently known about one connected device.
///
/// Enrichment is best-effort by construction: [`DeviceProps`] is whatever
/// `getprop` managed to answer, and a device is *present* because `adb devices`
/// listed it, not because it answered questions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub serial: String,
    pub state: DeviceState,
    /// The token adb reported, kept verbatim for diagnostics.
    pub raw_state: RawState,
    /// `model:` from `adb devices -l` — a fallback when `getprop` failed.
    pub listing_model: Option<String>,
    /// `product:` from `adb devices -l` (codename, e.g. `a53x`).
    pub listing_product: Option<String>,
    pub transport_id: Option<String>,
    pub props: DeviceProps,
}

impl DeviceInfo {
    /// Build from a listing entry. Props stay empty until
    /// [`DeviceInfo::with_props`] is called: a listing tick must never require a
    /// second round-trip.
    pub fn from_snapshot(snapshot: &DeviceSnapshot) -> Self {
        Self {
            serial: snapshot.serial.clone(),
            state: DeviceState::from_raw(&snapshot.state),
            raw_state: snapshot.state.clone(),
            listing_model: snapshot.model.clone(),
            listing_product: snapshot.product.clone(),
            transport_id: snapshot.transport_id.clone(),
            props: DeviceProps::default(),
        }
    }

    pub fn with_props(mut self, props: DeviceProps) -> Self {
        self.props = props;
        self
    }

    /// Props with the listing's `model`/`product` filled in where `getprop` was
    /// silent — this is what a display name is composed from.
    pub fn effective_props(&self) -> DeviceProps {
        DeviceProps {
            manufacturer: self.props.manufacturer.clone(),
            model: self
                .props
                .model
                .clone()
                .or_else(|| self.listing_model.clone()),
            product: self
                .props
                .product
                .clone()
                .or_else(|| self.listing_product.clone()),
            android_version: self.props.android_version.clone(),
        }
    }

    /// A display name derived from the device alone.
    ///
    /// **Not authoritative**: once the device has a row, the user-chosen
    /// `display_name` in the manifest wins (plan §5.2 — renaming must survive a
    /// reinstall of this app). This is for devices that have never been seen
    /// before, and for the "unknown device" case in logs.
    pub fn name_hint(&self) -> String {
        crate::database::devices::default_display_name(&self.serial, &self.effective_props())
    }

    /// Ready for a backup: authorized, online, answering.
    pub fn is_usable(&self) -> bool {
        self.state.is_usable()
    }

    /// The phone is waiting for the user to approve USB debugging.
    pub fn needs_authorization(&self) -> bool {
        self.state == DeviceState::Unauthorized
    }
}

/// Where devices become persistent records.
///
/// Implemented by `database::devices` for [`crate::database::Db`]. The watcher
/// calls [`DeviceSink::upsert`] on first sight and whenever the enriching
/// properties change — **never once per poll tick**, because that would mean a
/// write to SQLite every few seconds for every connected phone.
pub trait DeviceSink: Send + Sync {
    fn upsert(&self, device: &DeviceInfo);
}

/// Shared handle to a sink, as the watcher takes it.
pub type SharedSink = Arc<dyn DeviceSink>;

#[cfg(test)]
mod tests {
    use super::*;

    fn snapshot(serial: &str, state: RawState) -> DeviceSnapshot {
        DeviceSnapshot {
            serial: serial.to_string(),
            state,
            model: None,
            product: None,
            transport_id: None,
        }
    }

    #[test]
    fn a_mapping_from_listing_to_info_is_lossless() {
        let snap = DeviceSnapshot {
            serial: "R58M12ABCDE".into(),
            state: RawState::Device,
            model: Some("SM_A536B".into()),
            product: Some("a53x".into()),
            transport_id: Some("3".into()),
        };
        let info = DeviceInfo::from_snapshot(&snap);
        assert_eq!(info.serial, "R58M12ABCDE");
        assert_eq!(info.state, DeviceState::Connected);
        assert_eq!(info.raw_state, RawState::Device);
        assert_eq!(info.listing_model.as_deref(), Some("SM_A536B"));
        assert_eq!(info.transport_id.as_deref(), Some("3"));
        assert_eq!(
            info.props,
            DeviceProps::default(),
            "no props without getprop"
        );
    }

    #[test]
    fn the_listing_model_is_used_when_getprop_is_silent() {
        let mut snap = snapshot("SER1", RawState::Device);
        snap.model = Some("Pixel_7".into());
        let info = DeviceInfo::from_snapshot(&snap);
        assert_eq!(info.effective_props().model.as_deref(), Some("Pixel_7"));
        assert_eq!(info.name_hint(), "Pixel-7");
    }

    #[test]
    fn getprop_wins_over_the_listing() {
        let mut snap = snapshot("SER1", RawState::Device);
        snap.model = Some("SM_A536B".into());
        let info = DeviceInfo::from_snapshot(&snap).with_props(DeviceProps {
            manufacturer: Some("Samsung".into()),
            model: Some("SM_A536B".into()),
            ..Default::default()
        });
        assert_eq!(info.name_hint(), "Samsung SM-A536B");
    }

    #[test]
    fn a_nameless_device_falls_back_to_its_serial() {
        let info = DeviceInfo::from_snapshot(&snapshot("R58M12ABCDE", RawState::Device));
        assert_eq!(info.name_hint(), "Android device 2ABCDE");
    }

    #[test]
    fn the_name_hint_agrees_with_the_manifest_default() {
        // Two places compose a default name (here, for a device we have never
        // stored, and in `database::devices`, for the INSERT). They must not drift.
        let mut snap = snapshot("SER1", RawState::Device);
        snap.model = Some("SM_A536B".into());
        let info = DeviceInfo::from_snapshot(&snap).with_props(DeviceProps {
            manufacturer: Some("Samsung".into()),
            ..Default::default()
        });
        assert_eq!(
            info.name_hint(),
            crate::database::devices::default_display_name(&info.serial, &info.effective_props())
        );
    }

    #[test]
    fn usability_follows_the_state() {
        let mut info = DeviceInfo::from_snapshot(&snapshot("SER1", RawState::Device));
        assert!(info.is_usable());
        info.state = DeviceState::Unauthorized;
        assert!(!info.is_usable());
        assert!(info.needs_authorization());
    }
}
