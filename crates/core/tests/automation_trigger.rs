//! E09.5 — who started a job is durable, not a guess.
//!
//! The resume-on-reconnect rule (E09.2) turns on one question: was this job
//! started by a person or by automation? A person's job waits for that person; a
//! job automation started is automation's to continue. That distinction is only
//! worth anything if it survives the process, so it is a column — and this suite
//! pins down that both entry points write it, and that the plain
//! [`BackupController::start`] the Start button uses still means "a person".
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use android_backup_core::database::jobs::{self, JobTrigger};
use android_backup_core::mock::MockRemote;

use common::{collect_until_finished, Harness, TIMEOUT};

#[test]
fn the_start_button_records_a_manual_trigger() {
    let h = Harness::new(MockRemote::new());
    let handle = h.start();
    collect_until_finished(&handle, TIMEOUT);
    h.controller.prune_finished();

    assert_eq!(h.job_row(handle.id).trigger, JobTrigger::Manual);
}

#[test]
fn automation_records_its_own_trigger() {
    let h = Harness::new(MockRemote::new());
    let handle = h
        .controller
        .start_as(&h.device, h.config(), JobTrigger::Auto)
        .expect("automation may start a job");
    collect_until_finished(&handle, TIMEOUT);
    h.controller.prune_finished();

    assert_eq!(h.job_row(handle.id).trigger, JobTrigger::Auto);
}

/// The trigger is a property of the *start*, not of the engine: the same phone,
/// backed up twice for two different reasons, keeps both stories straight.
#[test]
fn triggers_are_per_job_not_per_device() {
    let h = Harness::new(MockRemote::new());

    let auto = h
        .controller
        .start_as(&h.device, h.config(), JobTrigger::Auto)
        .expect("automation may start a job");
    collect_until_finished(&auto, TIMEOUT);
    h.controller.prune_finished();

    let manual = h.start();
    collect_until_finished(&manual, TIMEOUT);
    h.controller.prune_finished();

    assert_eq!(h.job_row(auto.id).trigger, JobTrigger::Auto);
    assert_eq!(h.job_row(manual.id).trigger, JobTrigger::Manual);

    // And the history reads newest first, which is how the resume rule finds the
    // job it is allowed to continue.
    let latest = jobs::latest_job(&h.db, h.device.id)
        .expect("job query")
        .expect("a job exists");
    assert_eq!(latest.id, manual.id);
}
