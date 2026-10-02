//! Test doubles for the desktop crate.
//!
//! The installer seam exists because the real thing downloads 9 MB from Google
//! and then runs the binary it produced: no test can provoke a checksum
//! mismatch, a full disk or a cancellation against `dl.google.com`. A
//! [`ScriptedInstaller`] plays those outcomes back deterministically, including
//! the callback that asks it to stop.
//!
//! Shipped (not `#[cfg(test)]`) for the same reason as `core::mock`: the
//! integration tests in `tests/` are separate crates and cannot see
//! `#[cfg(test)]` items.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};

use android_backup_core::adb::installer::{
    Control, DigestAlgorithm, InstallError, InstallReport, Progress, Revision, Stage,
};
use android_backup_core::adb::locate::AdbVersion;

use crate::startup::AdbInstaller;

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
