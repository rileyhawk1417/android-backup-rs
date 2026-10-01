//! Typed ADB failures.
//!
//! [`classify`] is the **single** place where adb's stderr is turned into a
//! typed error. Every other module matches on [`AdbError`] instead of
//! re-parsing text, so messages and behaviour stay consistent across the app.

use std::path::PathBuf;

/// A failure while talking to ADB.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdbError {
    #[error("adb timed out after {secs}s: adb {args}", args = args.join(" "))]
    Timeout { args: Vec<String>, secs: u64 },

    #[error("could not start adb at {path}: {source}")]
    AdbMissing {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("adb {args} failed with exit code {code}: {stderr}", args = args.join(" "))]
    ProcessFailed {
        code: i32,
        stderr: String,
        args: Vec<String>,
    },

    #[error("device {serial} is not authorized — approve the USB debugging prompt on the phone", serial = serial.as_deref().unwrap_or("(unknown)"))]
    DeviceUnauthorized { serial: Option<String> },

    #[error("device {serial} is offline", serial = serial.as_deref().unwrap_or("(unknown)"))]
    DeviceOffline { serial: Option<String> },

    #[error("device {serial} was not found")]
    DeviceNotFound { serial: String },

    #[error("no Android devices are connected")]
    NoDevices,

    #[error("adb server problem: {msg}")]
    ServerError { msg: String },

    #[error("i/o error while running adb: {0}")]
    Io(#[from] std::io::Error),
}

impl AdbError {
    /// True when retrying the very same command could plausibly succeed later
    /// (device came back, user authorized, server restarted).
    ///
    /// The backup engine uses this to decide between "pause and tell the user"
    /// and "record the file as failed".
    pub fn is_transient(&self) -> bool {
        matches!(
            self,
            Self::Timeout { .. }
                | Self::DeviceUnauthorized { .. }
                | Self::DeviceOffline { .. }
                | Self::DeviceNotFound { .. }
                | Self::NoDevices
        )
    }

    /// True when the user must act on the phone before anything else can work.
    pub fn needs_authorization(&self) -> bool {
        matches!(self, Self::DeviceUnauthorized { .. })
    }

    /// True when the device itself is no longer answering.
    pub fn is_device_gone(&self) -> bool {
        matches!(
            self,
            Self::DeviceOffline { .. } | Self::DeviceNotFound { .. } | Self::NoDevices
        )
    }

    /// Build an equivalent error.
    ///
    /// `AdbError` wraps `std::io::Error`, which is not `Clone`, so test doubles
    /// and retry helpers use this to hand out a fresh copy of a canned failure.
    pub fn duplicate(&self) -> Self {
        fn copy_io(err: &std::io::Error) -> std::io::Error {
            std::io::Error::new(err.kind(), err.to_string())
        }
        match self {
            Self::Timeout { args, secs } => Self::Timeout {
                args: args.clone(),
                secs: *secs,
            },
            Self::AdbMissing { path, source } => Self::AdbMissing {
                path: path.clone(),
                source: copy_io(source),
            },
            Self::ProcessFailed { code, stderr, args } => Self::ProcessFailed {
                code: *code,
                stderr: stderr.clone(),
                args: args.clone(),
            },
            Self::DeviceUnauthorized { serial } => Self::DeviceUnauthorized {
                serial: serial.clone(),
            },
            Self::DeviceOffline { serial } => Self::DeviceOffline {
                serial: serial.clone(),
            },
            Self::DeviceNotFound { serial } => Self::DeviceNotFound {
                serial: serial.clone(),
            },
            Self::NoDevices => Self::NoDevices,
            Self::ServerError { msg } => Self::ServerError { msg: msg.clone() },
            Self::Io(err) => Self::Io(copy_io(err)),
        }
    }

    /// A short, user-facing hint to show in the UI.
    pub fn user_hint(&self) -> &'static str {
        match self {
            Self::DeviceUnauthorized { .. } => {
                "Unlock the phone, then approve the 'Allow USB debugging' prompt."
            }
            Self::DeviceOffline { .. } => "Unplug and replug the cable, or toggle USB debugging.",
            Self::DeviceNotFound { .. } | Self::NoDevices => {
                "Check the cable and that the phone is unlocked."
            }
            Self::ServerError { .. } => {
                "Another ADB server is running (Android Studio?). Close it or restart ADB."
            }
            Self::Timeout { .. } => "The device stopped responding; try again.",
            _ => "See the log for details.",
        }
    }
}

/// One stderr/stdout pattern → error mapping.
struct Pattern {
    needle: &'static str,
    /// Optional second substring that must also be present. adb words some errors
    /// around the serial (`device 'XYZ' not found`), so a single needle is not
    /// always enough to be specific without being fragile.
    also: Option<&'static str>,
    build: fn(Option<&str>) -> AdbError,
}

impl Pattern {
    fn matches(&self, haystack: &str) -> bool {
        haystack.contains(self.needle) && self.also.is_none_or(|also| haystack.contains(also))
    }
}

/// Order matters: more specific patterns first.
const PATTERNS: &[Pattern] = &[
    Pattern {
        needle: "device unauthorized",
        also: None,
        build: |serial| AdbError::DeviceUnauthorized {
            serial: serial.map(str::to_string),
        },
    },
    Pattern {
        needle: "device offline",
        also: None,
        build: |serial| AdbError::DeviceOffline {
            serial: serial.map(str::to_string),
        },
    },
    Pattern {
        needle: "no devices/emulators found",
        also: None,
        build: |_| AdbError::NoDevices,
    },
    Pattern {
        needle: "not found",
        also: Some("device"),
        build: |serial| AdbError::DeviceNotFound {
            serial: serial.unwrap_or("(unknown)").to_string(),
        },
    },
    Pattern {
        needle: "server version",
        also: None,
        build: |_| AdbError::ServerError {
            msg: "adb server version mismatch; another ADB installation is running".into(),
        },
    },
    Pattern {
        needle: "cannot connect to daemon",
        also: None,
        build: |_| AdbError::ServerError {
            msg: "cannot connect to the adb daemon".into(),
        },
    },
    Pattern {
        needle: "cannot bind",
        also: None,
        build: |_| AdbError::ServerError {
            msg: "cannot bind the adb server port; another server is running".into(),
        },
    },
    Pattern {
        needle: "daemon failed to start",
        also: None,
        build: |_| AdbError::ServerError {
            msg: "adb server failed to start".into(),
        },
    },
];

/// Turn a failed adb invocation into a typed error.
///
/// `serial` is the device the command was addressed to, when known — it is
/// carried through so the UI can name the device.
pub fn classify(
    serial: Option<&str>,
    stderr: &str,
    stdout: &str,
    code: i32,
    args: &[String],
) -> AdbError {
    let haystack = format!("{stderr}\n{stdout}").to_lowercase();
    for pattern in PATTERNS {
        if pattern.matches(&haystack) {
            return (pattern.build)(serial);
        }
    }
    AdbError::ProcessFailed {
        code,
        stderr: stderr.trim().to_string(),
        args: args.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> Vec<String> {
        vec!["shell".into(), "ls".into()]
    }

    #[test]
    fn unauthorized_is_typed() {
        let err = classify(
            Some("ABC123"),
            "error: device unauthorized.\nThis adb server's $ADB_VENDOR_KEYS is not set",
            "",
            1,
            &args(),
        );
        assert!(matches!(err, AdbError::DeviceUnauthorized { .. }));
        assert!(err.is_transient());
        assert!(err.to_string().contains("ABC123"));
    }

    #[test]
    fn offline_is_typed() {
        let err = classify(Some("ABC"), "error: device offline", "", 1, &args());
        assert!(matches!(err, AdbError::DeviceOffline { .. }));
    }

    #[test]
    fn no_devices_is_typed() {
        let err = classify(
            None,
            "adb: error: no devices/emulators found",
            "",
            1,
            &args(),
        );
        assert!(matches!(err, AdbError::NoDevices));
    }

    #[test]
    fn device_not_found_is_typed() {
        let err = classify(Some("XYZ"), "error: device 'XYZ' not found", "", 1, &args());
        assert!(matches!(err, AdbError::DeviceNotFound { .. }));
    }

    #[test]
    fn server_conflicts_are_typed() {
        let err = classify(
            None,
            "adb server version (41) doesn't match this client (39); killing...",
            "",
            1,
            &args(),
        );
        assert!(matches!(err, AdbError::ServerError { .. }));
        let err = classify(None, "cannot bind 'tcp:5037'", "", 1, &args());
        assert!(matches!(err, AdbError::ServerError { .. }));
    }

    #[test]
    fn unknown_failures_keep_stderr() {
        let err = classify(None, "ls: /root: Permission denied", "", 1, &args());
        let is_transient = err.is_transient();
        match err {
            AdbError::ProcessFailed { code, stderr, .. } => {
                assert_eq!(code, 1);
                assert!(stderr.contains("Permission denied"));
            }
            other => panic!("unexpected: {other:?}"),
        }
        assert!(!is_transient);
    }

    #[test]
    fn pattern_matching_is_case_insensitive() {
        let err = classify(Some("A"), "ERROR: DEVICE UNAUTHORIZED", "", 1, &args());
        assert!(matches!(err, AdbError::DeviceUnauthorized { .. }));
    }

    #[test]
    fn duplicate_preserves_the_error_kind() {
        let cases = [
            AdbError::Timeout {
                args: vec!["x".into()],
                secs: 3,
            },
            AdbError::DeviceUnauthorized {
                serial: Some("A".into()),
            },
            AdbError::DeviceOffline { serial: None },
            AdbError::DeviceNotFound { serial: "x".into() },
            AdbError::NoDevices,
            AdbError::ServerError { msg: "m".into() },
            AdbError::ProcessFailed {
                code: 2,
                stderr: "s".into(),
                args: vec![],
            },
        ];
        for case in cases {
            let copy = case.duplicate();
            assert_eq!(copy.to_string(), case.to_string());
            assert_eq!(copy.is_transient(), case.is_transient());
        }
        let io = AdbError::Io(std::io::Error::new(std::io::ErrorKind::NotFound, "nope"));
        assert!(io.duplicate().to_string().contains("nope"));
    }

    #[test]
    fn classification_helpers() {
        assert!(AdbError::DeviceUnauthorized { serial: None }.needs_authorization());
        assert!(!AdbError::NoDevices.needs_authorization());
        assert!(AdbError::NoDevices.is_device_gone());
        assert!(AdbError::DeviceOffline { serial: None }.is_device_gone());
        assert!(!AdbError::ServerError { msg: String::new() }.is_device_gone());
    }

    #[test]
    fn every_error_has_an_actionable_hint() {
        let cases = [
            AdbError::DeviceUnauthorized { serial: None },
            AdbError::DeviceOffline { serial: None },
            AdbError::DeviceNotFound { serial: "x".into() },
            AdbError::NoDevices,
            AdbError::ServerError { msg: "x".into() },
            AdbError::Timeout {
                args: vec![],
                secs: 1,
            },
            AdbError::ProcessFailed {
                code: 1,
                stderr: String::new(),
                args: vec![],
            },
        ];
        for case in cases {
            assert!(!case.user_hint().is_empty(), "{case:?}");
        }
    }
}
