//! Desktop application for Android Backup Utility.
//!
//! The UI is a **subscriber**: it renders state it receives as events and
//! forwards user intents into `android-backup-core`. It holds no backup logic —
//! no comparison, no transfer, no hashing. See the module docs:
//!
//! * [`view`] — the view model (no `egui`): folding events into renderable state.
//! * [`cli`] — `--version`/`--help`, for a release binary with no display.
//! * [`logging`] — where the log file lives and how the level is chosen.
//! * [`startup`] — first-launch decisions and adb resolution.
//! * [`app`] — application state and the intent handlers behind the widgets.
//! * [`ui`] — the `egui` rendering of [`app::App`].
//! * [`window`] — the `eframe` entry point (the only `eframe` in this crate).
//! * [`testutil`] — scripted doubles for the installer seam (tests only, but
//!   compiled unconditionally so the integration tests can reach them).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod app;
pub mod cli;
pub mod logging;
pub mod startup;
pub mod testutil;
pub mod ui;
pub mod view;
pub mod window;

pub use app::{App, AppDeps};
pub use view::{DeviceView, JobView, StartBlock, ViewState};
