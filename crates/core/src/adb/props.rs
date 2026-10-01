//! Device property queries (`getprop`).
//!
//! All properties are enrichment: a device is present because `adb devices` says
//! so, not because `getprop` answered. Every field is therefore `Option` and a
//! failing/blank `getprop` is never an error.
//!
//! The four properties are fetched in **one** shell round-trip and split with a
//! delimiter, because each adb invocation costs tens of milliseconds and the
//! watcher runs on a poll loop.

use std::time::Duration;

use super::runner::Adb;

/// Delimiter used to split the batched getprop output.
pub const SEPARATOR: &str = "__AB_PROP_SEP__";

/// Enrichment data about a device.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DeviceProps {
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub product: Option<String>,
    pub android_version: Option<String>,
}

impl DeviceProps {
    /// A human-friendly name candidate, e.g. `Samsung SM-A536B`.
    pub fn display_candidate(&self) -> Option<String> {
        let model = self.model.as_deref()?;
        let pretty_model = model.replace('_', "-");
        match self.manufacturer.as_deref() {
            Some(m) if !m.eq_ignore_ascii_case("unknown") && !m.is_empty() => {
                Some(format!("{m} {pretty_model}"))
            }
            _ => Some(pretty_model),
        }
    }
}

const PROPS: &[&str] = &[
    "ro.product.manufacturer",
    "ro.product.model",
    "ro.product.device",
    "ro.build.version.release",
];

/// The shell command used to fetch all properties at once.
pub fn props_command() -> String {
    let mut cmd = String::new();
    for (i, prop) in PROPS.iter().enumerate() {
        if i > 0 {
            cmd.push_str(&format!("; echo {SEPARATOR}; "));
        }
        cmd.push_str(&format!("getprop {prop}"));
    }
    cmd
}

/// Parse the batched output into properties.
pub fn parse_props(output: &str) -> DeviceProps {
    let cleaned = output.replace('\r', "");
    let mut parts = cleaned.split(SEPARATOR);
    let get = |it: &mut std::str::Split<'_, &str>| -> Option<String> {
        let raw = it.next().unwrap_or("").trim();
        if raw.is_empty() {
            None
        } else {
            Some(raw.to_string())
        }
    };
    let manufacturer = get(&mut parts);
    let model = get(&mut parts);
    let product = get(&mut parts);
    let android_version = get(&mut parts);
    DeviceProps {
        manufacturer,
        model,
        product,
        android_version,
    }
}

/// Fetch properties for a device. Failures are logged and yield empty props.
pub fn fetch_device_props(adb: &dyn Adb, serial: &str, timeout: Duration) -> DeviceProps {
    let command = props_command();
    match adb.serial_run(serial, &["shell", &command], timeout) {
        Ok(out) => parse_props(&out.stdout),
        Err(err) => {
            tracing::debug!(serial, error = %err, "could not read device properties");
            DeviceProps::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::AdbError;
    use super::*;
    use crate::mock::MockAdb;

    #[test]
    fn parses_full_output() {
        let out = format!("Samsung\n{SEPARATOR}\nSM_A536B\n{SEPARATOR}\na53x\n{SEPARATOR}\n14\n");
        let props = parse_props(&out);
        assert_eq!(props.manufacturer.as_deref(), Some("Samsung"));
        assert_eq!(props.model.as_deref(), Some("SM_A536B"));
        assert_eq!(props.product.as_deref(), Some("a53x"));
        assert_eq!(props.android_version.as_deref(), Some("14"));
    }

    #[test]
    fn missing_props_are_none() {
        let out = format!("Samsung\n{SEPARATOR}\n\n{SEPARATOR}\na53x\n{SEPARATOR}\n");
        let props = parse_props(&out);
        assert_eq!(props.manufacturer.as_deref(), Some("Samsung"));
        assert_eq!(props.model, None);
        assert_eq!(props.product.as_deref(), Some("a53x"));
        assert_eq!(props.android_version, None);
    }

    #[test]
    fn empty_output_yields_all_none() {
        let props = parse_props("");
        assert_eq!(props, DeviceProps::default());
    }

    #[test]
    fn display_candidate_is_friendly() {
        let props = DeviceProps {
            manufacturer: Some("Samsung".into()),
            model: Some("SM_A536B".into()),
            ..Default::default()
        };
        assert_eq!(
            props.display_candidate().as_deref(),
            Some("Samsung SM-A536B")
        );

        let unknown = DeviceProps {
            manufacturer: Some("unknown".into()),
            model: Some("Pixel_7".into()),
            ..Default::default()
        };
        assert_eq!(unknown.display_candidate().as_deref(), Some("Pixel-7"));

        assert_eq!(DeviceProps::default().display_candidate(), None);
    }

    #[test]
    fn fetches_through_a_single_shell_call() {
        let mock = MockAdb::new().on(
            "getprop",
            MockAdb::ok(&format!(
                "Xiaomi\n{SEPARATOR}\n2201123G\n{SEPARATOR}\nveux\n{SEPARATOR}\n13\n"
            )),
        );
        let props = fetch_device_props(&mock, "SER1", Duration::from_secs(5));
        assert_eq!(props.manufacturer.as_deref(), Some("Xiaomi"));
        assert_eq!(mock.call_count(), 1, "must batch into one round-trip");
        assert!(mock.calls()[0].starts_with("-s SER1 shell"));
    }

    #[test]
    fn failing_getprop_is_not_an_error() {
        let mock = MockAdb::new().on(
            "getprop",
            MockAdb::err(AdbError::DeviceOffline { serial: None }),
        );
        let props = fetch_device_props(&mock, "SER1", Duration::from_secs(5));
        assert_eq!(props, DeviceProps::default());
    }
}
