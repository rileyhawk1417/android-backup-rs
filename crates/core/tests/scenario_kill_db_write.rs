//! E07.3.2 — the process dies in the middle of a job.
//!
//! This one is the real thing: a child process runs a backup against a
//! **file-backed** manifest, the parent kills it with `SIGKILL` while a file is
//! in flight, and then the parent plays the part of the next app start
//! (`open_or_quarantine` → `recovery::recover_device`) and of the next job.
//!
//! What has to hold after an abrupt death:
//!
//! * the manifest is still readable (WAL, journal, no half-written index);
//! * the job that was running is closed instead of haunting the history as
//!   "copying" forever;
//! * no staging leftovers, and no verified row that the disk cannot back up;
//! * the next job copies what is missing and nothing else.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::{Duration, Instant};

use android_backup_core::backup::controller::BackupController;
use android_backup_core::backup::destination::LocalFsProbe;
use android_backup_core::backup::job::{JobConfig, JobEvent};
use android_backup_core::backup::recovery;
use android_backup_core::backup::transfer::{self, VerifyMode};
use android_backup_core::backup::RemoteFiles;
use android_backup_core::clock;
use android_backup_core::database::jobs::JobStatus;
use android_backup_core::database::{devices, files, jobs, open_or_quarantine, Db};
use android_backup_core::mock::MockRemote;

use common::{collect_until_finished, count_copied, partials, props, read, tree, Harness, ROOT};

/// The `n`th file on the scripted phone. Kept in one place so the killer and the
/// victim agree on what the phone holds.
fn phone_file(i: usize) -> String {
    format!("{ROOT}/DCIM/file{i}.jpg")
}

fn phone_content(i: usize) -> Vec<u8> {
    format!("payload {i}").into_bytes()
}

const PHONE_FILES: usize = 6;

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

/// Runs in a child process, started by the test below, and is killed mid-copy.
///
/// Run on its own (the default when the whole suite runs) it does nothing: the
/// environment marker is what makes it a worker.
#[test]
fn crash_worker() {
    let Some(dest) = std::env::var_os("AB_CRASH_DEST").map(PathBuf::from) else {
        return;
    };
    let db_path =
        PathBuf::from(std::env::var_os("AB_CRASH_DB").expect("AB_CRASH_DB is set by the parent"));
    let db = Arc::new(Db::open_file(&db_path).expect("open the manifest"));
    db.migrate().expect("migrate");
    let device = devices::upsert_seen(&db, "SER1", &props(), &clock::now_iso()).expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");

    let remote = Arc::new(scripted_phone().with_pull_delay(common::SLOW));
    let controller = BackupController::new(
        Arc::clone(&db),
        Arc::clone(&remote) as Arc<dyn RemoteFiles>,
        Arc::new(LocalFsProbe::new()),
    );
    let handle = controller
        .start(&device, job_config(&dest, device.id))
        .expect("job starts");
    // Live until the parent kills us. The job runs on its own thread.
    loop {
        let _ = handle.try_recv();
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_killed_job_leaves_a_readable_manifest_and_the_next_job_finishes_it() {
    let dir = tempfile::tempdir().expect("tempdir");
    let dest = dir.path().join("backup").join("Test Phone");
    let db_path = dir.path().join("manifest.db");
    // The app creates the destination folder when the user picks it; the worker
    // only ever sees a folder that exists.
    std::fs::create_dir_all(&dest).expect("destination folder");

    let mut child = Command::new(std::env::current_exe().expect("current exe"))
        .args(["--exact", "crash_worker", "--nocapture"])
        .env("AB_CRASH_DB", &db_path)
        .env("AB_CRASH_DEST", &dest)
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn the crash worker");

    // Wait until the child has committed at least one file and is still going,
    // then pull the plug. Progress is visible from here through the same
    // database, which is exactly what a second process would see.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut committed = 0;
    while committed == 0 {
        if let Ok(db) = Db::open_file(&db_path) {
            committed = files::manifest_for_device(&db, 1)
                .map(|rows| rows.len())
                .unwrap_or(0);
        }
        assert!(
            Instant::now() < deadline,
            "the worker never committed a file (see its output above)"
        );
        assert!(
            child.try_wait().expect("poll the worker").is_none(),
            "the worker exited before it copied anything"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    child.kill().expect("kill the worker mid-job");
    let status = child.wait().expect("reap the worker");
    assert!(
        !status.success(),
        "the worker was supposed to be killed, not to exit cleanly"
    );

    // --- the next app start ------------------------------------------------
    let (db, quarantined) = open_or_quarantine(&db_path).expect("manifest reopens");
    assert!(
        quarantined.is_none(),
        "an abrupt kill must not cost the manifest: {quarantined:?}"
    );
    db.migrate().expect("migrate");
    let db = Arc::new(db);
    let device = devices::resolve_active_device(&db, "SER1")
        .expect("device query")
        .expect("registered device");

    let open_jobs = jobs::unfinished_jobs(&db).expect("unfinished jobs");
    assert_eq!(
        open_jobs.len(),
        1,
        "the killed job is still open, as a crash leaves it"
    );
    assert_eq!(open_jobs[0].id, 1);
    assert!(
        (1..PHONE_FILES).contains(&committed),
        "the kill landed mid-job with {committed} of {PHONE_FILES} files done"
    );

    let probe = LocalFsProbe::new();
    let report = recovery::recover_device(&db, &probe, &dest, device.id).expect("recovery");
    assert_eq!(
        report.unfinished_jobs_closed,
        vec![open_jobs[0].id],
        "the job is closed instead of haunting the history"
    );
    assert!(
        report.stale_rows.is_empty(),
        "everything recorded as verified is still on disk: {:?}",
        report.stale_rows
    );
    assert!(
        partials(&dest).is_empty(),
        "staging leftovers were swept: {:?}",
        partials(&dest)
    );
    let closed = jobs::get_job(&db, open_jobs[0].id)
        .expect("job query")
        .expect("job row");
    assert!(
        matches!(closed.status, JobStatus::Failed | JobStatus::Cancelled),
        "an interrupted job is not left copying: {:?}",
        closed.status
    );

    // Whatever survived the crash is intact and correctly recorded.
    let survivors: Vec<String> = files::manifest_for_device(&db, device.id)
        .expect("manifest")
        .into_iter()
        .filter(|row| row.status == files::status::VERIFIED)
        .map(|row| row.source_path)
        .collect();
    assert_eq!(survivors.len(), committed);
    for row in files::manifest_for_device(&db, device.id).expect("manifest") {
        let rel = row.source_path.trim_start_matches(&format!("{ROOT}/"));
        let local = dest.join(rel);
        assert_eq!(
            read(&local),
            phone_content(index_of(&row.source_path)),
            "a verified row must point at a complete copy"
        );
    }

    // --- the next job ------------------------------------------------------
    let remote = Arc::new(scripted_phone());
    let controller = BackupController::new(
        Arc::clone(&db),
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
        PHONE_FILES - committed,
        "only the files that were missing after the crash are copied"
    );
    assert_eq!(outcome.counters.skipped, committed as u64);

    // Every file on the phone is now backed up exactly once, byte for byte.
    let rows = files::manifest_for_device(&db, device.id).expect("manifest");
    assert_eq!(rows.len(), PHONE_FILES);
    for (i, path) in (0..PHONE_FILES).map(phone_file).enumerate() {
        let matching: Vec<_> = rows.iter().filter(|row| row.source_path == path).collect();
        assert_eq!(matching.len(), 1, "one row per source file: {path}");
        assert_eq!(matching[0].status, files::status::VERIFIED, "{path}");
        assert_eq!(
            read(&dest.join(format!("DCIM/file{i}.jpg"))),
            phone_content(i),
            "{path}"
        );
    }
    assert!(partials(&dest).is_empty());
    let leftovers = tree(&transfer::staging_dir(&dest));
    assert!(leftovers.is_empty(), "{leftovers:?}");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, JobEvent::FileFailed { .. })),
        "a restart should not need to fail anything: {events:?}"
    );
}

/// The file index of a scripted phone path (`…/file3.jpg` → 3), used to check a
/// recovered copy against the contents the phone is supposed to hold.
fn index_of(source_path: &str) -> usize {
    source_path
        .trim_end_matches(".jpg")
        .rsplit("file")
        .next()
        .and_then(|n| n.parse().ok())
        .expect("a scripted phone path looks like …/fileN.jpg")
}

#[test]
fn a_verified_row_whose_file_vanished_is_reconciled_on_the_next_start() {
    // Not a kill, but the same reconciliation the restart path performs: the user
    // (or a disk cleanup tool) deleted a file behind the app's back, and the
    // manifest must stop vouching for it.
    let remote = scripted_phone();
    let harness = Harness::new(remote);
    let (first, _) = harness.run();
    assert!(first.succeeded(), "{first:?}");

    // One copy is deleted, another is truncated by a half-finished write.
    std::fs::remove_file(harness.dest("DCIM/file1.jpg")).expect("remove");
    let truncated = harness.dest("DCIM/file2.jpg");
    let bytes = read(&truncated);
    std::fs::write(&truncated, &bytes[..bytes.len() / 2]).expect("truncate");

    let report = recovery::recover_device(
        &harness.db,
        &LocalFsProbe::new(),
        &harness.root,
        harness.device.id,
    )
    .expect("recovery");
    assert_eq!(report.stale_rows.len(), 2, "{report:?}");
    assert!(report.stale_rows.contains(&phone_file(1)));
    assert!(report.stale_rows.contains(&phone_file(2)));

    let status_of = |source: &str| {
        harness
            .manifest()
            .into_iter()
            .find(|row| row.source_path == source)
            .map(|row| row.status)
            .expect("row")
    };
    assert_ne!(status_of(&phone_file(1)), files::status::VERIFIED);
    assert_ne!(status_of(&phone_file(2)), files::status::VERIFIED);

    // The next job re-copies exactly those two files and nothing else.
    let (second, events) = harness.run();
    assert!(second.succeeded(), "{second:?}");
    assert_eq!(count_copied(&events), 2);
    assert_eq!(second.counters.skipped, PHONE_FILES as u64 - 2);
    for i in [1, 2] {
        assert_eq!(
            read(&harness.dest(&format!("DCIM/file{i}.jpg"))),
            phone_content(i)
        );
    }
    assert!(partials(&harness.root).is_empty());
}

#[test]
fn a_file_committed_without_its_manifest_row_is_copied_again_safely() {
    // The narrowest possible crash window: the rename into place succeeded, the
    // process died before the manifest write. Both halves are visible from the
    // outside — a complete file on disk, no row to justify skipping it — so the
    // next job must treat the file as new, and it must not destroy the copy it
    // cannot account for.
    let harness = Harness::new(scripted_phone());

    // Exactly what a completed transfer leaves behind, minus the manifest row.
    let orphan = harness.dest("DCIM/file0.jpg");
    std::fs::create_dir_all(orphan.parent().expect("parent")).expect("dirs");
    std::fs::write(&orphan, b"half-remembered bytes").expect("orphan file");
    assert!(harness.manifest().is_empty(), "and no row to explain it");

    let (outcome, events) = harness.run();
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(
        count_copied(&events),
        PHONE_FILES,
        "an unaccounted file is copied again, not trusted"
    );
    assert_eq!(
        read(&orphan),
        phone_content(0),
        "the real bytes are in place"
    );

    let history = common::archived(&events);
    assert_eq!(
        history.len(),
        1,
        "the unexplained copy was kept: {history:?}"
    );
    assert_eq!(read(&history[0]), b"half-remembered bytes");
    assert_eq!(harness.manifest().len(), PHONE_FILES);

    // Settled: the retry put a row behind every file.
    let (again, events) = harness.run();
    assert!(again.succeeded(), "{again:?}");
    assert_eq!(count_copied(&events), 0);
    assert_eq!(again.counters.skipped, PHONE_FILES as u64);
    assert!(partials(&harness.root).is_empty());
}
