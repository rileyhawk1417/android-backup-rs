//! The native dialogs, behind a seam.
//!
//! Two things are true of `rfd` and both shape this module:
//!
//! 1. It **blocks** until the user answers, which is why the calls already happen
//!    on their own thread ([`crate::app`]'s `spawn_picker`) rather than in the frame
//!    loop.
//! 2. It opens a **real window**. That is the point in the application, and it is a
//!    defect in a test: a suite that dispatches a pick intent would pop a file
//!    dialog onto whatever desktop happens to be running it, and the test would
//!    still pass. Exactly like [`crate::startup::AdbInstaller`], the dialogs are
//!    therefore reached through a trait — the real one in the application,
//!    [`crate::testutil::ScriptedPicker`] in the tests.
//!
//! What the seam is *not* for: deciding anything. A picker returns a path or
//! nothing; whether that path is a usable destination is [`crate::app`]'s
//! validation, and stays there.

use std::path::{Path, PathBuf};

/// The file and folder dialogs the application needs.
///
/// Implementations block and may return `None` (the user cancelled). They are
/// called from a background thread, never from the frame loop.
pub trait Picker: Send + Sync {
    /// Choose an existing folder. `start` is where the dialog should open, when
    /// there is a sensible answer.
    fn folder(&self, title: &str, start: Option<&Path>) -> Option<PathBuf>;

    /// Choose an existing file.
    ///
    /// `suggested` pre-fills the file name box. There is deliberately no extension
    /// filter: the file we want is called `adb` — with no extension at all — and a
    /// filter is exactly what would hide it.
    fn file(&self, title: &str, start: Option<&Path>, suggested: Option<&str>) -> Option<PathBuf>;
}

/// The real dialogs, drawn by the desktop.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativePicker;

impl Picker for NativePicker {
    fn folder(&self, title: &str, start: Option<&Path>) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new();
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        dialog.set_title(title).pick_folder()
    }

    fn file(&self, title: &str, start: Option<&Path>, suggested: Option<&str>) -> Option<PathBuf> {
        let mut dialog = rfd::FileDialog::new();
        if let Some(start) = start {
            dialog = dialog.set_directory(start);
        }
        if let Some(name) = suggested {
            dialog = dialog.set_file_name(name);
        }
        dialog.set_title(title).pick_file()
    }
}
