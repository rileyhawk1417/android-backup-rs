//! The authorization gate (feature F4): **the single place** that decides whether
//! a device may be backed up.
//!
//! Plan §5.1 requires that the "approve the USB debugging prompt" case is handled
//! once and everywhere, with the same wording. Folder validation (Wave 4), job
//! start (Wave 5) and the Start button (Wave 6) all call [`check`]; none of them
//! re-implements the rules, so a device can never be "usable" in one place and
//! "not usable" in another.
//!
//! Every message here is written for the person looking at the screen: it says
//! **what is wrong with which phone** and **what to do about it**.

use std::sync::Arc;

use crate::devices::watcher::DeviceWatcher;
use crate::devices::{DeviceInfo, DeviceState};

/// Why a device cannot be backed up right now.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum GateError {
    #[error("no device is connected — plug in an Android phone with a data cable and wait for it to appear")]
    NoDevice,

    #[error(
        "device {serial} is not authorized — unlock the phone and approve the \
         \"Allow USB debugging?\" prompt, then try again"
    )]
    NotAuthorized { serial: String },

    #[error(
        "device {serial} is offline — reconnect the cable (or re-enable wireless \
         debugging), then try again"
    )]
    Offline { serial: String },

    #[error("device {serial} is no longer connected — plug it back in and wait for it to appear")]
    Disconnected { serial: String },

    #[error(
        "device {serial} reported a state this application does not support ({state}) — \
         reconnect the phone, or restart adb if it stays like this"
    )]
    UnknownState { serial: String, state: String },

    #[error(
        "the device list could not be read ({detail}) — check that adb is installed \
         and that the phone is connected"
    )]
    QueryFailed { detail: String },
}

/// The one authorization check (plan §5.1).
pub fn check(info: &DeviceInfo) -> Result<(), GateError> {
    match info.state {
        DeviceState::Connected => Ok(()),
        DeviceState::Unauthorized => Err(GateError::NotAuthorized {
            serial: info.serial.clone(),
        }),
        DeviceState::Offline => Err(GateError::Offline {
            serial: info.serial.clone(),
        }),
        DeviceState::Disconnected => Err(GateError::Disconnected {
            serial: info.serial.clone(),
        }),
        DeviceState::Error => Err(GateError::UnknownState {
            serial: info.serial.clone(),
            state: info.raw_state.as_str().to_string(),
        }),
    }
}

/// The gate as the UI needs it: over the watcher's current snapshot.
///
/// `serial` selects one device; `None` means "the only connected device", which
/// is the common case (one phone on the desk) and still refuses to guess when
/// several are attached.
pub fn check_watcher(
    watcher: &DeviceWatcher,
    serial: Option<&str>,
) -> Result<Arc<DeviceInfo>, GateError> {
    // Distinguish "adb cannot be asked" from "nothing is plugged in": the first is
    // a setup problem, the second a cable problem, and the fixes differ.
    if !watcher.last_poll_ok() {
        return Err(GateError::QueryFailed {
            detail: "adb did not answer the last request".to_string(),
        });
    }
    let devices = watcher.snapshot();
    if devices.is_empty() {
        return Err(GateError::NoDevice);
    }
    let device = match serial {
        Some(serial) => devices
            .iter()
            .find(|device| device.serial == serial)
            .cloned()
            .ok_or_else(|| GateError::Disconnected {
                serial: serial.to_string(),
            })?,
        None => {
            if devices.len() == 1 {
                Arc::clone(&devices[0])
            } else {
                return Err(GateError::NoDevice);
            }
        }
    };
    check(&device)?;
    Ok(device)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse::RawState;
    use crate::adb::props::DeviceProps;

    fn info(state: DeviceState, raw: RawState) -> DeviceInfo {
        DeviceInfo {
            serial: "R58M12ABCDE".into(),
            state,
            raw_state: raw,
            listing_model: None,
            listing_product: None,
            transport_id: None,
            props: DeviceProps::default(),
        }
    }

    #[test]
    fn only_a_connected_device_passes() {
        let mut expected = Vec::new();
        let cases = [
            (DeviceState::Connected, RawState::Device, None),
            (
                DeviceState::Unauthorized,
                RawState::Unauthorized,
                Some("not authorized"),
            ),
            (DeviceState::Offline, RawState::Offline, Some("offline")),
            (
                DeviceState::Disconnected,
                RawState::Device,
                Some("no longer connected"),
            ),
            (
                DeviceState::Error,
                RawState::Unrecognized("sideload".into()),
                Some("does not support"),
            ),
        ];
        for (state, raw, needle) in cases {
            let result = check(&info(state, raw));
            match needle {
                None => assert!(result.is_ok(), "{state} must pass"),
                Some(needle) => {
                    let message = result.unwrap_err().to_string();
                    assert!(
                        message.contains(needle),
                        "{state}: {message:?} should mention {needle:?}"
                    );
                    expected.push(message);
                }
            }
        }
        assert_eq!(expected.len(), 4);
    }

    #[test]
    fn every_message_names_the_device_and_says_what_to_do() {
        // Review checkpoint from the wave doc: a message that only says "failed"
        // is a bug, because the user cannot act on it.
        for state in [
            DeviceState::Unauthorized,
            DeviceState::Offline,
            DeviceState::Disconnected,
            DeviceState::Error,
        ] {
            let message = check(&info(state, RawState::Device))
                .unwrap_err()
                .to_string();
            assert!(message.contains("R58M12ABCDE"), "{message:?}");
            assert!(
                message.contains("then try again")
                    || message.contains("wait for it to appear")
                    || message.contains("reconnect"),
                "{message:?} must tell the user what to do"
            );
            assert!(message.len() > 40, "{message:?} is too terse to be useful");
        }
        let no_device = GateError::NoDevice.to_string();
        assert!(no_device.contains("plug in"), "{no_device:?}");
    }

    #[test]
    fn recovery_mode_is_an_unsupported_state_not_a_crash() {
        let err = check(&info(
            DeviceState::Error,
            RawState::Unrecognized("recovery".into()),
        ))
        .unwrap_err();
        assert_eq!(
            err,
            GateError::UnknownState {
                serial: "R58M12ABCDE".into(),
                state: "recovery".into(),
            }
        );
    }

    #[test]
    fn the_gate_refuses_to_guess_between_several_devices() {
        use crate::devices::watcher::{DeviceWatcher, WatcherConfig};
        use crate::mock::MockAdb;
        use std::time::Duration;

        let listing = "List of devices attached\n\
                        AAA111\tdevice product:a53x model:SM_A536B\n\
                        BBB222\tunauthorized\n";
        let adb = Arc::new(MockAdb::new().always(MockAdb::ok(listing)));
        let mut watcher = DeviceWatcher::spawn(
            adb,
            None,
            WatcherConfig::default().with_poll_interval(Duration::from_millis(1)),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while watcher.snapshot().len() < 2 && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }

        // Two phones attached and nobody named one: refuse, do not pick.
        assert_eq!(check_watcher(&watcher, None), Err(GateError::NoDevice));
        // Naming the connected one works…
        let picked = check_watcher(&watcher, Some("AAA111")).unwrap();
        assert_eq!(picked.serial, "AAA111");
        // …naming the unauthorized one explains why not…
        assert!(matches!(
            check_watcher(&watcher, Some("BBB222")),
            Err(GateError::NotAuthorized { .. })
        ));
        // …and naming a device that is not attached is a disconnect, not a guess.
        assert!(matches!(
            check_watcher(&watcher, Some("CCC333")),
            Err(GateError::Disconnected { .. })
        ));
        watcher.stop();
    }

    #[test]
    fn an_unreachable_adb_is_reported_as_a_setup_problem() {
        use crate::adb::AdbError;
        use crate::devices::watcher::{DeviceWatcher, WatcherConfig};
        use crate::mock::MockAdb;
        use std::time::Duration;

        let adb = Arc::new(MockAdb::new().always(MockAdb::err(AdbError::ServerError {
            msg: "adb not found".into(),
        })));
        let mut watcher = DeviceWatcher::spawn(
            adb,
            None,
            WatcherConfig::default().with_poll_interval(Duration::from_millis(1)),
        );
        // The first poll has to fail before the watcher can report that it did.
        while watcher.last_poll_ok() && watcher.drain().is_empty() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let result = check_watcher(&watcher, None);
        watcher.stop();
        assert!(
            matches!(result, Err(GateError::QueryFailed { .. })),
            "expected a query failure, got {result:?}"
        );
    }
}
