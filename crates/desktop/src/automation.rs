//! The automatic-backup host: where the core's scheduling policy meets this
//! application (Wave 9, E09.2/E09.3/E09.5).
//!
//! The policy in [`android_backup_core::automation`] decides *whether* an attempt
//! may happen. It owns no I/O: everything it can touch is behind
//! [`Host`]/[`TickHost`]. This module is that implementation, and it is
//! deliberately the only place where "automatic" and "the app" meet:
//!
//! * **The gate is the same gate.** [`backup_gate::check`] decides, so USB
//!   debugging off, a cable that is not quite in, or a phone still showing the
//!   "Allow debugging?" prompt produces the *same words* whether automation or the
//!   Start button asked. A phone that cannot be backed up is explained, never
//!   forced.
//! * **The job is the same job.** A [`JobConfig`] comes from
//!   [`crate::jobconfig`], the same call the button goes through, and the row is
//!   written by the same controller. The only difference is the durable
//!   [`JobTrigger::Auto`] mark, which is what makes the resume rule (E09.2)
//!   honest: automation continues its own work and never a person's.
//! * **The UI is handed the handle, not bypassed.** A job started here is adopted
//!   by [`crate::app::App`] through [`UiMessage::AutomationStarted`], so the panel,
//!   the history and the tray show the automatic backup exactly as they show a
//!   manual one. The alternative — the driver holding the only handle — would leave
//!   the window unable to pause or cancel what it is displaying.
//!
//! # Threading
//!
//! [`AppHost`] is called from two threads: the automation clock's (ticks), and the
//! UI's (`Policy::on_event` as devices come and go). Both call `start`/`resume`, so
//! nothing here may touch the view. It holds an `Arc<Db>`, an `Arc<BackupController>`
//! and a channel — all of which are already shared with the UI thread — and the
//! rule the core documents is kept: **the policy lock is taken before anything this
//! module locks**, never after.

use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use android_backup_core::automation::driver::TickHost;
use android_backup_core::automation::policy::{AttemptError, Host, PausedJob};
use android_backup_core::automation::Schedule;
use android_backup_core::backup::controller::{BackupController, JobHandle, StartError};
use android_backup_core::backup::destination::FsProbe;
use android_backup_core::clock;
use android_backup_core::config::AppConfig;
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::jobs::{self, JobStatus, JobTrigger};
use android_backup_core::database::Db;
use android_backup_core::devices::backup_gate;
use android_backup_core::devices::watcher::DeviceSnapshotHandle;
use android_backup_core::devices::{DeviceInfo, DeviceSink, SharedSink};

use crate::app::UiMessage;
use crate::jobconfig;

/// The host the automation clock and the device events both drive.
pub(crate) struct AppHost {
    db: Arc<Db>,
    cfg: AppConfig,
    controller: Arc<BackupController>,
    probe: Arc<dyn FsProbe>,
    devices: DeviceSnapshotHandle,
    /// Where a job that automation started goes to be adopted by the window.
    adopt: Sender<UiMessage>,
}

impl AppHost {
    pub fn new(
        db: Arc<Db>,
        cfg: AppConfig,
        controller: Arc<BackupController>,
        probe: Arc<dyn FsProbe>,
        devices: DeviceSnapshotHandle,
        adopt: Sender<UiMessage>,
    ) -> Self {
        Self {
            db,
            cfg,
            controller,
            probe,
            devices,
            adopt,
        }
    }

    /// The row for a serial, if the watcher's sink has recorded it yet.
    fn row(&self, serial: &str) -> Option<DeviceRow> {
        match devices::get_by_serial(&self.db, serial) {
            Ok(row) => row,
            Err(err) => {
                tracing::warn!(serial, %err, "automation could not read a device row");
                None
            }
        }
    }
}

/// Turn "when did this last run" from the manifest's history into an [`Instant`].
///
/// The manifest stores wall-clock strings; the policy counts with a monotonic
/// clock. The conversion is the one place the two meet, and it is done by
/// difference rather than by construction so that a clock that moved since the
/// row was written shifts both ends together: a laptop that was an hour fast when
/// it wrote the row and is right now reads "an hour ago" either way.
///
/// A row written in the *future* (the clock was wrong the other way) is reported as
/// a time in the future, as the [`Host`] contract requires — the schedule reads
/// anything after `now` as "just now", and clamping here would hide the difference
/// between "just ran" and "cannot happen yet".
fn instant_from_iso(iso: &str, now: Instant) -> Option<Instant> {
    let then = clock::iso_to_unix(iso)?;
    let now_unix = clock::now_unix();
    let delta = now_unix.checked_sub(then)?;
    if delta >= 0 {
        let delta = u64::try_from(delta).ok()?;
        now.checked_sub(std::time::Duration::from_secs(delta))
    } else {
        let delta = delta.unsigned_abs();
        now.checked_add(std::time::Duration::from_secs(delta))
    }
}

/// How a failed start is reported to the policy.
///
/// The distinction the policy turns on is "can the user fix this?" — a missing
/// destination or a device with no folders chosen is fixable and must take effect
/// immediately, while a database that will not answer is not, and deserves the
/// cooldown that stops a retry loop.
fn classify(error: &StartError) -> AttemptError {
    match error {
        StartError::DeviceBusy { .. } => AttemptError::Busy,
        StartError::NoDestination
        | StartError::Destination(_)
        | StartError::NoStorage
        | StartError::Thread(_) => AttemptError::NotReady(error.to_string()),
        StartError::Database(_) => AttemptError::Failed(error.to_string()),
        // `StartError` is `#[non_exhaustive]`: anything added later is treated as
        // infrastructure, which is the answer that cannot turn into a tight loop.
        _ => AttemptError::Failed(error.to_string()),
    }
}

impl Host for AppHost {
    fn schedule(&self, serial: &str) -> Schedule {
        match self.row(serial) {
            Some(row) => row.auto_schedule,
            // An unknown device has opted into nothing. This is also the answer
            // while the watcher's sink is still catching up on a phone plugged in
            // a moment ago, which the *next* tick then reads properly.
            None => Schedule::Off,
        }
    }

    fn last_attempt(&self, serial: &str) -> Option<Instant> {
        let row = self.row(serial)?;
        let job = jobs::latest_job(&self.db, row.id).ok().flatten()?;
        instant_from_iso(&job.started_at, Instant::now())
    }

    fn paused_job(&self, serial: &str) -> Option<PausedJob> {
        let row = self.row(serial)?;
        let job = jobs::latest_job(&self.db, row.id).ok().flatten()?;
        (job.status == JobStatus::Paused).then_some(PausedJob {
            id: job.id,
            trigger: job.trigger,
        })
    }

    fn start(&self, device: &DeviceInfo) -> Result<(), AttemptError> {
        if let Err(err) = backup_gate::check(device) {
            return Err(AttemptError::NotReady(err.to_string()));
        }
        let Some(row) = self.row(&device.serial) else {
            return Err(AttemptError::NotReady(
                "this device is not in the manifest yet — waiting for the first poll".to_string(),
            ));
        };
        let config = jobconfig::for_device(&self.db, &self.cfg, &self.probe, &row)
            .map_err(AttemptError::NotReady)?;

        match self.controller.start_as(&row, config, JobTrigger::Auto) {
            Ok(handle) => {
                tracing::info!(
                    job_id = handle.id,
                    device = %row.serial,
                    "automatic backup started"
                );
                self.hand_over(row.id, row.serial.clone(), handle);
                Ok(())
            }
            Err(err) => Err(classify(&err)),
        }
    }

    fn resume(&self, device: &DeviceInfo, job_id: i64) -> Result<(), AttemptError> {
        if let Err(err) = backup_gate::check(device) {
            return Err(AttemptError::NotReady(err.to_string()));
        }
        match self.controller.resume(job_id) {
            Ok(()) => {
                tracing::info!(job_id, device = %device.serial, "automatic backup resumed");
                Ok(())
            }
            // A resume that cannot happen (the job finished under us, or this
            // process never owned it) costs one cooldown and then another look,
            // which is cheaper than a loop that retries every tick.
            Err(err) => Err(AttemptError::Failed(err.to_string())),
        }
    }
}

impl TickHost for AppHost {
    fn connected(&self) -> Arc<[Arc<DeviceInfo>]> {
        self.devices.devices()
    }
}

impl AppHost {
    /// Give the job to the window. A send failure means the window is gone and
    /// the process is shutting down; the job itself is unaffected, and its row in
    /// the manifest stays truthful.
    fn hand_over(&self, device_id: i64, serial: String, handle: JobHandle) {
        if self
            .adopt
            .send(UiMessage::AutomationStarted {
                device_id,
                serial,
                handle,
            })
            .is_err()
        {
            tracing::warn!("automation started a backup after the window closed");
        }
    }
}

/// Gives a newly discovered device the answer the first-run wizard collected
/// (E09.6).
///
/// The wizard asks once whether phones should be backed up as they are plugged in.
/// Answering "yes" writes a *default*, not a promise about devices already known: it
/// is applied when a device is first recorded, and never again. A phone whose
/// schedule the user later turns off keeps it off, because by then the row exists and
/// this sink only ever speaks about rows that do not.
///
/// The check-then-write is safe because the sink is the only thing that creates
/// device rows, and it does so on one thread (the watcher's).
pub struct OptInSink {
    inner: SharedSink,
    db: Arc<Db>,
    /// The answer as it stands, not as it stood when the watcher was spawned: the
    /// wizard can be answered while a watcher is already running (it is shown on the
    /// first launch, and adb may have been found before it), and the phone that
    /// appears a moment later should hear the answer the user just gave.
    default: std::sync::RwLock<Schedule>,
}

impl OptInSink {
    pub fn new(inner: SharedSink, db: Arc<Db>, default: Schedule) -> Self {
        Self {
            inner,
            db,
            default: std::sync::RwLock::new(default),
        }
    }

    /// Change the answer for devices discovered from now on.
    pub fn set_default(&self, schedule: Schedule) {
        // A poisoned lock still holds the last value written, which is exactly the
        // value to keep; a panic in the watcher thread is not worth a second one.
        *self
            .default
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = schedule;
    }

    fn default(&self) -> Schedule {
        *self
            .default
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl DeviceSink for OptInSink {
    fn upsert(&self, device: &DeviceInfo) {
        let default = self.default();
        // "Known" is read *before* the write: the write is what makes it known.
        let known = devices::get_by_serial(&self.db, &device.serial);
        self.inner.upsert(device);
        if default == Schedule::Off || !matches!(known, Ok(None)) {
            return;
        }
        match devices::get_by_serial(&self.db, &device.serial) {
            Ok(Some(row)) => match devices::set_auto_schedule(&self.db, row.id, default) {
                Ok(()) => tracing::info!(
                    device = %device.serial,
                    schedule = ?default,
                    "new device opts into automatic backups"
                ),
                Err(err) => tracing::warn!(
                    device = %device.serial,
                    %err,
                    "could not apply the default schedule to a new device"
                ),
            },
            // The inner sink refused the write (a manifest that will not answer);
            // there is nothing to record the intent against.
            Ok(None) => {}
            Err(err) => {
                tracing::warn!(device = %device.serial, %err, "could not read a new device row")
            }
        }
    }
}

/// Wraps a note sink so that automation news also wakes the window.
///
/// The clock runs on its own thread and the window draws on demand: without this,
/// a phone that appeared while the window was hidden would keep its news until the
/// next poll tick happened to arrive. Cheap, and it keeps the tray icon honest.
pub struct RepaintingNote {
    inner: android_backup_core::automation::driver::SharedNoteSink,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl RepaintingNote {
    pub fn new(
        inner: android_backup_core::automation::driver::SharedNoteSink,
        ctx: Arc<Mutex<Option<egui::Context>>>,
    ) -> Self {
        Self { inner, ctx }
    }
}

impl android_backup_core::automation::driver::NoteSink for RepaintingNote {
    fn note(&self, note: android_backup_core::automation::Note) {
        self.inner.note(note);
        if let Some(ctx) = self
            .ctx
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
        {
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use android_backup_core::adb::parse::RawState;
    use android_backup_core::adb::props::DeviceProps;
    use android_backup_core::devices::DeviceState;

    fn info(serial: &str) -> DeviceInfo {
        DeviceInfo {
            serial: serial.to_string(),
            state: DeviceState::Connected,
            raw_state: RawState::Device,
            listing_model: Some("Pixel 8".to_string()),
            listing_product: None,
            transport_id: None,
            props: DeviceProps {
                manufacturer: Some("Google".to_string()),
                model: Some("Pixel 8".to_string()),
                product: Some("shiba".to_string()),
                android_version: Some("15".to_string()),
            },
        }
    }

    /// The real database sink, so the row the opt-in writes is the row the engine
    /// will read.
    fn sink(db: &Arc<Db>, default: Schedule) -> OptInSink {
        OptInSink::new(Arc::clone(db) as SharedSink, Arc::clone(db), default)
    }

    #[test]
    fn the_answer_can_change_after_the_watcher_is_running() {
        // The wizard is answered on the first frame, which can be after the watcher
        // has already been spawned: the sink must not be frozen at its spawn value.
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let sink = sink(&db, Schedule::Off);

        sink.upsert(&info("SER1"));
        assert_eq!(
            devices::get_by_serial(&db, "SER1")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::Off,
            "no answer yet, so nothing is scheduled"
        );

        sink.set_default(Schedule::EveryHours(6));
        sink.upsert(&info("SER2"));
        assert_eq!(
            devices::get_by_serial(&db, "SER2")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::EveryHours(6)
        );

        // And unticking it again is an answer too.
        sink.set_default(Schedule::Off);
        sink.upsert(&info("SER3"));
        assert_eq!(
            devices::get_by_serial(&db, "SER3")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::Off
        );
    }

    #[test]
    fn a_new_device_inherits_the_wizards_answer_once() {
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let sink = sink(&db, Schedule::OnConnect);

        sink.upsert(&info("SER1"));
        let row = devices::get_by_serial(&db, "SER1").unwrap().unwrap();
        assert_eq!(row.auto_schedule, Schedule::OnConnect);

        // The user's own answer wins from then on: the default is a starting
        // point, not a rule that reasserts itself on every poll.
        devices::set_auto_schedule(&db, row.id, Schedule::Off).unwrap();
        sink.upsert(&info("SER1"));
        assert_eq!(
            devices::get_by_serial(&db, "SER1")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::Off
        );

        // Another phone, though, has not answered anything yet.
        sink.upsert(&info("SER2"));
        assert_eq!(
            devices::get_by_serial(&db, "SER2")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::OnConnect
        );
    }

    #[test]
    fn without_the_opt_in_new_devices_stay_off() {
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let sink = sink(&db, Schedule::Off);
        sink.upsert(&info("SER1"));
        assert_eq!(
            devices::get_by_serial(&db, "SER1")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::Off
        );
    }

    /// The interval form is a default too, and it is stored clamped.
    #[test]
    fn an_interval_default_is_stored_in_range() {
        let db = Arc::new(Db::open_in_memory().expect("in-memory db"));
        let sink = sink(&db, Schedule::EveryHours(1_000));
        sink.upsert(&info("SER1"));
        assert_eq!(
            devices::get_by_serial(&db, "SER1")
                .unwrap()
                .unwrap()
                .auto_schedule,
            Schedule::EveryHours(android_backup_core::automation::MAX_INTERVAL_HOURS)
        );
    }
}
