//! The managed installer against the *real* Google repository (E07.1).
//!
//! Everything else about the installer is tested with fixtures: a pinned
//! manifest, a synthetic archive, a `MockFetcher`, a `MockFsProbe`. That proves
//! the logic, but it cannot prove the things that change without us: that the
//! manifest Google serves today still parses, that the digest it publishes still
//! matches the archive it serves, and that the extracted binary really runs on
//! this machine with no help from Android Studio.
//!
//! So this test is real, and it is `#[ignore]`d, because it downloads ~9 MB,
//! executes the downloaded binary and fails when the network is down — none of
//! which belongs in the per-commit suite:
//!
//! ```text
//! cargo test -p android-backup-core --test installer_network -- --ignored --nocapture
//! ```
//!
//! Nightly CI runs it (`.github/workflows/nightly.yml`) as a non-blocking job, so
//! a Google outage shows up as a red nightly run and never as a blocked PR.

// A test that cannot fail loudly is a test nobody trusts: panicking on a broken
// assertion or a bad download is exactly what these lints are about.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use android_backup_core::adb::installer::{
    Control, DigestAlgorithm, Fetcher, HostOs, InstallError, Installer, Progress, Stage,
};
use android_backup_core::adb::locate::{adb_binary_name, probe_version};
use android_backup_core::adb::runner::ProcessAdbRunner;
use android_backup_core::backup::destination::LocalFsProbe;

/// "adb runs" means a process that exits 0, prints a version, and agrees with
/// the version the installer's own probe saw.
fn assert_adb_runs(adb: &Path, expected: &android_backup_core::adb::locate::AdbVersion) {
    let output = Command::new(adb)
        .arg("version")
        .output()
        .unwrap_or_else(|err| panic!("could not run {}: {err}", adb.display()));
    assert!(
        output.status.success(),
        "adb version exited {:?}: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    let version = probe_version(&ProcessAdbRunner::new(adb.to_path_buf()))
        .expect("the installed adb reports a version");
    assert_eq!(&version, expected);
}

#[test]
#[ignore = "downloads platform-tools from dl.google.com (~9 MB) and executes it"]
fn installs_platform_tools_from_google_end_to_end() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("platform-tools");

    let os = HostOs::current().expect("host os");
    let installer = Installer::http().expect("http client");

    let mut stages: Vec<Stage> = Vec::new();
    let mut download: Option<Progress> = None;
    let report = installer
        .install(&dest, os, &mut |progress| {
            if stages.last() != Some(&progress.stage) {
                stages.push(progress.stage);
            }
            if progress.stage == Stage::Download {
                download = Some(progress);
            }
            Control::Continue
        })
        .expect("install from Google's repository");

    // 1. It came from Google over HTTPS, it is the real archive, and a real
    //    digest was checked against it.
    assert!(
        report.archive_url.starts_with("https://dl.google.com/"),
        "unexpected archive host: {}",
        report.archive_url
    );
    assert!(
        report.archive_url.contains("platform-tools"),
        "unexpected archive: {}",
        report.archive_url
    );
    assert!(
        report.downloaded_bytes > 1_000_000,
        "the archive was suspiciously small: {} bytes",
        report.downloaded_bytes
    );
    assert!(
        matches!(
            report.digest,
            DigestAlgorithm::Sha1 | DigestAlgorithm::Sha256
        ),
        "no published digest was verified"
    );

    // 2. The binary is exactly where the locator looks in `Managed` mode, which
    //    is what makes the installation usable by the rest of the app.
    assert_eq!(report.adb_path, dest.join(adb_binary_name()));
    assert!(report.adb_path.is_file(), "no adb at the reported path");

    // 3. It runs, here, with its own shared libraries.
    assert_adb_runs(&report.adb_path, &report.version);

    // 4. The allow-list kept the extraction to adb's own artifacts: no
    //    hyper/thread/vulkan shader caches, no fastboot, no SDK manager.
    for rel in &report.files {
        let name = rel.rsplit('/').next().unwrap_or(rel);
        assert!(
            name == adb_binary_name()
                || name.starts_with("AdbWin")
                || name.starts_with("libc++")
                || name.starts_with("NOTICE")
                || name.starts_with("source.properties"),
            "the allow-list extracted something unexpected: {rel}"
        );
    }

    // 5. Progress covered the whole journey, and the download had a total so the
    //    UI could draw a fraction instead of a spinner.
    for stage in [
        Stage::Manifest,
        Stage::Download,
        Stage::Verify,
        Stage::Extract,
        Stage::Check,
    ] {
        assert!(stages.contains(&stage), "{stage:?} was never reported");
    }
    let download = download.expect("the download reported progress");
    assert!(
        download.total.is_some_and(|total| total >= 1_000_000),
        "the download never announced its size: {download:?}"
    );
    assert!(
        download.done > 0,
        "the download reported a total but no bytes: {download:?}"
    );

    println!(
        "installed adb {} (platform-tools {}) from {} — {} bytes, {} files kept",
        report.version,
        report.revision,
        report.archive_url,
        report.downloaded_bytes,
        report.files.len()
    );
}

/// The manifest host is a single constant, and this is what pins it.
///
/// If someone points the installer at a mirror, they have to change this test
/// too — which is the point: the plan allows exactly one source of adb binaries.
#[test]
fn the_manifest_source_is_googles_official_repository() {
    assert_eq!(
        android_backup_core::adb::installer::MANIFEST_URL,
        "https://dl.google.com/android/repository/repository2-3.xml"
    );
}

/// A fetcher that fails the test if it is asked for anything at all.
struct PanicFetcher;

impl Fetcher for PanicFetcher {
    fn get(&self, url: &str) -> Result<Vec<u8>, InstallError> {
        panic!("cancelling before the first byte must not fetch {url}");
    }

    fn get_to_file(
        &self,
        url: &str,
        _dest: &Path,
        _on_progress: &mut dyn FnMut(u64, Option<u64>) -> Control,
    ) -> Result<(), InstallError> {
        panic!("cancelling before the first byte must not download {url}");
    }
}

/// Cancel the instant the modal appears: no network, no staging, no trace.
///
/// This is the "user clicks Cancel before the manifest comes back" case. It runs
/// in the normal suite: the point is that it needs no server.
#[test]
fn cancelling_on_the_first_tick_touches_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("platform-tools");
    let installer = Installer::new(Arc::new(PanicFetcher), Arc::new(LocalFsProbe::new()));

    let err = installer
        .install(&dest, HostOs::current().expect("host os"), &mut |_| {
            Control::Cancel
        })
        .expect_err("a cancel must be reported, not swallowed");
    assert!(matches!(err, InstallError::Cancelled), "{err:?}");

    assert!(!dest.exists(), "a cancelled install created {dest:?}");
    let leftovers: Vec<String> = std::fs::read_dir(dir.path())
        .expect("read_dir")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    assert!(
        leftovers.is_empty(),
        "a cancelled install left {leftovers:?} behind"
    );
    // Cancelling a download that has already started is covered with fixtures in
    // `adb::installer::tests::cancelling_stops_the_download_and_leaves_the_target_alone`.
}
