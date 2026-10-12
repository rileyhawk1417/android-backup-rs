//! Desktop application for Android Backup Utility.
//!
//! The UI is a **subscriber**: it renders state it receives as events and
//! forwards user intents into `android-backup-core`. It holds no backup logic —
//! no comparison, no transfer, no hashing. See the module docs:
//!
//! * [`view`] — the view model (no `egui`): folding events into renderable state.
//! * [`notifications`] — OS notifications, and the policy that decides which are
//!   worth showing.
//! * [`automation`] — the automatic-backup host (E09.5): the seam between the
//!   scheduling policy in the core and this application's db, controller and
//!   window.
//! * [`tray`] — the system tray: its state, its menu, and the intents it emits.
//! * [`single_instance`] — one process per manifest, or the automation would run
//!   twice.
//! * [`jobconfig`] — the one place a manifest row becomes a `JobConfig`.
//! * [`cli`] — `--version`/`--help`, for a release binary with no display.
//! * [`logging`] — where the log file lives and how the level is chosen.
//! * [`picker`] — the native dialogs, behind a trait so a test never draws one.
//! * [`startup`] — first-launch decisions and adb resolution.
//! * [`app`] — application state and the intent handlers behind the widgets.
//! * [`ui`] — the `egui` rendering of [`app::App`].
//! * [`window`] — the `eframe` entry point (the only `eframe` in this crate).
//! * [`testutil`] — scripted doubles for the installer, picker and notification
//!   seams (tests only, but compiled unconditionally so the integration tests can
//!   reach them).

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod app;
pub mod automation;
pub mod autostart;
pub mod cli;
pub mod jobconfig;
pub mod logging;
pub mod notifications;
pub mod picker;
pub mod single_instance;
pub mod startup;
pub mod testutil;
pub mod tray;
pub mod ui;
pub mod view;
pub mod window;

pub use app::{App, AppDeps};
pub use view::{DeviceView, JobView, StartBlock, ViewState};
