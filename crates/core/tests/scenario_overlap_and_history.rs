//! E07.3.6 — renames, replaced files, and the history tree at scale.
//!
//! The rule the user cares about (plan P2) is *nothing is ever silently
//! overwritten*. Every re-copy of a file that is already on disk therefore has to
//! answer one question: where did the previous bytes go? This scenario follows
//! the two ways a file gets replaced —
//!
//! * the same photo changes on the phone (ten versions of one file name);
//! * a *different* phone file ends up at the same local path, because the phone
//!   renamed it or because two different remote names sanitize to one local name;
//!
//! — and checks that each version is kept, uniquely named, and reachable, and
//! that the run after that is a no-op instead of an endless re-copy.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::path::PathBuf;
use std::sync::Arc;

use android_backup_core::backup::controller::BackupController;
use android_backup_core::backup::destination::LocalFsProbe;
use android_backup_core::backup::job::{JobConfig, JobEvent, JobOutcome};
use android_backup_core::backup::transfer::VerifyMode;
use android_backup_core::backup::RemoteFiles;
use android_backup_core::clock;
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::{files, Db};
use android_backup_core::mock::MockRemote;

use common::{archived, collect_until_finished, count_copied, read, tree, ROOT};

/// A file's modification time; each version of a file gets its own.
const BASE_MTIME: i64 = 1_767_225_600;

struct Phone {
    _dir: tempfile::TempDir,
    root: PathBuf,
    db: Arc<Db>,
    device: DeviceRow,
}

impl Phone {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("backup").join("Test Phone");
        std::fs::create_dir_all(&root).expect("destination folder");
        let db = Arc::new(Db::open_in_memory().expect("in-memory manifest"));
        let device = devices::upsert_seen(&db, "SER1", &common::props(), &clock::now_iso())
            .expect("device row");
        devices::seed_default_folders(&db, device.id).expect("default folders");
        Self {
            _dir: dir,
            root,
            db,
            device,
        }
    }

    /// Run one backup against a freshly scripted phone.
    fn run(&self, remote: MockRemote) -> (JobOutcome, Vec<JobEvent>) {
        let controller = BackupController::new(
            Arc::clone(&self.db),
            Arc::new(remote) as Arc<dyn RemoteFiles>,
            Arc::new(LocalFsProbe::new()),
        );
        let handle = controller
            .start(&self.device, self.config())
            .expect("job starts");
        let result = collect_until_finished(&handle, common::TIMEOUT);
        controller.prune_finished();
        result
    }

    fn config(&self) -> JobConfig {
        JobConfig {
            device_id: self.device.id,
            serial: self.device.serial.clone(),
            display_name: self.device.display_name.clone(),
            root: self.root.clone(),
            folders: vec!["DCIM".into()],
            custom_folders: Vec::new(),
            whole_storage: false,
            verify: VerifyMode::Size,
            storage_root: Some(ROOT.into()),
            free_space_floor: 0,
        }
    }

    fn dest(&self, rel: &str) -> PathBuf {
        self.root.join(rel)
    }

    fn manifest(&self) -> Vec<files::BackupFileRow> {
        files::manifest_for_device(&self.db, self.device.id).expect("manifest")
    }

    fn run_ok(&self, remote: MockRemote) -> Vec<JobEvent> {
        let (outcome, events) = self.run(remote);
        assert!(outcome.succeeded(), "{outcome:?}");
        events
    }
}

fn file(name: &str) -> String {
    format!("{ROOT}/DCIM/{name}")
}

fn scripted(name: &str, content: &[u8], modified_at: i64) -> MockRemote {
    MockRemote::new().with_file_at(&file(name), content, modified_at)
}

/// The live copy plus everything in the history tree, sorted by path.
fn all_copies(phone: &Phone) -> Vec<(PathBuf, Vec<u8>)> {
    tree(&phone.root)
}

#[test]
fn ten_versions_of_one_photo_are_all_kept() {
    let phone = Phone::new();
    let mut written: Vec<Vec<u8>> = Vec::new();
    let mut history_seen: Vec<PathBuf> = Vec::new();

    for version in 1..=10u32 {
        // Sizes differ so that each change is unambiguous evidence of a change;
        // the mtime moves with it, exactly like a re-saved photo.
        let content = format!("version {version}")
            .repeat(version as usize)
            .into_bytes();
        let events = phone.run_ok(scripted("IMG_1.jpg", &content, BASE_MTIME + version as i64));

        let archived = archived(&events);
        if version == 1 {
            assert!(
                archived.is_empty(),
                "the first copy replaces nothing: {archived:?}"
            );
        } else {
            assert_eq!(archived.len(), 1, "the previous version is moved aside");
        }
        history_seen.extend(archived);

        assert_eq!(
            read(&phone.dest("DCIM/IMG_1.jpg")),
            content,
            "the live copy is always the newest version"
        );
        written.push(content);
    }

    // Nine previous versions on disk, each under its own name and each holding a
    // distinct older version of the file — nothing was lost on the way.
    assert_eq!(history_seen.len(), 9, "{history_seen:?}");
    let mut unique = history_seen.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        9,
        "a version is never reused: {history_seen:?}"
    );

    let mut found: Vec<Vec<u8>> = history_seen.iter().map(|path| read(path)).collect();
    found.sort();
    let mut wanted: Vec<Vec<u8>> = written[..9].to_vec();
    wanted.sort();
    assert_eq!(found, wanted, "every version is present exactly once");

    // One source path is one manifest row, no matter how many times it changed.
    assert_eq!(phone.manifest().len(), 1);
    // Ten copies: the live one and nine versions in history.
    assert_eq!(all_copies(&phone).len(), 10);

    // Nothing changed on the phone, so the next run copies nothing.
    let events = phone.run_ok(scripted(
        "IMG_1.jpg",
        written.last().expect("ten versions"),
        BASE_MTIME + 10,
    ));
    assert_eq!(count_copied(&events), 0);
    assert!(archived(&events).is_empty());
    assert_eq!(all_copies(&phone).len(), 10, "and adds nothing new");
}

#[test]
fn a_photo_renamed_on_the_phone_keeps_the_old_copy() {
    let phone = Phone::new();
    phone.run_ok(scripted("IMG_1.jpg", b"the original photo", BASE_MTIME));

    // The phone renames the file (and, as is typical, rewrites it).
    let renamed = file("IMG_1_renamed.jpg");
    let remote = MockRemote::new().with_file_at(&renamed, b"a renamed photo", BASE_MTIME + 60);
    let events = phone.run_ok(remote);
    assert_eq!(count_copied(&events), 1);
    assert!(
        archived(&events).is_empty(),
        "a new name is a new file: nothing was replaced"
    );

    assert_eq!(
        read(&phone.dest("DCIM/IMG_1_renamed.jpg")),
        b"a renamed photo"
    );
    assert_eq!(
        read(&phone.dest("DCIM/IMG_1.jpg")),
        b"the original photo",
        "the copy of the old name stays: the phone deleting it is not evidence \
         that the user wants it gone"
    );
    assert_eq!(phone.manifest().len(), 2, "both source paths are recorded");

    // The renamed phone is now stable: no thrash, no re-copy.
    let again =
        phone.run_ok(MockRemote::new().with_file_at(&renamed, b"a renamed photo", BASE_MTIME + 60));
    assert_eq!(count_copied(&again), 0);
    assert_eq!(all_copies(&phone).len(), 2);
}

#[test]
fn a_new_file_taking_another_files_place_is_never_a_silent_overwrite() {
    let phone = Phone::new();
    // `A:B.jpg` and `A_B.jpg` are two different phone files that sanitize to the
    // very same local name (plan: `:` is not legal on Windows).
    phone.run_ok(scripted("A:B.jpg", b"the colon file", BASE_MTIME));
    assert_eq!(read(&phone.dest("DCIM/A_B.jpg")), b"the colon file");

    // The phone deletes the colon file and creates one whose name needs no
    // sanitizing. Its destination is already occupied — by unrelated bytes.
    let replacement = MockRemote::new().with_file_at(
        &file("A_B.jpg"),
        b"a different file entirely",
        BASE_MTIME + 60,
    );
    let events = phone.run_ok(replacement);
    assert_eq!(count_copied(&events), 1);
    let history = archived(&events);
    assert_eq!(history.len(), 1, "the occupied name is versioned aside");
    assert_eq!(
        read(&history[0]),
        b"the colon file",
        "the displaced bytes are preserved exactly"
    );
    assert_eq!(
        read(&phone.dest("DCIM/A_B.jpg")),
        b"a different file entirely"
    );

    // Two source paths, one local path: both are recorded, and the run after
    // this one settles instead of copying forever.
    let manifest = phone.manifest();
    assert_eq!(manifest.len(), 2);
    let sources: Vec<&str> = manifest
        .iter()
        .map(|row| row.source_path.as_str())
        .collect();
    assert!(sources.contains(&format!("{ROOT}/DCIM/A:B.jpg").as_str()));
    assert!(sources.contains(&format!("{ROOT}/DCIM/A_B.jpg").as_str()));

    let again = phone.run_ok(MockRemote::new().with_file_at(
        &file("A_B.jpg"),
        b"a different file entirely",
        BASE_MTIME + 60,
    ));
    assert_eq!(count_copied(&again), 0);
    assert_eq!(all_copies(&phone).len(), 2, "one live copy, one version");
}

#[test]
fn a_changed_photo_of_the_same_size_is_still_noticed() {
    // Same size, new mtime: the manifest's size check cannot see the change, the
    // timestamp can, and the previous bytes still end up in history.
    let phone = Phone::new();
    phone.run_ok(scripted("IMG_2.jpg", b"aaaaaaaa", BASE_MTIME));
    let events = phone.run_ok(scripted("IMG_2.jpg", b"bbbbbbbb", BASE_MTIME + 60));

    assert_eq!(count_copied(&events), 1);
    assert_eq!(read(&phone.dest("DCIM/IMG_2.jpg")), b"bbbbbbbb");
    let history = archived(&events);
    assert_eq!(history.len(), 1);
    assert_eq!(read(&history[0]), b"aaaaaaaa");
}
