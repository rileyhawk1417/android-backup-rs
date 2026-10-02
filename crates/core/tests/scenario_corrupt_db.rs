//! E07.3.4 — the manifest itself is damaged.
//!
//! The manifest is an index, not the archive (plan §9), so a damaged one must
//! cost the user *time*, never data. This scenario damages a real file-backed
//! manifest in the ways that actually happen (a truncated write, garbage from a
//! disk error, a stray directory in the way) and checks what the app does with
//! it:
//!
//! * the unreadable bytes are kept under a `*.corrupt-*` name — nothing deleted;
//! * the app starts on a fresh manifest and can back up again;
//! * every backed-up file in the destination is still there afterwards;
//! * the re-copy that follows *versions* the old copies instead of overwriting
//!   them, because the manifest no longer has a row that would justify a skip.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use android_backup_core::backup::destination::LocalFsProbe;
use android_backup_core::backup::job::JobConfig;
use android_backup_core::backup::transfer::VerifyMode;
use android_backup_core::backup::{controller::BackupController, RemoteFiles};
use android_backup_core::clock;
use android_backup_core::database::{devices, files, open_or_quarantine, Db};
use android_backup_core::mock::MockRemote;

use common::{archived, collect_until_finished, count_copied, props_for, read, tree, ROOT};

fn phone_file(i: usize) -> String {
    format!("{ROOT}/DCIM/file{i}.jpg")
}

fn phone_content(i: usize) -> Vec<u8> {
    format!("payload {i}").into_bytes()
}

const PHONE_FILES: usize = 4;

fn scripted_phone() -> MockRemote {
    (0..PHONE_FILES).fold(MockRemote::new(), |remote, i| {
        remote.with_file(&phone_file(i), &phone_content(i))
    })
}

fn job_config(root: &Path, device_id: i64) -> JobConfig {
    JobConfig {
        device_id,
        serial: "SER1".into(),
        display_name: "Test Phone".into(),
        root: root.to_path_buf(),
        folders: vec!["DCIM".into()],
        custom_folders: Vec::new(),
        whole_storage: false,
        verify: VerifyMode::Size,
        storage_root: Some(ROOT.into()),
        free_space_floor: 0,
    }
}

/// Back up `PHONE_FILES` files into `dest`, using `db_path` as the manifest.
/// Returns the device id and the number of files copied.
fn run_a_backup(_dir: &Path, db_path: &Path, dest: &Path) -> (i64, usize) {
    let db = Arc::new(Db::open_file(db_path).expect("open the manifest"));
    db.migrate().expect("migrate");
    let device =
        devices::upsert_seen(&db, "SER1", &props_for("Phone"), &clock::now_iso()).expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");

    let remote = Arc::new(scripted_phone());
    let controller = BackupController::new(
        Arc::clone(&db),
        Arc::clone(&remote) as Arc<dyn RemoteFiles>,
        Arc::new(LocalFsProbe::new()),
    );
    let handle = controller
        .start(&device, job_config(dest, device.id))
        .expect("job starts");
    let (outcome, events) = collect_until_finished(&handle, common::TIMEOUT);
    assert!(outcome.succeeded(), "{outcome:?}");
    (device.id, count_copied(&events))
}

fn stash_files(dir: &Path) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = std::fs::read_dir(dir)
        .expect("read dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .map(|name| name.to_string_lossy().contains(".corrupt-"))
                .unwrap_or(false)
        })
        .collect();
    out.sort();
    out
}

#[test]
fn a_truncated_manifest_costs_time_not_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("manifest.db");
    let dest = dir.path().join("backup").join("Test Phone");
    std::fs::create_dir_all(&dest).expect("destination folder");

    assert_eq!(run_a_backup(dir.path(), &db_path, &dest).1, PHONE_FILES);
    let before = tree(&dest);
    assert_eq!(before.len(), PHONE_FILES);

    // A half-written manifest, the classic result of a power cut.
    let bytes = std::fs::read(&db_path).expect("read the manifest");
    std::fs::write(&db_path, &bytes[..bytes.len() / 3]).expect("truncate");

    let (db, quarantine) = open_or_quarantine(&db_path).expect("the app still starts");
    let report = quarantine
        .clone()
        .expect("a damaged manifest is quarantined");
    assert_eq!(report.original, db_path);
    assert_eq!(report.stash, stash_files(dir.path())[0]);
    let stash_name = report
        .stash
        .file_name()
        .and_then(|name| name.to_str())
        .expect("stash name");
    assert!(
        stash_name.starts_with("manifest.corrupt-") && stash_name.ends_with(".db"),
        "the kept file is named after the manifest it came from: {stash_name}"
    );
    assert!(
        report
            .guidance
            .contains(&report.stash.display().to_string()),
        "the user is told exactly which file was kept: {}",
        report.guidance
    );

    // The damaged bytes are all still there, under the new name.
    let kept = std::fs::read(&report.stash).expect("the quarantined file is readable");
    assert_eq!(
        kept.len(),
        bytes.len() / 3,
        "every kept byte is the old byte"
    );
    assert_eq!(&kept[..], &bytes[..bytes.len() / 3]);

    // The fresh manifest is usable and knows nothing (it is an index, and the
    // index is what was lost).
    db.migrate().expect("migrate");
    assert!(files::manifest_for_device(&db, 1)
        .expect("manifest")
        .is_empty());

    // Nothing in the destination was touched by any of this.
    assert_eq!(tree(&dest), before, "the archive is untouched");

    // The next backup re-copies everything — the manifest cannot prove what is
    // already there — and the copies it replaces are versioned, not destroyed.
    let device =
        devices::upsert_seen(&db, "SER1", &props_for("Phone"), &clock::now_iso()).expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    let remote = Arc::new(scripted_phone());
    let controller = BackupController::new(
        Arc::new(db),
        Arc::clone(&remote) as Arc<dyn RemoteFiles>,
        Arc::new(LocalFsProbe::new()),
    );
    let handle = controller
        .start(&device, job_config(&dest, device.id))
        .expect("job starts");
    let (outcome, events) = collect_until_finished(&handle, common::TIMEOUT);
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(
        count_copied(&events),
        PHONE_FILES,
        "a fresh manifest means a full re-copy"
    );

    let history = archived(&events);
    assert_eq!(
        history.len(),
        PHONE_FILES,
        "every replaced copy was versioned, not overwritten: {history:?}"
    );
    for path in &history {
        assert!(path.exists(), "{} is missing", path.display());
        assert!(
            path.starts_with(&dest),
            "history stays inside the destination: {}",
            path.display()
        );
        let expected: Vec<Vec<u8>> = (0..PHONE_FILES).map(phone_content).collect();
        assert!(
            expected.contains(&read(path)),
            "a versioned copy holds the previous contents: {}",
            path.display()
        );
        assert!(
            path.to_string_lossy().contains("history"),
            "versions live in a history folder: {}",
            path.display()
        );
    }
    for i in 0..PHONE_FILES {
        assert_eq!(
            read(&dest.join(format!("DCIM/file{i}.jpg"))),
            phone_content(i)
        );
    }
    // Old copies plus new ones: strictly more files than before, never fewer.
    assert!(tree(&dest).len() >= before.len());
}

#[test]
fn garbage_bytes_are_quarantined_the_same_way() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("manifest.db");
    std::fs::write(&db_path, vec![0xab; 4096]).expect("write garbage");

    let (db, report) = open_or_quarantine(&db_path).expect("the app still starts");
    let report = report.expect("garbage is not a database");
    assert!(report.stash.exists());
    assert_eq!(
        std::fs::read(&report.stash).expect("kept"),
        vec![0xab; 4096]
    );
    db.migrate().expect("the replacement is a working manifest");
    devices::upsert_seen(&db, "SER1", &props_for("Phone"), &clock::now_iso()).expect("device");
}

#[test]
fn a_manifest_that_cannot_be_opened_at_all_is_not_silently_rotated() {
    // A directory where the manifest should be: not "corrupt", just unusable, and
    // rotating it would be destructive nonsense. The error must come back to the
    // caller, which is why the app reports it instead of pretending.
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("manifest.db");
    std::fs::create_dir(&db_path).expect("directory in the way");

    let err = open_or_quarantine(&db_path).expect_err("a directory is not a manifest");
    assert!(
        stash_files(dir.path()).is_empty(),
        "nothing was rotated: {:?}",
        stash_files(dir.path())
    );
    assert!(db_path.is_dir(), "and the directory was left alone");
    assert!(!err.to_string().is_empty());
}

#[test]
fn a_healthy_manifest_is_never_touched() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("manifest.db");
    let dest = dir.path().join("backup").join("Test Phone");
    std::fs::create_dir_all(&dest).expect("destination folder");
    run_a_backup(dir.path(), &db_path, &dest);

    let before = std::fs::read(&db_path).expect("read");
    let (db, report) = open_or_quarantine(&db_path).expect("open");
    assert!(report.is_none(), "a good manifest is opened as-is");
    assert!(stash_files(dir.path()).is_empty());
    assert_eq!(std::fs::read(&db_path).expect("read"), before);
    assert_eq!(
        files::manifest_for_device(&db, 1).expect("manifest").len(),
        PHONE_FILES,
        "and its contents are still there"
    );
}
