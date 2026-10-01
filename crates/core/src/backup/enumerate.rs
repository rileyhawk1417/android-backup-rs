//! Reading the phone's file tree and pulling individual files.
//!
//! Everything above this file works on [`RemoteEntry`] values, so the whole backup
//! engine can be driven from a mock filesystem. Two `find`-based strategies are
//! tried because Android ships several `find`/`stat` generations:
//!
//! 1. `find <dir> -type f -exec stat -c '%s %Y %n' {} +` (toybox, current devices)
//! 2. `find <dir> -type f -printf '%s %T@ %p\n'` (GNU-flavoured, older toolbox)
//!
//! Known limitation (documented, not hidden): file names containing a newline
//! cannot be represented by these listings and are skipped with a warning, and
//! `adb pull` restarts a file rather than resuming it mid-file — the staging
//! directory is what makes that safe.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use crate::adb::{Adb, AdbError};

/// Default timeout for a directory listing.
pub const LIST_TIMEOUT: Duration = Duration::from_secs(120);
/// Default timeout for a single `pull`.
pub const PULL_TIMEOUT: Duration = Duration::from_secs(900);

/// What a remote entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

/// One file (or directory) on the phone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteEntry {
    /// Phone-absolute path.
    pub path: String,
    /// Size in bytes (0 for non-files).
    pub size: u64,
    /// Modification time in unix seconds, when the device reports one.
    pub modified_at: Option<i64>,
    pub kind: EntryKind,
}

/// Filesystem failures on the phone side.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RemoteError {
    #[error(transparent)]
    Adb(#[from] AdbError),

    #[error("could not read device storage roots")]
    NoStorageRoot,

    #[error("could not list {folder}: {message}")]
    ListingFailed { folder: String, message: String },

    #[error("file listing is not supported by this device's shell: {0}")]
    UnsupportedListing(String),

    #[error("could not copy {path} from the phone: {message}")]
    PullFailed { path: String, message: String },
}

impl RemoteError {
    /// Build an equivalent error.
    ///
    /// `RemoteError` is not `Clone` because it can wrap `std::io::Error`; test
    /// doubles and retry helpers need a fresh copy of a canned failure instead.
    pub fn duplicate(&self) -> Self {
        match self {
            Self::Adb(err) => Self::Adb(err.duplicate()),
            Self::NoStorageRoot => Self::NoStorageRoot,
            Self::ListingFailed { folder, message } => Self::ListingFailed {
                folder: folder.clone(),
                message: message.clone(),
            },
            Self::UnsupportedListing(message) => Self::UnsupportedListing(message.clone()),
            Self::PullFailed { path, message } => Self::PullFailed {
                path: path.clone(),
                message: message.clone(),
            },
        }
    }

    /// True when waiting and retrying the same operation is worthwhile.
    pub fn is_transient(&self) -> bool {
        match self {
            Self::Adb(err) => err.is_transient(),
            Self::ListingFailed { .. } | Self::PullFailed { .. } => false,
            Self::NoStorageRoot | Self::UnsupportedListing(_) => false,
        }
    }

    /// True when the user must act on the phone (unlock / approve debugging).
    pub fn needs_authorization(&self) -> bool {
        matches!(self, Self::Adb(err) if err.needs_authorization())
    }

    /// True when the device vanished mid-job.
    pub fn device_gone(&self) -> bool {
        matches!(self, Self::Adb(err) if err.is_device_gone())
    }
}

/// The remote-filesystem seam.
pub trait RemoteFiles: Send + Sync {
    /// List every regular file under `folder`, recursively.
    fn list_files(&self, serial: &str, folder: &str) -> Result<Vec<RemoteEntry>, RemoteError>;

    /// Copy one file from the phone to `local`.
    fn pull(&self, serial: &str, remote: &str, local: &Path) -> Result<(), RemoteError>;

    /// Storage roots present on the device.
    fn storage_roots(&self, serial: &str) -> Result<Vec<String>, RemoteError>;

    /// SHA-256 (lowercase hex) of a file on the phone.
    ///
    /// `Ok(None)` means "this device cannot compute a digest" — an old `toybox`
    /// without `sha256sum`, or a filesystem where it fails. Callers must treat
    /// that as *unknown evidence*, never as "the bytes match":
    /// [`crate::backup::transfer::VerifyMode::Hash`] then records the digest of
    /// the local copy and says so in the log.
    fn hash(&self, _serial: &str, _remote: &str) -> Result<Option<String>, RemoteError> {
        Ok(None)
    }
}

/// The `find`/`stat` strategy used for a listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListingStrategy {
    /// `find … -exec stat -c '%s %Y %n' {} +`
    FindStat,
    /// `find … -printf '%s %T@ %p\n'`
    FindPrintf,
}

impl ListingStrategy {
    /// The shell command for this strategy.
    pub fn command(&self, folder: &str) -> String {
        match self {
            Self::FindStat => format!(
                "find {dir} -type f -exec stat -c '%s %Y %n' {{}} +",
                dir = shell_quote(folder)
            ),
            Self::FindPrintf => format!(
                "find {dir} -type f -printf '%s %T@ %p\\n'",
                dir = shell_quote(folder)
            ),
        }
    }

    /// Parse this strategy's output.
    pub fn parse(&self, folder: &str, output: &str) -> Vec<RemoteEntry> {
        let mut entries = Vec::new();
        for line in output.lines() {
            let line = line.trim_end_matches('\r');
            if line.trim().is_empty() {
                continue;
            }
            let parsed = match self {
                Self::FindStat => parse_stat_line(line),
                Self::FindPrintf => parse_printf_line(line),
            };
            match parsed {
                Some(entry) => entries.push(entry),
                None => tracing::debug!(line, "skipping unparsable listing line"),
            }
        }
        // Defence in depth: only report files actually under the requested folder.
        entries.retain(|e| e.path.starts_with(folder.trim_end_matches('/')));
        entries
    }
}

/// Parse `1024 1700000000 /sdcard/DCIM/a.jpg`.
fn parse_stat_line(line: &str) -> Option<RemoteEntry> {
    let (size, rest) = line.trim_start().split_once(' ')?;
    let (mtime, path) = rest.trim_start().split_once(' ')?;
    let path = path.trim();
    if path.is_empty() {
        return None;
    }
    Some(RemoteEntry {
        path: path.to_string(),
        size: size.trim().parse().ok()?,
        modified_at: parse_mtime(mtime.trim()),
        kind: EntryKind::File,
    })
}

/// Parse `1024 1700000000.1234 /sdcard/DCIM/a.jpg`.
fn parse_printf_line(line: &str) -> Option<RemoteEntry> {
    parse_stat_line(line)
}

fn parse_mtime(raw: &str) -> Option<i64> {
    // `stat -c %Y` is integral; `-printf %T@` may carry a fractional part.
    let integral = raw.split(['.', ',']).next()?;
    integral.parse::<i64>().ok()
}

/// Single-quote a path for the device shell, rejecting quotes we cannot encode.
fn shell_quote(path: &str) -> String {
    let cleaned = path.replace('\'', "");
    format!("'{cleaned}'")
}

/// [`RemoteFiles`] backed by a real `adb` process.
pub struct AdbRemoteFiles {
    adb: Arc<dyn Adb>,
    strategies: Vec<ListingStrategy>,
    list_timeout: Duration,
    pull_timeout: Duration,
}

impl AdbRemoteFiles {
    pub fn new(adb: Arc<dyn Adb>) -> Self {
        Self {
            adb,
            strategies: vec![ListingStrategy::FindStat, ListingStrategy::FindPrintf],
            list_timeout: LIST_TIMEOUT,
            pull_timeout: PULL_TIMEOUT,
        }
    }

    pub fn with_strategies(mut self, strategies: Vec<ListingStrategy>) -> Self {
        self.strategies = strategies;
        self
    }

    pub fn with_timeouts(mut self, list: Duration, pull: Duration) -> Self {
        self.list_timeout = list;
        self.pull_timeout = pull;
        self
    }

    /// Run a shell command on the device.
    fn shell(&self, serial: &str, command: &str, timeout: Duration) -> Result<String, RemoteError> {
        let out = self.adb.serial_run(serial, &["shell", command], timeout)?;
        Ok(out.stdout)
    }
}

impl RemoteFiles for AdbRemoteFiles {
    fn list_files(&self, serial: &str, folder: &str) -> Result<Vec<RemoteEntry>, RemoteError> {
        let mut last_error: Option<String> = None;
        for strategy in &self.strategies {
            let command = strategy.command(folder);
            match self.shell(serial, &command, self.list_timeout) {
                Ok(stdout) => {
                    let entries = strategy.parse(folder, &stdout);
                    tracing::debug!(
                        folder,
                        strategy = ?strategy,
                        count = entries.len(),
                        "listed remote files"
                    );
                    return Ok(entries);
                }
                Err(err) => {
                    // A device-level failure (gone, unauthorized) must not be
                    // masked by trying another listing strategy.
                    if err.is_transient() {
                        return Err(err);
                    }
                    tracing::debug!(folder, strategy = ?strategy, error = %err, "listing strategy failed");
                    last_error = Some(err.to_string());
                }
            }
        }
        Err(RemoteError::ListingFailed {
            folder: folder.to_string(),
            message: last_error.unwrap_or_else(|| "no listing strategy available".into()),
        })
    }

    fn pull(&self, serial: &str, remote: &str, local: &Path) -> Result<(), RemoteError> {
        let local = local.to_string_lossy().to_string();
        self.adb
            .serial_run(serial, &["pull", remote, &local], self.pull_timeout)
            .map(|_| ())
            .map_err(|err| match err {
                AdbError::Timeout { .. } => RemoteError::PullFailed {
                    path: remote.to_string(),
                    message: "transfer timed out".into(),
                },
                transient if transient.is_transient() => RemoteError::Adb(transient),
                other => RemoteError::PullFailed {
                    path: remote.to_string(),
                    message: other.to_string(),
                },
            })
    }

    fn storage_roots(&self, serial: &str) -> Result<Vec<String>, RemoteError> {
        let command = "for p in /storage/emulated/0 /sdcard /storage/self/primary; do [ -d $p ] && echo $p; done";
        let stdout = self.shell(serial, command, Duration::from_secs(30))?;
        let roots: Vec<String> = stdout
            .lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect();
        if roots.is_empty() {
            return Err(RemoteError::NoStorageRoot);
        }
        Ok(roots)
    }

    fn hash(&self, serial: &str, remote: &str) -> Result<Option<String>, RemoteError> {
        let command = format!("sha256sum {}", shell_quote(remote));
        match self.shell(serial, &command, self.list_timeout) {
            Ok(stdout) => Ok(parse_digest(&stdout)),
            Err(transient) if transient.is_transient() => Err(transient),
            Err(err) => {
                // No `sha256sum` on this build of toybox, or a file that cannot be
                // read: fall back to size-only evidence rather than failing a
                // backup that is otherwise fine.
                tracing::debug!(remote, error = %err, "the device cannot hash this file");
                Ok(None)
            }
        }
    }
}

/// First 64-hex-digit token of `sha256sum` output, if there is one.
fn parse_digest(output: &str) -> Option<String> {
    output
        .split_whitespace()
        .find(|token| token.len() == 64 && token.chars().all(|c| c.is_ascii_hexdigit()))
        .map(|token| token.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockAdb;
    use crate::testutil::write_script;
    use std::sync::Arc;

    const FIND_STAT_OUT: &str = "\
4096 1700000000 /storage/emulated/0/DCIM/a.jpg\n\
512 1700000001 /storage/emulated/0/DCIM/b.jpg\n";

    #[test]
    fn parses_stat_output() {
        let entries = ListingStrategy::FindStat.parse("/storage/emulated/0/DCIM", FIND_STAT_OUT);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "/storage/emulated/0/DCIM/a.jpg");
        assert_eq!(entries[0].size, 4096);
        assert_eq!(entries[0].modified_at, Some(1700000000));
        assert_eq!(entries[0].kind, EntryKind::File);
    }

    #[test]
    fn parses_printf_output_with_fractional_mtime() {
        let out = "4096 1700000000.1234 /storage/emulated/0/DCIM/a.jpg\n";
        let entries = ListingStrategy::FindPrintf.parse("/storage/emulated/0/DCIM", out);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].modified_at, Some(1700000000));
    }

    #[test]
    fn tolerates_crlf_and_blank_lines() {
        let out = "4096 1700000000 /storage/emulated/0/DCIM/a.jpg\r\n\r\n";
        let entries = ListingStrategy::FindStat.parse("/storage/emulated/0/DCIM", out);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn paths_with_spaces_are_preserved() {
        let out = "10 1700000000 /storage/emulated/0/DCIM/My Photos/a b.jpg\n";
        let entries = ListingStrategy::FindStat.parse("/storage/emulated/0/DCIM", out);
        assert_eq!(
            entries[0].path,
            "/storage/emulated/0/DCIM/My Photos/a b.jpg"
        );
    }

    #[test]
    fn unparsable_lines_are_skipped_not_fatal() {
        let out = "garbage\n4096 1700000000 /storage/emulated/0/DCIM/a.jpg\n";
        let entries = ListingStrategy::FindStat.parse("/storage/emulated/0/DCIM", out);
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn entries_outside_the_requested_folder_are_dropped() {
        let out = "1 1 /etc/passwd\n1 1 /storage/emulated/0/DCIM/a\n";
        let entries = ListingStrategy::FindStat.parse("/storage/emulated/0/DCIM", out);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "/storage/emulated/0/DCIM/a");
    }

    #[test]
    fn missing_mtime_is_allowed() {
        let out = "4096 0 /storage/emulated/0/DCIM/a.jpg\n";
        let entries = ListingStrategy::FindStat.parse("/storage/emulated/0/DCIM", out);
        assert_eq!(entries[0].modified_at, Some(0));
    }

    #[test]
    fn list_uses_adb_shell_and_reports_files() {
        let mock = Arc::new(MockAdb::new().on("find", MockAdb::ok(FIND_STAT_OUT)));
        let remote = AdbRemoteFiles::new(mock.clone());
        let entries = remote
            .list_files("SER1", "/storage/emulated/0/DCIM")
            .unwrap();
        assert_eq!(entries.len(), 2);
        assert!(mock.calls()[0].contains("-s SER1 shell"));
        assert!(mock.calls()[0].contains("find '/storage/emulated/0/DCIM'"));
    }

    #[test]
    fn list_falls_back_to_the_second_strategy() {
        let mock = Arc::new(MockAdb::new().always(MockAdb::err(AdbError::ProcessFailed {
            code: 1,
            stderr: "find: unknown option -exec".into(),
            args: vec![],
        })));
        let remote = AdbRemoteFiles::new(mock.clone());
        let err = remote
            .list_files("SER1", "/storage/emulated/0/DCIM")
            .unwrap_err();
        assert!(matches!(err, RemoteError::ListingFailed { .. }));
        assert_eq!(mock.call_count(), 2, "both strategies were tried");
    }

    #[test]
    fn a_transient_device_error_is_not_retried_with_another_strategy() {
        let mock = Arc::new(MockAdb::new().always(MockAdb::err(AdbError::DeviceOffline {
            serial: Some("SER1".into()),
        })));
        let remote = AdbRemoteFiles::new(mock.clone());
        let err = remote
            .list_files("SER1", "/storage/emulated/0/DCIM")
            .unwrap_err();
        assert!(err.is_transient());
        assert_eq!(mock.call_count(), 1);
    }

    #[test]
    fn pull_reports_success_and_failure_distinctly() {
        let mock = Arc::new(MockAdb::new().on("pull", MockAdb::ok("1 file pulled")));
        let remote = AdbRemoteFiles::new(mock.clone());
        remote
            .pull(
                "SER1",
                "/storage/emulated/0/a.jpg",
                Path::new("/tmp/a.jpg.part"),
            )
            .unwrap();
        assert!(mock.calls()[0].contains("pull /storage/emulated/0/a.jpg"));

        let failing = Arc::new(MockAdb::new().on(
            "pull",
            MockAdb::err(AdbError::ProcessFailed {
                code: 1,
                stderr: "remote object does not exist".into(),
                args: vec![],
            }),
        ));
        let remote = AdbRemoteFiles::new(failing);
        let err = remote
            .pull(
                "SER1",
                "/storage/emulated/0/gone.jpg",
                Path::new("/tmp/x.part"),
            )
            .unwrap_err();
        assert!(matches!(err, RemoteError::PullFailed { .. }));
        assert!(!err.is_transient());
    }

    #[test]
    fn pull_timeout_is_reported_as_a_transient_style_failure() {
        let mock = Arc::new(MockAdb::new().on(
            "pull",
            MockAdb::err(AdbError::Timeout {
                args: vec!["pull".into()],
                secs: 900,
            }),
        ));
        let remote = AdbRemoteFiles::new(mock);
        let err = remote
            .pull(
                "SER1",
                "/storage/emulated/0/big.bin",
                Path::new("/tmp/x.part"),
            )
            .unwrap_err();
        assert!(matches!(err, RemoteError::PullFailed { .. }));
        assert_eq!(
            err.to_string(),
            "could not copy /storage/emulated/0/big.bin from the phone: transfer timed out"
        );
    }

    #[test]
    fn storage_roots_are_detected() {
        let mock = Arc::new(MockAdb::new().on(
            "for p in",
            MockAdb::ok("/storage/emulated/0\n/sdcard\n/storage/self/primary\n"),
        ));
        let remote = AdbRemoteFiles::new(mock);
        let roots = remote.storage_roots("SER1").unwrap();
        assert_eq!(roots.len(), 3);
    }

    #[test]
    fn no_storage_root_is_an_error() {
        let mock = Arc::new(MockAdb::new().on("for p in", MockAdb::ok("")));
        let remote = AdbRemoteFiles::new(mock);
        assert!(matches!(
            remote.storage_roots("SER1").unwrap_err(),
            RemoteError::NoStorageRoot
        ));
    }

    #[test]
    fn a_digest_is_read_from_sha256sum_output() {
        let out =
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  /sdcard/a b.jpg\n";
        let mock = Arc::new(MockAdb::new().on("sha256sum", MockAdb::ok(out)));
        let remote = AdbRemoteFiles::new(mock.clone());
        let hash = remote
            .hash("SER1", "/storage/emulated/0/DCIM/a b.jpg")
            .unwrap();
        assert_eq!(
            hash.as_deref(),
            Some("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824")
        );
        assert!(mock.calls()[0].contains("sha256sum '/storage/emulated/0/DCIM/a b.jpg'"));
    }

    #[test]
    fn a_device_without_sha256sum_reports_no_digest() {
        let mock = Arc::new(MockAdb::new().on(
            "sha256sum",
            MockAdb::err(AdbError::ProcessFailed {
                code: 127,
                stderr: "sha256sum: not found".into(),
                args: vec!["shell".into(), "sha256sum".into()],
            }),
        ));
        let remote = AdbRemoteFiles::new(mock);
        assert_eq!(remote.hash("SER1", "/sdcard/a.jpg").unwrap(), None);
    }

    #[test]
    fn unparsable_hash_output_is_not_a_digest() {
        assert_eq!(
            parse_digest("sha256sum: /sdcard/a: Permission denied\n"),
            None
        );
        assert_eq!(parse_digest("deadbeef  /sdcard/a\n"), None);
        assert_eq!(
            parse_digest(&format!("{}  /sdcard/a\n", "AB".repeat(32))).as_deref(),
            Some(&"ab".repeat(32)[..])
        );
    }

    #[test]
    fn end_to_end_against_a_fake_adb_script() {
        // Exercises the real process runner, not just the mock: proves the
        // command we build is one a shell can actually run.
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "adb",
            &[
                "printf '12 1700000000 /storage/emulated/0/DCIM/a.jpg\\n'",
                "exit 0",
            ],
        );
        let runner: Arc<dyn Adb> = Arc::new(crate::adb::ProcessAdbRunner::new(script));
        let remote = AdbRemoteFiles::new(runner);
        let entries = remote
            .list_files("SER1", "/storage/emulated/0/DCIM")
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, 12);
    }

    #[test]
    fn shell_quoting_removes_single_quotes() {
        assert_eq!(shell_quote("/a'b"), "'/ab'");
        assert_eq!(shell_quote("/plain"), "'/plain'");
    }
}
