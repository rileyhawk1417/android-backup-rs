//! The ADB process runner.
//!
//! Design notes:
//!
//! * Everything goes through the [`Adb`] trait so higher layers (and the
//!   reliability suite) can substitute a mock.
//! * stdout/stderr are drained on **dedicated reader threads**. `adb` can produce
//!   unbounded output (`find`, `ls -R`); a naive `wait_with_output`-after-sleep
//!   loop deadlocks as soon as the pipe buffer fills.
//! * On timeout the child is killed and its captured output is still logged.
//! * Process-group killing (`setsid` + `killpg`, `taskkill /T`) needs `unsafe`
//!   `pre_exec` on unix, which this crate forbids. It is therefore injected via
//!   [`ProcessHost`] and implemented in the `android-backup-platform` crate.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::error::{classify, AdbError};

/// Default timeout for quick commands (`devices`, `getprop`, `ls`).
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);
/// Default timeout for long-running commands (large `pull`s are chunked by the
/// transfer engine, so this stays generous rather than unlimited).
pub const LONG_TIMEOUT: Duration = Duration::from_secs(600);

/// Captured result of a successful (exit code 0) adb invocation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: i32,
}

impl CmdOutput {
    /// stdout split into non-empty, trimmed lines.
    pub fn lines(&self) -> impl Iterator<Item = &str> {
        self.stdout.lines().map(str::trim).filter(|l| !l.is_empty())
    }
}

/// How processes are spawned and killed. Replaced by the platform crate with an
/// implementation that kills the whole process group, and by tests with recorders.
pub trait ProcessHost: Send + Sync {
    fn spawn(&self, cmd: &mut Command) -> std::io::Result<Child>;
    fn kill(&self, child: &mut Child);
}

/// Portable fallback: plain spawn, plain kill.
#[derive(Debug, Default, Clone, Copy)]
pub struct PlainProcessHost;

impl ProcessHost for PlainProcessHost {
    fn spawn(&self, cmd: &mut Command) -> std::io::Result<Child> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
    }

    fn kill(&self, child: &mut Child) {
        if let Err(err) = child.kill() {
            tracing::debug!(error = %err, "failed to kill adb process");
        }
    }
}

/// The seam every higher layer depends on.
pub trait Adb: Send + Sync {
    /// Run `adb <args>`.
    fn run(&self, args: &[&str], timeout: Duration) -> Result<CmdOutput, AdbError>;

    /// Path of the adb executable in use, when known.
    fn adb_path(&self) -> Option<&Path> {
        None
    }

    /// Run `adb -s <serial> <args>`.
    fn serial_run(
        &self,
        serial: &str,
        args: &[&str],
        timeout: Duration,
    ) -> Result<CmdOutput, AdbError> {
        let mut full: Vec<&str> = Vec::with_capacity(args.len() + 2);
        full.push("-s");
        full.push(serial);
        full.extend_from_slice(args);
        self.run(&full, timeout)
    }

    /// Convenience: `adb devices -l`.
    fn devices(&self) -> Result<CmdOutput, AdbError> {
        self.run(&["devices", "-l"], DEFAULT_TIMEOUT)
    }

    /// Convenience: `adb start-server` (idempotent; reuses a running server).
    fn start_server(&self) -> Result<CmdOutput, AdbError> {
        self.run(&["start-server"], DEFAULT_TIMEOUT)
    }

    /// Convenience: `adb version`.
    fn version(&self) -> Result<CmdOutput, AdbError> {
        self.run(&["version"], Duration::from_secs(15))
    }
}

/// Runs the real `adb` binary.
pub struct ProcessAdbRunner {
    path: PathBuf,
    host: Arc<dyn ProcessHost>,
    default_timeout: Duration,
}

impl ProcessAdbRunner {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            host: Arc::new(PlainProcessHost),
            default_timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_host(mut self, host: Arc<dyn ProcessHost>) -> Self {
        self.host = host;
        self
    }

    pub fn with_default_timeout(mut self, timeout: Duration) -> Self {
        self.default_timeout = timeout;
        self
    }

    pub fn default_timeout(&self) -> Duration {
        self.default_timeout
    }

    fn build(&self, args: &[&str]) -> Command {
        let mut cmd = Command::new(&self.path);
        cmd.args(args);
        // Windows batch wrappers cannot be spawned directly.
        if cfg!(windows) && is_batch_file(&self.path) {
            let mut wrapped = Command::new("cmd");
            wrapped.arg("/C").arg(&self.path).args(args);
            return wrapped;
        }
        cmd
    }
}

fn is_batch_file(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("bat") || e.eq_ignore_ascii_case("cmd"))
}

impl Adb for ProcessAdbRunner {
    fn run(&self, args: &[&str], timeout: Duration) -> Result<CmdOutput, AdbError> {
        let effective_timeout = if timeout.is_zero() {
            self.default_timeout
        } else {
            timeout
        };
        let mut cmd = self.build(args);
        let serial = serial_of(args);
        let started = Instant::now();

        let mut child = self
            .host
            .spawn(&mut cmd)
            .map_err(|source| AdbError::AdbMissing {
                path: self.path.clone(),
                source,
            })?;

        let out_reader = match child.stdout.take() {
            Some(handle) => Some(match spawn_reader(handle, "stdout") {
                Ok(reader) => reader,
                Err(err) => {
                    self.host.kill(&mut child);
                    let _ = child.wait();
                    return Err(AdbError::Io(err));
                }
            }),
            None => None,
        };
        let err_reader = match child.stderr.take() {
            Some(handle) => Some(match spawn_reader(handle, "stderr") {
                Ok(reader) => reader,
                Err(err) => {
                    self.host.kill(&mut child);
                    let _ = child.wait();
                    return Err(AdbError::Io(err));
                }
            }),
            None => None,
        };

        let deadline = started + effective_timeout;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Some(status),
                Ok(None) => {
                    if Instant::now() >= deadline {
                        timed_out = true;
                        self.host.kill(&mut child);
                        // Reap the process so the readers observe EOF.
                        match child.wait() {
                            Ok(status) => break Some(status),
                            Err(_) => break None,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(err) => {
                    self.host.kill(&mut child);
                    let _ = child.wait();
                    return Err(AdbError::Io(err));
                }
            }
        };

        let stdout = join_reader(out_reader, !timed_out);
        let stderr = join_reader(err_reader, !timed_out);
        let elapsed = started.elapsed();

        if timed_out {
            tracing::warn!(
                args = ?args,
                elapsed_ms = elapsed.as_millis(),
                "adb command timed out; killed after {}s",
                effective_timeout.as_secs()
            );
            if !stdout.is_empty() {
                tracing::debug!(stdout = %truncate(&stdout), "partial stdout at timeout");
            }
            return Err(AdbError::Timeout {
                args: args.iter().map(|s| s.to_string()).collect(),
                secs: effective_timeout.as_secs(),
            });
        }

        let code = status.and_then(|s| s.code()).unwrap_or(-1);
        tracing::trace!(args = ?args, code, elapsed_ms = elapsed.as_millis(), "adb finished");

        if code == 0 {
            Ok(CmdOutput {
                stdout,
                stderr,
                code,
            })
        } else {
            let typed = classify(
                serial.as_deref(),
                &stderr,
                &stdout,
                code,
                &args.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            );
            tracing::debug!(args = ?args, error = %typed, "adb command failed");
            Err(typed)
        }
    }

    fn adb_path(&self) -> Option<&Path> {
        Some(&self.path)
    }
}

/// Output collected from one of the child's pipes.
struct Reader {
    handle: std::thread::JoinHandle<()>,
    /// Filled chunk by chunk, so a partial read is available even if the pipe
    /// never reaches EOF.
    buffer: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// How long to wait for output after killing the child.
///
/// The direct child is dead at this point, but a *grandchild* can still hold the
/// pipe open: a shell script's `sleep`, or `adb`'s own server. Waiting for it
/// would make "kill on timeout" as slow as the very command we gave up on, so the
/// output read so far is returned instead.
const READER_GRACE: Duration = Duration::from_millis(250);

fn spawn_reader(
    mut handle: impl Read + Send + 'static,
    which: &'static str,
) -> std::io::Result<Reader> {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    let buffer = Arc::new(Mutex::new(Vec::new()));
    let done = Arc::new(AtomicBool::new(false));
    let sink = Arc::clone(&buffer);
    let finished = Arc::clone(&done);

    let handle = std::thread::Builder::new()
        .name(format!("adb-{which}"))
        .spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match handle.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        if let Ok(mut buf) = sink.lock() {
                            buf.extend_from_slice(&chunk[..n]);
                        }
                    }
                    Err(err) => {
                        tracing::debug!(which, error = %err, "failed reading adb output");
                        break;
                    }
                }
            }
            finished.store(true, Ordering::SeqCst);
        })?;

    Ok(Reader {
        handle,
        buffer,
        done,
    })
}

/// Collect a reader's output. `wait_for_eof` is false after a timeout, where the
/// pipe may be held open by an orphaned grandchild; the reader thread is then
/// left to finish on its own (it owns its buffer and exits when the pipe closes).
fn join_reader(reader: Option<Reader>, wait_for_eof: bool) -> String {
    use std::sync::atomic::Ordering;

    let Some(reader) = reader else {
        return String::new();
    };
    if wait_for_eof {
        let _ = reader.handle.join();
    } else {
        let deadline = Instant::now() + READER_GRACE;
        while !reader.done.load(Ordering::SeqCst) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
    }
    let text = match reader.buffer.lock() {
        Ok(buf) => String::from_utf8_lossy(&buf).into_owned(),
        Err(poisoned) => String::from_utf8_lossy(&poisoned.into_inner()).into_owned(),
    };
    text
}

/// Extract the device serial from `-s <serial>`.
fn serial_of(args: &[&str]) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == "-s" || w[0] == "--serial")
        .map(|w| w[1].to_string())
}

fn truncate(s: &str) -> String {
    const MAX: usize = 400;
    if s.len() <= MAX {
        return s.to_string();
    }
    let mut end = MAX;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}… ({} bytes total)", &s[..end], s.len())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write_script;

    #[test]
    fn runs_a_successful_command() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "ok", &["echo hello", "exit 0"]);
        let runner = ProcessAdbRunner::new(script);
        let out = runner.run(&["version"], Duration::from_secs(10)).unwrap();
        assert_eq!(out.stdout.trim(), "hello");
        assert_eq!(out.code, 0);
    }

    #[test]
    fn failing_command_is_classified() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(
            dir.path(),
            "unauth",
            &["echo 'error: device unauthorized.' 1>&2", "exit 1"],
        );
        let runner = ProcessAdbRunner::new(script);
        let err = runner
            .serial_run("ABC", &["shell", "ls"], Duration::from_secs(10))
            .unwrap_err();
        assert!(matches!(err, AdbError::DeviceUnauthorized { .. }));
        assert!(err.is_transient());
    }

    #[test]
    fn timeout_kills_the_process_and_reports_partial_output() {
        let dir = tempfile::tempdir().unwrap();
        let script = write_script(dir.path(), "slow", &["echo starting", "sleep 30"]);
        let runner = ProcessAdbRunner::new(script);
        let started = Instant::now();
        let err = runner
            .run(&["pull"], Duration::from_millis(300))
            .unwrap_err();
        assert!(matches!(err, AdbError::Timeout { .. }), "{err:?}");
        // The kill must be prompt, not a full wait for the 30s sleep.
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn large_output_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        // ~2 MiB of stdout: far beyond a pipe buffer.
        let script = write_script(
            dir.path(),
            "big",
            &["for i in $(seq 1 40000); do echo \"line $i padding padding padding padding\"; done"],
        );
        let runner = ProcessAdbRunner::new(script);
        let out = runner.run(&["find"], Duration::from_secs(60)).unwrap();
        assert!(
            out.stdout.len() > 1_000_000,
            "got {} bytes",
            out.stdout.len()
        );
        assert_eq!(out.lines().count(), 40_000);
    }

    #[test]
    fn missing_executable_is_reported_as_adb_missing() {
        let runner = ProcessAdbRunner::new("/nonexistent/adb-does-not-exist");
        let err = runner
            .run(&["version"], Duration::from_secs(5))
            .unwrap_err();
        assert!(matches!(err, AdbError::AdbMissing { .. }), "{err:?}");
    }

    #[test]
    fn serial_is_extracted_for_error_context() {
        assert_eq!(serial_of(&["-s", "XYZ", "shell"]), Some("XYZ".to_string()));
        assert_eq!(serial_of(&["devices"]), None);
    }

    #[test]
    fn batch_wrapping_is_detected() {
        assert!(is_batch_file(Path::new("C:/tools/adb.bat")));
        assert!(!is_batch_file(Path::new("/usr/bin/adb")));
    }
}
