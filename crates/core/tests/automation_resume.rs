//! E09.2 — the automatic path, end to end, against the real engine.
//!
//! The policy's own tests are pure: they prove *that* `resume` is called, using a
//! host that only writes down what it was asked to do. What they cannot prove is
//! the claim that makes automatic resume worth having: that the job which
//! continues is the *same* job, and that continuing it copies the remainder
//! rather than the whole phone again.
//!
//! So this test wires the real [`Policy`] to the real [`BackupController`] over a
//! real manifest and a scripted phone whose cable slips in the middle of a copy.
//! Plugging the phone back in must finish the interrupted job with two copied
//! files and one skipped — not three copied. The file that was already verified
//! must not even be rewritten, because the manifest is the reason the resume path
//! exists at all.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::{Duration, Instant};

use android_backup_core::automation::policy::{AttemptError, Host, Note, PausedJob, Policy};
use android_backup_core::automation::schedule::Schedule;
use android_backup_core::clock;
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::jobs::{self, JobStatus, JobTrigger};
use android_backup_core::database::Db;
use android_backup_core::devices::{DeviceEvent, DeviceInfo, DeviceState};
use android_backup_core::mock::MockRemote;

use common::{props, Harness, ROOT, TIMEOUT};

/// A host over the real manifest and the real controller — the smallest thing
/// that can answer the policy's five questions honestly.
struct TestHost<'a> {
    harness: &'a Harness,
    schedule: Schedule,
    /// The instant the policy's timeline is measured from. `last_attempt` has to
    /// answer in terms of some origin; being explicit about it here keeps the
    /// stored timestamp from looking like it is in the future.
    origin: Instant,
}

impl<'a> TestHost<'a> {
    fn new(harness: &'a Harness, schedule: Schedule) -> Self {
        Self {
            harness,
            schedule,
            origin: Instant::now(),
        }
    }

    fn row(&self, serial: &str) -> Option<DeviceRow> {
        devices::get_by_serial(&self.harness.db, serial)
            .expect("device query")
            .filter(|row| row.serial == serial)
    }

    fn latest(&self, serial: &str) -> Option<jobs::JobRow> {
        let row = self.row(serial)?;
        jobs::latest_job(&self.harness.db, row.id).expect("job query")
    }
}

impl Host for TestHost<'_> {
    fn schedule(&self, serial: &str) -> Schedule {
        // Only the enrolled phone has opted in; every other serial answers `Off`,
        // exactly as the app does for a phone nobody has configured.
        if serial == self.harness.device.serial {
            self.schedule
        } else {
            Schedule::Off
        }
    }

    fn last_attempt(&self, serial: &str) -> Option<Instant> {
        let job = self.latest(serial)?;
        let then = clock::iso_to_unix(&job.started_at)?;
        let delta = clock::now_unix().checked_sub(then)?.max(0);
        self.origin
            .checked_sub(Duration::from_secs(u64::try_from(delta).ok()?))
    }

    fn paused_job(&self, serial: &str) -> Option<PausedJob> {
        let job = self.latest(serial)?;
        (job.status == JobStatus::Paused).then_some(PausedJob {
            id: job.id,
            trigger: job.trigger,
        })
    }

    fn start(&self, device: &DeviceInfo) -> Result<(), AttemptError> {
        let row = self
            .row(&device.serial)
            .ok_or_else(|| AttemptError::NotReady("no such device".to_string()))?;
        // The gate the app applies before the button: this test is about the
        // pipeline, so it starts a device that is ready.
        self.harness
            .controller
            .start_as(&row, self.harness.config(), JobTrigger::Auto)
            .map(|_| ())
            .map_err(|err| AttemptError::NotReady(err.to_string()))
    }

    fn resume(&self, _device: &DeviceInfo, job_id: i64) -> Result<(), AttemptError> {
        self.harness
            .controller
            .resume(job_id)
            .map_err(|err| AttemptError::Failed(err.to_string()))
    }
}

fn connected(serial: &str) -> Arc<DeviceInfo> {
    Arc::new(DeviceInfo {
        serial: serial.to_string(),
        state: DeviceState::Connected,
        raw_state: android_backup_core::adb::parse::RawState::Device,
        listing_model: Some("SM-A536B".to_string()),
        listing_product: None,
        transport_id: None,
        props: props(),
    })
}

/// Poll the manifest until the job reaches `wanted`, the way the app's UI does
/// while a job runs on its own thread.
fn wait_for_status(db: &Db, job_id: i64, wanted: JobStatus) -> jobs::JobRow {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        let row = jobs::get_job(db, job_id)
            .expect("job query")
            .expect("job row");
        if row.status == wanted {
            return row;
        }
        assert!(
            Instant::now() < deadline,
            "job {job_id} stayed {:?}, never reached {wanted:?}",
            row.status
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_reconnected_phone_resumes_the_job_it_interrupted_and_copies_only_the_rest() {
    let a = format!("{ROOT}/DCIM/a.jpg");
    let b = format!("{ROOT}/DCIM/b.jpg");
    let c = format!("{ROOT}/DCIM/c.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"file a content")
        .with_file(&b, b"file b content")
        .with_file(&c, b"file c content")
        // More failures than the engine's silent retry budget, so the pull really
        // fails and the job pauses as interrupted rather than grinding on.
        .with_transient_pull_failures(&b, 4);
    let harness = Harness::new(remote);
    devices::set_auto_schedule(&harness.db, harness.device.id, Schedule::OnConnect)
        .expect("enrol the phone");

    let host = TestHost::new(&harness, Schedule::OnConnect);
    let mut policy = Policy::new();
    let info = connected(&harness.device.serial);

    // Plugging the phone in starts a backup nobody asked for. It is recorded as
    // automation's, which is what entitles automation to continue it later.
    let notes = policy.on_event(
        &host,
        &DeviceEvent::FirstSeen(Arc::clone(&info)),
        host.origin,
    );
    assert_eq!(
        notes,
        vec![Note::Started {
            serial: harness.device.serial.clone()
        }],
        "a device configured to back up on connection must start one"
    );

    let started = host.latest(&harness.device.serial).expect("a job row");
    assert_eq!(started.trigger, JobTrigger::Auto);

    // The cable slips while `b` is being copied: the job pauses at the boundary,
    // with `a` verified and nothing else claimed.
    let paused = wait_for_status(&harness.db, started.id, JobStatus::Paused);
    assert_eq!(paused.files_copied, 1, "`a` was copied before the slip");

    // Unplug, then plug back in: an ordinary reconnect, which is exactly when the
    // user expects the interrupted backup to pick itself up.
    policy.on_event(
        &host,
        &DeviceEvent::Disconnected {
            serial: harness.device.serial.clone(),
        },
        host.origin,
    );
    let notes = policy.on_event(
        &host,
        &DeviceEvent::FirstSeen(Arc::clone(&info)),
        host.origin + Duration::from_secs(5),
    );
    assert_eq!(
        notes,
        vec![Note::Resumed {
            serial: harness.device.serial.clone(),
            job_id: started.id,
        }],
        "the pause left a job that was automation's to finish"
    );

    // The same job finishes — no second row, and only the remainder crosses the
    // cable. Note what "the remainder" means inside one job's ledger: `a` was
    // copied by *this* job before the slip, so it is neither copied again nor
    // counted as skipped (`skipped` is for what earlier jobs left behind). The
    // evidence is the byte count and the untouched file, not that counter.
    let before = std::fs::metadata(harness.dest("DCIM/a.jpg"))
        .expect("stat")
        .modified()
        .expect("mtime");
    let finished = wait_for_status(&harness.db, started.id, JobStatus::Completed);
    assert_eq!(
        jobs::history(&harness.db, harness.device.id, 50)
            .expect("job list")
            .len(),
        1,
        "resuming continues the interrupted job rather than starting another"
    );
    assert_eq!(
        finished.files_copied, 3,
        "one file before the slip, two after — counted across the whole job"
    );
    assert_eq!(
        finished.bytes_copied - paused.bytes_copied,
        i64::try_from(b"file b content".len() + b"file c content".len()).expect("fits"),
        "only b and c were pulled after reconnecting"
    );
    assert_eq!(finished.files_skipped, 0, "nothing was skipped by this job");
    assert_eq!(finished.files_failed, 0);
    assert_eq!(
        std::fs::metadata(harness.dest("DCIM/a.jpg"))
            .expect("stat")
            .modified()
            .expect("mtime"),
        before,
        "the file that was already verified was not rewritten"
    );

    for source in [&a, &b, &c] {
        let rows = harness.rows_for(source);
        assert_eq!(rows.len(), 1, "exactly one manifest row for {source}");
        assert!(rows[0].is_verified(), "{source} must end up verified");
    }
    assert!(common::partials(&harness.root).is_empty());
}

#[test]
fn a_job_a_person_started_is_left_for_that_person_to_resume() {
    // The other half of the ownership rule: a manual job that pauses on unplug is
    // *not* automation's to continue, however due the schedule is.
    let a = format!("{ROOT}/DCIM/a.jpg");
    let remote = MockRemote::new()
        .with_file(&a, b"file a content")
        .with_unauthorized_pull_failures(&a, 99);
    let harness = Harness::new(remote);
    devices::set_auto_schedule(&harness.db, harness.device.id, Schedule::OnConnect)
        .expect("enrol the phone");

    let handle = harness.start();
    let mut seen = Vec::new();
    assert!(common::wait_for(&handle, &mut seen, |event| matches!(
        event,
        android_backup_core::backup::job::JobEvent::Paused { .. }
    )));

    let host = TestHost::new(&harness, Schedule::OnConnect);
    let mut policy = Policy::new();
    let info = connected(&harness.device.serial);
    let notes = policy.on_event(&host, &DeviceEvent::FirstSeen(info), host.origin);

    assert!(
        notes.is_empty(),
        "a person's paused job waits for the person: {notes:?}"
    );
    assert_eq!(
        jobs::get_job(&harness.db, handle.id)
            .expect("job query")
            .expect("job row")
            .status,
        JobStatus::Paused
    );
}
