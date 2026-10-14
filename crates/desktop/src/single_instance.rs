//! One process per manifest (Wave 9, E09.5).
//!
//! # Why this exists
//!
//! Before Wave 9 the app only ever acted when a person clicked something, and two
//! copies of it were merely untidy. Automation changes that: an automatic backup
//! starts *on its own*, so a second process on the same manifest would start the
//! same backup for the same phone at the same time — two writers on one destination
//! tree, and two copies of every file. That is a real way to lose data, and it is
//! the kind of thing that only happens to somebody who launched the app at login and
//! then launched it again from the desktop shortcut.
//!
//! # How
//!
//! The lock is taken with `std::fs::File::try_lock` (stable since Rust 1.89), which is
//! `flock` on Linux and `LockFileEx` on Windows — in both cases owned by the *kernel*,
//! so a process that is killed with `SIGKILL`, crashes, or is closed while writing
//! releases it. There is no stale lock to clean up and no pid to liveness-check,
//! because there is nothing to be wrong about: if the lock is held, its holder is
//! running.
//!
//! The holder's pid is recorded for one thing only: to tell the user which process
//! already has the manifest. It is never trusted and never consulted to decide
//! anything. It lives in a separate, **unlocked** sidecar (`instance.pid`), not in
//! the lock file — on Windows a byte-range lock is mandatory, and while the lock is
//! held no other handle may read the locked file at all (Linux's advisory `flock`
//! hides that, which is why the first cut of this module only worked on Linux). The
//! sidecar is removed *before* the lock is released, so a pid never outlives its
//! claim and a takeover always writes a fresh one.
//!
//! # What is deliberately absent
//!
//! No IPC. A second launch reports that the first is running and exits; it does not
//! ask the running copy to show its window. Reaching the running copy would mean a
//! socket on Linux and a window message on Windows — two more platform protocols to
//! get wrong, in service of a convenience the notification area already provides
//! (the tray icon reopens the window, E09.5). The message therefore names the tray
//! instead of pretending to be a hand-off.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process;

use android_backup_core::config::AppPaths;

/// The name of the lock file, inside the data directory.
pub const LOCK_FILE: &str = "instance.lock";

/// The name of the unlocked sidecar that carries the holder's pid, beside the lock.
///
/// The pid cannot live inside the lock file: on Windows, byte-range locks are
/// mandatory, so while the lock is held a second handle reading the file fails with
/// a lock violation (os error 33) — Linux's advisory `flock` never tripped over
/// this. Display text only; see the module header.
pub const PID_FILE: &str = "instance.pid";

/// A held lock, or the news that somebody else holds it.
#[derive(Debug)]
pub enum Claim {
    /// This process owns the manifest.
    Ours(InstanceLock),
    /// Another process owns the manifest. The string says which, when the other
    /// process was polite enough to write it down.
    Taken(Option<u32>),
}

/// A held lock. Dropping it releases the manifest for the next launch.
#[derive(Debug)]
pub struct InstanceLock {
    /// Not read again: the lock lives exactly as long as the file handle, so holding
    /// the handle *is* holding the lock.
    file: std::fs::File,
    path: PathBuf,
    /// Removed on drop, before [`InstanceLock::file`] is unlocked.
    pid_path: PathBuf,
}

impl InstanceLock {
    /// Where the lock lives. Beside the manifest, because the manifest is what is
    /// being protected: two installations that share a manifest share a lock, and two
    /// that do not, do not.
    pub fn path(paths: &AppPaths) -> PathBuf {
        paths.data_dir.join(LOCK_FILE)
    }

    /// Where the holder's pid sidecar lives.
    pub fn pid_path(paths: &AppPaths) -> PathBuf {
        paths.data_dir.join(PID_FILE)
    }

    /// Who last wrote its pid into the pid sidecar. Only used to word a message.
    pub fn holder(path: &Path) -> Option<u32> {
        std::fs::read_to_string(path)
            .ok()?
            .trim()
            .parse::<u32>()
            .ok()
    }

    pub fn path_of(&self) -> &Path {
        &self.path
    }
}

impl Drop for InstanceLock {
    fn drop(&mut self) {
        // The pid goes first, *before* the lock: a claimant that takes the lock over
        // writes a fresh pid, and a removal after unlocking could race that write
        // and erase it. While the lock is still held, no claimant can pass the lock
        // to read or rewrite the pid, so this order is the race-free one. Failing
        // here is not interesting: a leftover pid names a claim that is already
        // gone, and the next holder rewrites it.
        let _ = std::fs::remove_file(&self.pid_path);
        // Explicit, so that a launch immediately after a clean exit cannot race the
        // process teardown. Failing here is not interesting: the handle closing would
        // release it anyway.
        let _ = self.file.unlock();
    }
}

/// Try to become the one instance for this manifest.
///
/// * `Ok(Claim::Ours(_))` — carry on; keep the value alive for the life of the process.
/// * `Ok(Claim::Taken(pid))` — another copy is running; the caller shows the message
///   in [`taken_message`] and exits.
/// * `Err(message)` — the lock file could not be opened. Not fatal: the app can still
///   run, without the guarantee. The caller logs it and carries on, because refusing to
///   start over a broken lock file would be worse than the risk it guards against.
pub fn claim(paths: &AppPaths) -> Result<Claim, String> {
    let path = InstanceLock::path(paths);
    let pid_path = InstanceLock::pid_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("could not create {}: {err}", parent.display()))?;
    }
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|err| format!("could not open the lock file {}: {err}", path.display()))?;

    match file.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Ok(Claim::Taken(InstanceLock::holder(&pid_path)))
        }
        Err(std::fs::TryLockError::Error(err)) => {
            return Err(format!("could not lock {}: {err}", path.display()));
        }
    }

    // Ours: record the pid where a second launch can read it — the unlocked
    // sidecar, because this very file is now locked shut on Windows. A failure
    // here costs a readable pid and nothing else, so it is logged rather than
    // propagated.
    if let Err(err) = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&pid_path)
        .and_then(|mut pid_file| {
            pid_file
                .write_all(process::id().to_string().as_bytes())
                .and_then(|()| pid_file.flush())
        })
    {
        tracing::warn!(%err, "could not write the lock holder's pid");
    }

    Ok(Claim::Ours(InstanceLock {
        file,
        path,
        pid_path,
    }))
}

/// What to tell somebody who started a second copy.
pub fn taken_message(pid: Option<u32>) -> String {
    let who = match pid {
        Some(pid) => format!("(process {pid})"),
        None => String::new(),
    };
    format!(
        "Android Backup is already running {who}.\n\n\
         It keeps running after its window is closed, so that automatic backups \
         can happen while it is out of the way. Open the window from its icon in \
         the notification area (the system tray), next to the clock."
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn paths(dir: &Path) -> AppPaths {
        AppPaths {
            config_file: dir.join("config.toml"),
            data_dir: dir.to_path_buf(),
            log_dir: dir.to_path_buf(),
            managed_adb_dir: dir.join("platform-tools"),
        }
    }

    #[test]
    fn the_first_claim_wins_and_the_second_is_told_who_has_it() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());

        let first = claim(&paths).expect("first claim");
        let Claim::Ours(lock) = first else {
            panic!("the first process must get the manifest");
        };

        let second = claim(&paths).expect("second claim");
        match second {
            Claim::Taken(pid) => assert_eq!(
                pid,
                Some(process::id()),
                "the holder is this very process, and it wrote its pid down"
            ),
            Claim::Ours(_) => panic!("two processes must not both own the manifest"),
        }

        // Releasing it lets the next launch in — which is what a restart looks like.
        drop(lock);
        assert!(matches!(claim(&paths), Ok(Claim::Ours(_))));
    }

    #[test]
    fn the_message_names_the_holder_and_the_notification_area() {
        let with_pid = taken_message(Some(4242));
        assert!(with_pid.contains("4242"));
        assert!(with_pid.contains("already running"));
        // The one thing the user can act on: where the running copy went.
        assert!(with_pid.to_lowercase().contains("notification area"));

        let without_pid = taken_message(None);
        assert!(without_pid.contains("already running"));
        assert_eq!(
            with_pid.lines().next().map(str::to_string),
            Some("Android Backup is already running (process 4242).".to_string())
        );
    }

    #[test]
    fn the_lock_lives_beside_the_manifest() {
        let dir = tempfile::tempdir().unwrap();
        let paths = paths(dir.path());
        assert_eq!(InstanceLock::path(&paths), dir.path().join(LOCK_FILE));
        assert_eq!(InstanceLock::pid_path(&paths), dir.path().join(PID_FILE));
    }

    /// A lock file that cannot be opened must not stop the application: it is a
    /// guard rail, not a gate.
    #[test]
    fn a_missing_directory_is_an_error_not_a_claim() {
        let dir = tempfile::tempdir().unwrap();
        let mut paths = paths(dir.path());
        // A file where the directory should be: `create_dir_all` cannot make it.
        let blocked = dir.path().join("blocked");
        std::fs::write(&blocked, b"not a directory").unwrap();
        paths.data_dir = blocked.join("data");

        let message = claim(&paths).expect_err("cannot lock a file inside a plain file");
        assert!(message.contains("could not create"), "{message}");
        assert!(message.contains("blocked"), "{message}");
    }
}
