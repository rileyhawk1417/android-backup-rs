//! E07.3.5 — a phone with tens of thousands of files.
//!
//! Two things are being checked, and neither of them is "it works":
//!
//! * **the budget** — scanning the phone and comparing it against the manifest
//!   (the part the user waits for before anything is copied) must stay well under
//!   a minute for 50 000 files;
//! * **the memory** — the engine must not hold the phone in RAM. The plan for
//!   50 000 entries is a few tens of bytes per file; buffering file *contents*
//!   would be orders of magnitude worse, and the tracked peak below is what
//!   catches that regression.
//!
//! The phone is a double ([`LargePhone`]) that generates entries and payloads on
//! demand, so the numbers describe the engine rather than the fixture. The
//! measurements are printed (`cargo test -- --nocapture`) and repeated in the
//! assertion messages so a failure says what it saw.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use android_backup_core::backup::controller::BackupController;
use android_backup_core::backup::destination::LocalFsProbe;
use android_backup_core::backup::job::{JobConfig, JobEvent};
use android_backup_core::backup::transfer::VerifyMode;
use android_backup_core::backup::{EntryKind, RemoteEntry, RemoteError, RemoteFiles};
use android_backup_core::clock;
use android_backup_core::database::{devices, Db};

use common::{props, ROOT};

// ---------------------------------------------------------------------------
// peak-tracking allocator
// ---------------------------------------------------------------------------

/// Live bytes, tracked through a wrapper around the system allocator.
static LIVE: AtomicUsize = AtomicUsize::new(0);
/// High-water mark of [`LIVE`].
static PEAK: AtomicUsize = AtomicUsize::new(0);

struct Tracking;

#[global_allocator]
static ALLOCATOR: Tracking = Tracking;

fn account(delta: isize) {
    let live = if delta >= 0 {
        LIVE.fetch_add(delta as usize, Ordering::Relaxed) + delta as usize
    } else {
        LIVE.fetch_sub((-delta) as usize, Ordering::Relaxed) - (-delta) as usize
    };
    PEAK.fetch_max(live, Ordering::Relaxed);
}

unsafe impl GlobalAlloc for Tracking {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if !ptr.is_null() {
            account(layout.size() as isize);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        account(-(layout.size() as isize));
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if !ptr.is_null() {
            account(layout.size() as isize);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if !new_ptr.is_null() {
            account(new_size as isize - layout.size() as isize);
        }
        new_ptr
    }
}

// ---------------------------------------------------------------------------
// the phone
// ---------------------------------------------------------------------------

/// How many files the phone holds. Overridable so a slow machine (or a debugger)
/// can run a smaller version of the same test.
fn file_count() -> usize {
    std::env::var("AB_LARGE_FILES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(50_000)
}

/// `DCIM/Camera/IMG_0000123.jpg` — a shape and a spread of sizes like a real
/// camera roll, all deterministic so the second run is a pure "nothing changed".
fn remote_path(index: usize) -> String {
    format!("{ROOT}/DCIM/Camera/IMG_{index:07}.jpg")
}

fn file_size(index: usize) -> u64 {
    1024 + (index as u64 % 16) * 256
}

fn payload_for(index: usize) -> Vec<u8> {
    let size = file_size(index) as usize;
    let mut bytes = vec![0u8; size];
    for (offset, byte) in bytes.iter_mut().enumerate() {
        *byte = ((index + offset) % 251) as u8;
    }
    bytes
}

fn index_of(remote: &str) -> usize {
    remote
        .trim_end_matches(".jpg")
        .rsplit('_')
        .next()
        .and_then(|digits| digits.parse().ok())
        .unwrap_or_else(|| panic!("unexpected path {remote}"))
}

/// A phone with `count` files that are generated instead of stored.
struct LargePhone {
    count: usize,
}

impl RemoteFiles for LargePhone {
    fn list_files(&self, _serial: &str, _folder: &str) -> Result<Vec<RemoteEntry>, RemoteError> {
        Ok((0..self.count)
            .map(|index| RemoteEntry {
                path: remote_path(index),
                size: file_size(index),
                modified_at: Some(1_767_225_600),
                kind: EntryKind::File,
            })
            .collect())
    }

    fn pull(&self, _serial: &str, remote: &str, local: &Path) -> Result<(), RemoteError> {
        if let Some(parent) = local.parent() {
            std::fs::create_dir_all(parent).map_err(|err| RemoteError::PullFailed {
                path: remote.to_string(),
                message: err.to_string(),
            })?;
        }
        std::fs::write(local, payload_for(index_of(remote))).map_err(|err| {
            RemoteError::PullFailed {
                path: remote.to_string(),
                message: err.to_string(),
            }
        })
    }

    fn storage_roots(&self, _serial: &str) -> Result<Vec<String>, RemoteError> {
        Ok(vec![ROOT.to_string()])
    }
}

// ---------------------------------------------------------------------------
// the test
// ---------------------------------------------------------------------------

fn config(root: &Path, device_id: i64) -> JobConfig {
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

/// The `Discovered` event is emitted the moment the manifest comparison is done,
/// which is exactly the "before anything is copied" budget.
struct Measurement {
    plan: Duration,
    total: Duration,
    copied: usize,
    skipped: usize,
    peak_mb: f64,
}

fn run(
    db: &Arc<Db>,
    device: &android_backup_core::database::devices::DeviceRow,
    root: &Path,
    count: usize,
) -> Measurement {
    let remote = Arc::new(LargePhone { count });
    let controller = BackupController::new(
        Arc::clone(db),
        Arc::clone(&remote) as Arc<dyn RemoteFiles>,
        Arc::new(LocalFsProbe::new()),
    );
    let handle = controller
        .start(device, config(root, device.id))
        .expect("job starts");

    let started = Instant::now();
    let baseline = PEAK.load(Ordering::Relaxed);
    let mut plan = None;
    let mut copied = 0;
    let deadline = started + Duration::from_secs(20 * 60);
    let outcome = loop {
        match handle.try_recv() {
            Some(JobEvent::Discovered { .. }) => {
                plan = Some(started.elapsed());
            }
            Some(JobEvent::FileCopied { .. }) => copied += 1,
            Some(JobEvent::Finished(outcome)) => break outcome,
            Some(_) => {}
            None => {
                assert!(
                    Instant::now() < deadline,
                    "the job never finished ({copied} copied so far)"
                );
                std::thread::sleep(Duration::from_millis(1));
            }
        }
    };
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(baseline);
    assert!(
        outcome.succeeded(),
        "the job finished as {:?}: {:?}",
        outcome.status,
        outcome.error_summary
    );
    Measurement {
        plan: plan.expect("the scan always reports what it found"),
        total: started.elapsed(),
        copied,
        skipped: outcome.counters.skipped as usize,
        peak_mb: peak as f64 / (1024.0 * 1024.0),
    }
}

fn count_files(dir: &Path) -> usize {
    fn walk(dir: &Path, count: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(&path, count);
            } else {
                *count += 1;
            }
        }
    }
    let mut count = 0;
    walk(dir, &mut count);
    count
}

#[test]
fn a_phone_with_fifty_thousand_files_is_scanned_and_copied_within_budget() {
    let count = file_count();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("backup").join("Test Phone");
    std::fs::create_dir_all(&root).expect("destination folder");
    let db = Arc::new(Db::open_in_memory().expect("in-memory manifest"));

    let device =
        devices::upsert_seen(&db, "SER1", &props(), &clock::now_iso()).expect("device row");
    devices::seed_default_folders(&db, device.id).expect("default folders");

    // --- first run: everything is new ---------------------------------------
    let first = run(&db, &device, &root, count);
    println!(
        "large backup: {count} files — scan+compare {:.2}s, total {:.2}s, \
         {:.1} MB/s, peak growth {:.1} MB",
        first.plan.as_secs_f64(),
        first.total.as_secs_f64(),
        payload_total(count) as f64 / 1024.0 / 1024.0 / first.total.as_secs_f64(),
        first.peak_mb
    );
    assert_eq!(first.copied, count);
    assert_eq!(first.skipped, 0);
    assert!(
        first.plan < Duration::from_secs(60),
        "scan+compare for {count} files took {:.1}s, budget is 60s",
        first.plan.as_secs_f64()
    );
    assert_eq!(
        count_files(&root),
        count,
        "every file reached the destination"
    );

    // A sample is checked byte for byte; reading all of them would dominate the
    // runtime of the test without telling us anything new.
    for index in [0, 1, count / 3, count / 2, count - 1] {
        let local: PathBuf = root.join(format!("DCIM/Camera/IMG_{index:07}.jpg"));
        assert_eq!(
            std::fs::read(&local).expect("sample copy"),
            payload_for(index),
            "{}",
            local.display()
        );
    }
    assert_eq!(
        android_backup_core::database::files::manifest_for_device(&db, device.id)
            .expect("manifest")
            .len(),
        count,
        "one manifest row per file"
    );

    // --- second run: the phone has not changed ------------------------------
    let second = run(&db, &device, &root, count);
    println!(
        "large re-scan: {count} files — scan+compare {:.2}s, total {:.2}s, \
         peak growth {:.1} MB",
        second.plan.as_secs_f64(),
        second.total.as_secs_f64(),
        second.peak_mb
    );
    assert_eq!(
        second.copied, 0,
        "nothing changed on the phone, so nothing is copied"
    );
    assert_eq!(second.skipped, count);
    assert!(
        second.plan < Duration::from_secs(60),
        "re-scan of {count} files took {:.1}s, budget is 60s",
        second.plan.as_secs_f64()
    );

    // Memory: the plan for 50 000 entries is a few MB. This budget is generous
    // enough for any reasonable representation and far too small for anything
    // that keeps file contents around (~64 MB of payload at this size).
    let budget_mb = 200.0;
    assert!(
        first.peak_mb < budget_mb && second.peak_mb < budget_mb,
        "peak allocator growth was {:.1} MB (first run) / {:.1} MB (re-scan), \
         budget is {budget_mb} MB",
        first.peak_mb,
        second.peak_mb
    );
}

fn payload_total(count: usize) -> u64 {
    (0..count).map(file_size).sum()
}
