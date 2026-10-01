//! Job control: starting, watching, pausing and cancelling backups.
//!
//! This is the **only** surface a UI or a CLI may use to run a backup. It owns:
//!
//! * the registry of live jobs (one per device — two backups writing into the same
//!   phone folder at once is never what the user wants),
//! * the pause/resume/cancel signals, and
//! * the event streams, so the UI only ever renders what it is told.
//!
//! ```text
//! UI ── start() ──► controller ── spawns ──► Engine thread ──► JobEvent stream
//!  ▲                     │                                        │
//!  └── pause/resume/cancel ── RunControl ─────────────────────────┘
//! ```
//!
//! Jobs run on their own threads and report progress as events; nobody polls job
//! internals, and a UI that stops reading the stream only loses *display* updates,
//! never data (the channel is bounded by the sender's `send` returning `Err` when
//! the receiver is gone, which the engine tolerates).

use std::collections::HashMap;
use std::sync::mpsc::{channel, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock;
use crate::database::devices::DeviceRow;
use crate::database::{jobs, Db, DbError};

use super::destination::{self, FsProbe};
use super::enumerate::RemoteFiles;
use super::job::{ChannelSink, Engine, EventSink, JobConfig, JobEvent, JobOutcome, RunControl};
use super::recovery;

/// Identifier of a running job: the `backup_jobs.id` row it writes to.
pub type JobId = i64;

/// Where a job is, as far as the controller is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    /// Spawned, but the engine has not reported itself running yet.
    Starting,
    Running,
    Paused,
    /// Cancelled by the user; the thread is unwinding.
    Cancelling,
    Finished(jobs::JobStatus),
}

impl JobPhase {
    /// `true` while the job still occupies its device.
    pub fn is_live(&self) -> bool {
        matches!(
            self,
            Self::Starting | Self::Running | Self::Paused | Self::Cancelling
        )
    }

    pub fn is_paused(&self) -> bool {
        matches!(self, Self::Paused)
    }
}

/// Why a job could not be started.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StartError {
    #[error("a backup of this device is already running")]
    DeviceBusy { device_id: i64 },

    #[error("the backup location is not ready: {0}")]
    Destination(String),

    #[error("could not create the job record: {0}")]
    Database(#[from] DbError),

    #[error("no backup folder is configured for this device")]
    NoDestination,

    #[error("this device has no readable shared storage")]
    NoStorage,

    #[error("could not start the backup thread: {0}")]
    Thread(String),
}

/// Why a control command was refused.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ControlError {
    #[error("no backup with id {id} is running")]
    UnknownJob { id: JobId },

    #[error("backup {id} already finished")]
    AlreadyFinished { id: JobId },

    #[error("backup {id} is already paused")]
    AlreadyPaused { id: JobId },

    #[error("backup {id} is not paused")]
    NotPaused { id: JobId },
}

/// A handle to one running job.
///
/// Holds the event stream (so the receiver can never be dropped by accident while
/// the engine is still sending) and the control flag. Dropping the handle does
/// **not** stop the job — that is what [`BackupController::cancel`] is for.
pub struct JobHandle {
    pub id: JobId,
    pub device_id: i64,
    pub serial: String,
    control: Arc<RunControl>,
    events: Receiver<JobEvent>,
}

impl JobHandle {
    /// Block until the next event arrives; `None` once the job is over.
    pub fn recv(&self) -> Option<JobEvent> {
        self.events.recv().ok()
    }

    /// Next event, if any, without blocking.
    pub fn try_recv(&self) -> Option<JobEvent> {
        match self.events.try_recv() {
            Ok(event) => Some(event),
            Err(TryRecvError::Empty) | Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Every event that has arrived so far (never blocks). Used by the UI's frame
    /// loop and by tests.
    pub fn drain(&self) -> Vec<JobEvent> {
        let mut out = Vec::new();
        while let Some(event) = self.try_recv() {
            out.push(event);
        }
        out
    }

    /// Wait for the terminal event, or `None` after `timeout`.
    pub fn wait(&self, timeout: Duration) -> Option<JobOutcome> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return None;
            }
            match self.events.recv_timeout(remaining) {
                Ok(JobEvent::Finished(outcome)) => return Some(*outcome),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Drain events until the job finishes, without a timeout.
    pub fn wait_forever(&self) -> Option<JobOutcome> {
        loop {
            match self.events.recv() {
                Ok(JobEvent::Finished(outcome)) => return Some(*outcome),
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    }

    /// Pause after the file currently being copied (never mid-file).
    pub fn pause(&self) {
        self.control.pause();
    }

    pub fn resume(&self) {
        self.control.resume();
    }

    /// Stop as soon as possible; the in-flight staging file is discarded.
    pub fn cancel(&self) {
        self.control.cancel();
    }

    /// The raw control flag, for callers that drive the engine directly in tests.
    pub fn control(&self) -> Arc<RunControl> {
        Arc::clone(&self.control)
    }
}

impl std::fmt::Debug for JobHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JobHandle")
            .field("id", &self.id)
            .field("device_id", &self.device_id)
            .field("serial", &self.serial)
            .finish_non_exhaustive()
    }
}

/// What the controller remembers about a job it started.
struct Entry {
    device_id: i64,
    control: Arc<RunControl>,
    phase: Arc<Mutex<JobPhase>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// Shared state, cloned into every job thread so it can deregister itself.
struct Shared {
    db: Arc<Db>,
    remote: Arc<dyn RemoteFiles>,
    probe: Arc<dyn FsProbe>,
    jobs: Mutex<HashMap<JobId, Entry>>,
}

/// Starts and controls backup jobs.
///
/// Cheap to clone-share: hold it in an `Arc` (or in the UI's app state) and call
/// [`BackupController::start`] per device.
pub struct BackupController {
    shared: Arc<Shared>,
}

impl BackupController {
    pub fn new(db: Arc<Db>, remote: Arc<dyn RemoteFiles>, probe: Arc<dyn FsProbe>) -> Self {
        Self {
            shared: Arc::new(Shared {
                db,
                remote,
                probe,
                jobs: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Start a backup of `device`.
    ///
    /// The destination is re-validated here even though `config` already carries a
    /// resolved path: a UI can hand over a stale one (the drive was unplugged, the
    /// folder was deleted) and the engine must not find that out halfway through
    /// the first file.
    pub fn start(&self, device: &DeviceRow, config: JobConfig) -> Result<JobHandle, StartError> {
        self.prune_finished();

        {
            let jobs = self.shared.jobs.lock_or_recover();
            if jobs.values().any(|entry| {
                entry.device_id == device.id && entry.phase.lock_or_recover().is_live()
            }) {
                return Err(StartError::DeviceBusy {
                    device_id: device.id,
                });
            }
        }

        if config.root.as_os_str().is_empty() {
            return Err(StartError::NoDestination);
        }
        // A destination folder that is simply missing is created ("choose a folder
        // on a fresh drive"); one whose parent is missing means the drive is gone.
        if !self.shared.probe.exists(&config.root) {
            if let Err(err) = self.shared.probe.ensure_dir(&config.root) {
                return Err(StartError::Destination(err.to_string()));
            }
        }
        if let Err(err) = destination::validate(&*self.shared.probe, &config.root, None) {
            return Err(StartError::Destination(err.to_string()));
        }

        // The row is created here so the handle can carry a stable id, and so a
        // crashed run leaves a `planning` row that recovery will close.
        let job_id = jobs::insert_job(&self.shared.db, device.id, &clock::now_iso())?;

        let control = RunControl::new();
        let phase = Arc::new(Mutex::new(JobPhase::Starting));
        let (sender, events) = channel();

        let handle = JobHandle {
            id: job_id,
            device_id: device.id,
            serial: device.serial.clone(),
            control: Arc::clone(&control),
            events,
        };

        let shared = Arc::clone(&self.shared);
        let config_for_thread = config.clone();
        let phase_for_thread = Arc::clone(&phase);
        let control_for_thread = Arc::clone(&control);

        let thread = match std::thread::Builder::new()
            .name(format!("backup-job-{job_id}"))
            .spawn(move || {
                let sink = PhaseSink {
                    phase: phase_for_thread,
                    inner: ChannelSink::new(sender),
                };
                let engine = Engine::new(&shared.db, &*shared.remote, &*shared.probe);
                let result =
                    engine.run_recorded(job_id, &config_for_thread, &sink, &*control_for_thread);
                if let Err(err) = result {
                    tracing::error!(job_id, error = %err, "backup job could not run");
                }
                // The engine emits `Finished` itself; dropping the sink here closes
                // the channel, which is how the UI knows the thread is done.
            }) {
            Ok(thread) => thread,
            Err(err) => {
                // The row must not stay `planning`: recovery would close it much
                // later, and the UI would show a job that nobody is running.
                let summary = format!("could not start the backup thread: {err}");
                if let Err(finalize_err) = jobs::finalize(
                    &self.shared.db,
                    job_id,
                    jobs::JobStatus::Failed,
                    &jobs::JobCounters::default(),
                    Some(&summary),
                    &clock::now_iso(),
                ) {
                    tracing::error!(job_id, error = %finalize_err, "could not close the failed job");
                }
                return Err(StartError::Thread(err.to_string()));
            }
        };

        self.shared.jobs.lock_or_recover().insert(
            job_id,
            Entry {
                device_id: device.id,
                control,
                phase,
                thread: Some(thread),
            },
        );

        Ok(handle)
    }

    /// Start a job and wait for it, for callers with no UI (CLI, tests, cron).
    ///
    /// Returns the outcome; `None` (with an error) means the job never started.
    pub fn run_blocking(
        &self,
        device: &DeviceRow,
        config: JobConfig,
        timeout: Duration,
    ) -> Result<JobOutcome, Box<dyn std::error::Error + Send + Sync>> {
        let handle = self.start(device, config)?;
        match handle.wait(timeout) {
            Some(outcome) => Ok(outcome),
            None => Err(format!(
                "backup of {} did not finish within {:?}",
                device.display_name, timeout
            )
            .into()),
        }
    }

    /// Ask a running job to pause after the current file.
    pub fn pause(&self, id: JobId) -> Result<(), ControlError> {
        let jobs = self.shared.jobs.lock_or_recover();
        let entry = jobs.get(&id).ok_or(ControlError::UnknownJob { id })?;
        let mut phase = entry.phase.lock_or_recover();
        match *phase {
            JobPhase::Paused => return Err(ControlError::AlreadyPaused { id }),
            JobPhase::Finished(_) => return Err(ControlError::AlreadyFinished { id }),
            _ => {}
        }
        entry.control.pause();
        *phase = JobPhase::Paused;
        Ok(())
    }

    /// Let a paused job continue.
    ///
    /// A job that paused itself (the phone was unplugged) is resumable exactly like
    /// one the user paused: both simply wait for the flag to clear.
    pub fn resume(&self, id: JobId) -> Result<(), ControlError> {
        let jobs = self.shared.jobs.lock_or_recover();
        let entry = jobs.get(&id).ok_or(ControlError::UnknownJob { id })?;
        let mut phase = entry.phase.lock_or_recover();
        match *phase {
            JobPhase::Paused => {}
            JobPhase::Finished(_) => return Err(ControlError::AlreadyFinished { id }),
            _ => return Err(ControlError::NotPaused { id }),
        }
        entry.control.resume();
        *phase = JobPhase::Running;
        Ok(())
    }

    /// Stop a job. Files already copied stay copied; the partial one is discarded.
    pub fn cancel(&self, id: JobId) -> Result<(), ControlError> {
        let jobs = self.shared.jobs.lock_or_recover();
        let entry = jobs.get(&id).ok_or(ControlError::UnknownJob { id })?;
        let mut phase = entry.phase.lock_or_recover();
        if matches!(*phase, JobPhase::Finished(_)) {
            return Err(ControlError::AlreadyFinished { id });
        }
        entry.control.cancel();
        *phase = JobPhase::Cancelling;
        Ok(())
    }

    /// The phase of a job, finished jobs included until they are pruned.
    pub fn phase(&self, id: JobId) -> Option<JobPhase> {
        let jobs = self.shared.jobs.lock_or_recover();
        jobs.get(&id).map(|entry| *entry.phase.lock_or_recover())
    }

    /// Every job that still holds its device.
    pub fn active_jobs(&self) -> Vec<(JobId, i64)> {
        let jobs = self.shared.jobs.lock_or_recover();
        jobs.iter()
            .filter(|(_, entry)| entry.phase.lock_or_recover().is_live())
            .map(|(id, entry)| (*id, entry.device_id))
            .collect()
    }

    /// Is a backup of this device running (or paused) right now?
    pub fn is_busy(&self, device_id: i64) -> bool {
        self.active_jobs().iter().any(|(_, id)| *id == device_id)
    }

    /// Join a finished job's thread and forget it. Also sweeps other finished jobs.
    pub fn join(&self, id: JobId) -> Result<(), ControlError> {
        let entry = self.shared.jobs.lock_or_recover().remove(&id);
        let Some(mut entry) = entry else {
            return Err(ControlError::UnknownJob { id });
        };
        if let Some(thread) = entry.thread.take() {
            if thread.join().is_err() {
                tracing::warn!(id, "a backup thread panicked");
            }
        }
        Ok(())
    }

    /// Drop registry entries for jobs that have finished, joining their threads.
    pub fn prune_finished(&self) {
        let finished: Vec<JobId> = {
            let jobs = self.shared.jobs.lock_or_recover();
            jobs.iter()
                .filter(|(_, entry)| !entry.phase.lock_or_recover().is_live())
                .map(|(id, _)| *id)
                .collect()
        };
        for id in finished {
            let _ = self.join(id);
        }
    }

    /// Run startup recovery for one device: partials, unfinished jobs, stale rows.
    ///
    /// The UI calls this once per device when it connects, so a crash from last
    /// week cannot make the manifest claim a file is backed up when it is not.
    pub fn recover(
        &self,
        device_id: i64,
        root: &std::path::Path,
    ) -> Result<recovery::RecoveryReport, DbError> {
        recovery::recover_device(&self.shared.db, &*self.shared.probe, root, device_id)
    }

    /// A cloneable handle to the manifest, for screens that only read.
    pub fn db(&self) -> Arc<Db> {
        Arc::clone(&self.shared.db)
    }
}

/// Wraps the caller's sink so the controller always knows the job phase.
struct PhaseSink {
    phase: Arc<Mutex<JobPhase>>,
    inner: ChannelSink,
}

impl EventSink for PhaseSink {
    fn emit(&self, event: JobEvent) {
        {
            let mut phase = self.phase.lock_or_recover();
            match &event {
                JobEvent::Paused { .. } => *phase = JobPhase::Paused,
                JobEvent::Resumed | JobEvent::StateChanged(_) => {
                    if !matches!(*phase, JobPhase::Cancelling) {
                        *phase = JobPhase::Running;
                    }
                }
                JobEvent::Finished(outcome) => *phase = JobPhase::Finished(outcome.status),
                JobEvent::Discovered { .. }
                | JobEvent::FolderWarning { .. }
                | JobEvent::FileCopied { .. }
                | JobEvent::FileFailed { .. }
                | JobEvent::Progress(_)
                | JobEvent::Fatal { .. } => {}
            }
        }
        self.inner.emit(event);
    }
}

/// Small helper so a poisoned registry lock cannot take the whole app down: a
/// panic in one job must not stop the user from pausing another.
trait MutexExt<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T>;
}

impl<T> MutexExt<T> for Mutex<T> {
    fn lock_or_recover(&self) -> std::sync::MutexGuard<'_, T> {
        match self.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        }
    }
}
