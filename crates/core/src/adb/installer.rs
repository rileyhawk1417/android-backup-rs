//! Managed platform-tools installer (plan §4.1).
//!
//! This module is optional: if the user already has `adb` on `PATH`, or points
//! at one, nothing here runs. It exists so that somebody without Android Studio
//! never has to visit a download page.
//!
//! ## Where the bytes come from, and why
//!
//! * Manifest: `https://dl.google.com/android/repository/repository2-3.xml` —
//!   Google's official SDK repository manifest, the same document the SDK
//!   Manager reads. Per package it pins the revision, the archive URL, the
//!   exact byte size and a digest, all in one place.
//! * Archive: `https://dl.google.com/android/repository/platform-tools_r<rev>-linux.zip`
//!   or `..._r<rev>-win.zip`, taken **out of the manifest** rather than
//!   hardcoded, so URL, size and digest cannot drift apart.
//!
//! The convenience aliases `platform-tools-latest-linux.zip` /
//! `platform-tools-latest-windows.zip` still exist and are what the older docs
//! link to, but as of 2026-10-02
//! `developer.android.com/tools/releases/platform-tools` publishes **no**
//! checksum for them (the page was checked: the old checksum table is gone). An
//! archive we cannot verify is an archive we will not execute, so the aliases
//! are deliberately unused.
//!
//! ## Digests
//!
//! The manifest carries `<checksum type="sha1">` for `platform-tools` and
//! `sha256` for most other packages. We verify whichever algorithm the manifest
//! states, prefer SHA-256 when several are offered, and report which one was
//! used. No published digest ⇒ [`InstallError::DigestMissing`], no install. The
//! SHA-1 check is an integrity check against a truncated or swapped download;
//! it is not a defence against a forged manifest (that is TLS's job — and the
//! reason the manifest is only ever fetched over HTTPS from `dl.google.com`).
//!
//! ## Sizes, measured on platform-tools 37.0.1
//!
//! | archive | zip | extracted |
//! |---|---|---|
//! | linux | 9,054,187 B | 22,198,160 B (2.45×) |
//! | windows | 8,044,989 B | ~8.5 MB for the allow-listed files |
//!
//! Only the files `adb` actually needs are extracted ([`HostOs::is_allowed`]):
//! `adb`, `lib64/` and `source.properties` on Linux (`lib64/libc++.so` is what
//! adb's `$ORIGIN/lib64` RUNPATH points at), `adb.exe`, its three DLLs and
//! `source.properties` on Windows. `fastboot`, `sqlite3`, `mke2fs*`, `etc1tool`,
//! `hprof-conv`, `NOTICE.txt` and the rest stay in the zip: extracting them
//! would triple the install for tools this app never runs.
//!
//! ## Interruption safety
//!
//! Everything happens in a staging directory next to the target, and the target
//! is only replaced once the download is verified, the archive is extracted and
//! the extracted `adb` has answered `adb version`. A crash or a cancel leaves
//! the previous installation untouched; [`Staging`] removes its leftovers on the
//! way out (including on the error paths).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use quick_xml::events::{BytesStart, Event};
use quick_xml::Reader;
use sha2::Digest as _;
use zip::ZipArchive;

use crate::backup::destination::{FsProbe, LocalFsProbe};
use crate::config::AdbMode;

use super::locate::{find_adb_with, probe_version, AdbLocateError, AdbVersion, VersionProbe};

/// The official SDK repository manifest. The only URL this module trusts.
pub const MANIFEST_URL: &str = "https://dl.google.com/android/repository/repository2-3.xml";

/// Package path of platform-tools inside the manifest.
pub const PACKAGE_PATH: &str = "platform-tools";

/// How many times a download is attempted before giving up.
///
/// A network error or a short read re-downloads the whole file: resuming a
/// partially-read HTTPS body is exactly the kind of cleverness that produces
/// corrupt archives, and the file is small.
const DOWNLOAD_ATTEMPTS: u32 = 3;
const DOWNLOAD_BACKOFF: Duration = Duration::from_millis(400);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const CHUNK: usize = 64 * 1024;

/// Guard against a hostile or broken mirror streaming us a "manifest" forever.
const MAX_MANIFEST_BYTES: u64 = 32 * 1024 * 1024;

/// Free space asked for, as a multiple of the compressed size (measured
/// expansion is 2.45×; the extra is headroom), and never less than this.
const UNPACK_FACTOR: u64 = 3;
const MIN_FREE_BYTES: u64 = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Platform
// ---------------------------------------------------------------------------

/// The platforms this application ships for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostOs {
    Linux,
    Windows,
}

impl HostOs {
    /// The platform we are running on, if it is one we support.
    pub fn current() -> Result<Self, InstallError> {
        if cfg!(target_os = "linux") {
            Ok(Self::Linux)
        } else if cfg!(target_os = "windows") {
            Ok(Self::Windows)
        } else {
            Err(InstallError::UnsupportedPlatform {
                os: std::env::consts::OS.to_string(),
            })
        }
    }

    /// How the manifest spells this platform.
    pub const fn manifest_name(self) -> &'static str {
        match self {
            Self::Linux => "linux",
            Self::Windows => "windows",
        }
    }

    /// Name of the executable the locator will look for.
    pub const fn adb_binary(self) -> &'static str {
        match self {
            Self::Linux => "adb",
            Self::Windows => "adb.exe",
        }
    }

    pub const fn describe(self) -> &'static str {
        match self {
            Self::Linux => "Linux",
            Self::Windows => "Windows",
        }
    }

    /// Archive-relative files worth extracting.
    ///
    /// `lib64/` is not optional on Linux: `adb` is built with
    /// `RUNPATH=$ORIGIN/lib64` and ships `libc++.so` there. On Windows the two
    /// `AdbWin*` DLLs are the USB backend. `libwinpthread-1.dll` is included as
    /// cheap insurance for future builds; it is 243 KB.
    const fn allowed(self) -> &'static [&'static str] {
        match self {
            Self::Linux => &["adb", "source.properties"],
            Self::Windows => &[
                "adb.exe",
                "AdbWinApi.dll",
                "AdbWinUsbApi.dll",
                "libwinpthread-1.dll",
                "source.properties",
            ],
        }
    }

    /// Is this archive entry (`platform-tools/` already stripped) one we keep?
    pub fn is_allowed(self, rel: &str) -> bool {
        if self.allowed().contains(&rel) {
            return true;
        }
        // adb's RUNPATH points at $ORIGIN/lib64, so the shared libraries there
        // are part of adb. One level, regular files only.
        matches!(self, Self::Linux)
            && rel
                .strip_prefix("lib64/")
                .is_some_and(|name| !name.is_empty() && !name.contains('/'))
    }
}

// ---------------------------------------------------------------------------
// Progress
// ---------------------------------------------------------------------------

/// What the installer is busy with. Shown in the UI modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Manifest,
    Download,
    Verify,
    Extract,
    Check,
}

impl Stage {
    pub const fn describe(self) -> &'static str {
        match self {
            Self::Manifest => "Checking the latest release",
            Self::Download => "Downloading platform-tools",
            Self::Verify => "Verifying the download",
            Self::Extract => "Extracting adb",
            Self::Check => "Checking that adb runs",
        }
    }
}

/// A progress tick. `total` is `None` when the size is unknown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Progress {
    pub stage: Stage,
    pub done: u64,
    /// `None` when the source did not announce a length.
    pub total: Option<u64>,
}

impl Progress {
    /// 0.0–1.0 when a total is known.
    pub fn fraction(&self) -> Option<f32> {
        let total = self.total?;
        if total == 0 {
            return None;
        }
        Some((self.done as f64 / total as f64).clamp(0.0, 1.0) as f32)
    }
}

/// What a progress callback wants to happen next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Continue,
    Cancel,
}

// ---------------------------------------------------------------------------
// Manifest
// ---------------------------------------------------------------------------

/// A pinned release of platform-tools.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Revision {
    pub major: u32,
    pub minor: u32,
    pub micro: u32,
}

impl std::fmt::Display for Revision {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.micro)
    }
}

/// Digest algorithms the manifest can publish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestAlgorithm {
    Sha256,
    Sha1,
}

impl std::fmt::Display for DigestAlgorithm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl DigestAlgorithm {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Sha256 => "sha256",
            Self::Sha1 => "sha1",
        }
    }

    /// Hex digest of an in-memory buffer.
    ///
    /// Used for tests and for anything that already holds the bytes; files go
    /// through [`DigestAlgorithm::hash`] instead, which streams.
    pub fn hash_bytes(self, bytes: &[u8]) -> String {
        match self {
            Self::Sha256 => {
                let mut hasher = sha2::Sha256::new();
                hasher.update(bytes);
                hex::encode(hasher.finalize())
            }
            Self::Sha1 => {
                let mut hasher = sha1::Sha1::new();
                hasher.update(bytes);
                hex::encode(hasher.finalize())
            }
        }
    }

    /// Hash a file, streaming, so a large archive is never held in memory.
    pub fn hash(self, path: &Path) -> Result<Vec<u8>, InstallError> {
        let mut file = std::fs::File::open(path).map_err(|source| InstallError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        let mut buf = vec![0u8; CHUNK];
        let mut sha256 = sha2::Sha256::new();
        let mut sha1 = sha1::Sha1::new();
        loop {
            let read = file.read(&mut buf).map_err(|source| InstallError::Io {
                path: path.to_path_buf(),
                source,
            })?;
            if read == 0 {
                break;
            }
            match self {
                Self::Sha256 => sha256.update(&buf[..read]),
                Self::Sha1 => sha1.update(&buf[..read]),
            }
        }
        Ok(match self {
            Self::Sha256 => sha256.finalize().to_vec(),
            Self::Sha1 => sha1.finalize().to_vec(),
        })
    }
}

/// A digest as published in the manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Digest {
    pub algorithm: DigestAlgorithm,
    /// Lower-case hex, as printed by the manifest.
    pub hex: String,
}

impl Digest {
    /// Parse a `<checksum type="…">hex</checksum>` pair.
    pub fn parse(algorithm: &str, hex: &str) -> Result<Self, InstallError> {
        let algorithm = match algorithm.trim().to_ascii_lowercase().as_str() {
            "sha256" | "sha-256" => DigestAlgorithm::Sha256,
            "sha1" | "sha-1" => DigestAlgorithm::Sha1,
            other => {
                return Err(InstallError::ManifestParse {
                    message: format!("unsupported checksum type {other:?}"),
                })
            }
        };
        let hex = hex.trim().to_ascii_lowercase();
        let expected = match algorithm {
            DigestAlgorithm::Sha256 => 64,
            DigestAlgorithm::Sha1 => 40,
        };
        if hex.len() != expected || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(InstallError::ManifestParse {
                message: format!("malformed {} digest {hex:?}", algorithm.name()),
            });
        }
        Ok(Self { algorithm, hex })
    }

    /// Hash `path` and compare. Mismatch is its own typed error.
    pub fn verify(&self, path: &Path, url: &str) -> Result<(), InstallError> {
        let actual = hex::encode(self.algorithm.hash(path)?);
        if actual != self.hex {
            return Err(InstallError::DigestMismatch {
                url: url.to_string(),
                algorithm: self.algorithm,
                expected: self.hex.clone(),
                actual,
            });
        }
        Ok(())
    }
}

/// One downloadable archive of a package.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageArchive {
    /// Absolute URL, resolved against the manifest's directory.
    pub url: String,
    /// Last path segment of `url`; used as the temporary file name.
    pub file_name: String,
    /// Exact size in bytes, as published.
    pub size: u64,
    pub digest: Digest,
    pub os: HostOs,
}

/// A pinned package plus the archive that matches this platform.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Package {
    pub revision: Revision,
    pub archive: PackageArchive,
}

/// Parse the SDK repository manifest and pick the newest platform-tools archive
/// for `os`.
pub fn parse_manifest(xml: &str, os: HostOs, manifest_url: &str) -> Result<Package, InstallError> {
    let mut reader = Reader::from_str(xml);
    {
        // `expand_empty_elements` turns `<checksum type="sha1"/>` into a
        // Start/End pair, so the state machine below only has to know one shape.
        let config = reader.config_mut();
        config.trim_text(true);
        config.expand_empty_elements = true;
    }

    let mut best: Option<(Revision, Vec<ArchiveBuilder>)> = None;
    let mut in_package = false;
    let mut revision = Revision::default();
    let mut archives: Vec<ArchiveBuilder> = Vec::new();
    let mut current: Option<ArchiveBuilder> = None;
    let mut text = String::new();

    loop {
        let event = reader
            .read_event()
            .map_err(|err| InstallError::ManifestParse {
                message: err.to_string(),
            })?;
        match event {
            Event::Eof => break,
            Event::Start(start) => {
                let name = local_name(&start);
                if in_package {
                    match name.as_str() {
                        "revision" => revision = Revision::default(),
                        "archive" => current = Some(ArchiveBuilder::default()),
                        "checksum" => {
                            let kind = attribute(&start, "type")?.unwrap_or_default();
                            if let Some(archive) = current.as_mut() {
                                archive.checksum_type = Some(kind);
                            }
                        }
                        _ => {}
                    }
                    text.clear();
                } else if name == "remotepackage"
                    && attribute(&start, "path")?.as_deref() == Some(PACKAGE_PATH)
                {
                    in_package = true;
                    revision = Revision::default();
                    archives.clear();
                    current = None;
                }
            }
            Event::Text(fragment) => {
                if in_package {
                    text.push_str(fragment.as_ref());
                }
            }
            Event::End(end) => {
                if !in_package {
                    continue;
                }
                let name = end.local_name().as_ref().to_ascii_lowercase();
                let value = text.trim().to_string();
                match name.as_str() {
                    "major" => revision.major = value.parse().unwrap_or(0),
                    "minor" => revision.minor = value.parse().unwrap_or(0),
                    "micro" => revision.micro = value.parse().unwrap_or(0),
                    "host-os" => {
                        if let Some(archive) = current.as_mut() {
                            archive.host_os = Some(value);
                        }
                    }
                    "host-arch" => {
                        if let Some(archive) = current.as_mut() {
                            archive.host_arch = Some(value);
                        }
                    }
                    "size" => {
                        if let Some(archive) = current.as_mut() {
                            archive.size = value.parse().ok();
                        }
                    }
                    "url" => {
                        if let Some(archive) = current.as_mut() {
                            archive.url = Some(value);
                        }
                    }
                    "checksum" => {
                        if let Some(archive) = current.as_mut() {
                            let kind = archive.checksum_type.take().unwrap_or_default();
                            archive.digest = Some(Digest::parse(&kind, &value)?);
                        }
                    }
                    "archive" => {
                        if let Some(archive) = current.take() {
                            archives.push(archive);
                        }
                    }
                    "remotepackage" => {
                        // Keep only the newest revision that has any archive.
                        let better = best.as_ref().is_none_or(|(known, _)| revision > *known);
                        if better && !archives.is_empty() {
                            best = Some((revision, std::mem::take(&mut archives)));
                        }
                        in_package = false;
                        archives.clear();
                    }
                    _ => {}
                }
                text.clear();
            }
            _ => {}
        }
    }

    let (revision, archives) = best.ok_or(InstallError::PackageMissing)?;
    let chosen = archives
        .into_iter()
        .find(|archive| archive.matches(os))
        .ok_or(InstallError::NoArchiveFor {
            os: os.manifest_name(),
        })?;
    let raw_url = chosen.url.ok_or(InstallError::NoArchiveFor {
        os: os.manifest_name(),
    })?;
    let url = resolve_url(manifest_url, &raw_url);
    let file_name = file_name_of(&url)?;
    let digest = chosen
        .digest
        .ok_or_else(|| InstallError::DigestMissing { url: url.clone() })?;
    Ok(Package {
        revision,
        archive: PackageArchive {
            url,
            file_name,
            size: chosen.size.unwrap_or(0),
            digest,
            os,
        },
    })
}

#[derive(Debug, Clone, Default)]
struct ArchiveBuilder {
    host_os: Option<String>,
    host_arch: Option<String>,
    size: Option<u64>,
    url: Option<String>,
    checksum_type: Option<String>,
    digest: Option<Digest>,
}

impl ArchiveBuilder {
    fn matches(&self, os: HostOs) -> bool {
        if self.host_os.as_deref() != Some(os.manifest_name()) {
            return false;
        }
        // Today's platform-tools archives are architecture-independent; if a
        // future one gains a host-arch, only accept the one we can run.
        self.host_arch.as_deref().is_none_or(|arch| {
            arch.eq_ignore_ascii_case("x64") || arch.eq_ignore_ascii_case("x86_64")
        })
    }
}

fn local_name(start: &BytesStart<'_>) -> String {
    start.name().local_name().as_ref().to_ascii_lowercase()
}

fn attribute(start: &BytesStart<'_>, key: &str) -> Result<Option<String>, InstallError> {
    for attr in start.attributes() {
        let attr = attr.map_err(|err| InstallError::ManifestParse {
            message: err.to_string(),
        })?;
        if attr.key.as_ref().eq_ignore_ascii_case(key) {
            let value = attr
                .normalized_value(quick_xml::XmlVersion::Explicit1_0)
                .map_err(|err| InstallError::ManifestParse {
                    message: err.to_string(),
                })?;
            return Ok(Some(value.into_owned()));
        }
    }
    Ok(None)
}

/// Absolute URLs pass through; relative ones resolve against the manifest's
/// directory, which is how the SDK managers treat them.
fn resolve_url(manifest_url: &str, url: &str) -> String {
    if url.contains("://") {
        return url.to_string();
    }
    match manifest_url.rfind('/') {
        Some(cut) => format!("{}/{}", &manifest_url[..cut], url.trim_start_matches('/')),
        None => url.to_string(),
    }
}

/// The temporary file name for an archive URL, defensively checked because it
/// ends up on disk.
fn file_name_of(url: &str) -> Result<String, InstallError> {
    let tail = url.rsplit('/').next().unwrap_or_default();
    if tail.is_empty() || tail == "." || tail == ".." || tail.contains(['\\', ':']) {
        return Err(InstallError::ManifestParse {
            message: format!("archive url {url:?} has no usable file name"),
        });
    }
    Ok(tail.to_string())
}

// ---------------------------------------------------------------------------
// Fetching
// ---------------------------------------------------------------------------

/// Byte source, behind a trait so tests can serve a scripted Google.
pub trait Fetcher: Send + Sync {
    /// Fetch a small document (the manifest) into memory.
    fn get(&self, url: &str) -> Result<Vec<u8>, InstallError>;
    /// Stream a large file to `dest`, reporting progress and honouring cancel.
    fn get_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>) -> Control,
    ) -> Result<(), InstallError>;
}

/// The real fetcher: `reqwest`'s blocking client with rustls.
#[derive(Debug, Clone)]
pub struct HttpFetcher {
    client: reqwest::blocking::Client,
}

impl HttpFetcher {
    pub fn new() -> Result<Self, InstallError> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(concat!(
                "android-backup-utility/",
                env!("CARGO_PKG_VERSION")
            ))
            .connect_timeout(CONNECT_TIMEOUT)
            // Deliberately no overall timeout: an 9 MB download over a slow
            // link may legitimately take minutes, and the user can cancel from
            // the progress modal instead.
            .build()
            .map_err(|source| InstallError::Network {
                url: MANIFEST_URL.to_string(),
                source: Box::new(source),
            })?;
        Ok(Self { client })
    }

    fn get_response(&self, url: &str) -> Result<reqwest::blocking::Response, InstallError> {
        let response = self
            .client
            .get(url)
            .send()
            .map_err(|source| InstallError::Network {
                url: url.to_string(),
                source: Box::new(source),
            })?;
        let status = response.status();
        if !status.is_success() {
            return Err(InstallError::Http {
                url: url.to_string(),
                status: status.as_u16(),
            });
        }
        Ok(response)
    }
}

impl Fetcher for HttpFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
        let response = self.get_response(url)?;
        if response
            .content_length()
            .is_some_and(|len| len > MAX_MANIFEST_BYTES)
        {
            return Err(InstallError::TooLarge {
                url: url.to_string(),
                limit: MAX_MANIFEST_BYTES,
            });
        }
        let body = response.bytes().map_err(|source| InstallError::Network {
            url: url.to_string(),
            source: Box::new(source),
        })?;
        if body.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(InstallError::TooLarge {
                url: url.to_string(),
                limit: MAX_MANIFEST_BYTES,
            });
        }
        Ok(body.to_vec())
    }

    fn get_to_file(
        &self,
        url: &str,
        dest: &Path,
        on_progress: &mut dyn FnMut(u64, Option<u64>) -> Control,
    ) -> Result<(), InstallError> {
        let mut response = self.get_response(url)?;
        let total = response.content_length();
        let mut sink = std::fs::File::create(dest).map_err(|source| InstallError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        let mut buf = vec![0u8; CHUNK];
        let mut done: u64 = 0;
        loop {
            let read = response
                .read(&mut buf)
                .map_err(|source| InstallError::Network {
                    url: url.to_string(),
                    source: Box::new(source),
                })?;
            if read == 0 {
                break;
            }
            sink.write_all(&buf[..read])
                .map_err(|source| InstallError::Io {
                    path: dest.to_path_buf(),
                    source,
                })?;
            done += read as u64;
            if on_progress(done, total) == Control::Cancel {
                return Err(InstallError::Cancelled);
            }
        }
        sink.flush().map_err(|source| InstallError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        if let Some(expected) = total {
            if done < expected {
                return Err(InstallError::DownloadIncomplete {
                    url: url.to_string(),
                    expected,
                    got: done,
                });
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Staging
// ---------------------------------------------------------------------------

/// A scratch directory next to the installation target.
///
/// Sibling (not a system temp dir) on purpose: the final move must be a plain
/// `rename`, which only works within one filesystem. Dropping it cleans up the
/// downloaded zip and a partial extraction, on every path out — including the
/// error ones.
struct Staging {
    root: PathBuf,
}

impl Staging {
    fn create(dest: &Path) -> Result<Self, InstallError> {
        let name = dest
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "platform-tools".to_string());
        let parent = dest.parent().unwrap_or_else(|| Path::new("."));
        let root = parent.join(format!(".{name}.installing-{}", std::process::id()));
        // A stale directory can only be from a crash with the same pid.
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).map_err(|source| InstallError::Io {
            path: root.clone(),
            source,
        })?;
        Ok(Self { root })
    }

    fn root(&self) -> &Path {
        &self.root
    }

    /// Where the archive's `platform-tools/` prefix is stripped to.
    fn extract_dir(&self) -> PathBuf {
        self.root.join(PACKAGE_PATH)
    }
}

impl Drop for Staging {
    fn drop(&mut self) {
        if let Err(err) = std::fs::remove_dir_all(&self.root) {
            tracing::warn!(path = %self.root.display(), error = %err, "could not clean installer staging area");
        }
    }
}

// ---------------------------------------------------------------------------
// Installer
// ---------------------------------------------------------------------------

/// A completed installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallReport {
    /// Directory adb now lives in (`<data>/platform-tools`).
    pub dir: PathBuf,
    pub adb_path: PathBuf,
    pub revision: Revision,
    pub version: AdbVersion,
    pub digest: DigestAlgorithm,
    pub archive_url: String,
    pub downloaded_bytes: u64,
    /// Archive-relative files that were kept.
    pub files: Vec<String>,
}

impl InstallReport {
    /// Point the configuration at the installation we just verified.
    pub fn apply_to(&self, config: &mut crate::config::AppConfig) {
        config.adb_mode = AdbMode::Managed;
        config.managed_adb_dir = Some(self.dir.clone());
    }
}

/// Downloads, verifies and installs platform-tools.
pub struct Installer {
    fetcher: Arc<dyn Fetcher>,
    probe: Arc<dyn FsProbe>,
    manifest_url: String,
    version_probe: Arc<VersionProbe<'static>>,
}

impl Installer {
    /// Build from explicit seams (the app passes `HttpFetcher` + `LocalFsProbe`).
    pub fn new(fetcher: Arc<dyn Fetcher>, probe: Arc<dyn FsProbe>) -> Self {
        Self {
            fetcher,
            probe,
            manifest_url: MANIFEST_URL.to_string(),
            version_probe: Arc::new(|path| {
                let runner = super::runner::ProcessAdbRunner::new(path.to_path_buf());
                probe_version(&runner)
            }),
        }
    }

    /// The production installer.
    pub fn http() -> Result<Self, InstallError> {
        Ok(Self::new(
            Arc::new(HttpFetcher::new()?),
            Arc::new(LocalFsProbe::new()),
        ))
    }

    /// Point the manifest fetch somewhere else; tests use this for a local
    /// server. Archive URLs inside a manifest may already be absolute.
    pub fn using_manifest(mut self, url: impl Into<String>) -> Self {
        self.manifest_url = url.into();
        self
    }

    /// Replace the "does adb actually run?" probe (tests cannot execute a
    /// synthetic binary on every platform).
    pub fn with_version_probe(mut self, probe: Arc<VersionProbe<'static>>) -> Self {
        self.version_probe = probe;
        self
    }

    /// Fetch and parse the manifest.
    pub fn plan(&self, os: HostOs) -> Result<Package, InstallError> {
        let bytes = self.fetcher.get(&self.manifest_url)?;
        let xml = String::from_utf8_lossy(&bytes).to_string();
        parse_manifest(&xml, os, &self.manifest_url)
    }

    /// Install platform-tools into `dest` (e.g. `<data>/platform-tools`).
    ///
    /// `dest` is only touched after the archive has been verified and the
    /// extracted binary has answered a version probe.
    pub fn install(
        &self,
        dest: &Path,
        os: HostOs,
        on_progress: &mut dyn FnMut(Progress) -> Control,
    ) -> Result<InstallReport, InstallError> {
        on_progress(Progress {
            stage: Stage::Manifest,
            done: 0,
            total: None,
        });
        let package = self.plan(os)?;
        tracing::info!(
            revision = %package.revision,
            url = %package.archive.url,
            size = package.archive.size,
            digest = package.archive.digest.algorithm.name(),
            "installing platform-tools"
        );

        self.ensure_space(dest, &package.archive)?;
        let staging = Staging::create(dest)?;
        let archive_path = staging.root().join(&package.archive.file_name);

        self.download(&package.archive, &archive_path, on_progress)?;

        on_progress(Progress {
            stage: Stage::Verify,
            done: 0,
            total: None,
        });
        package
            .archive
            .digest
            .verify(&archive_path, &package.archive.url)?;

        let extract_dir = staging.extract_dir();
        let files = extract_archive(&archive_path, &extract_dir, os, on_progress)?;

        on_progress(Progress {
            stage: Stage::Check,
            done: 0,
            total: None,
        });
        let staged_adb = extract_dir.join(os.adb_binary());
        let found = find_adb_with(
            &AdbMode::Manual {
                path: staged_adb.clone(),
            },
            &extract_dir,
            &*self.version_probe,
        )
        .map_err(|source| InstallError::Verify {
            path: staged_adb.clone(),
            source,
        })?;
        activate(&extract_dir, dest)?;
        tracing::info!(
            version = %found.version,
            dir = %dest.display(),
            "platform-tools installed and verified"
        );

        Ok(InstallReport {
            dir: dest.to_path_buf(),
            adb_path: dest.join(os.adb_binary()),
            revision: package.revision,
            version: found.version,
            digest: package.archive.digest.algorithm,
            archive_url: package.archive.url,
            downloaded_bytes: package.archive.size,
            files,
        })
    }

    /// Download with whole-file retries, then check the size we were promised.
    fn download(
        &self,
        archive: &PackageArchive,
        path: &Path,
        on_progress: &mut dyn FnMut(Progress) -> Control,
    ) -> Result<(), InstallError> {
        let mut last: Option<InstallError> = None;
        for attempt in 1..=DOWNLOAD_ATTEMPTS {
            let _ = std::fs::remove_file(path);
            let result = self
                .fetcher
                .get_to_file(&archive.url, path, &mut |done, total| {
                    on_progress(Progress {
                        stage: Stage::Download,
                        done,
                        total: total.or(Some(archive.size)),
                    })
                });
            match result {
                Ok(()) => {
                    let got = std::fs::metadata(path).map(|m| m.len()).map_err(|source| {
                        InstallError::Io {
                            path: path.to_path_buf(),
                            source,
                        }
                    })?;
                    if got == archive.size || archive.size == 0 {
                        return Ok(());
                    }
                    last = Some(InstallError::SizeMismatch {
                        url: archive.url.clone(),
                        expected: archive.size,
                        got,
                    });
                }
                // The user asked to stop; retrying would be rude.
                Err(InstallError::Cancelled) => return Err(InstallError::Cancelled),
                Err(err) => last = Some(err),
            }
            if attempt < DOWNLOAD_ATTEMPTS {
                tracing::warn!(
                    attempt,
                    error = %last.as_ref().map(|e| e.to_string()).unwrap_or_default(),
                    "platform-tools download failed, retrying"
                );
                std::thread::sleep(DOWNLOAD_BACKOFF * attempt);
            }
        }
        Err(last.unwrap_or(InstallError::Cancelled))
    }

    /// Refuse before downloading if the disk obviously cannot take it.
    ///
    /// A failure to *measure* space never blocks the install: the download
    /// would fail with a real error soon enough, and a wrong guess would be
    /// worse than a late failure.
    fn ensure_space(&self, dest: &Path, archive: &PackageArchive) -> Result<(), InstallError> {
        let needed = archive
            .size
            .max(1)
            .saturating_mul(UNPACK_FACTOR)
            .max(MIN_FREE_BYTES);
        let Some(root) = nearest_existing(&*self.probe, dest) else {
            return Ok(());
        };
        match self.probe.free_bytes(&root) {
            Ok(Some(available)) if available < needed => Err(InstallError::NoSpace {
                needed,
                available,
                path: root,
            }),
            Ok(_) => Ok(()),
            Err(err) => {
                tracing::warn!(error = %err, "cannot measure free space, proceeding");
                Ok(())
            }
        }
    }
}

/// Walk up from `path` until something exists (the target usually does not).
fn nearest_existing(probe: &dyn FsProbe, path: &Path) -> Option<PathBuf> {
    let mut candidate = Some(path.to_path_buf());
    for _ in 0..8 {
        let current = candidate?;
        if probe.exists(&current) {
            return Some(current);
        }
        candidate = current.parent().map(Path::to_path_buf);
    }
    None
}

/// Extract the allow-listed entries, guarding against escapes and symlinks.
fn extract_archive(
    archive_path: &Path,
    dest: &Path,
    os: HostOs,
    on_progress: &mut dyn FnMut(Progress) -> Control,
) -> Result<Vec<String>, InstallError> {
    let file = std::fs::File::open(archive_path).map_err(|source| InstallError::Io {
        path: archive_path.to_path_buf(),
        source,
    })?;
    let mut archive = ZipArchive::new(file).map_err(|source| InstallError::Zip { source })?;
    std::fs::create_dir_all(dest).map_err(|source| InstallError::Io {
        path: dest.to_path_buf(),
        source,
    })?;

    let total = archive.len();
    let mut written: Vec<String> = Vec::new();
    for index in 0..total {
        let mut entry = archive
            .by_index(index)
            .map_err(|source| InstallError::Zip { source })?;
        let raw_name = entry.name().to_string();
        // `enclosed_name` resolves the entry and rejects anything with an
        // absolute path or enough `..` to escape the extraction root.
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| InstallError::UnsafeEntry {
                entry: raw_name.clone(),
            })?;
        if entry.is_dir() {
            continue;
        }
        // A symlink can point anywhere, and we never need one.
        if entry.is_symlink() {
            return Err(InstallError::UnsafeEntry { entry: raw_name });
        }
        let relative =
            enclosed
                .strip_prefix(PACKAGE_PATH)
                .map_err(|_| InstallError::UnsafeEntry {
                    entry: raw_name.clone(),
                })?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        if relative.is_empty() || !os.is_allowed(&relative) {
            continue;
        }
        let out = dest.join(&relative);
        if let Some(parent) = out.parent() {
            std::fs::create_dir_all(parent).map_err(|source| InstallError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let mut sink = std::fs::File::create(&out).map_err(|source| InstallError::Io {
            path: out.clone(),
            source,
        })?;
        std::io::copy(&mut entry, &mut sink).map_err(|source| InstallError::Io {
            path: out.clone(),
            source,
        })?;
        set_mode(&out, entry.unix_mode(), relative == os.adb_binary())?;
        written.push(relative);

        if on_progress(Progress {
            stage: Stage::Extract,
            done: index as u64 + 1,
            total: Some(total as u64),
        }) == Control::Cancel
        {
            return Err(InstallError::Cancelled);
        }
    }

    if !written.iter().any(|name| name == os.adb_binary()) {
        return Err(InstallError::ArchiveIncomplete {
            missing: os.adb_binary().to_string(),
        });
    }
    Ok(written)
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: Option<u32>, must_execute: bool) -> Result<(), InstallError> {
    use std::os::unix::fs::PermissionsExt;
    let mut bits = match mode {
        Some(bits) if bits & 0o777 != 0 => bits & 0o777,
        _ => 0o755,
    };
    if must_execute {
        bits |= 0o755;
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(bits)).map_err(|source| {
        InstallError::Io {
            path: path.to_path_buf(),
            source,
        }
    })
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _mode: Option<u32>, _must_execute: bool) -> Result<(), InstallError> {
    Ok(())
}

/// Move the verified extraction into place, keeping the old one until it works.
fn activate(staging_extract: &Path, dest: &Path) -> Result<(), InstallError> {
    let previous = dest.with_file_name(format!(
        ".{}.replaced-{}",
        dest.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "platform-tools".to_string()),
        std::process::id()
    ));
    let mut stashed = false;
    if dest.exists() {
        let _ = std::fs::remove_dir_all(&previous);
        // Same parent directory, so this is a rename, not a copy.
        std::fs::rename(dest, &previous).map_err(|source| InstallError::Io {
            path: dest.to_path_buf(),
            source,
        })?;
        stashed = true;
    }
    match std::fs::rename(staging_extract, dest) {
        Ok(()) => {
            if stashed {
                if let Err(err) = std::fs::remove_dir_all(&previous) {
                    tracing::warn!(path = %previous.display(), error = %err, "could not remove the replaced platform-tools copy");
                }
            }
            Ok(())
        }
        Err(source) => {
            if stashed {
                // Put the working copy back before reporting the failure.
                if let Err(err) = std::fs::rename(&previous, dest) {
                    tracing::error!(error = %err, "could not restore the previous platform-tools");
                }
            }
            Err(InstallError::Io {
                path: dest.to_path_buf(),
                source,
            })
        }
    }
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum InstallError {
    #[error("automatic installation is only available on Windows and Linux, not on {os}")]
    UnsupportedPlatform { os: String },
    #[error("could not reach {url}: {source}")]
    Network {
        url: String,
        #[source]
        source: Box<dyn std::error::Error + Send + Sync>,
    },
    #[error("{url} answered HTTP {status}")]
    Http { url: String, status: u16 },
    #[error("{url} is larger than the {limit} byte limit")]
    TooLarge { url: String, limit: u64 },
    #[error("could not parse the platform-tools manifest: {message}")]
    ManifestParse { message: String },
    #[error("the SDK manifest does not list {PACKAGE_PATH}")]
    PackageMissing,
    #[error("platform-tools has no archive for {os}")]
    NoArchiveFor { os: &'static str },
    #[error("no checksum is published for {url}, so it will not be installed")]
    DigestMissing { url: String },
    #[error(
        "{url} failed its {algorithm} check: expected {expected}, got {actual} — the download was discarded"
    )]
    DigestMismatch {
        url: String,
        algorithm: DigestAlgorithm,
        expected: String,
        actual: String,
    },
    #[error("{url} should be {expected} bytes but the download is {got} bytes")]
    DownloadIncomplete {
        url: String,
        expected: u64,
        got: u64,
    },
    #[error("{url} is {got} bytes, not the published {expected} bytes")]
    SizeMismatch {
        url: String,
        expected: u64,
        got: u64,
    },
    #[error("not enough free space in {path}: {needed} bytes needed, {available} available")]
    NoSpace {
        needed: u64,
        available: u64,
        path: PathBuf,
    },
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read the downloaded archive: {source}")]
    Zip {
        #[source]
        source: zip::result::ZipError,
    },
    #[error("the archive contains an unsafe entry {entry:?}, so nothing was installed")]
    UnsafeEntry { entry: String },
    #[error("the archive does not contain {missing}")]
    ArchiveIncomplete { missing: String },
    #[error("the installed adb at {path} does not run: {source}")]
    Verify {
        path: PathBuf,
        #[source]
        source: AdbLocateError,
    },
    #[error("the installation was cancelled")]
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;
    use crate::mock::{manifest_with_platform_tools, platform_tools_zip, MockFetcher, MockFsProbe};

    fn cont(_: Progress) -> Control {
        Control::Continue
    }

    fn linux_zip() -> Vec<u8> {
        platform_tools_zip(&[
            ("platform-tools/adb", b"#!/bin/sh\necho hello\n".as_slice()),
            ("platform-tools/lib64/libc++.so", b"\x7fELF fake".as_slice()),
            ("platform-tools/fastboot", b"not needed".as_slice()),
            (
                "platform-tools/source.properties",
                b"Pkg.Revision=99.0.0\n".as_slice(),
            ),
        ])
    }

    fn sha1_of(bytes: &[u8]) -> String {
        DigestAlgorithm::Sha1.hash_bytes(bytes)
    }

    /// Installer wired to a scripted fetcher that serves `xml` and `zip`.
    fn scripted(
        xml: String,
        zip: Vec<u8>,
        url: &str,
    ) -> (
        Installer,
        Arc<MockFetcher>,
        Arc<MockFsProbe>,
        tempfile::TempDir,
    ) {
        let fetcher = Arc::new(MockFetcher::new().with_manifest(&xml).with_file(url, zip));
        let probe = Arc::new(MockFsProbe::new().with_existing(&["/data"]));
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(
            Arc::clone(&fetcher) as Arc<dyn Fetcher>,
            Arc::clone(&probe) as Arc<dyn FsProbe>,
        )
        .using_manifest("http://127.0.0.1:1/repository2-3.xml")
        .with_version_probe(Arc::new(|path| {
            // Any extracted file is "runnable" here; the real check is exercised
            // by the local-HTTP integration test and by the ignored network test.
            Ok(super::super::locate::AdbVersion {
                major: 99,
                minor: 0,
                patch: 0,
                raw: format!("Version 99.0.0 (staged at {})", path.display()),
            })
        }));
        (installer, fetcher, probe, dir)
    }

    #[test]
    fn parses_the_real_manifest_shape_and_picks_the_platform_archive() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<ns2:sdkRepository xmlns:ns2="http://schemas.android.com/sdk/android/repo/repository2/03">
 <remotePackage path="build-tools;35.0.0">
  <revision><major>35</major></revision>
  <archives><archive><complete><url>build-tools_r35-linux.zip</url></complete><host-os>linux</host-os></archive></archives>
 </remotePackage>
 <remotePackage path="platform-tools">
  <revision><major>37</major><minor>0</minor><micro>1</micro></revision>
  <archives>
   <archive><complete><size>9054187</size><checksum type="sha1">477254aa5f903c15cf51001717bdf347fb6b53e0</checksum><url>platform-tools_r37.0.1-linux.zip</url></complete><host-os>linux</host-os></archive>
   <archive><complete><size>16110554</size><checksum type="sha1">6ae73f4de6452dc57e62ec02b68eed92a4c21661</checksum><url>platform-tools_r37.0.1-darwin.zip</url></complete><host-os>macosx</host-os></archive>
   <archive><complete><size>8044989</size><checksum type="sha1">e03e78b1d80b396f1c3358e31251cb31740e1110</checksum><url>platform-tools_r37.0.1-win.zip</url></complete><host-os>windows</host-os></archive>
  </archives>
 </remotePackage>
</ns2:sdkRepository>"#;
        let manifest = MANIFEST_URL;
        let package = parse_manifest(xml, HostOs::Linux, manifest).unwrap();
        assert_eq!(
            package.revision,
            Revision {
                major: 37,
                minor: 0,
                micro: 1
            }
        );
        assert_eq!(
            package.archive.file_name,
            "platform-tools_r37.0.1-linux.zip"
        );
        assert_eq!(
            package.archive.url,
            "https://dl.google.com/android/repository/platform-tools_r37.0.1-linux.zip"
        );
        assert_eq!(package.archive.size, 9_054_187);
        assert_eq!(package.archive.digest.algorithm, DigestAlgorithm::Sha1);
        assert_eq!(
            package.archive.digest.hex,
            "477254aa5f903c15cf51001717bdf347fb6b53e0"
        );

        let windows = parse_manifest(xml, HostOs::Windows, manifest).unwrap();
        assert_eq!(windows.archive.file_name, "platform-tools_r37.0.1-win.zip");
    }

    #[test]
    fn newest_revision_wins_and_sha256_beats_sha1() {
        let xml = r#"<sdkRepository>
 <remotePackage path="platform-tools">
  <revision><major>35</major><minor>0</minor><micro>2</micro></revision>
  <archives><archive><complete><size>1</size><checksum type="sha1">477254aa5f903c15cf51001717bdf347fb6b53e0</checksum><url>old-linux.zip</url></complete><host-os>linux</host-os></archive></archives>
 </remotePackage>
 <remotePackage path="platform-tools">
  <revision><major>36</major><minor>0</minor><micro>0</micro></revision>
  <archives><archive><complete><size>2</size><checksum type="sha1">477254aa5f903c15cf51001717bdf347fb6b53e0</checksum><checksum type="sha256">9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08</checksum><url>new-linux.zip</url></complete><host-os>linux</host-os></archive></archives>
 </remotePackage>
</sdkRepository>"#;
        let package = parse_manifest(xml, HostOs::Linux, MANIFEST_URL).unwrap();
        assert_eq!(package.revision.major, 36);
        assert_eq!(package.archive.file_name, "new-linux.zip");
    }

    #[test]
    fn missing_package_archive_or_digest_are_typed_errors() {
        let empty = "<sdkRepository></sdkRepository>";
        assert!(matches!(
            parse_manifest(empty, HostOs::Linux, MANIFEST_URL),
            Err(InstallError::PackageMissing)
        ));

        let no_linux = r#"<sdkRepository><remotePackage path="platform-tools">
 <revision><major>1</major></revision>
 <archives><archive><complete><size>1</size><checksum type="sha1">477254aa5f903c15cf51001717bdf347fb6b53e0</checksum><url>x-win.zip</url></complete><host-os>windows</host-os></archive></archives>
</remotePackage></sdkRepository>"#;
        assert!(matches!(
            parse_manifest(no_linux, HostOs::Linux, MANIFEST_URL),
            Err(InstallError::NoArchiveFor { .. })
        ));

        let no_digest = r#"<sdkRepository><remotePackage path="platform-tools">
 <revision><major>1</major></revision>
 <archives><archive><complete><size>1</size><url>x-linux.zip</url></complete><host-os>linux</host-os></archive></archives>
</remotePackage></sdkRepository>"#;
        assert!(matches!(
            parse_manifest(no_digest, HostOs::Linux, MANIFEST_URL),
            Err(InstallError::DigestMissing { .. })
        ));
    }

    #[test]
    fn malformed_digests_and_unsupported_types_are_rejected() {
        assert!(Digest::parse("sha1", "nothex").is_err());
        assert!(Digest::parse("sha1", "aa").is_err());
        assert!(Digest::parse("md5", &"a".repeat(32)).is_err());
        assert_eq!(
            Digest::parse("SHA-256", &"AB".repeat(32)).unwrap().hex,
            "ab".repeat(32)
        );
    }

    #[test]
    fn allow_list_keeps_only_what_adb_needs() {
        assert!(HostOs::Linux.is_allowed("adb"));
        assert!(HostOs::Linux.is_allowed("lib64/libc++.so"));
        assert!(HostOs::Linux.is_allowed("source.properties"));
        assert!(!HostOs::Linux.is_allowed("fastboot"));
        assert!(!HostOs::Linux.is_allowed("etc1tool"));
        assert!(!HostOs::Linux.is_allowed("lib64/nested/deep.so"));
        assert!(!HostOs::Linux.is_allowed("adb.exe"));
        assert!(HostOs::Windows.is_allowed("adb.exe"));
        assert!(HostOs::Windows.is_allowed("AdbWinApi.dll"));
        assert!(!HostOs::Windows.is_allowed("lib64/libc++.so"));
        assert!(!HostOs::Windows.is_allowed("sqlite3.exe"));
    }

    #[test]
    fn installs_a_verified_archive_and_skips_the_rest() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let dest = dir.path().join("platform-tools");

        let report = installer.install(&dest, HostOs::Linux, &mut cont).unwrap();
        assert_eq!(
            report.revision,
            Revision {
                major: 99,
                minor: 0,
                micro: 0
            }
        );
        assert_eq!(report.version.major, 99);
        assert_eq!(report.digest, DigestAlgorithm::Sha1);
        assert_eq!(report.adb_path, dest.join("adb"));
        assert!(report.adb_path.is_file());
        assert!(dest.join("lib64/libc++.so").is_file());
        assert!(dest.join("source.properties").is_file());
        assert!(
            !dest.join("fastboot").exists(),
            "fastboot must stay in the zip"
        );
        assert!(dest.join("adb").exists());

        assert!(!staging_leftovers(dir.path()), "staging area must be gone");

        // The config now points at the managed install.
        let mut cfg = AppConfig::default();
        report.apply_to(&mut cfg);
        assert_eq!(cfg.adb_mode, AdbMode::Managed);
        assert_eq!(cfg.managed_adb_dir.as_deref(), Some(dest.as_path()));
    }

    #[test]
    fn a_corrupt_archive_is_rejected_before_anything_is_installed() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        // Manifest promises the digest of the *good* archive; the server sends
        // a tampered one.
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let mut tampered = zip.clone();
        let last = tampered.len() - 1;
        tampered[last] ^= 0xff;
        let (installer, _fetcher, _probe, dir) = scripted(xml, tampered, url);
        let dest = dir.path().join("platform-tools");

        let err = installer
            .install(&dest, HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(
            matches!(err, InstallError::DigestMismatch { ref algorithm, .. } if *algorithm == DigestAlgorithm::Sha1),
            "{err:?}"
        );
        assert!(!dest.exists(), "the target must not be created");
        assert!(
            !staging_leftovers(dir.path()),
            "a rejected download must not leave a staging area"
        );
    }

    #[test]
    fn a_truncated_download_is_retried_and_then_succeeds() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let fetcher = Arc::new(
            MockFetcher::new()
                .with_manifest(&xml)
                .with_file(url, zip)
                .truncating(1),
        );
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(
            Arc::clone(&fetcher) as Arc<dyn Fetcher>,
            Arc::new(MockFsProbe::new().with_existing(&["/data"])) as Arc<dyn FsProbe>,
        )
        .using_manifest("http://127.0.0.1:1/repository2-3.xml")
        .with_version_probe(Arc::new(|_| {
            Ok(super::super::locate::AdbVersion {
                major: 99,
                minor: 0,
                patch: 0,
                raw: "Version 99.0.0".into(),
            })
        }));
        let dest = dir.path().join("platform-tools");
        installer.install(&dest, HostOs::Linux, &mut cont).unwrap();
        assert!(dest.join("adb").is_file());
        assert_eq!(
            downloads(&fetcher).len(),
            2,
            "one truncated attempt, one good one"
        );
    }

    #[test]
    fn a_permanently_truncated_download_gives_up_with_a_typed_error() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let fetcher = Arc::new(
            MockFetcher::new()
                .with_manifest(&xml)
                .with_file(url, zip)
                .truncating(99),
        );
        let dir = tempfile::tempdir().unwrap();
        let installer = Installer::new(
            Arc::clone(&fetcher) as Arc<dyn Fetcher>,
            Arc::new(MockFsProbe::new()) as Arc<dyn FsProbe>,
        )
        .using_manifest("http://127.0.0.1:1/repository2-3.xml");
        let dest = dir.path().join("platform-tools");
        let err = installer
            .install(&dest, HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(
            matches!(err, InstallError::DownloadIncomplete { .. }),
            "{err:?}"
        );
        assert_eq!(downloads(&fetcher).len(), DOWNLOAD_ATTEMPTS as usize);
        assert!(!dest.exists());
    }

    #[test]
    fn a_size_mismatch_is_caught_even_when_the_server_claims_success() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        // Publish a size that is not the size we actually serve.
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64 + 7, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, fetcher, _probe, dir) = scripted(xml, zip, url);
        let err = installer
            .install(&dir.path().join("pt"), HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(matches!(err, InstallError::SizeMismatch { .. }), "{err:?}");
        assert_eq!(downloads(&fetcher).len(), DOWNLOAD_ATTEMPTS as usize);
    }

    #[test]
    fn a_zip_slip_entry_is_rejected_before_it_touches_the_disk() {
        let zip = platform_tools_zip(&[
            ("platform-tools/adb", b"#!/bin/sh\n".as_slice()),
            ("platform-tools/../../escaped.txt", b"pwned".as_slice()),
        ]);
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let dest = dir.path().join("platform-tools");
        let err = installer
            .install(&dest, HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(matches!(err, InstallError::UnsafeEntry { .. }), "{err:?}");
        assert!(!dest.exists());
        assert!(!dir.path().join("escaped.txt").exists());
        assert!(!dir.path().parent().unwrap().join("escaped.txt").exists());
    }

    #[test]
    fn an_absolute_entry_is_rejected_too() {
        let zip = platform_tools_zip(&[
            ("platform-tools/adb", b"#!/bin/sh\n".as_slice()),
            ("/etc/passwd", b"nope".as_slice()),
        ]);
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let err = installer
            .install(&dir.path().join("pt"), HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(matches!(err, InstallError::UnsafeEntry { .. }), "{err:?}");
    }

    #[test]
    fn an_archive_without_adb_is_incomplete() {
        let zip = platform_tools_zip(&[("platform-tools/fastboot", b"only fastboot".as_slice())]);
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let err = installer
            .install(&dir.path().join("pt"), HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(
            matches!(err, InstallError::ArchiveIncomplete { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_low_disk_is_refused_before_downloading() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", 9_000_000, "sha1", &sha1_of(&zip), url)],
        );
        let fetcher = Arc::new(MockFetcher::new().with_manifest(&xml).with_file(url, zip));
        let dir = tempfile::tempdir().unwrap();
        // The install target does not exist yet, so the probe has to know the
        // directory that will hold it.
        let probe = Arc::new(
            MockFsProbe::new()
                .with_existing(&[dir.path().to_str().unwrap()])
                .free_bytes(1024),
        );
        let installer = Installer::new(
            Arc::clone(&fetcher) as Arc<dyn Fetcher>,
            Arc::clone(&probe) as Arc<dyn FsProbe>,
        )
        .using_manifest("http://127.0.0.1:1/repository2-3.xml");
        let err = installer
            .install(&dir.path().join("pt"), HostOs::Linux, &mut cont)
            .unwrap_err();
        match err {
            InstallError::NoSpace {
                needed, available, ..
            } => {
                assert!(needed >= MIN_FREE_BYTES);
                assert_eq!(available, 1024);
            }
            other => panic!("{other:?}"),
        }
        assert!(
            downloads(&fetcher).is_empty(),
            "no archive may be fetched when the disk is full: {:?}",
            fetcher.requests()
        );
    }

    #[test]
    fn cancelling_stops_the_download_and_leaves_the_target_alone() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let dest = dir.path().join("platform-tools");
        let err = installer
            .install(&dest, HostOs::Linux, &mut |_| Control::Cancel)
            .unwrap_err();
        assert!(matches!(err, InstallError::Cancelled), "{err:?}");
        assert!(!dest.exists());
        assert!(!staging_leftovers(dir.path()));
    }

    #[test]
    fn an_existing_installation_is_replaced_and_the_old_one_removed() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let dest = dir.path().join("platform-tools");
        std::fs::create_dir_all(dest.join("lib64")).unwrap();
        std::fs::write(dest.join("adb"), b"old broken adb").unwrap();
        std::fs::write(dest.join("lib64/old.so"), b"old").unwrap();

        installer.install(&dest, HostOs::Linux, &mut cont).unwrap();
        let adb = std::fs::read(dest.join("adb")).unwrap();
        assert!(adb.starts_with(b"#!/bin/sh"), "adb must be the new one");
        assert!(!dest.join("lib64/old.so").exists(), "the old copy is gone");
        assert!(!staging_leftovers(dir.path()));
    }

    #[test]
    fn a_broken_extraction_keeps_the_previous_installation() {
        // The archive is fine, but the version probe says adb does not run:
        // this is the "upgrade to a build that does not start" case.
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let installer = installer.with_version_probe(Arc::new(|path| {
            Err(AdbLocateError::NotExecutable(path.to_path_buf()))
        }));
        let dest = dir.path().join("platform-tools");
        std::fs::create_dir_all(&dest).unwrap();
        std::fs::write(dest.join("adb"), b"the old, working adb").unwrap();

        let err = installer
            .install(&dest, HostOs::Linux, &mut cont)
            .unwrap_err();
        assert!(matches!(err, InstallError::Verify { .. }), "{err:?}");
        assert_eq!(
            std::fs::read(dest.join("adb")).unwrap(),
            b"the old, working adb",
            "the working installation must survive a failed upgrade"
        );
        assert!(!staging_leftovers(dir.path()));
    }

    #[test]
    fn progress_is_reported_for_every_stage() {
        let zip = linux_zip();
        let url = "http://127.0.0.1:1/platform-tools_r99-linux.zip";
        let xml = manifest_with_platform_tools(
            (99, 0, 0),
            &[("linux", zip.len() as u64, "sha1", &sha1_of(&zip), url)],
        );
        let (installer, _fetcher, _probe, dir) = scripted(xml, zip, url);
        let mut stages: Vec<Stage> = Vec::new();
        installer
            .install(&dir.path().join("pt"), HostOs::Linux, &mut |progress| {
                if stages.last() != Some(&progress.stage) {
                    stages.push(progress.stage);
                }
                if let Some(fraction) = progress.fraction() {
                    assert!((0.0..=1.0).contains(&fraction));
                }
                Control::Continue
            })
            .unwrap();
        assert_eq!(
            stages,
            vec![
                Stage::Manifest,
                Stage::Download,
                Stage::Verify,
                Stage::Extract,
                Stage::Check
            ]
        );
    }

    #[test]
    fn unsupported_platforms_are_named_in_the_error() {
        let err = InstallError::UnsupportedPlatform {
            os: "macos".to_string(),
        };
        assert!(err.to_string().contains("macos"), "{err}");
    }

    /// Archive requests only; the manifest fetch is not a download attempt.
    fn downloads(fetcher: &MockFetcher) -> Vec<String> {
        fetcher
            .requests()
            .into_iter()
            .filter(|url| !url.contains("repository2-3.xml"))
            .collect()
    }

    fn staging_leftovers(dir: &Path) -> bool {
        std::fs::read_dir(dir)
            .map(|entries| {
                entries.filter_map(|e| e.ok()).any(|e| {
                    let name = e.file_name().to_string_lossy().to_string();
                    name.contains(".installing-") || name.contains(".replaced-")
                })
            })
            .unwrap_or(false)
    }
}
