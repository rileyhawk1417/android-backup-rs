//! Mapping from raw adb state tokens to the five application states (plan §5.1).
//!
//! This module is deliberately **pure and stateless**: it maps *one* snapshot to
//! *one* state. Two of the five states cannot be derived from a single listing —
//! [`DeviceState::Disconnected`] ("this serial was here last tick and is not any
//! more") and [`DeviceState::Error`] ("the listing itself could not be read")
//! are derived by the [watcher](super::watcher) from *two* consecutive ticks, and
//! `Error` is also used for a state token we do not understand.
//!
//! Keeping the distinction here is what stops the UI from inventing its own
//! rules: the mapper takes one snapshot (see the signature of [`DeviceState::from_raw`]),
//! the watcher takes two.

use std::fmt;

use crate::adb::parse::RawState;

/// The state of a device as the rest of the application understands it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum DeviceState {
    /// Ready to talk: `adb` reports `device`.
    Connected,
    /// The phone is asking for permission for this computer (plan §5.1).
    Unauthorized,
    /// `adb` knows the serial but the device is not answering.
    Offline,
    /// Was present, is not any more — **derived by the watcher**, never by
    /// [`DeviceState::from_raw`].
    Disconnected,
    /// The listing failed, or the device reported a state we do not handle
    /// (recovery mode is explicitly out of scope, plan §13).
    Error,
}

impl DeviceState {
    /// Map one raw token. See the module docs: this cannot produce
    /// [`DeviceState::Disconnected`].
    pub fn from_raw(raw: &RawState) -> Self {
        match raw {
            RawState::Device => Self::Connected,
            RawState::Unauthorized => Self::Unauthorized,
            RawState::Offline => Self::Offline,
            RawState::Recovery => {
                // Recovery mode is out of scope (plan §13). Say so once here rather
                // than letting it look like a mysterious failure later.
                tracing::warn!(
                    "device is in recovery mode, which this application does not support"
                );
                Self::Error
            }
            RawState::Unrecognized(token) => {
                tracing::warn!(token, "unrecognized device state reported by adb");
                Self::Error
            }
        }
    }

    /// Stable identifier, used in logs and in the manifest.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Connected => "connected",
            Self::Unauthorized => "unauthorized",
            Self::Offline => "offline",
            Self::Disconnected => "disconnected",
            Self::Error => "error",
        }
    }

    /// True only for [`DeviceState::Connected`]: the state in which a backup may
    /// actually start.
    pub fn is_usable(self) -> bool {
        matches!(self, Self::Connected)
    }

    /// True when the user can resolve this by acting on the phone or the cable —
    /// the UI uses it to decide whether to show instructions or a plain error.
    pub fn needs_user_action(self) -> bool {
        matches!(
            self,
            Self::Unauthorized | Self::Offline | Self::Disconnected
        )
    }
}

impl fmt::Display for DeviceState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_raw_state_maps_to_exactly_one_state() {
        let cases = [
            (RawState::Device, DeviceState::Connected),
            (RawState::Unauthorized, DeviceState::Unauthorized),
            (RawState::Offline, DeviceState::Offline),
            (RawState::Recovery, DeviceState::Error),
            (
                RawState::Unrecognized("sideload".into()),
                DeviceState::Error,
            ),
        ];
        for (raw, expected) in cases {
            assert_eq!(DeviceState::from_raw(&raw), expected, "raw: {raw:?}");
        }
    }

    #[test]
    fn the_derived_states_are_only_usable_from_the_watcher() {
        // Disconnected is not reachable through the mapper: the API shape (one
        // snapshot in) makes it impossible. This test pins that down so nobody
        // "helpfully" adds a branch for it.
        let mut all = Vec::new();
        for raw in [
            RawState::Device,
            RawState::Unauthorized,
            RawState::Offline,
            RawState::Recovery,
            RawState::Unrecognized("x".into()),
        ] {
            all.push(DeviceState::from_raw(&raw));
        }
        assert!(!all.contains(&DeviceState::Disconnected));
    }

    #[test]
    fn only_connected_is_usable() {
        assert!(DeviceState::Connected.is_usable());
        for state in [
            DeviceState::Unauthorized,
            DeviceState::Offline,
            DeviceState::Disconnected,
            DeviceState::Error,
        ] {
            assert!(!state.is_usable(), "{state} must not be usable");
        }
    }

    #[test]
    fn states_are_named_stably() {
        assert_eq!(DeviceState::Connected.as_str(), "connected");
        assert_eq!(DeviceState::Error.to_string(), "error");
    }
}
