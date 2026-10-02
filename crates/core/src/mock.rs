//! Test doubles for the I/O seams.
//!
//! These live in the library (not behind `#[cfg(test)]`) so that **integration
//! tests in `tests/`** — which are separate crates — can drive the whole engine
//! without a phone attached. They are tiny, dependency-free and never used by the
//! production code paths, so a fixture that is asked for something impossible
//! panics rather than returning a `Result` the caller would have to handle.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::adb::runner::{Adb, CmdOutput};
use crate::adb::AdbError;
use crate::backup::destination::FsProbe;
use crate::backup::destination::{DestError, SpaceInfo};
use crate::backup::enumerate::{EntryKind, RemoteEntry, RemoteError, RemoteFiles};

/// A canned response: either a successful output or an error.
#[derive(Clone)]
pub struct Response {
    inner: Arc<dyn Fn() -> Result<CmdOutput, AdbError> + Send + Sync>,
}

impl Response {
    /// Successful command with `stdout` and exit code 0.
    pub fn ok(stdout: &str) -> Self {
        let stdout = stdout.to_string();
        Self {
            inner: Arc::new(move || {
                Ok(CmdOutput {
                    stdout: stdout.clone(),
                    stderr: String::new(),
                    code: 0,
                })
            }),
        }
    }

    /// Failing command. The error is duplicated for every call.
    pub fn err(err: AdbError) -> Self {
        let err = Arc::new(err);
        Self {
            inner: Arc::new(move || Err(err.duplicate())),
        }
    }

    /// Fully dynamic behaviour, for tests that need to react to arguments.
    pub fn dynamic(f: impl Fn() -> Result<CmdOutput, AdbError> + Send + Sync + 'static) -> Self {
        Self { inner: Arc::new(f) }
    }

    fn call(&self) -> Result<CmdOutput, AdbError> {
        (self.inner)()
    }
}

struct Handler {
    pattern: String,
    response: Response,
}

/// A scripted [`Adb`] implementation.
///
/// Resolution order for each call:
/// 1. the scripted queue (consumed front-to-back), if non-empty;
/// 2. the `always` response, if set;
/// 3. the first handler whose pattern is contained in the joined arguments;
/// 4. otherwise a `ProcessFailed` error so a missing expectation is loud, not silent.
pub struct MockAdb {
    handlers: Mutex<Vec<Handler>>,
    scripted: Mutex<VecDeque<Response>>,
    always: Mutex<Option<Response>>,
    calls: Mutex<Vec<Vec<String>>>,
    adb_path: Option<PathBuf>,
}

impl Default for MockAdb {
    fn default() -> Self {
        Self::new()
    }
}

impl MockAdb {
    pub fn new() -> Self {
        Self {
            handlers: Mutex::new(Vec::new()),
            scripted: Mutex::new(VecDeque::new()),
            always: Mutex::new(None),
            calls: Mutex::new(Vec::new()),
            adb_path: None,
        }
    }

    /// Convenience: `Response::ok`.
    pub fn ok(stdout: &str) -> Response {
        Response::ok(stdout)
    }

    /// Convenience: `Response::err`.
    pub fn err(err: AdbError) -> Response {
        Response::err(err)
    }

    pub fn with_adb_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.adb_path = Some(path.into());
        self
    }

    /// Register a handler for calls whose joined arguments contain `pattern`.
    pub fn on(self, pattern: &str, response: Response) -> Self {
        self.handlers.lock_or_recover().push(Handler {
            pattern: pattern.to_string(),
            response,
        });
        self
    }

    /// Response used when no scripted entry and no handler matches.
    pub fn always(self, response: Response) -> Self {
        *self.always.lock_or_recover() = Some(response);
        self
    }

    /// Pre-load a queue of responses consumed in order, before handlers.
    pub fn script(self, responses: Vec<Response>) -> Self {
        self.scripted.lock_or_recover().extend(responses);
        self
    }

    /// All calls made so far, as space-joined argument strings.
    pub fn calls(&self) -> Vec<String> {
        self.calls
            .lock_or_recover()
            .iter()
            .map(|args| args.join(" "))
            .collect()
    }

    pub fn call_count(&self) -> usize {
        self.calls.lock_or_recover().len()
    }

    /// How many recorded calls contain `pattern`.
    pub fn count_matching(&self, pattern: &str) -> usize {
        self.calls().iter().filter(|c| c.contains(pattern)).count()
    }
}

impl Adb for MockAdb {
    fn run(&self, args: &[&str], _timeout: Duration) -> Result<CmdOutput, AdbError> {
        self.calls
            .lock_or_recover()
            .push(args.iter().map(|s| s.to_string()).collect());
        let joined = args.join(" ");

        if let Some(resp) = self.scripted.lock_or_recover().pop_front() {
            return resp.call();
        }
        let handler = {
            let handlers = self.handlers.lock_or_recover();
            handlers
                .iter()
                .find(|h| joined.contains(&h.pattern))
                .map(|h| h.response.clone())
        };
        if let Some(resp) = handler {
            return resp.call();
        }
        if let Some(resp) = self.always.lock_or_recover().clone() {
            return resp.call();
        }
        Err(AdbError::ProcessFailed {
            code: 127,
            stderr: format!("mock adb: no handler for `{joined}`"),
            args: args.iter().map(|s| s.to_string()).collect(),
        })
    }

    fn adb_path(&self) -> Option<&std::path::Path> {
        self.adb_path.as_deref()
    }
}

trait MutexExt<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}

/// An injectable filesystem probe for destination-validation tests.
#[derive(Default)]
pub struct MockFsProbe {
    pub existing: Mutex<Vec<PathBuf>>,
    pub sizes: Mutex<std::collections::HashMap<PathBuf, u64>>,
    pub read_only: Vec<PathBuf>,
    pub free_bytes: Option<u64>,
    pub fail_create: bool,
    pub created: Mutex<Vec<PathBuf>>,
}

impl MockFsProbe {
    pub fn new() -> Self {
        Self {
            free_bytes: Some(u64::MAX),
            ..Default::default()
        }
    }

    pub fn with_existing(self, paths: &[&str]) -> Self {
        self.existing
            .lock_or_recover()
            .extend(paths.iter().map(PathBuf::from));
        self
    }

    /// Register a local file with a known size (implies existence).
    pub fn with_file(self, path: &str, size: u64) -> Self {
        let path = PathBuf::from(path);
        self.existing.lock_or_recover().push(path.clone());
        self.sizes.lock_or_recover().insert(path, size);
        self
    }

    pub fn read_only(self, path: &str) -> Self {
        let mut s = self;
        s.read_only.push(PathBuf::from(path));
        s
    }

    pub fn free_bytes(mut self, bytes: u64) -> Self {
        self.free_bytes = Some(bytes);
        self
    }
}

impl FsProbe for MockFsProbe {
    fn ensure_dir(&self, path: &std::path::Path) -> Result<(), DestError> {
        if self.fail_create {
            return Err(DestError::CannotCreate {
                path: path.to_path_buf(),
                message: "mock create failure".into(),
            });
        }
        self.created.lock_or_recover().push(path.to_path_buf());
        Ok(())
    }

    fn probe_writable(&self, _path: &std::path::Path) -> Result<(), DestError> {
        Ok(())
    }

    fn free_bytes(&self, _path: &std::path::Path) -> Result<Option<u64>, DestError> {
        Ok(self.free_bytes)
    }

    fn exists(&self, path: &std::path::Path) -> bool {
        self.existing.lock_or_recover().iter().any(|p| p == path)
            || self.sizes.lock_or_recover().contains_key(path)
    }

    fn local_size(&self, path: &std::path::Path) -> Option<u64> {
        self.sizes.lock_or_recover().get(path).copied()
    }
}

/// Build a [`SpaceInfo`] for assertions.
pub fn space_info(free: Option<u64>) -> SpaceInfo {
    SpaceInfo {
        free_bytes: free,
        writable: true,
    }
}

/// One file inside [`MockRemote`].
#[derive(Debug, Clone)]
struct MockFile {
    content: Vec<u8>,
    modified_at: Option<i64>,
    /// When set, the pull writes only this many bytes — a truncated transfer.
    truncate_to: Option<u64>,
    /// Remaining transient failures before the pull succeeds.
    transient_failures: u32,
    /// Present the transient failure as "not authorized" rather than "offline".
    unauthorized: bool,
    /// A permanent pull failure (e.g. the file vanished between listing and read).
    unreadable: bool,
    /// Deliver a wrong-byte, correct-length copy (only a hash check sees this).
    corrupt: bool,
}

/// A scripted phone filesystem.
///
/// This is the double the Wave 5 gate runs on: together with
/// [`crate::backup::destination::LocalFsProbe`] it exercises enumeration,
/// comparison, staging, verification, atomic commit, history archiving and the
/// manifest — everything except `adb` itself, which has its own double
/// ([`MockAdb`]).
///
/// A remote file's *content* is kept in memory and `pull` writes it into the
/// local staging path, so the bytes that get verified and renamed are real.
#[derive(Default)]
pub struct MockRemote {
    roots: Mutex<Vec<String>>,
    files: Mutex<std::collections::BTreeMap<String, MockFile>>,
    denied: Mutex<Vec<String>>,
    pulls: Mutex<Vec<String>>,
    listings: Mutex<Vec<String>>,
    root_error: Option<RemoteError>,
    pull_delay: Duration,
    /// Scripted as a device whose `toybox` cannot compute digests.
    no_remote_hash: bool,
}

impl MockRemote {
    /// An empty phone that reports the usual shared-storage root.
    pub fn new() -> Self {
        Self {
            roots: Mutex::new(vec!["/storage/emulated/0".to_string()]),
            ..Default::default()
        }
    }

    /// A phone with no shared storage at all (a broken or locked device).
    pub fn without_storage() -> Self {
        Self::default()
    }

    /// Override the reported storage roots.
    pub fn with_roots(self, roots: &[&str]) -> Self {
        *self.roots.lock_or_recover() = roots.iter().map(|r| r.to_string()).collect();
        self
    }

    /// Add a readable file with content.
    pub fn with_file(self, path: &str, content: &[u8]) -> Self {
        self.insert(path, content, Some(1_700_000_000), None);
        self
    }

    /// Add a file with an explicit phone-reported modification time.
    pub fn with_file_at(self, path: &str, content: &[u8], modified_at: i64) -> Self {
        self.insert(path, content, Some(modified_at), None);
        self
    }

    /// Add a file whose reported mtime is unknown, like some `toybox` builds.
    pub fn with_file_without_mtime(self, path: &str, content: &[u8]) -> Self {
        self.insert(path, content, None, None);
        self
    }

    /// A folder the phone refuses to list (`Android/data` in the real world).
    ///
    /// Every folder below it is refused too, exactly like `find` on the device.
    pub fn with_denied_folder(self, folder: &str) -> Self {
        self.denied.lock_or_recover().push(folder.to_string());
        self
    }

    /// The next `n` pulls of `path` fail with a transient error (unplugged cable).
    pub fn with_transient_pull_failures(self, path: &str, n: u32) -> Self {
        self.set_transient_pull_failures(path, n);
        self
    }

    /// Arm a transient failure on a phone that is already being used, as the
    /// scenario suites do when the cable comes out mid-run.
    pub fn set_transient_pull_failures(&self, path: &str, n: u32) {
        self.mutate(path, |file| file.transient_failures = n);
    }

    /// The next `n` pulls of `path` fail because the phone is not authorized.
    pub fn with_unauthorized_pull_failures(self, path: &str, n: u32) -> Self {
        self.mutate(path, |file| {
            file.transient_failures = n;
            file.unauthorized = true;
        });
        self
    }

    /// Pulls of `path` stop after `bytes` (a truncated transfer).
    pub fn with_truncated_pull(self, path: &str, bytes: u64) -> Self {
        self.mutate(path, |file| file.truncate_to = Some(bytes));
        self
    }

    /// Pulls of `path` fail permanently, as if the file were deleted mid-run.
    pub fn with_unreadable_file(self, path: &str) -> Self {
        self.mutate(path, |file| file.unreadable = true);
        self
    }

    /// Deliver the file with a flipped byte while keeping its length.
    ///
    /// This is the one corruption a size check cannot see: the byte count matches
    /// what the phone reported, the bytes on disk are wrong.
    pub fn with_corrupted_pull(self, path: &str) -> Self {
        self.mutate(path, |file| file.corrupt = true);
        self
    }

    /// The listing itself fails: the device never answers.
    pub fn with_root_error(self, err: RemoteError) -> Self {
        Self {
            root_error: Some(err),
            ..self
        }
    }

    /// Make every successful pull take at least this long.
    ///
    /// A real `adb pull` of a photo takes tens of milliseconds; without this,
    /// tests that pause or cancel "while a transfer is in flight" would race
    /// against a job that can finish in under a millisecond.
    pub fn with_pull_delay(self, delay: Duration) -> Self {
        Self {
            pull_delay: delay,
            ..self
        }
    }

    /// A device whose `toybox` has no `sha256sum`: [`RemoteFiles::hash`] answers
    /// "cannot", so hash mode records only the local digest.
    pub fn with_no_remote_hash(self) -> Self {
        Self {
            no_remote_hash: true,
            ..self
        }
    }

    /// Clear every scripted failure for `path`: the phone came back, the file
    /// became readable, storage was freed.
    pub fn restore(&self, path: &str) {
        self.mutate(path, |file| {
            file.transient_failures = 0;
            file.unauthorized = false;
            file.unreadable = false;
            file.truncate_to = None;
            file.corrupt = false;
        });
    }

    /// Remove a file from the phone (the user deleted it there).
    pub fn remove(&self, path: &str) {
        self.files.lock_or_recover().remove(path);
    }

    /// Replace a file's content and bump its mtime, as a phone would.
    pub fn replace(&self, path: &str, content: &[u8]) {
        let mut files = self.files.lock_or_recover();
        if let Some(file) = files.get_mut(path) {
            file.content = content.to_vec();
            file.modified_at = file.modified_at.map(|m| m + 1);
        }
    }

    /// Every pull attempt, in order.
    pub fn pulls(&self) -> Vec<String> {
        self.pulls.lock_or_recover().clone()
    }

    /// Every folder listing, in order.
    pub fn listings(&self) -> Vec<String> {
        self.listings.lock_or_recover().clone()
    }

    /// Files currently on the phone (paths only).
    pub fn file_paths(&self) -> Vec<String> {
        self.files.lock_or_recover().keys().cloned().collect()
    }

    fn insert(
        &self,
        path: &str,
        content: &[u8],
        modified_at: Option<i64>,
        truncate_to: Option<u64>,
    ) {
        self.files.lock_or_recover().insert(
            path.to_string(),
            MockFile {
                content: content.to_vec(),
                modified_at,
                truncate_to,
                transient_failures: 0,
                unauthorized: false,
                unreadable: false,
                corrupt: false,
            },
        );
    }

    fn mutate(&self, path: &str, f: impl FnOnce(&mut MockFile)) {
        match self.files.lock_or_recover().get_mut(path) {
            Some(file) => f(file),
            None => panic!("MockRemote: unknown remote file {path} (add it first)"),
        }
    }
}

impl RemoteFiles for MockRemote {
    fn list_files(&self, _serial: &str, folder: &str) -> Result<Vec<RemoteEntry>, RemoteError> {
        if let Some(err) = &self.root_error {
            return Err(err.duplicate());
        }
        self.listings.lock_or_recover().push(folder.to_string());

        // `find` is recursive, so a folder the user cannot read also hides
        // everything below it.
        let trimmed = folder.trim_end_matches('/');
        let denied = self.denied.lock_or_recover().clone();
        let is_denied = denied.iter().any(|d| {
            let d = d.trim_end_matches('/');
            trimmed == d || trimmed.starts_with(&format!("{d}/"))
        });
        if is_denied {
            return Err(RemoteError::ListingFailed {
                folder: folder.to_string(),
                message: "Permission denied".to_string(),
            });
        }

        let prefix = format!("{trimmed}/");
        let files = self.files.lock_or_recover();
        Ok(files
            .iter()
            .filter(|(path, _)| path.starts_with(&prefix))
            .map(|(path, file)| RemoteEntry {
                path: path.clone(),
                size: file.content.len() as u64,
                modified_at: file.modified_at,
                kind: EntryKind::File,
            })
            .collect())
    }

    fn pull(&self, serial: &str, remote: &str, local: &std::path::Path) -> Result<(), RemoteError> {
        self.pulls.lock_or_recover().push(remote.to_string());

        let (content, truncate_to, corrupt) = {
            let mut files = self.files.lock_or_recover();
            let Some(file) = files.get_mut(remote) else {
                return Err(RemoteError::PullFailed {
                    path: remote.to_string(),
                    message: "No such file or directory".to_string(),
                });
            };
            if file.unreadable {
                return Err(RemoteError::PullFailed {
                    path: remote.to_string(),
                    message: "Permission denied".to_string(),
                });
            }
            if file.transient_failures > 0 {
                file.transient_failures -= 1;
                let err = if file.unauthorized {
                    AdbError::DeviceUnauthorized {
                        serial: Some(serial.to_string()),
                    }
                } else {
                    AdbError::DeviceOffline {
                        serial: Some(serial.to_string()),
                    }
                };
                return Err(RemoteError::Adb(err));
            }
            (file.content.clone(), file.truncate_to, file.corrupt)
        };

        let mut content = content;
        if let Some(limit) = truncate_to {
            content.truncate(limit as usize);
        }
        if corrupt {
            if let Some(first) = content.first_mut() {
                *first ^= 0xff;
            }
        }
        if !self.pull_delay.is_zero() {
            std::thread::sleep(self.pull_delay);
        }
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent).map_err(|err| RemoteError::PullFailed {
                path: remote.to_string(),
                message: err.to_string(),
            })?;
        }
        std::fs::write(local, content).map_err(|err| RemoteError::PullFailed {
            path: remote.to_string(),
            message: err.to_string(),
        })
    }

    fn storage_roots(&self, _serial: &str) -> Result<Vec<String>, RemoteError> {
        Ok(self.roots.lock_or_recover().clone())
    }

    fn hash(&self, _serial: &str, remote: &str) -> Result<Option<String>, RemoteError> {
        if self.no_remote_hash {
            return Ok(None);
        }
        let files = self.files.lock_or_recover();
        match files.get(remote) {
            // The digest describes the file *on the phone*: a truncated or
            // corrupted transfer happens after this, which is the whole point.
            Some(file) => Ok(Some(sha256_hex(&file.content))),
            None => Err(RemoteError::PullFailed {
                path: remote.to_string(),
                message: "No such file or directory".to_string(),
            }),
        }
    }
}

fn sha256_hex(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(content);
    hex::encode(hasher.finalize())
}

// ---------------------------------------------------------------------------
// platform-tools installer doubles
// ---------------------------------------------------------------------------

/// Build an in-memory zip with the given `(name, bytes)` entries.
///
/// Entries are stored uncompressed so the fixtures stay trivial to reason
/// about; names are taken verbatim, which is how zip-slip cases are built.
pub fn platform_tools_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    use std::io::Write as _;
    use zip::write::SimpleFileOptions;

    let mut buffer = std::io::Cursor::new(Vec::new());
    {
        let mut writer = zip::ZipWriter::new(&mut buffer);
        let options = SimpleFileOptions::default()
            .compression_method(zip::CompressionMethod::Stored)
            .unix_permissions(0o755);
        for (name, bytes) in entries {
            writer.start_file(*name, options).expect("start entry");
            writer.write_all(bytes).expect("write entry");
        }
        writer.finish().expect("finish zip");
    }
    buffer.into_inner()
}

/// A manifest in the shape Google publishes, with one `platform-tools` package.
///
/// `archives` is `(host-os, size, digest kind, digest hex, url)`.
pub fn manifest_with_platform_tools(
    revision: (u32, u32, u32),
    archives: &[(&str, u64, &str, &str, &str)],
) -> String {
    let mut xml = String::from("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<sdkRepository>\n");
    xml.push_str(" <remotePackage path=\"platform-tools\">\n");
    xml.push_str(&format!(
        "  <revision><major>{}</major><minor>{}</minor><micro>{}</micro></revision>\n",
        revision.0, revision.1, revision.2
    ));
    xml.push_str("  <archives>\n");
    for (os, size, kind, hex, url) in archives {
        xml.push_str(&format!(
            "   <archive><complete><size>{size}</size><checksum type=\"{kind}\">{hex}</checksum><url>{url}</url></complete><host-os>{os}</host-os></archive>\n"
        ));
    }
    xml.push_str("  </archives>\n </remotePackage>\n</sdkRepository>\n");
    xml
}

/// A scripted [`crate::adb::installer::Fetcher`].
///
/// Understands the failure modes the installer has to survive: a response that
/// stops early, a corrupt body, a 404, a mid-stream error and cancellation.
pub struct MockFetcher {
    state: Mutex<FetcherState>,
}

#[derive(Default)]
struct FetcherState {
    manifest: Option<String>,
    files: std::collections::HashMap<String, Vec<u8>>,
    /// The next N file downloads stop half-way (as a short read).
    truncate_requests: u32,
    /// Every request, in order; asserted on to prove a retry happened.
    requests: Vec<String>,
    /// Bytes at which a download gives up with a network error.
    fail_after: Option<u64>,
}

impl MockFetcher {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(FetcherState::default()),
        }
    }

    pub fn with_manifest(self, xml: &str) -> Self {
        self.state.lock_or_recover().manifest = Some(xml.to_string());
        self
    }

    pub fn with_file(self, url: &str, bytes: Vec<u8>) -> Self {
        self.state
            .lock_or_recover()
            .files
            .insert(url.to_string(), bytes);
        self
    }

    /// Make the next `count` downloads stop half-way.
    pub fn truncating(self, count: u32) -> Self {
        self.state.lock_or_recover().truncate_requests = count;
        self
    }

    /// Break a download after `bytes`, as a flaky connection would.
    pub fn failing_after(self, bytes: u64) -> Self {
        self.state.lock_or_recover().fail_after = Some(bytes);
        self
    }

    pub fn requests(&self) -> Vec<String> {
        self.state.lock_or_recover().requests.clone()
    }
}

impl Default for MockFetcher {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::adb::installer::Fetcher for MockFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, crate::adb::installer::InstallError> {
        use crate::adb::installer::InstallError;
        let mut state = self.state.lock_or_recover();
        state.requests.push(url.to_string());
        match state.manifest.clone() {
            Some(xml) => Ok(xml.into_bytes()),
            None => Err(InstallError::Http {
                url: url.to_string(),
                status: 404,
            }),
        }
    }

    fn get_to_file(
        &self,
        url: &str,
        dest: &std::path::Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>) -> crate::adb::installer::Control,
    ) -> Result<(), crate::adb::installer::InstallError> {
        use crate::adb::installer::{Control, InstallError};
        let mut state = self.state.lock_or_recover();
        state.requests.push(url.to_string());
        let bytes = match state.files.get(url) {
            Some(bytes) => bytes.clone(),
            None => {
                return Err(InstallError::Http {
                    url: url.to_string(),
                    status: 404,
                })
            }
        };
        let total = bytes.len() as u64;
        let truncate = state.truncate_requests > 0;
        if truncate {
            state.truncate_requests -= 1;
        }
        let fail_after = state.fail_after;
        drop(state);

        let cut = if truncate {
            bytes.len() / 2
        } else {
            bytes.len()
        };
        let mut written = Vec::with_capacity(cut);
        for (index, chunk) in bytes[..cut].chunks(64 * 1024).enumerate() {
            written.extend_from_slice(chunk);
            let done = (index as u64 + 1) * 64 * 1024;
            let done = done.min(written.len() as u64);
            if fail_after.is_some_and(|limit| done >= limit) {
                return Err(InstallError::Network {
                    url: url.to_string(),
                    source: "simulated connection reset".into(),
                });
            }
            if on_progress(done, Some(total)) == Control::Cancel {
                return Err(InstallError::Cancelled);
            }
        }
        std::fs::write(dest, &written).map_err(|source| InstallError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        if truncate {
            return Err(InstallError::DownloadIncomplete {
                url: url.to_string(),
                expected: total,
                got: written.len() as u64,
            });
        }
        Ok(())
    }
}
