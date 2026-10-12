//! Test doubles for the desktop crate.
//!
//! The installer seam exists because the real thing downloads 9 MB from Google
//! and then runs the binary it produced: no test can provoke a checksum
//! mismatch, a full disk or a cancellation against `dl.google.com`. A
//! [`ScriptedInstaller`] plays those outcomes back deterministically, including
//! the callback that asks it to stop. [`ScriptedPicker`] and
//! [`RecordingNotifier`] exist for the same reason: the real dialogs draw a window
//! and the real toasts leave the process, and neither belongs in a test run.
//!
//! Shipped (not `#[cfg(test)]`) for the same reason as `core::mock`: the
//! integration tests in `tests/` are separate crates and cannot see
//! `#[cfg(test)]` items.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use android_backup_core::adb::installer::{
    Control, DigestAlgorithm, InstallError, InstallReport, Progress, Revision, Stage,
};
use android_backup_core::adb::locate::AdbVersion;

use crate::app::TrayFactory;
use crate::notifications::{Notice, Notifier};
use crate::picker::Picker;
use crate::startup::AdbInstaller;
use crate::tray::{TrayCommand, TrayHandle, TrayState, Wiring};

/// A mutex guard that survives a poisoned lock: one failing assertion must not
/// turn the next one into a different failure.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// An installer that follows a script instead of the network.
///
/// * `script` — the progress the installer reports, in order.
/// * `outcome` — what it answers once the script (and the gate) are done.
/// * `gate` — an optional receiver the installer blocks on before finishing, so
///   a test can act (press Cancel, drop the window) while an install is "in
///   flight".
/// * `done` — signalled after the installer has sent its result, so a test knows
///   the message is queued before it drains.
pub struct ScriptedInstaller {
    script: Vec<Progress>,
    /// A final tick after the gate: the real installer notices a cancel on its
    /// next progress callback, and this is that callback.
    closing_tick: Progress,
    outcome: Mutex<Option<Result<InstallReport, InstallError>>>,
    gate: Mutex<Option<Receiver<()>>>,
    done: Mutex<Option<Sender<()>>>,
    installs: AtomicUsize,
    cancelled: AtomicBool,
}

impl ScriptedInstaller {
    /// Report two download ticks, then succeed.
    pub fn succeeding() -> Self {
        Self::new(
            vec![
                Progress {
                    stage: Stage::Manifest,
                    done: 0,
                    total: None,
                },
                Progress {
                    stage: Stage::Download,
                    done: 1024,
                    total: Some(4096),
                },
            ],
            Ok(sample_report()),
        )
    }

    /// Report one tick, then fail with `err`.
    pub fn failing(err: InstallError) -> Self {
        Self::new(
            vec![Progress {
                stage: Stage::Manifest,
                done: 0,
                total: None,
            }],
            Err(err),
        )
    }

    /// Report progress and hold before finishing until `gate` is signalled.
    pub fn gated(mut self, gate: Receiver<()>) -> Self {
        self.gate = Mutex::new(Some(gate));
        self
    }

    /// Report progress, report completion, then signal `done`.
    pub fn announcing(mut self, done: Sender<()>) -> Self {
        self.done = Mutex::new(Some(done));
        self
    }

    fn new(script: Vec<Progress>, outcome: Result<InstallReport, InstallError>) -> Self {
        Self {
            script,
            closing_tick: Progress {
                stage: Stage::Download,
                done: 4096,
                total: Some(4096),
            },
            outcome: Mutex::new(Some(outcome)),
            gate: Mutex::new(None),
            done: Mutex::new(None),
            installs: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
        }
    }

    /// How many times `install` was called.
    pub fn installs(&self) -> usize {
        self.installs.load(Ordering::SeqCst)
    }

    /// Did the installer stop because the callback asked it to?
    pub fn was_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }

    fn stop_if(&self, control: Control) -> bool {
        if control == Control::Cancel {
            self.cancelled.store(true, Ordering::SeqCst);
            true
        } else {
            false
        }
    }

    fn announce(&self) {
        if let Some(done) = lock(&self.done).take() {
            let _ = done.send(());
        }
    }
}

impl AdbInstaller for ScriptedInstaller {
    fn install(
        &self,
        on_progress: &mut dyn FnMut(Progress) -> Control,
    ) -> Result<InstallReport, InstallError> {
        self.installs.fetch_add(1, Ordering::SeqCst);
        for progress in &self.script {
            if self.stop_if(on_progress(*progress)) {
                self.announce();
                return Err(InstallError::Cancelled);
            }
        }

        // Let the test act while the install is "running".
        if let Some(gate) = lock(&self.gate).take() {
            let _ = gate.recv();
        }

        // One tick after the gate: this is where a cancel is noticed.
        if self.stop_if(on_progress(self.closing_tick)) {
            self.announce();
            return Err(InstallError::Cancelled);
        }

        let outcome = lock(&self.outcome)
            .take()
            .unwrap_or(Err(InstallError::Cancelled));
        self.announce();
        outcome
    }
}

/// Where a [`sample_report`] pretends platform-tools was installed.
pub const SAMPLE_ADB_DIR: &str = "/tmp/android-backup-test/platform-tools";

/// A plausible completed installation.
pub fn sample_report() -> InstallReport {
    let dir = PathBuf::from(SAMPLE_ADB_DIR);
    let adb_path = dir.join("adb");
    InstallReport {
        dir,
        adb_path,
        revision: Revision {
            major: 37,
            minor: 0,
            micro: 1,
        },
        version: AdbVersion {
            major: 1,
            minor: 0,
            patch: 41,
            raw: "Android Debug Bridge version 1.0.41".to_string(),
        },
        digest: DigestAlgorithm::Sha256,
        archive_url: "https://example.invalid/platform-tools-latest-linux.zip".to_string(),
        downloaded_bytes: 4096,
        files: vec!["adb".to_string(), "source.properties".to_string()],
    }
}

/// A digest mismatch, as reported for a truncated or swapped download.
pub fn digest_mismatch() -> InstallError {
    InstallError::DigestMismatch {
        url: "https://example.invalid/platform-tools-latest-linux.zip".to_string(),
        algorithm: DigestAlgorithm::Sha256,
        expected: "0".repeat(64),
        actual: "1".repeat(64),
    }
}

/// A scripted installer in an `Arc`, which is what `AppDeps` wants.
pub fn installer(script: ScriptedInstaller) -> Arc<dyn AdbInstaller> {
    Arc::new(script)
}

/// A notifier that keeps what it was asked to show (Wave 9, E09.4).
///
/// Notifications are the one thing the tests cannot observe through the window —
/// they leave the process — so they are recorded here instead, and the assertions
/// are on the exact payload. Nothing in this file's `recording` half touches a
/// notification daemon, which is what lets the whole suite pass on a machine that
/// has none.
///
/// [`RecordingNotifier::failing`] is the other half of the story: it stands in for a
/// desktop bus that never answers, or a Windows toast Focus Assist swallowed. A
/// failed toast must be invisible to everything except the log.
pub struct RecordingNotifier {
    notices: Mutex<Vec<Notice>>,
    attempts: AtomicUsize,
    /// Show nothing and record only the attempt.
    failing: bool,
}

impl RecordingNotifier {
    pub fn new() -> Self {
        Self {
            notices: Mutex::new(Vec::new()),
            attempts: AtomicUsize::new(0),
            failing: false,
        }
    }

    /// A notifier on a machine where every toast fails.
    pub fn failing() -> Self {
        Self {
            failing: true,
            ..Self::new()
        }
    }

    /// Everything shown, in order.
    pub fn notices(&self) -> Vec<Notice> {
        lock(&self.notices).clone()
    }

    /// The first lines only — what a toast list looks like from across the room.
    pub fn titles(&self) -> Vec<String> {
        lock(&self.notices)
            .iter()
            .map(|notice| notice.title.clone())
            .collect()
    }

    /// The second lines, for the tests that care about the detail.
    pub fn bodies(&self) -> Vec<String> {
        lock(&self.notices)
            .iter()
            .map(|notice| notice.body.clone())
            .collect()
    }

    /// How many notices were offered to the OS, whether or not it took them.
    pub fn attempts(&self) -> usize {
        self.attempts.load(Ordering::SeqCst)
    }

    pub fn clear(&self) {
        lock(&self.notices).clear();
    }
}

impl Default for RecordingNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier for RecordingNotifier {
    fn show(&self, notice: &Notice) {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        if self.failing {
            tracing::warn!(title = %notice.title, "a scripted notification failed");
            return;
        }
        lock(&self.notices).push(notice.clone());
    }
}

/// A tray icon that is not one, for tests (Wave 9, E09.5).
///
/// The real icon lives in the desktop's notification area, is created on the event
/// loop thread, and on Linux needs an appindicator host that a test machine does not
/// have — so the assertions are on *what the application asked the icon to show*
/// instead. `TrayState`s and pause toggles arrive in order, and menu clicks are sent
/// by the test through the [`Wiring`] it was handed, which is also how the real icon
/// sends them.
///
/// The handle is not `Send` (see [`TrayHandle`]) and neither is the record, since
/// both are used from the single thread that owns the window.
#[derive(Default)]
pub struct RecordingTray {
    states: Mutex<Vec<TrayState>>,
    pauses: Mutex<Vec<bool>>,
    closed: AtomicBool,
    /// The channel the application gave the icon; kept so [`RecordingTray::click`]
    /// can do what a menu click does.
    wiring: Mutex<Option<Wiring>>,
}

impl RecordingTray {
    pub fn new() -> Self {
        Self::default()
    }

    /// Every state the icon was told to show, oldest first. The application sets the
    /// first one as it creates the icon, so a fresh tray has one entry already.
    pub fn states(&self) -> Vec<TrayState> {
        lock(&self.states).clone()
    }

    /// The last state, which is what the user would be looking at.
    pub fn state(&self) -> Option<TrayState> {
        self.states().pop()
    }

    /// Every pause toggle, in order.
    pub fn pauses(&self) -> Vec<bool> {
        lock(&self.pauses).clone()
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Click a menu item, as the desktop would.
    ///
    /// Fails when the icon has no wiring, which would mean the application created
    /// the tray without giving it anywhere to send clicks — a bug the test should
    /// see rather than a silently dropped command.
    pub fn click(&self, command: TrayCommand) -> Result<(), String> {
        let wiring = lock(&self.wiring);
        let wiring = wiring
            .as_ref()
            .ok_or_else(|| "the tray was never wired".to_string())?;
        wiring
            .commands
            .send(command)
            .map_err(|err| format!("the application is not listening for tray commands: {err}"))
    }

    /// The context the icon was told to wake, if any. `Some` means a menu click can
    /// make a *hidden* window draw the frame that reads the command.
    pub fn repaint_context(&self) -> Option<egui::Context> {
        lock(&self.wiring)
            .as_ref()
            .and_then(|wiring| lock(&wiring.ctx).clone())
    }

    /// The pause state the icon was created with.
    pub fn initial_pause(&self) -> Option<bool> {
        lock(&self.wiring)
            .as_ref()
            .map(|wiring| wiring.automation_paused)
    }
}

impl TrayHandle for RecordingTray {
    fn set_state(&self, state: TrayState) {
        lock(&self.states).push(state);
    }

    fn set_automation_paused(&self, paused: bool) {
        lock(&self.pauses).push(paused);
    }

    fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// A tray factory that succeeds, with a handle to the double it produced.
///
/// Takes the same shape as [`crate::tray::spawn`] so that switching an application
/// under test from the real icon to this one is one line in the deps.
pub fn tray() -> (TrayFactory, std::rc::Rc<RecordingTray>) {
    let recorder = std::rc::Rc::new(RecordingTray::new());
    let handle = std::rc::Rc::clone(&recorder);
    let factory: TrayFactory = Box::new(move |wiring: Wiring| {
        *lock(&handle.wiring) = Some(wiring);
        Ok(Box::new(RcHandle(std::rc::Rc::clone(&handle))) as Box<dyn TrayHandle>)
    });
    (factory, recorder)
}

/// A tray factory that reports that this desktop has no notification area.
pub fn failing_tray() -> (TrayFactory, Arc<Mutex<Vec<String>>>) {
    let reasons = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&reasons);
    let factory: TrayFactory = Box::new(move |_wiring: Wiring| {
        lock(&recorder).push("no appindicator host".to_string());
        Err("no appindicator host".to_string())
    });
    (factory, reasons)
}

/// A [`TrayHandle`] that forwards to a shared [`RecordingTray`].
///
/// The application owns the handle for the life of the window, and the test needs to
/// look at it at the same time; without `Rc` that would be a second tray.
struct RcHandle(std::rc::Rc<RecordingTray>);

impl TrayHandle for RcHandle {
    fn set_state(&self, state: TrayState) {
        self.0.set_state(state);
    }

    fn set_automation_paused(&self, paused: bool) {
        self.0.set_automation_paused(paused);
    }

    fn close(&self) {
        self.0.close();
    }
}

/// A recorder in an `Arc`, which is what [`crate::notifications::Notifications`]
/// wants.
pub fn recorder() -> Arc<RecordingNotifier> {
    Arc::new(RecordingNotifier::new())
}

/// What a scripted picker was asked for, so a test can assert the dialog itself
/// rather than only its consequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PickerCall {
    Folder {
        title: String,
        start: Option<PathBuf>,
    },
    File {
        title: String,
        start: Option<PathBuf>,
        suggested: Option<String>,
    },
}

/// A picker that answers without a desktop (Wave 6, extended for E09.4's suite).
///
/// The real dialog opens a window on whatever machine runs the tests; this one
/// returns a path the test chose, or nothing at all, and remembers what it was
/// asked. A test that dispatches "choose a folder" therefore exercises the whole
/// handler without drawing anything.
pub struct ScriptedPicker {
    folder: Option<PathBuf>,
    file: Option<PathBuf>,
    calls: Mutex<Vec<PickerCall>>,
}

impl ScriptedPicker {
    /// Cancels everything: the path where the user closes the dialog.
    pub fn cancelling() -> Self {
        Self {
            folder: None,
            file: None,
            calls: Mutex::new(Vec::new()),
        }
    }

    /// Accepts `path` when asked for a folder.
    pub fn accepting_folder(path: impl Into<PathBuf>) -> Self {
        Self {
            folder: Some(path.into()),
            ..Self::cancelling()
        }
    }

    /// Accepts `path` when asked for a file.
    pub fn accepting_file(path: impl Into<PathBuf>) -> Self {
        Self {
            file: Some(path.into()),
            ..Self::cancelling()
        }
    }

    /// Every dialog that was opened, in order.
    pub fn calls(&self) -> Vec<PickerCall> {
        lock(&self.calls).clone()
    }

    /// How many dialogs were opened.
    pub fn call_count(&self) -> usize {
        lock(&self.calls).len()
    }
}

impl Default for ScriptedPicker {
    fn default() -> Self {
        Self::cancelling()
    }
}

impl Picker for ScriptedPicker {
    fn folder(&self, title: &str, start: Option<&Path>) -> Option<PathBuf> {
        lock(&self.calls).push(PickerCall::Folder {
            title: title.to_string(),
            start: start.map(Path::to_path_buf),
        });
        self.folder.clone()
    }

    fn file(&self, title: &str, start: Option<&Path>, suggested: Option<&str>) -> Option<PathBuf> {
        lock(&self.calls).push(PickerCall::File {
            title: title.to_string(),
            start: start.map(Path::to_path_buf),
            suggested: suggested.map(str::to_string),
        });
        self.file.clone()
    }
}

/// A scripted picker in an `Arc`, which is what `AppDeps` wants. Not a native
/// dialog: this is the one every test uses.
pub fn picker(scripted: ScriptedPicker) -> Arc<dyn Picker> {
    Arc::new(scripted)
}
