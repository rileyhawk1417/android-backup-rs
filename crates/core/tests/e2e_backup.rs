//! Wave 5 acceptance suite: the whole backup engine, end to end, on a scripted
//! phone.
//!
//! This is the **hard gate** in front of any UI work (plan `wave-5-backup-engine.md`).
//! Everything below the `RemoteFiles` trait is real: the real engine, the real
//! manifest, the real staging/verification/rename/history code, the real
//! filesystem. Only the phone is a double ([`MockRemote`]) and only the destination
//! is a temporary directory — so a green run here means "this would work on a
//! device that behaves like the mock", not "the pieces compile".
//!
//! Covered: happy path · changed file + history · cancel · kill recovery · denied
//! folder · weird file names · overlapping folder selection · failed-then-retry ·
//! pause/resume · control-API error matrix.

// A test that cannot fail loudly is a test nobody trusts: panicking on a broken
// assertion or a bad fixture is exactly what these lints are about.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use android_backup_core::adb::props::DeviceProps;
use android_backup_core::backup::controller::{
    BackupController, ControlError, JobHandle, StartError,
};
use android_backup_core::backup::destination::LocalFsProbe;
use android_backup_core::backup::job::{JobConfig, JobEvent, JobOutcome};
use android_backup_core::backup::transfer::{self, VerifyMode};
use android_backup_core::backup::PauseReason;
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::jobs::{self, JobStatus};
use android_backup_core::database::{files, Db};
use android_backup_core::mock::MockRemote;

/// The shared-storage root every test phone reports.
const ROOT: &str = "/storage/emulated/0";
/// How long a scripted transfer takes, so control tests are not a race with an
/// instant job.
const SLOW: Duration = Duration::from_millis(60);
/// Upper bound for a job that should finish promptly.
const TIMEOUT: Duration = Duration::from_secs(30);

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

struct Harness {
    /// Kept alive so the destination directory outlives the test.
    _dir: tempfile::TempDir,
    /// Destination root of the device's backup.
    root: PathBuf,
    db: Arc<Db>,
    remote: Arc<MockRemote>,
    device: DeviceRow,
    controller: BackupController,
}

impl Harness {
    fn new(remote: MockRemote) -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("backup").join("Test Phone");
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let device = devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z")
            .expect("device row");
        devices::seed_default_folders(&db, device.id).expect("default folders");
        let remote = Arc::new(remote);
        let controller = BackupController::new(
            Arc::clone(&db),
            Arc::clone(&remote) as Arc<dyn android_backup_core::backup::RemoteFiles>,
            Arc::new(LocalFsProbe::new()),
        );
        Self {
            _dir: dir,
            root,
            db,
            remote,
            device,
            controller,
        }
    }

    fn config(&self) -> JobConfig {
        self.config_for(&["DCIM"])
    }

    fn config_for(&self, folders: &[&str]) -> JobConfig {
        JobConfig {
            device_id: self.device.id,
            serial: self.device.serial.clone(),
            display_name: self.device.display_name.clone(),
            root: self.root.clone(),
            folders: folders.iter().map(|f| f.to_string()).collect(),
            custom_folders: Vec::new(),
            whole_storage: false,
            verify: VerifyMode::Size,
            storage_root: Some(ROOT.to_string()),
            free_space_floor: 0,
        }
    }

    fn start(&self) -> JobHandle {
        self.controller
            .start(&self.device, self.config())
            .expect("job should start")
    }

    /// Start a job, collect every event it emits, and wait for it to finish.
    fn run(&self) -> (JobOutcome, Vec<JobEvent>) {
        self.run_with(self.config())
    }

    fn run_with(&self, config: JobConfig) -> (JobOutcome, Vec<JobEvent>) {
        let handle = self
            .controller
            .start(&self.device, config)
            .expect("job should start");
        let (outcome, events) = collect_until_finished(&handle);
        self.controller.prune_finished();
        (outcome, events)
    }

    fn job_row(&self, id: i64) -> jobs::JobRow {
        jobs::get_job(&self.db, id)
            .expect("job query")
            .expect("job row")
    }

    fn manifest(&self) -> Vec<files::BackupFileRow> {
        files::manifest_for_device(&self.db, self.device.id).expect("manifest")
    }

    fn dest(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

fn props() -> DeviceProps {
    DeviceProps {
        manufacturer: Some("Test".into()),
        model: Some("Phone".into()),
        product: None,
        android_version: Some("13".into()),
    }
}

/// Drain events until `Finished`, failing loudly if the job never ends.
fn collect_until_finished(handle: &JobHandle) -> (JobOutcome, Vec<JobEvent>) {
    let deadline = Instant::now() + TIMEOUT;
    let mut events = Vec::new();
    loop {
        match handle.try_recv() {
            Some(JobEvent::Finished(outcome)) => return (*outcome, events),
            Some(event) => events.push(event),
            None => {
                assert!(
                    Instant::now() < deadline,
                    "job {} did not finish within {TIMEOUT:?}; events so far: {events:?}",
                    handle.id
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Wait for an event matching `wanted`, collecting everything seen meanwhile.
fn wait_for(
    handle: &JobHandle,
    seen: &mut Vec<JobEvent>,
    mut wanted: impl FnMut(&JobEvent) -> bool,
) -> bool {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match handle.try_recv() {
            Some(event) => {
                let hit = wanted(&event);
                let finished = matches!(event, JobEvent::Finished(_));
                seen.push(event);
                if hit {
                    return true;
                }
                if finished {
                    return false;
                }
            }
            None => {
                if Instant::now() >= deadline {
                    return false;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Every leftover staging file under `root` (a crash's calling card).
fn partials(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if path.extension().is_some_and(|e| e == "part") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&transfer::staging_dir(root), &mut out);
    out
}

fn count_copied(events: &[JobEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, JobEvent::FileCopied { .. }))
        .count()
}

fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

// ---------------------------------------------------------------------------
// happy path
// ---------------------------------------------------------------------------

#[test]
fn five_files_are_copied_verified_and_recorded() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/Camera/IMG_001.jpg"), b"one")
        .with_file(&format!("{ROOT}/DCIM/Camera/IMG_002.jpg"), b"two two")
        .with_file(&format!("{ROOT}/DCIM/Camera/IMG_003.jpg"), b"three three")
        .with_file(&format!("{ROOT}/DCIM/Screenshots/shot.png"), b"png")
        .with_file(&format!("{ROOT}/DCIM/.thumbnails/thumb"), b"thumb");
    let harness = Harness::new(remote);

    let (outcome, events) = harness.run();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.scanned, 5);
    assert_eq!(outcome.counters.copied, 5);
    assert_eq!(outcome.counters.failed, 0);
    assert_eq!(outcome.counters.skipped, 0);
    assert_eq!(count_copied(&events), 5);

    // The files really exist, with the phone's bytes.
    let expected = [
        ("DCIM/Camera/IMG_001.jpg", b"one".as_slice()),
        ("DCIM/Camera/IMG_002.jpg", b"two two".as_slice()),
        ("DCIM/Camera/IMG_003.jpg", b"three three".as_slice()),
        ("DCIM/Screenshots/shot.png", b"png".as_slice()),
        ("DCIM/.thumbnails/thumb", b"thumb".as_slice()),
    ];
    for (rel, bytes) in expected {
        assert_eq!(read(&harness.dest(rel)), bytes, "{rel}");
    }
    assert!(partials(&harness.root).is_empty(), "no staging leftovers");

    // The manifest agrees with the disk.
    let manifest = harness.manifest();
    assert_eq!(manifest.len(), 5);
    for row in &manifest {
        assert!(row.is_verified(), "{} is {:?}", row.source_path, row.status);
        assert!(row.last_successful_backup.is_some());
        assert!(Path::new(&row.destination_path).exists());
    }

    // The job row matches the outcome (plan E05.6.5).
    let job = harness.job_row(outcome.job_id);
    assert_eq!(job.status, JobStatus::Completed);
    assert_eq!(job.files_scanned, 5);
    assert_eq!(job.files_copied, 5);
    assert_eq!(job.files_skipped, 0);
    assert_eq!(job.files_failed, 0);
    assert!(job.completed_at.is_some());

    // Events: states are monotonic, discovery precedes copying, one Finished.
    let states: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            JobEvent::StateChanged(status) => Some(*status),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![JobStatus::Scanning, JobStatus::Copying],
        "planning is implicit, finalizing is not a row state"
    );
    let discovered = events
        .iter()
        .position(|event| matches!(event, JobEvent::Discovered { .. }))
        .expect("Discovered event");
    let first_copied = events
        .iter()
        .position(|event| matches!(event, JobEvent::FileCopied { .. }))
        .expect("FileCopied event");
    assert!(discovered < first_copied);
    assert!(
        harness.controller.active_jobs().is_empty(),
        "the finished job was retired from the registry"
    );
}

#[test]
fn a_second_run_copies_nothing() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"aaaa")
        .with_file(&format!("{ROOT}/DCIM/b.jpg"), b"bbbb");
    let harness = Harness::new(remote);

    let (first, _) = harness.run();
    assert_eq!(first.counters.copied, 2);
    let modified = std::fs::metadata(harness.dest("DCIM/a.jpg"))
        .expect("stat")
        .modified()
        .expect("mtime");

    let (second, events) = harness.run();

    assert!(second.succeeded(), "{second:?}");
    assert_eq!(second.counters.copied, 0);
    assert_eq!(second.counters.skipped, 2);
    assert!(count_copied(&events) == 0, "nothing was re-copied");
    assert!(
        harness.remote.pulls().len() == 2,
        "no extra pulls on the phone"
    );
    // The untouched file was not rewritten.
    assert_eq!(
        std::fs::metadata(harness.dest("DCIM/a.jpg"))
            .expect("stat")
            .modified()
            .expect("mtime"),
        modified
    );
}

#[test]
fn a_file_deleted_on_the_phone_is_kept_on_the_pc() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/keep.jpg"), b"keep")
        .with_file(&format!("{ROOT}/DCIM/gone.jpg"), b"gone");
    let harness = Harness::new(remote);
    let (first, _) = harness.run();
    assert_eq!(first.counters.copied, 2);

    harness.remote.remove(&format!("{ROOT}/DCIM/gone.jpg"));
    let (second, _) = harness.run();

    assert!(second.succeeded(), "{second:?}");
    assert_eq!(
        second.counters.scanned, 1,
        "the phone only has one file left"
    );
    assert!(harness.dest("DCIM/gone.jpg").exists(), "the PC copy stays");
    assert_eq!(read(&harness.dest("DCIM/gone.jpg")), b"gone");
    let manifest = harness.manifest();
    assert_eq!(manifest.len(), 2, "no manifest row is ever deleted");
    let gone = manifest
        .iter()
        .find(|row| row.source_path.ends_with("gone.jpg"))
        .expect("row for the deleted file");
    assert!(
        gone.is_verified(),
        "the row keeps describing the local copy"
    );
}

// ---------------------------------------------------------------------------
// changed file + history
// ---------------------------------------------------------------------------

#[test]
fn a_changed_file_is_replaced_and_the_old_version_kept() {
    let path = format!("{ROOT}/DCIM/Camera/a.jpg");
    let remote = MockRemote::new().with_file(&path, b"old bytes");
    let harness = Harness::new(remote);
    let (first, _) = harness.run();
    assert_eq!(first.counters.copied, 1);

    harness.remote.replace(&path, b"new bytes, same length!");
    let (second, events) = harness.run();

    assert!(second.succeeded(), "{second:?}");
    assert_eq!(second.counters.copied, 1, "a change is not skipped");
    assert_eq!(
        read(&harness.dest("DCIM/Camera/a.jpg")),
        b"new bytes, same length!"
    );

    let archived: Vec<PathBuf> = events
        .iter()
        .filter_map(|event| match event {
            JobEvent::FileCopied { archived, .. } => archived.clone(),
            _ => None,
        })
        .collect();
    assert_eq!(archived.len(), 1, "the previous version was archived");
    assert_eq!(read(&archived[0]), b"old bytes");
    assert!(
        archived[0].starts_with(harness.root.join(".backup/history")),
        "history lives under .backup: {}",
        archived[0].display()
    );
    // The user-visible name stays plain.
    assert_eq!(
        harness.dest("DCIM/Camera/a.jpg").file_name().unwrap(),
        "a.jpg"
    );
}

#[test]
fn a_locally_deleted_backup_is_re_copied_from_the_phone() {
    let path = format!("{ROOT}/DCIM/a.jpg");
    let remote = MockRemote::new().with_file(&path, b"content");
    let harness = Harness::new(remote);
    harness.run();

    // The user deleted the file from the backup drive by hand.
    std::fs::remove_file(harness.dest("DCIM/a.jpg")).expect("remove backup copy");
    let (second, events) = harness.run();

    assert!(second.succeeded(), "{second:?}");
    assert_eq!(second.counters.copied, 1);
    assert_eq!(read(&harness.dest("DCIM/a.jpg")), b"content");
    assert!(matches!(
        events
            .iter()
            .find(|event| matches!(event, JobEvent::FileCopied { .. })),
        Some(JobEvent::FileCopied { .. })
    ));
}

#[test]
fn a_truncated_local_copy_is_re_copied() {
    let path = format!("{ROOT}/DCIM/a.jpg");
    let remote = MockRemote::new().with_file(&path, b"0123456789");
    let harness = Harness::new(remote);
    harness.run();

    // Something ate the tail of the backup copy.
    std::fs::write(harness.dest("DCIM/a.jpg"), b"0123").expect("truncate");
    let (second, _) = harness.run();

    assert_eq!(
        second.counters.copied, 1,
        "the manifest cannot see this, the size check can"
    );
    assert_eq!(read(&harness.dest("DCIM/a.jpg")), b"0123456789");
}

// ---------------------------------------------------------------------------
// pause / resume / cancel
// ---------------------------------------------------------------------------

#[test]
fn a_paused_job_can_be_resumed_and_finishes() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"aaaa")
        .with_file(&format!("{ROOT}/DCIM/b.jpg"), b"bbbb")
        .with_file(&format!("{ROOT}/DCIM/c.jpg"), b"cccc")
        .with_pull_delay(SLOW);
    let harness = Harness::new(remote);

    let handle = harness.start();
    // Pause immediately: the engine checks the flag before every file, so it can
    // only ever pause *between* files (never mid-file).
    handle.pause();

    let mut events = Vec::new();
    assert!(
        wait_for(&handle, &mut events, |event| matches!(
            event,
            JobEvent::Paused { .. }
        )),
        "expected a Paused event, saw {events:?}"
    );
    assert_eq!(
        harness.job_row(handle.id).status,
        JobStatus::Paused,
        "the state is persisted, not just announced"
    );

    handle.resume();
    assert!(
        wait_for(&handle, &mut events, |event| matches!(
            event,
            JobEvent::Resumed
        )),
        "expected a Resumed event, saw {events:?}"
    );

    let outcome = handle
        .wait_forever()
        .expect("job should finish after resuming");
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 3);
    assert_eq!(harness.job_row(outcome.job_id).status, JobStatus::Completed);
    assert!(partials(&harness.root).is_empty());
}

#[test]
fn cancelling_mid_copy_keeps_what_was_already_verified() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"aaaa")
        .with_file(&format!("{ROOT}/DCIM/b.jpg"), b"bbbb")
        .with_file(&format!("{ROOT}/DCIM/c.jpg"), b"cccc")
        .with_pull_delay(SLOW);
    let harness = Harness::new(remote);

    let handle = harness.start();
    let mut events = Vec::new();
    assert!(
        wait_for(&handle, &mut events, |event| matches!(
            event,
            JobEvent::FileCopied { .. }
        )),
        "expected a first copy before cancelling, saw {events:?}"
    );
    handle.cancel();
    let outcome = handle.wait_forever().expect("cancelled job still ends");

    assert_eq!(outcome.status, JobStatus::Cancelled);
    assert!(
        outcome.counters.copied >= 1 && outcome.counters.copied < 3,
        "cancel should stop between files, got {:?}",
        outcome.counters
    );
    // Whatever was copied is on disk, verified in the manifest, and accounted for.
    let manifest = harness.manifest();
    let verified: Vec<_> = manifest.iter().filter(|row| row.is_verified()).collect();
    assert_eq!(verified.len() as u64, outcome.counters.copied);
    for row in verified {
        assert!(Path::new(&row.destination_path).exists());
    }
    // A cancelled file leaves no half-written file behind.
    assert!(
        partials(&harness.root).is_empty(),
        "no partials after a cancel"
    );
    let job = harness.job_row(outcome.job_id);
    assert_eq!(job.status, JobStatus::Cancelled);
    assert_eq!(job.files_copied as u64, outcome.counters.copied);
}

#[test]
fn cancelling_before_the_first_file_copies_nothing() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"aaaa")
        .with_pull_delay(SLOW);
    let harness = Harness::new(remote);

    let handle = harness.start();
    handle.cancel();
    let outcome = handle.wait_forever().expect("cancelled job still ends");

    assert_eq!(outcome.status, JobStatus::Cancelled);
    assert!(partials(&harness.root).is_empty());
    assert!(harness.remote.pulls().is_empty(), "nothing was pulled");
    assert!(
        harness.manifest().iter().all(|row| !row.is_verified()),
        "nothing is claimed to be backed up"
    );
}

// ---------------------------------------------------------------------------
// kill recovery
// ---------------------------------------------------------------------------

#[test]
fn a_killed_run_leaves_no_doubt_and_the_next_run_repairs_it() {
    let a = format!("{ROOT}/DCIM/a.jpg");
    let b = format!("{ROOT}/DCIM/b.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"file a content")
        .with_file(&b, b"file b content");
    let harness = Harness::new(remote);

    // Simulate a process killed mid-transfer: `a` had been copied and recorded,
    // `b` was being pulled when everything stopped. The crash leaves a staging
    // file and an open job row behind.
    let crashed_job =
        jobs::insert_job(&harness.db, harness.device.id, "2026-01-01T00:00:00Z").expect("job row");
    jobs::transition(&harness.db, crashed_job, JobStatus::Scanning).expect("scanning");
    jobs::transition(&harness.db, crashed_job, JobStatus::Copying).expect("copying");
    let staged = harness.root.join(".incomplete/DCIM/b.jpg.part");
    std::fs::create_dir_all(staged.parent().unwrap()).expect("staging dir");
    std::fs::write(&staged, b"file b c").expect("partial bytes");
    files::record_verified_now(
        &harness.db,
        harness.device.id,
        crashed_job,
        &files::FileRecord {
            source_path: a.clone(),
            destination_path: harness.dest("DCIM/a.jpg").to_string_lossy().to_string(),
            source_size: Some(b"file a content".len() as u64),
            source_modified_at: Some(1_700_000_000),
            content_hash: None,
        },
    )
    .expect("verified record");
    std::fs::create_dir_all(harness.dest("DCIM")).expect("dest dir");
    std::fs::write(harness.dest("DCIM/a.jpg"), b"file a content").expect("copy a");
    files::record_failed(
        &harness.db,
        harness.device.id,
        crashed_job,
        &files::FileRecord {
            source_path: b.clone(),
            destination_path: harness.dest("DCIM/b.jpg").to_string_lossy().to_string(),
            source_size: Some(b"file b content".len() as u64),
            source_modified_at: Some(1_700_000_000),
            content_hash: None,
        },
    )
    .expect("failed record");

    // Startup recovery (what the UI calls when a device appears).
    let report = harness
        .controller
        .recover(harness.device.id, &harness.root)
        .expect("recovery");

    assert_eq!(report.partials_removed, vec![staged.clone()]);
    assert_eq!(report.unfinished_jobs_closed, vec![crashed_job]);
    assert!(!staged.exists(), "the partial is gone");
    assert_eq!(harness.job_row(crashed_job).status, JobStatus::Failed);

    // The next run re-copies only what is not verified; the good file is untouched.
    let before = std::fs::metadata(harness.dest("DCIM/a.jpg"))
        .expect("stat")
        .modified()
        .expect("mtime");
    let (outcome, _) = harness.run();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 1, "only b needed copying");
    assert_eq!(outcome.counters.skipped, 1, "a was already verified");
    assert_eq!(read(&harness.dest("DCIM/b.jpg")), b"file b content");
    assert_eq!(
        std::fs::metadata(harness.dest("DCIM/a.jpg"))
            .expect("stat")
            .modified()
            .expect("mtime"),
        before,
        "the already-verified file was not rewritten"
    );
    assert!(partials(&harness.root).is_empty());
}

/// Only a hash check can catch a transfer that delivered the right number of
/// wrong bytes — and the `Hash` mode must actually refuse to commit it.
#[test]
fn hash_verification_catches_corrupted_bytes() {
    let path = format!("{ROOT}/DCIM/photo.jpg");
    let remote = MockRemote::new()
        .with_file(&path, b"these are the real bytes")
        .with_corrupted_pull(&path);
    let harness = Harness::new(remote);

    let mut config = harness.config();
    config.verify = VerifyMode::Hash;
    let (outcome, events) = harness.run_with(config);

    assert_eq!(outcome.status, JobStatus::Failed);
    assert_eq!(
        outcome.counters.copied, 0,
        "the corrupt copy was not committed"
    );
    assert_eq!(outcome.counters.failed, 1);
    assert!(!harness.dest("DCIM/photo.jpg").exists());
    assert!(partials(&harness.root).is_empty());
    assert!(events
        .iter()
        .find_map(|event| match event {
            JobEvent::FileFailed { source, message } if source == &path => Some(message.clone()),
            _ => None,
        })
        .is_some_and(|message| {
            let message = message.to_lowercase();
            message.contains("sha-256") && message.contains("does not match")
        }));

    // With the corruption gone, the same job succeeds and the hash is recorded.
    harness.remote.restore(&path);
    let (second, _) = harness.run_with(harness_config_with_hash(&harness));
    assert!(second.succeeded(), "{second:?}");
    let row = harness
        .manifest()
        .into_iter()
        .find(|row| row.source_path == path)
        .expect("row");
    assert!(row.is_verified());
    assert!(
        row.content_hash.is_some(),
        "hash mode records the hash it checked"
    );
    assert_eq!(
        read(&harness.dest("DCIM/photo.jpg")),
        b"these are the real bytes"
    );
}

fn harness_config_with_hash(harness: &Harness) -> JobConfig {
    let mut config = harness.config();
    config.verify = VerifyMode::Hash;
    config
}

/// On a device whose `toybox` cannot hash, hash mode degrades to recording the
/// digest of the copy it wrote — and says so, instead of pretending it verified.
#[test]
fn hash_mode_without_a_remote_digest_still_backs_up() {
    let path = format!("{ROOT}/DCIM/photo.jpg");
    let remote = MockRemote::new()
        .with_file(&path, b"some bytes")
        .with_no_remote_hash();
    let harness = Harness::new(remote);

    let (outcome, _) = harness.run_with(harness_config_with_hash(&harness));

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 1);
    let row = harness
        .manifest()
        .into_iter()
        .find(|row| row.source_path == path)
        .expect("row");
    assert!(row.is_verified());
    assert!(
        row.content_hash.is_some(),
        "the local digest is still recorded as evidence"
    );
    assert_eq!(read(&harness.dest("DCIM/photo.jpg")), b"some bytes");
}

#[test]
fn a_verify_failure_leaves_no_final_file_and_is_retried_next_run() {
    let path = format!("{ROOT}/DCIM/big.bin");
    let remote = MockRemote::new()
        .with_file(&path, b"0123456789")
        .with_truncated_pull(&path, 4);
    let harness = Harness::new(remote);

    let (first, events) = harness.run();

    assert_eq!(first.status, JobStatus::Failed);
    assert_eq!(first.counters.copied, 0);
    assert_eq!(first.counters.failed, 1);
    assert!(events
        .iter()
        .any(|event| matches!(event, JobEvent::FileFailed { .. })));
    assert!(
        !harness.dest("DCIM/big.bin").exists(),
        "no truncated file in the tree"
    );
    assert!(
        partials(&harness.root).is_empty(),
        "the partial was discarded"
    );
    let row = harness
        .manifest()
        .into_iter()
        .find(|row| row.source_path == path);
    assert!(!row.expect("row exists").is_verified());

    // The transfer works on the next attempt.
    harness.remote.restore(&path);
    let (second, _) = harness.run();
    assert!(second.succeeded(), "{second:?}");
    assert_eq!(second.counters.copied, 1);
}

// ---------------------------------------------------------------------------
// denied folders, weird names, overlapping selection
// ---------------------------------------------------------------------------

#[test]
fn an_unreadable_folder_is_a_warning_not_a_failure() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/Camera/ok.jpg"), b"ok")
        .with_file(&format!("{ROOT}/Pictures/pic.jpg"), b"pic")
        .with_denied_folder(&format!("{ROOT}/DCIM/Android/data"));
    let harness = Harness::new(remote);

    let (outcome, events) =
        harness.run_with(harness.config_for(&["DCIM/Android/data", "Pictures", "DCIM/Camera"]));

    assert!(
        outcome.succeeded(),
        "an unreadable folder must not fail the job"
    );
    assert_eq!(outcome.counters.copied, 2);
    let warning = events
        .iter()
        .find_map(|event| match event {
            JobEvent::FolderWarning { folder, message } => Some((folder.clone(), message.clone())),
            _ => None,
        })
        .expect("a FolderWarning event");
    assert!(warning.0.ends_with("DCIM/Android/data"), "{warning:?}");
    assert!(
        warning.1.to_lowercase().contains("denied"),
        "the message says why: {warning:?}"
    );
    assert!(harness.dest("Pictures/pic.jpg").exists());
}

#[test]
fn hostile_file_names_are_sanitized_without_colliding() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a:b.jpg"), b"colon")
        .with_file(&format!("{ROOT}/DCIM/a?b.jpg"), b"question")
        .with_file(&format!("{ROOT}/DCIM/CON.txt"), b"reserved")
        .with_file(&format!("{ROOT}/DCIM/trailing. "), b"trailing")
        .with_file(&format!("{ROOT}/DCIM/Фото 001.jpg"), b"unicode");
    let harness = Harness::new(remote);

    let (outcome, _) = harness.run();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 5);

    let manifest = harness.manifest();
    assert_eq!(manifest.len(), 5);
    let mut destinations: Vec<String> = Vec::new();
    for row in &manifest {
        let path = PathBuf::from(&row.destination_path);
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        assert!(
            !name.contains([':', '?', '*', '<', '>', '|', '"', '\\']),
            "unsafe character survived in {name}"
        );
        assert!(
            !name.ends_with('.') && !name.ends_with(' '),
            "Windows would strip {name}"
        );
        assert!(path.exists(), "{} is missing", path.display());
        destinations.push(row.destination_path.clone());
    }
    destinations.sort();
    destinations.dedup();
    assert_eq!(
        destinations.len(),
        5,
        "the two colliding names got distinct paths"
    );

    let contents: Vec<Vec<u8>> = manifest
        .iter()
        .map(|row| read(Path::new(&row.destination_path)))
        .collect();
    assert!(contents.contains(&b"colon".to_vec()));
    assert!(contents.contains(&b"question".to_vec()));
    assert!(contents.contains(&b"unicode".to_vec()));
}

#[test]
fn overlapping_folders_copy_each_file_exactly_once() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/Camera/deep.jpg"), b"deep")
        .with_file(&format!("{ROOT}/DCIM/top.jpg"), b"top");
    let harness = Harness::new(remote);

    let config = harness.config_for(&["DCIM", "DCIM/Camera", "DCIM", "DCIM/Camera"]);
    let (outcome, events) = harness.run_with(config);

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 2, "no file is copied twice");
    assert_eq!(count_copied(&events), 2);
    // The overlapping folder was never listed: DCIM already covers it.
    let listings = harness.remote.listings();
    assert_eq!(listings.len(), 1, "listed {listings:?}");
    let pulls = harness.remote.pulls();
    let unique: std::collections::HashSet<_> = pulls.iter().collect();
    assert_eq!(unique.len(), pulls.len(), "no duplicate pulls: {pulls:?}");
}

#[test]
fn whole_storage_backs_up_everything_once() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"a")
        .with_file(&format!("{ROOT}/Download/b.pdf"), b"b")
        .with_file(&format!("{ROOT}/Music/song.mp3"), b"song");
    let harness = Harness::new(remote);

    let mut config = harness.config_for(&["DCIM"]);
    config.whole_storage = true;
    let (outcome, _) = harness.run_with(config);

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 3);
    assert_eq!(harness.remote.listings(), vec![ROOT.to_string()]);
    assert!(harness.dest("Music/song.mp3").exists());
}

// ---------------------------------------------------------------------------
// failure and retry
// ---------------------------------------------------------------------------

#[test]
fn a_permanently_unreadable_file_fails_then_succeeds_when_it_comes_back() {
    let path = format!("{ROOT}/DCIM/locked.jpg");
    let remote = MockRemote::new()
        .with_file(&path, b"locked")
        .with_file(&format!("{ROOT}/DCIM/fine.jpg"), b"fine")
        .with_unreadable_file(&path);
    let harness = Harness::new(remote);

    let (first, events) = harness.run();

    assert_eq!(
        first.status,
        JobStatus::Failed,
        "a partial backup is not a success"
    );
    assert_eq!(first.counters.copied, 1);
    assert_eq!(first.counters.failed, 1);
    assert!(first.failures.iter().any(|(source, _)| source == &path));
    assert!(first
        .error_summary
        .as_deref()
        .is_some_and(|summary| summary.contains("retried")));
    assert!(events
        .iter()
        .any(|event| matches!(event, JobEvent::FileFailed { source, .. } if source == &path)));
    assert!(!harness.dest("DCIM/locked.jpg").exists());

    // The phone becomes readable, or the user grants access.
    harness.remote.restore(&path);
    let (second, _) = harness.run();

    assert!(second.succeeded(), "{second:?}");
    assert_eq!(second.counters.copied, 1, "the failed file was retried");
    assert_eq!(second.counters.skipped, 1, "the good file was skipped");
    assert_eq!(read(&harness.dest("DCIM/locked.jpg")), b"locked");
    assert!(harness.manifest().iter().all(|row| row.is_verified()));
}

#[test]
fn a_transient_disconnect_is_retried_in_place() {
    let path = format!("{ROOT}/DCIM/flaky.jpg");
    let remote = MockRemote::new()
        .with_file(&path, b"flaky bytes")
        .with_transient_pull_failures(&path, 2);
    let harness = Harness::new(remote);

    let (outcome, _) = harness.run();

    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 1);
    assert_eq!(
        harness.remote.pulls().len(),
        3,
        "two failures, then the real pull"
    );
    assert_eq!(read(&harness.dest("DCIM/flaky.jpg")), b"flaky bytes");
}

#[test]
fn a_lost_device_pauses_the_job_with_a_reason() {
    let path = format!("{ROOT}/DCIM/unplugged.jpg");
    let remote = MockRemote::new()
        .with_file(&path, b"payload")
        .with_unauthorized_pull_failures(&path, 1)
        .with_pull_delay(SLOW);
    let harness = Harness::new(remote);

    // Pause the job, then let the failing pull happen: the engine reports *why*
    // it is waiting rather than dying.
    let handle = harness.start();
    handle.pause();
    let mut events = Vec::new();
    let saw_pause = wait_for(&handle, &mut events, |event| {
        matches!(event, JobEvent::Paused { .. })
    });
    assert!(saw_pause, "expected a Paused event, saw {events:?}");

    handle.resume();
    let outcome = handle.wait_forever().expect("job finishes");
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(read(&harness.dest("DCIM/unplugged.jpg")), b"payload");

    let reasons: Vec<PauseReason> = events
        .iter()
        .filter_map(|event| match event {
            JobEvent::Paused { reason, .. } => Some(*reason),
            _ => None,
        })
        .collect();
    assert!(
        reasons.contains(&PauseReason::UserRequested)
            || reasons.contains(&PauseReason::AuthorizationNeeded),
        "the pause carries a reason: {reasons:?}"
    );
}

// ---------------------------------------------------------------------------
// destination problems
// ---------------------------------------------------------------------------

/// "Choose a folder on a fresh drive": a destination folder that does not
/// exist yet is created, not refused — the drive is reachable, only the tree
/// is new. (This test was once `a_missing_destination_is_refused_at_start`,
/// pointing at `/definitely/not/a/mounted/drive` — refused on Linux only
/// because `/` is not writable there; on Windows the runner owns `C:\`, so
/// start did what it documents, created the folder and began. The refusal
/// was an accident of Linux permissions, not a rule, and the rule — cannot be
/// created at all — is the test below.)
#[test]
fn a_missing_destination_folder_is_created_not_refused() {
    let remote = MockRemote::new().with_file(&format!("{ROOT}/DCIM/a.jpg"), b"a");
    let harness = Harness::new(remote);

    let mut config = harness.config();
    let root = harness.root.join("fresh/Phone");
    config.root = root.clone();
    let (outcome, _) = harness.run_with(config);
    assert!(outcome.succeeded(), "{outcome:?}");
    assert!(
        root.join("DCIM").is_dir(),
        "the destination tree was created"
    );
}

#[test]
fn a_device_without_shared_storage_fails_cleanly() {
    let harness = Harness::new(MockRemote::without_storage());
    let mut config = harness.config();
    config.storage_root = None;

    let (outcome, events) = harness.run_with(config);

    assert_eq!(outcome.status, JobStatus::Failed);
    assert_eq!(outcome.counters.copied, 0);
    assert!(events
        .iter()
        .any(|event| matches!(event, JobEvent::Fatal { .. })));
}

// ---------------------------------------------------------------------------
// control API
// ---------------------------------------------------------------------------

#[test]
fn a_second_backup_of_the_same_device_is_refused_but_another_device_is_fine() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"aaaa")
        .with_pull_delay(SLOW);
    let harness = Harness::new(remote);

    let first = harness.start();
    let err = harness
        .controller
        .start(&harness.device, harness.config())
        .expect_err("double start must fail");
    assert!(matches!(err, StartError::DeviceBusy { device_id } if device_id == harness.device.id));

    // A different device may run in parallel.
    let other = devices::upsert_seen(&harness.db, "SER2", &props(), "2026-01-02T00:00:00Z")
        .expect("second device");
    let other_config = JobConfig {
        device_id: other.id,
        serial: other.serial.clone(),
        display_name: other.display_name.clone(),
        ..harness.config()
    };
    let second = harness
        .controller
        .start(&other, other_config)
        .expect("a second device is not busy");
    assert_ne!(first.id, second.id);

    first.wait_forever().expect("first job");
    second.wait_forever().expect("second job");
    assert!(harness.controller.active_jobs().is_empty());
}

#[test]
fn control_errors_are_typed() {
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/a.jpg"), b"aaaa")
        .with_pull_delay(SLOW);
    let harness = Harness::new(remote);

    assert!(matches!(
        harness.controller.pause(9_999),
        Err(ControlError::UnknownJob { .. })
    ));
    assert!(matches!(
        harness.controller.resume(9_999),
        Err(ControlError::UnknownJob { .. })
    ));
    assert!(matches!(
        harness.controller.cancel(9_999),
        Err(ControlError::UnknownJob { .. })
    ));

    let handle = harness.start();
    assert!(
        matches!(
            harness.controller.resume(handle.id),
            Err(ControlError::NotPaused { .. })
        ),
        "resuming a job that never paused is an error"
    );
    harness.controller.pause(handle.id).expect("pause");
    assert!(matches!(
        harness.controller.pause(handle.id),
        Err(ControlError::AlreadyPaused { .. })
    ));
    harness.controller.resume(handle.id).expect("resume");
    handle.wait_forever().expect("finish");

    assert!(
        matches!(
            harness.controller.cancel(handle.id),
            Err(ControlError::AlreadyFinished { .. })
        ),
        "a finished job cannot be cancelled"
    );
    assert!(
        matches!(
            harness.controller.pause(handle.id),
            Err(ControlError::AlreadyFinished { .. })
        ),
        "a finished job cannot be paused"
    );
}

#[test]
fn a_destination_that_cannot_be_created_is_reported_by_start() {
    let harness = Harness::new(MockRemote::new().with_file(&format!("{ROOT}/DCIM/a.jpg"), b"a"));
    // A file where the destination directory belongs: creating the root fails
    // on every platform, because the blocker is a fact about the path, not
    // about who is allowed to write to the filesystem root.
    let blocked = harness.root.join("blocked");
    std::fs::create_dir_all(&harness.root).expect("dest root");
    std::fs::write(&blocked, b"not a directory").expect("blocking file");

    let mut config = harness.config();
    config.root = blocked.join("Phone");
    let err = harness
        .controller
        .start(&harness.device, config)
        .expect_err("start must fail");
    assert!(matches!(err, StartError::Destination(_)), "{err:?}");
    assert!(
        harness.remote.listings().is_empty(),
        "the phone was never listed"
    );
    assert!(harness.remote.pulls().is_empty());
    assert!(harness.manifest().is_empty(), "nothing was recorded");
}
