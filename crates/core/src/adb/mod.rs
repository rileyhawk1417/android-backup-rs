//! ADB integration: locating the executable, running commands safely, parsing
//! output, and (later) managing its installation.
//!
//! Everything that touches a process goes through [`runner::Adb`], a trait, so the
//! higher layers (device discovery, backup engine, reliability tests) can be
//! driven by mocks instead of a phone.

// `installer` lands in Wave 7 (managed ADB download); the engine does not need it.
// pub mod installer;
pub mod error;
pub mod locate;
pub mod parse;
pub mod props;
pub mod runner;

pub use error::AdbError;
pub use locate::{AdbFound, AdbLocateError, AdbSource, AdbVersion};
pub use parse::{parse_devices, DeviceSnapshot, ParseError, RawState};
pub use runner::{Adb, CmdOutput, PlainProcessHost, ProcessAdbRunner, ProcessHost};
