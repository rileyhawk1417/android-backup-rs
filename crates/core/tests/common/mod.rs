//! Shared harness for the scenario suites (Wave 7, E07.3).
//!
//! Identical in spirit to the Wave 5 harness: everything below `RemoteFiles` is
//! real (engine, manifest, staging, verification, rename, history, filesystem)
//! and only the phone is a double. The scenarios differ from the Wave 5 suite in
//! what they interrupt: a cable, the process itself, the database file.
#![allow(dead_code, clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use android_backup_core::adb::props::DeviceProps;
use android_backup_core::backup::controller::{BackupController, JobHandle};
use android_backup_core::backup::destination::LocalFsProbe;
use android_backup_core::backup::job::{JobConfig, JobEvent, JobOutcome};
use android_backup_core::backup::transfer::{self, VerifyMode};
use android_backup_core::backup::RemoteFiles;
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::jobs;
use android_backup_core::database::{files, Db};
use android_backup_core::mock::MockRemote;

/// The shared-storage root every test phone reports.
pub const ROOT: &str = "/storage/emulated/0";
/// How long a scripted transfer takes, so control tests are not a race with an
/// instant job.
pub const SLOW: Duration = Duration::from_millis(60);
/// Upper bound for a job that should finish promptly.
pub const TIMEOUT: Duration = Duration::from_secs(30);
/// Upper bound for the deliberately large scenario.
pub const LONG_TIMEOUT: Duration = Duration::from_secs(30 * 60);

pub struct Harness {
    /// Kept alive so the destination directory outlives the test. Shared between
    /// the phones of a multi-device scenario.
    pub dir: Arc<tempfile::TempDir>,
    pub root: PathBuf,
    pub db: Arc<Db>,
    pub remote: Arc<MockRemote>,
    pub device: DeviceRow,
    pub controller: BackupController,
}

impl Harness {
    pub fn new(remote: MockRemote) -> Self {
        let dir = Arc::new(tempfile::tempdir().expect("tempdir"));
        let root = dir.path().join("backup").join("Test Phone");
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let device = devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z")
            .expect("device row");
        devices::seed_default_folders(&db, device.id).expect("default folders");
        Self::sharing(dir, db, root, device, remote)
    }

    /// Another phone on the same manifest and the same temp directory, with its
    /// own destination root. Used by the multi-device scenarios.
    pub fn with_device(&self, serial: &str, display_name: &str, remote: MockRemote) -> Self {
        let device = devices::upsert_seen(
            &self.db,
            serial,
            &props_for(display_name),
            "2026-01-01T00:00:00Z",
        )
        .expect("device row");
        devices::seed_default_folders(&self.db, device.id).expect("default folders");
        let root = self.dir.path().join("backup").join(display_name);
        Self::sharing(
            Arc::clone(&self.dir),
            Arc::clone(&self.db),
            root,
            device,
            remote,
        )
    }

    fn sharing(
        dir: Arc<tempfile::TempDir>,
        db: Arc<Db>,
        root: PathBuf,
        device: DeviceRow,
        remote: MockRemote,
    ) -> Self {
        let remote = Arc::new(remote);
        let controller = BackupController::new(
            Arc::clone(&db),
            Arc::clone(&remote) as Arc<dyn RemoteFiles>,
            Arc::new(LocalFsProbe::new()),
        );
        Self {
            dir,
            root,
            db,
            remote,
            device,
            controller,
        }
    }

    pub fn config(&self) -> JobConfig {
        self.config_for(&["DCIM"])
    }

    pub fn config_for(&self, folders: &[&str]) -> JobConfig {
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

    pub fn start(&self) -> JobHandle {
        self.controller
            .start(&self.device, self.config())
            .expect("job should start")
    }

    pub fn start_with(&self, config: JobConfig) -> JobHandle {
        self.controller
            .start(&self.device, config)
            .expect("job should start")
    }

    /// Start a job, collect every event it emits, and wait for it to finish.
    pub fn run(&self) -> (JobOutcome, Vec<JobEvent>) {
        self.run_with(self.config())
    }

    pub fn run_with(&self, config: JobConfig) -> (JobOutcome, Vec<JobEvent>) {
        let handle = self.start_with(config);
        let (outcome, events) = collect_until_finished(&handle, TIMEOUT);
        self.controller.prune_finished();
        (outcome, events)
    }

    pub fn job_row(&self, id: i64) -> jobs::JobRow {
        jobs::get_job(&self.db, id)
            .expect("job query")
            .expect("job row")
    }

    pub fn manifest(&self) -> Vec<files::BackupFileRow> {
        files::manifest_for_device(&self.db, self.device.id).expect("manifest")
    }

    /// Manifest rows for one source path.
    pub fn rows_for(&self, source: &str) -> Vec<files::BackupFileRow> {
        self.manifest()
            .into_iter()
            .filter(|row| row.source_path == source)
            .collect()
    }

    pub fn dest(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }
}

pub fn props() -> DeviceProps {
    props_for("Phone")
}

pub fn props_for(model: &str) -> DeviceProps {
    DeviceProps {
        manufacturer: Some("Test".into()),
        model: Some(model.into()),
        product: None,
        android_version: Some("13".into()),
    }
}

/// Drain events until `Finished`, failing loudly if the job never ends.
pub fn collect_until_finished(
    handle: &JobHandle,
    timeout: Duration,
) -> (JobOutcome, Vec<JobEvent>) {
    let deadline = Instant::now() + timeout;
    let mut events = Vec::new();
    loop {
        match handle.try_recv() {
            Some(JobEvent::Finished(outcome)) => return (*outcome, events),
            Some(event) => events.push(event),
            None => {
                assert!(
                    Instant::now() < deadline,
                    "job {} did not finish within {timeout:?}; events so far: {events:?}",
                    handle.id
                );
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

/// Wait for an event matching `wanted`, collecting everything seen meanwhile.
pub fn wait_for(
    handle: &JobHandle,
    seen: &mut Vec<JobEvent>,
    mut wanted: impl FnMut(&JobEvent) -> bool,
) -> bool {
    wait_for_with(handle, seen, TIMEOUT, &mut wanted)
}

pub fn wait_for_with(
    handle: &JobHandle,
    seen: &mut Vec<JobEvent>,
    timeout: Duration,
    wanted: &mut impl FnMut(&JobEvent) -> bool,
) -> bool {
    let deadline = Instant::now() + timeout;
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
pub fn partials(root: &Path) -> Vec<PathBuf> {
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
    out.sort();
    out
}

pub fn count_copied(events: &[JobEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, JobEvent::FileCopied { .. }))
        .count()
}

/// Every archived version of a file, in the order the events reported them.
pub fn archived(events: &[JobEvent]) -> Vec<PathBuf> {
    events
        .iter()
        .filter_map(|event| match event {
            JobEvent::FileCopied { archived, .. } => archived.clone(),
            _ => None,
        })
        .collect()
}

pub fn read(path: &Path) -> Vec<u8> {
    std::fs::read(path).unwrap_or_else(|err| panic!("cannot read {}: {err}", path.display()))
}

/// Every file under `dir`, recursively, as `(path, bytes)` — sorted by path.
pub fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    fn walk(dir: &Path, out: &mut Vec<(PathBuf, Vec<u8>)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, out);
            } else if let Ok(bytes) = std::fs::read(&path) {
                out.push((path, bytes));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, &mut out);
    out.sort();
    out
}
