//! E07.3.1 — the cable comes out in the middle of a copy.
//!
//! A physical unplug is what `adb` reports as `device offline` (or `not found`),
//! which the runner classifies as [`AdbError::DeviceOffline`] — transient, so the
//! engine pauses the job instead of burning the file. What this scenario pins
//! down is the contract around that pause:
//!
//! * the job stops **at a file boundary**, with the manifest telling the truth
//!   about what is verified and what is not;
//! * nothing half-written is left lying around after a cancel;
//! * a *new* job (the app was restarted, or the user pressed Start again) copies
//!   the remainder and does not re-copy what is already verified.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use android_backup_core::backup::job::{JobEvent, PauseReason};
use android_backup_core::backup::transfer;
use android_backup_core::database::files::status;
use android_backup_core::database::jobs::JobStatus;
use android_backup_core::mock::MockRemote;

use common::{count_copied, partials, read, wait_for, Harness, ROOT};

#[test]
fn an_interrupted_recopy_keeps_the_previous_version_verified() {
    let a = format!("{ROOT}/DCIM/a.jpg");
    let b = format!("{ROOT}/DCIM/b.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"a v1")
        .with_file(&b, b"b v1");
    let harness = Harness::new(remote);

    // A first, complete backup: both files are verified.
    let (first, _) = harness.run();
    assert!(first.succeeded(), "{first:?}");
    let verified_b = harness.rows_for(&b);
    assert_eq!(verified_b.len(), 1);
    assert_eq!(verified_b[0].status, status::VERIFIED);
    assert_eq!(read(&harness.dest("DCIM/b.jpg")), b"b v1");

    // `b` changes on the phone, and the cable comes out while it is being copied.
    harness.remote.replace(&b, b"b v2, quite a bit longer");
    harness.remote.set_transient_pull_failures(&b, 4);

    let handle = harness.start();
    let mut seen = Vec::new();
    let paused = wait_for(&handle, &mut seen, |event| {
        matches!(
            event,
            JobEvent::Paused {
                reason: PauseReason::DeviceDisconnected,
                ..
            }
        )
    });
    assert!(
        paused,
        "the job must pause when the phone goes away; saw {seen:?}"
    );

    // The pause names the file that was interrupted, and the job row agrees.
    assert_eq!(
        seen.iter().find_map(|event| match event {
            JobEvent::Paused { file, .. } => file.clone(),
            _ => None,
        }),
        Some(b.clone())
    );
    assert_eq!(harness.job_row(handle.id).status, JobStatus::Paused);

    // The truth about `b`: the *previous* verified copy is still on disk and the
    // manifest still vouches for exactly that version — the interrupted attempt
    // must not make a stale copy look fresh.
    assert_eq!(read(&harness.dest("DCIM/b.jpg")), b"b v1");
    assert_eq!(harness.rows_for(&b), verified_b);
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, JobEvent::FileFailed { .. })),
        "an interrupted file is not a failed file: {seen:?}"
    );

    // Whatever the interrupted attempt left in the staging tree, none of it sits
    // at the destination: staged bytes are only ever committed after verification.
    for (path, _) in common::tree(&transfer::staging_dir(&harness.root)) {
        assert_ne!(path, harness.dest("DCIM/b.jpg"));
    }

    // The user gives up on the cable and cancels: the partial goes away, the
    // verified copy stays.
    handle.cancel();
    let (outcome, _) = common::collect_until_finished(&handle, common::TIMEOUT);
    assert_eq!(outcome.status, JobStatus::Cancelled, "{outcome:?}");
    assert!(
        common::tree(&transfer::staging_dir(&harness.root)).is_empty(),
        "cancelling must sweep the staging tree"
    );
    assert_eq!(harness.job_row(handle.id).status, JobStatus::Cancelled);
    assert_eq!(read(&harness.dest("DCIM/b.jpg")), b"b v1");
    assert_eq!(harness.rows_for(&b), verified_b);
}

#[test]
fn the_next_job_copies_only_the_remainder() {
    let a = format!("{ROOT}/DCIM/a.jpg");
    let b = format!("{ROOT}/DCIM/b.jpg");
    let c = format!("{ROOT}/DCIM/c.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"file a content")
        .with_file(&b, b"file b content")
        .with_file(&c, b"file c content")
        // More than the engine's silent retry budget, so the job really pauses.
        .with_transient_pull_failures(&b, 4);
    let harness = Harness::new(remote);

    // First job: `b` fails once, the engine pauses, and the user cancels.
    let handle = harness.start();
    let mut seen = Vec::new();
    assert!(wait_for(&handle, &mut seen, |event| matches!(
        event,
        JobEvent::Paused { .. }
    )));
    handle.cancel();
    let (first, _) = common::collect_until_finished(&handle, common::TIMEOUT);
    assert!(!first.succeeded(), "{first:?}");

    // The cable is back (the scripted failure is temporary) and the user starts
    // a fresh job, as they would after reconnecting the phone.
    let before = std::fs::metadata(harness.dest("DCIM/a.jpg"))
        .expect("stat")
        .modified()
        .expect("mtime");
    let (second, events) = harness.run();

    assert!(second.succeeded(), "{second:?}");
    assert_eq!(count_copied(&events), 2, "b and c still have to be copied");
    assert_eq!(second.counters.skipped, 1, "a was already verified");
    assert_eq!(read(&harness.dest("DCIM/b.jpg")), b"file b content");
    assert_eq!(read(&harness.dest("DCIM/c.jpg")), b"file c content");
    assert_eq!(
        std::fs::metadata(harness.dest("DCIM/a.jpg"))
            .expect("stat")
            .modified()
            .expect("mtime"),
        before,
        "the already-verified file was not rewritten"
    );
    assert!(partials(&harness.root).is_empty());
    for source in [&a, &b, &c] {
        assert_eq!(
            harness.rows_for(source).len(),
            1,
            "exactly one manifest row per source file: {source}"
        );
    }
}

#[test]
fn an_unauthorized_phone_pauses_with_authorization_guidance() {
    // A different interruption with the same shape: the user revoked USB
    // debugging mid-copy. The pause must point at the phone, not at the disk.
    let a = format!("{ROOT}/DCIM/a.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"file a content")
        .with_unauthorized_pull_failures(&a, 99);
    let harness = Harness::new(remote);

    let handle = harness.start();
    let mut seen = Vec::new();
    assert!(
        wait_for(&handle, &mut seen, |event| matches!(
            event,
            JobEvent::Paused {
                reason: PauseReason::AuthorizationNeeded,
                ..
            }
        )),
        "saw {seen:?}"
    );
    assert!(PauseReason::AuthorizationNeeded
        .message()
        .contains("USB debugging"));
    assert_eq!(harness.job_row(handle.id).status, JobStatus::Paused);

    // Nothing is recorded as failed while the phone is unreachable, so the user
    // can approve the prompt and pick up where they left off.
    assert!(
        !seen
            .iter()
            .any(|event| matches!(event, JobEvent::FileFailed { .. })),
        "a reachable-but-unauthorized phone is not a failed file: {seen:?}"
    );
    assert!(
        harness.manifest().is_empty(),
        "nothing is written about a file that was never finished: {:?}",
        harness.manifest()
    );

    // The user approves the prompt, but the phone stays unreachable: the same
    // job pauses again rather than giving up on the file.
    seen.clear();
    handle.resume();
    assert!(
        wait_for(&handle, &mut seen, |event| matches!(
            event,
            JobEvent::Paused { .. }
        )),
        "saw {seen:?}"
    );
    assert!(seen.iter().any(|event| matches!(event, JobEvent::Resumed)));

    handle.cancel();
    let (outcome, _) = common::collect_until_finished(&handle, common::TIMEOUT);
    assert_eq!(outcome.status, JobStatus::Cancelled, "{outcome:?}");
    assert!(partials(&harness.root).is_empty());
}

#[test]
fn a_transient_failure_that_clears_itself_never_pauses_at_all() {
    // The engine retries a transient pull in place before it bothers the user:
    // a single blip must be invisible.
    let a = format!("{ROOT}/DCIM/a.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"file a content")
        .with_transient_pull_failures(&a, 1);
    let harness = Harness::new(remote);

    let handle = harness.start();
    let (outcome, events) = common::collect_until_finished(&handle, common::TIMEOUT);
    assert!(outcome.succeeded(), "{outcome:?}");
    assert_eq!(outcome.counters.copied, 1);
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, JobEvent::Paused { .. })),
        "one blip is retried silently: {events:?}"
    );
    assert_eq!(read(&harness.dest("DCIM/a.jpg")), b"file a content");
}
