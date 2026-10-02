//! E07.3.3 — names that collide.
//!
//! Two kinds of collision, and both are ways to lose data silently if they are
//! handled wrong:
//!
//! * **two phones with the same display name** — "Pixel" and "Pixel". They must
//!   not share a destination folder, and neither phone's history may reach into
//!   the other's files.
//! * **two phone files that sanitize to the same name** — `a:b.jpg` and
//!   `a?b.jpg` both become `a_b.jpg` on a safe filesystem, so the second one has
//!   to be disambiguated instead of overwriting the first.
//!
//! The tests drive the real engine: they are about what actually lands on disk.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use android_backup_core::backup::destination;
use android_backup_core::database::files::status;
use android_backup_core::database::{devices, Db};
use android_backup_core::mock::MockRemote;

use common::{count_copied, read, Harness, ROOT};

#[test]
fn two_phones_with_the_same_name_get_different_folders() {
    let parent = tempfile::tempdir().expect("tempdir");
    let db = std::sync::Arc::new(Db::open_in_memory().expect("in-memory db"));
    let seed = |serial: &str, name: &str| {
        let device = devices::upsert_seen(
            &db,
            serial,
            &common::props_for("Pixel"),
            "2026-01-01T00:00:00Z",
        )
        .expect("device row");
        devices::rename(&db, device.id, name).expect("rename");
        device.id
    };
    // The user named both phones "Pixel".
    let first_id = seed("AAA111", "Pixel");
    let second_id = seed("BBB222", "Pixel");
    let all = devices::list(&db).expect("devices");
    let row = |id: i64| {
        all.iter()
            .find(|row| row.id == id)
            .cloned()
            .expect("device row")
    };

    let first = destination::resolve(&row(first_id), &all, Some(parent.path())).expect("resolve");
    let second = destination::resolve(&row(second_id), &all, Some(parent.path())).expect("resolve");
    assert!(first.device_subfolder && second.device_subfolder);
    assert_ne!(
        first.root, second.root,
        "two phones called Pixel must not write into one folder"
    );
    assert_eq!(
        first.root.file_name().and_then(|n| n.to_str()),
        Some("Pixel_A111"),
        "a clashing name gains the serial tail"
    );
    assert_eq!(
        second.root.file_name().and_then(|n| n.to_str()),
        Some("Pixel_B222")
    );

    // Resolution is deterministic: asking again gives the same folder, so a
    // restart does not move the whole backup.
    assert_eq!(
        destination::resolve(&row(first_id), &all, Some(parent.path()))
            .expect("resolve")
            .root,
        first.root
    );
    assert_eq!(
        destination::resolve(&row(second_id), &all, Some(parent.path()))
            .expect("resolve")
            .root,
        second.root
    );

    // A name of its own gets the plain folder, and a name that sanitizes to
    // nothing still gets a usable one.
    seed("CCC333", "Pixel 8");
    let all = devices::list(&db).expect("devices");
    let third = all
        .iter()
        .find(|row| row.serial == "CCC333")
        .cloned()
        .expect("row");
    assert_eq!(
        destination::resolve(&third, &all, Some(parent.path()))
            .expect("resolve")
            .root
            .file_name()
            .and_then(|n| n.to_str()),
        Some("Pixel 8")
    );

    devices::rename(&db, third.id, "///").expect("rename");
    let all = devices::list(&db).expect("devices");
    let renamed = all
        .iter()
        .find(|row| row.id == third.id)
        .cloned()
        .expect("row");
    assert_eq!(
        destination::resolve(&renamed, &all, Some(parent.path()))
            .expect("resolve")
            .root
            .file_name()
            .and_then(|n| n.to_str()),
        Some("device")
    );
}

#[test]
fn the_same_file_name_on_two_phones_stays_two_backups() {
    let remote_a = MockRemote::new().with_file(&format!("{ROOT}/DCIM/photo.jpg"), b"A photo");
    let harness_a = Harness::new(remote_a);

    // The second phone has the same source path (and, in the folder-collision
    // test above, the same display name would give it its own folder too).
    let remote_b = MockRemote::new().with_file(&format!("{ROOT}/DCIM/photo.jpg"), b"B photo");
    let harness_b = harness_a.with_device("BBB222", "Second Phone", remote_b);

    let (first, _) = harness_a.run();
    assert!(first.succeeded(), "{first:?}");
    let (second, _) = harness_b.run();
    assert!(second.succeeded(), "{second:?}");

    // The same relative path under two different device folders, with the right
    // bytes in each: no phone's backup was written over the other's.
    assert_eq!(read(&harness_a.dest("DCIM/photo.jpg")), b"A photo");
    assert_eq!(read(&harness_b.dest("DCIM/photo.jpg")), b"B photo");
    assert_ne!(harness_a.root, harness_b.root);

    // And two independent manifest rows, one per device.
    let rows_a = harness_a.manifest();
    let rows_b = harness_b.manifest();
    assert_eq!(rows_a.len(), 1);
    assert_eq!(rows_b.len(), 1);
    assert_eq!(rows_a[0].source_path, format!("{ROOT}/DCIM/photo.jpg"));
    assert_eq!(rows_b[0].source_path, rows_a[0].source_path);
    assert_eq!(rows_a[0].device_id, harness_a.device.id);
    assert_eq!(rows_b[0].device_id, harness_b.device.id);
    assert_ne!(rows_a[0].destination_path, rows_b[0].destination_path);
    assert_eq!(rows_a[0].status, status::VERIFIED);
    assert_eq!(rows_b[0].status, status::VERIFIED);

    // Re-running one phone neither re-copies nor disturbs the other.
    let (again, events) = harness_a.run();
    assert!(again.succeeded(), "{again:?}");
    assert_eq!(count_copied(&events), 0);
    assert_eq!(again.counters.skipped, 1);
    assert_eq!(read(&harness_b.dest("DCIM/photo.jpg")), b"B photo");
}

#[test]
fn two_names_that_sanitize_alike_are_both_backed_up() {
    // `a:b.jpg` and `a?b.jpg` are distinct files on the phone and would both be
    // written as `a_b.jpg` here, so one of them has to move aside.
    let colon = format!("{ROOT}/DCIM/a:b.jpg");
    let question = format!("{ROOT}/DCIM/a?b.jpg");
    let harness = Harness::new(
        MockRemote::new()
            .with_file(&format!("{ROOT}/DCIM/z.jpg"), b"unrelated")
            .with_file(&colon, b"colon file")
            .with_file(&question, b"question file"),
    );

    let (first, _events) = harness.run();
    assert!(first.succeeded(), "{first:?}");
    assert_eq!(first.counters.copied, 3, "all three files are copied");

    let rows = harness.manifest();
    assert_eq!(rows.len(), 3, "one row per source file");
    let destinations: Vec<&str> = rows
        .iter()
        .map(|row| row.destination_path.as_str())
        .collect();
    let mut unique = destinations.clone();
    unique.sort_unstable();
    unique.dedup();
    assert_eq!(
        unique.len(),
        destinations.len(),
        "no two files share a destination: {destinations:?}"
    );

    // Both sanitized files are on disk, byte for byte, under different names.
    let mut found = Vec::new();
    for row in &rows {
        let path = std::path::PathBuf::from(&row.destination_path);
        found.push(read(&path));
    }
    found.sort();
    assert_eq!(
        found,
        vec![
            b"colon file".to_vec(),
            b"question file".to_vec(),
            b"unrelated".to_vec()
        ]
    );

    // Determinism: the second run must map both files to the very same places,
    // otherwise "unchanged" would never be true and every run would re-copy.
    let (second, events) = harness.run();
    assert!(second.succeeded(), "{second:?}");
    assert_eq!(count_copied(&events), 0, "nothing is re-copied: {events:?}");
    assert_eq!(second.counters.skipped, 3);
    assert_eq!(harness.manifest().len(), 3, "and no rows were duplicated");
}

#[test]
fn a_case_only_clash_is_resolved_where_the_filesystem_cares() {
    // On Windows `IMG.JPG` and `img.jpg` are the same file; on Linux they are
    // two. Whichever platform this runs on, the outcome must be "both files are
    // backed up", not "one silently replaced the other".
    let upper = format!("{ROOT}/DCIM/IMG.jpg");
    let lower = format!("{ROOT}/DCIM/img.jpg");
    let harness = Harness::new(
        MockRemote::new()
            .with_file(&upper, b"upper")
            .with_file(&lower, b"lower"),
    );

    let (outcome, _) = harness.run();
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 2);

    let rows = harness.manifest();
    assert_eq!(rows.len(), 2);
    let bytes: Vec<Vec<u8>> = rows
        .iter()
        .map(|row| read(std::path::Path::new(&row.destination_path)))
        .collect();
    assert!(
        bytes.contains(&b"upper".to_vec()) && bytes.contains(&b"lower".to_vec()),
        "both files survived: {bytes:?}"
    );

    let (again, events) = harness.run();
    assert!(again.succeeded(), "{again:?}");
    assert_eq!(count_copied(&events), 0);
    assert_eq!(again.counters.skipped, 2);
}

#[test]
fn files_with_the_same_name_in_different_folders_are_all_backed_up() {
    // `DCIM/Camera/IMG_1.jpg` and `DCIM/Screenshots/IMG_1.jpg` are the same name
    // twice; the destination mirrors the folder structure, so both survive.
    let remote = MockRemote::new()
        .with_file(&format!("{ROOT}/DCIM/Camera/IMG_1.jpg"), b"camera")
        .with_file(&format!("{ROOT}/DCIM/Screenshots/IMG_1.jpg"), b"screenshot");
    let harness = Harness::new(remote);

    let (outcome, events) = harness.run();
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(count_copied(&events), 2);
    assert_eq!(
        read(&harness.dest("DCIM/Camera/IMG_1.jpg")),
        b"camera",
        "the folder structure is preserved, so identical names cannot collide"
    );
    assert_eq!(
        read(&harness.dest("DCIM/Screenshots/IMG_1.jpg")),
        b"screenshot"
    );
    assert_eq!(harness.manifest().len(), 2);
}

#[test]
fn a_file_reachable_through_two_selections_is_copied_once() {
    // The user ticked a folder, one of its subfolders, and a subfolder again.
    // Redundant selections must not turn into two copies of the same file (the
    // second copy would look like a replaced file and land in history).
    let remote = MockRemote::new().with_file(
        &format!("{ROOT}/DCIM/Camera/IMG_1.jpg"),
        b"one photo, three ticks",
    );
    let harness = Harness::new(remote);

    let (outcome, events) = harness.run_with(harness.config_for(&[
        "DCIM",
        "DCIM/Camera",
        "DCIM/Camera",
        "DCIM/Screenshots",
    ]));
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(
        count_copied(&events),
        1,
        "copied once, not once per selection"
    );
    assert_eq!(
        common::archived(&events).len(),
        0,
        "and nothing was versioned over"
    );
    assert_eq!(
        outcome.counters.scanned, 1,
        "the file is scanned once as well"
    );
    assert_eq!(harness.manifest().len(), 1);
    assert_eq!(
        read(&harness.dest("DCIM/Camera/IMG_1.jpg")),
        b"one photo, three ticks"
    );

    // The same selections again: everything is up to date.
    let (again, events) =
        harness.run_with(harness.config_for(&["DCIM/Camera", "DCIM", "DCIM/Screenshots"]));
    assert!(again.succeeded(), "{again:?}");
    assert_eq!(count_copied(&events), 0);
    assert_eq!(again.counters.skipped, 1);
}
