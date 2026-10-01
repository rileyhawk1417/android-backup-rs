//! The backup job engine.
//!
//! One job = one device, one destination root, one immutable selection. The engine
//! is deliberately boring: enumerate → compare → stage → verify → record, with two
//! escape hatches a backup tool cannot do without:
//!
//! * **pause** — the device was unplugged, the user hit "pause", or the drive
//!   filled up. The job moves to `paused`, says why, and waits for a resume
//!   instead of failing the whole run or hammering a disconnected phone.
//! * **cancel** — stop promptly, discard the in-flight staging file, and record
//!   the job as `cancelled`. Everything already copied stays.
//!
//! Nothing here deletes a backup file or a manifest row (plan P1/P2/P10). A file
//! that failed is recorded and simply re-planned on the next run.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::clock;
use crate::database::files::{self, FileRecord};
use crate::database::jobs::{self, JobCounters, JobStatus};
use crate::database::Db;
use crate::paths::{map_all, MappedPath, RelPath};

use super::compare::{self, Decision, Plan, Reason};
use super::destination::{space_verdict, FsProbe, SpaceInfo, SpaceVerdict};
use super::enumerate::{EntryKind, RemoteEntry, RemoteFiles};
use super::selection::{self, Selection};
use super::transfer::{self, ArchiveRequest, VerifyMode};
use super::{recovery, BackupError};

/// How many times a single file is retried after the device comes back.
pub const MAX_TRANSIENT_RETRIES: u32 = 3;

/// Granularity at which the control signal is polled while pausing.
pub const PAUSE_POLL: Duration = Duration::from_millis(200);

/// Minimum interval between progress events.
pub const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);

/// How many per-file failures are kept for display.
const MAX_REPORTED_FAILURES: usize = 50;

/// Why a job is not running right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseReason {
    UserRequested,
    AuthorizationNeeded,
    DeviceDisconnected,
    LowSpace,
}

impl PauseReason {
    pub fn message(&self) -> &'static str {
        match self {
            Self::UserRequested => "Paused. Press Resume to continue.",
            Self::AuthorizationNeeded => {
                "The phone is not authorized. Unlock it and approve the USB debugging prompt, then press Resume."
            }
            Self::DeviceDisconnected => {
                "The phone disconnected. Reconnect it and press Resume to continue where the backup stopped."
            }
            Self::LowSpace => {
                "The backup drive is nearly full. Free some space, then press Resume."
            }
        }
    }

    /// True when the engine paused on its own rather than because the user asked.
    pub fn is_automatic(&self) -> bool {
        !matches!(self, Self::UserRequested)
    }
}

/// Progress snapshot for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Progress {
    /// Files already copied or skipped.
    pub files_done: u64,
    /// Files that belong to this job (planned copies + skips).
    pub files_total: u64,
    pub bytes_done: u64,
    pub bytes_total: u64,
    pub files_skipped: u64,
    pub files_failed: u64,
    pub current_file: Option<String>,
    pub bytes_per_sec: Option<u64>,
    pub eta_secs: Option<u64>,
}

impl Progress {
    /// Fraction of the *copy* work done, in `0.0..=1.0`.
    ///
    /// Failures count as processed so a job with unreadable files still reaches
    /// 100% instead of hanging at 99%.
    pub fn fraction(&self) -> f32 {
        if self.files_total == 0 {
            return 1.0;
        }
        ((self.files_done + self.files_failed) as f32 / self.files_total as f32).clamp(0.0, 1.0)
    }
}

/// Everything the UI needs to know.
#[derive(Debug, Clone, PartialEq)]
pub enum JobEvent {
    /// The job moved to a new status.
    StateChanged(JobStatus),
    /// Enumeration finished; carries the plan summary.
    Discovered {
        folders: usize,
        files: u64,
        bytes: u64,
        to_copy: u64,
        to_copy_bytes: u64,
        skipped: u64,
    },
    /// One folder could not be read; the job continues with the others.
    FolderWarning { folder: String, message: String },
    /// A file was copied and verified.
    FileCopied {
        source: String,
        destination: String,
        bytes: u64,
        reason: Reason,
        /// Set when a previous version was moved into the history tree.
        archived: Option<PathBuf>,
    },
    /// A file could not be copied; it is recorded and retried next run.
    FileFailed { source: String, message: String },
    /// The job is waiting.
    Paused {
        reason: PauseReason,
        file: Option<String>,
    },
    /// The job is running again.
    Resumed,
    /// Throttled progress update.
    Progress(Progress),
    /// The job cannot continue (destination gone, no storage root, …).
    Fatal { message: String },
    /// Terminal event, emitted exactly once.
    Finished(Box<JobOutcome>),
}

/// Final state of a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobOutcome {
    pub job_id: i64,
    pub device_id: i64,
    pub status: JobStatus,
    pub counters: JobCounters,
    pub error_summary: Option<String>,
    pub duration: Duration,
    /// Files the user should look at, capped to keep the UI honest.
    pub failures: Vec<(String, String)>,
}

impl JobOutcome {
    pub fn succeeded(&self) -> bool {
        self.status == JobStatus::Completed
    }
}

/// Receives job events. Implementations must not block.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: JobEvent);
}

/// Forwards events to a channel (the desktop app).
pub struct ChannelSink {
    sender: std::sync::mpsc::Sender<JobEvent>,
}

impl ChannelSink {
    pub fn new(sender: std::sync::mpsc::Sender<JobEvent>) -> Self {
        Self { sender }
    }
}

impl EventSink for ChannelSink {
    fn emit(&self, event: JobEvent) {
        // A closed channel means the UI is gone; the job must still finish.
        if self.sender.send(event).is_err() {
            tracing::debug!("job event dropped: no receiver");
        }
    }
}

/// Keeps every event for tests and for the CLI's summary.
#[derive(Default)]
pub struct CollectingSink {
    events: Mutex<Vec<JobEvent>>,
}

impl CollectingSink {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<JobEvent> {
        match self.events.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    pub fn outcomes(&self) -> Vec<JobOutcome> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                JobEvent::Finished(outcome) => Some(*outcome),
                _ => None,
            })
            .collect()
    }

    pub fn count_of(&self, predicate: impl Fn(&JobEvent) -> bool) -> usize {
        self.events().iter().filter(|e| predicate(e)).count()
    }
}

impl EventSink for CollectingSink {
    fn emit(&self, event: JobEvent) {
        match self.events.lock() {
            Ok(mut guard) => guard.push(event),
            Err(poisoned) => poisoned.into_inner().push(event),
        }
    }
}

/// Discards events.
pub struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: JobEvent) {}
}

/// What the engine should do at a moment in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlSignal {
    Running,
    Paused,
    Cancelled,
}

/// Lets the UI (or a test) drive a running job.
pub trait ControlSource: Send + Sync {
    fn signal(&self) -> ControlSignal;

    /// Block until the job may run again, the timeout elapses, or it is cancelled.
    fn wait_for_running(&self, timeout: Duration) -> ControlSignal;
}

/// Shared pause/cancel flags.
#[derive(Debug, Default)]
pub struct RunControl {
    paused: AtomicBool,
    cancelled: AtomicBool,
}

impl RunControl {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn pause(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    pub fn resume(&self) {
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::SeqCst);
        self.paused.store(false, Ordering::SeqCst);
    }

    pub fn reset(&self) {
        self.paused.store(false, Ordering::SeqCst);
        self.cancelled.store(false, Ordering::SeqCst);
    }

    pub fn is_paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

impl ControlSource for RunControl {
    fn signal(&self) -> ControlSignal {
        if self.is_cancelled() {
            ControlSignal::Cancelled
        } else if self.is_paused() {
            ControlSignal::Paused
        } else {
            ControlSignal::Running
        }
    }

    fn wait_for_running(&self, timeout: Duration) -> ControlSignal {
        let deadline = Instant::now() + timeout;
        loop {
            match self.signal() {
                ControlSignal::Running => return ControlSignal::Running,
                ControlSignal::Cancelled => return ControlSignal::Cancelled,
                ControlSignal::Paused => {
                    if Instant::now() >= deadline {
                        return ControlSignal::Paused;
                    }
                    std::thread::sleep(PAUSE_POLL.min(timeout));
                }
            }
        }
    }
}

/// A control source that never pauses — used by non-interactive runs.
pub struct Uncontrolled;

impl ControlSource for Uncontrolled {
    fn signal(&self) -> ControlSignal {
        ControlSignal::Running
    }

    fn wait_for_running(&self, _timeout: Duration) -> ControlSignal {
        ControlSignal::Running
    }
}

/// Everything a job needs to know that is not the engine itself.
///
/// Every field is already resolved by the caller (the controller re-validates the
/// destination, because the UI cannot be trusted with a stale path).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobConfig {
    pub device_id: i64,
    pub serial: String,
    pub display_name: String,
    /// Resolved destination root (device subfolder already applied).
    pub root: PathBuf,
    /// Configured folders, relative to the storage root or absolute.
    pub folders: Vec<String>,
    /// Extra folders the user added.
    pub custom_folders: Vec<String>,
    /// Back up the whole storage root.
    pub whole_storage: bool,
    pub verify: VerifyMode,
    /// Storage root to use, if already detected.
    pub storage_root: Option<String>,
    /// Bytes that must remain free (safety margin for the OS).
    pub free_space_floor: u64,
}

impl JobConfig {
    /// Bytes that must be free before starting: everything we plan to copy plus the
    /// safety margin.
    pub fn space_requirement(&self, copy_bytes: u64) -> u64 {
        copy_bytes.saturating_add(self.free_space_floor)
    }
}

/// The engine. Cheap to construct; holds borrows so it cannot outlive its inputs.
pub struct Engine<'a> {
    pub db: &'a Db,
    pub remote: &'a dyn RemoteFiles,
    pub probe: &'a dyn FsProbe,
}

/// Bookkeeping while a job runs.
struct RunState {
    job_id: i64,
    device_id: i64,
    /// Unix seconds at job start; used for history folder names.
    stamp: String,
    counters: JobCounters,
    failures: Vec<(String, String)>,
    cancelled: bool,
    fatal: Option<String>,
}

impl RunState {
    fn record_failure(&mut self, source: &str, message: String) {
        self.counters.failed += 1;
        if self.failures.len() < MAX_REPORTED_FAILURES {
            self.failures.push((source.to_string(), message));
        }
    }
}

impl<'a> Engine<'a> {
    pub fn new(db: &'a Db, remote: &'a dyn RemoteFiles, probe: &'a dyn FsProbe) -> Self {
        Self { db, remote, probe }
    }

    /// Run one job to completion (or to a pause the user never resolves).
    ///
    /// This creates the job row. Callers that need the row *before* the work
    /// starts (the controller, so it can hand out a job id) use
    /// [`Engine::run_recorded`].
    ///
    /// `Err` is reserved for failures that prevent a job record from existing at
    /// all. Everything after that is reported through the outcome and the events.
    pub fn run(
        &self,
        config: &JobConfig,
        sink: &dyn EventSink,
        control: &dyn ControlSource,
    ) -> Result<JobOutcome, BackupError> {
        let job_id = jobs::insert_job(self.db, config.device_id, &clock::now_iso())?;
        self.run_recorded(job_id, config, sink, control)
    }

    /// Run against a job row that already exists.
    ///
    /// Used for a *resumed* job: the row is still `paused`, its partial results are
    /// already in the manifest, and the fresh scan simply classifies everything
    /// again (plan E05.8). Nothing is copied twice and nothing is deleted.
    pub fn run_recorded(
        &self,
        job_id: i64,
        config: &JobConfig,
        sink: &dyn EventSink,
        control: &dyn ControlSource,
    ) -> Result<JobOutcome, BackupError> {
        let started = Instant::now();
        let mut state = RunState {
            job_id,
            device_id: config.device_id,
            stamp: clock::now_unix().to_string(),
            counters: JobCounters::default(),
            failures: Vec::new(),
            cancelled: false,
            fatal: None,
        };

        // `drive` returns `Ok` for every recoverable stop (it emits `Fatal` itself
        // first); an `Err` here is an infrastructure failure the sink has not been
        // told about yet, so tell it before finalising.
        if let Err(err) = self.drive(config, &mut state, sink, control) {
            tracing::error!(job_id, error = %err, "backup job aborted");
            let message = err.to_string();
            sink.emit(JobEvent::Fatal {
                message: message.clone(),
            });
            state.fatal = Some(message);
        }

        // Finalizing: the manifest is the record of what actually happened, so it
        // wins over the counters this loop kept in memory (plan E05.6.5).
        state.counters = self.reconcile_counters(job_id, state.counters);

        let status = if state.cancelled {
            JobStatus::Cancelled
        } else if state.fatal.is_some() || state.counters.failed > 0 {
            JobStatus::Failed
        } else {
            JobStatus::Completed
        };

        let error_summary = match (&state.fatal, state.counters.failed) {
            (Some(fatal), _) => Some(fatal.clone()),
            (None, 0) => None,
            (None, n) => Some(format!(
                "{n} file(s) could not be copied; they will be retried next time"
            )),
        };

        if let Err(err) = jobs::finalize(
            self.db,
            job_id,
            status,
            &state.counters,
            error_summary.as_deref(),
            &clock::now_iso(),
        ) {
            tracing::error!(job_id, error = %err, "could not finalise job record");
        }

        let outcome = JobOutcome {
            job_id,
            device_id: config.device_id,
            status,
            counters: state.counters,
            error_summary,
            duration: started.elapsed(),
            failures: state.failures.clone(),
        };
        sink.emit(JobEvent::Finished(Box::new(outcome.clone())));
        Ok(outcome)
    }

    /// The body of a job. `Err` here is an infrastructure failure.
    fn drive(
        &self,
        config: &JobConfig,
        state: &mut RunState,
        sink: &dyn EventSink,
        control: &dyn ControlSource,
    ) -> Result<(), BackupError> {
        let job_id = state.job_id;

        // --- planning ---------------------------------------------------------
        // Clean up after a previous crash before we look at the destination, so a
        // stale partial can never be mistaken for a finished file.
        let recovered = recovery::recover_root(&config.root);
        if !recovered.is_empty() {
            tracing::info!(?recovered, "cleaned up after an interrupted run");
        }

        if let Err(err) = super::destination::validate(self.probe, &config.root, None) {
            let message = err.to_string();
            state.fatal = Some(message.clone());
            sink.emit(JobEvent::Fatal { message });
            return Ok(());
        }

        // --- scanning ---------------------------------------------------------
        jobs::transition(self.db, job_id, JobStatus::Scanning)?;
        sink.emit(JobEvent::StateChanged(JobStatus::Scanning));

        let storage_root = self.resolve_storage_root(config)?;
        let selection = self.build_selection(config, &storage_root)?;
        let folders = selection.enumerable_folders();
        let entries = self.enumerate(&config.serial, &selection, &folders, sink);

        let manifest = files::manifest_for_device(self.db, config.device_id)?;
        let mut plan = compare::plan(entries, &manifest);

        // Destination mapping (with collision disambiguation) stays index-aligned
        // with the plan.
        let mappings = map_all(
            &config.root,
            plan.files
                .iter()
                .map(|file| (file.remote.path.clone(), file.rel.clone())),
            cfg!(windows),
        );

        // A file the manifest calls "unchanged" is only skipped when the local
        // copy is really there with the right size; anything else is re-fetched.
        for index in 0..plan.files.len() {
            if plan.files[index].decision != Decision::Skip {
                continue;
            }
            let Some(mapped) = mappings.get(index) else {
                continue;
            };
            let destination = mapped.destination.clone();
            let expected_size = plan.files[index].remote.size;
            let local_size = self.probe.local_size(&destination);
            if local_size != Some(expected_size) {
                let reason = match local_size {
                    None => Reason::LocalCopyMissing,
                    Some(_) => Reason::LocalSizeChanged,
                };
                tracing::debug!(
                    path = %destination.display(),
                    expected = expected_size,
                    found = ?local_size,
                    "{}",
                    reason.describe()
                );
                plan.force_copy(index, reason);
                continue;
            }
            // In hash mode the size is not enough: the bytes must match what was
            // recorded, or a subtly corrupted copy would be trusted forever.
            if config.verify != VerifyMode::Hash {
                continue;
            }
            let Some(known) = plan.files[index].known_hash.clone() else {
                continue;
            };
            match transfer::hash_file(&destination) {
                Ok(actual) if actual != known => {
                    plan.force_copy(index, Reason::LocalContentChanged)
                }
                Ok(_) => {}
                Err(err) => tracing::debug!(
                    path = %destination.display(),
                    error = %err,
                    "could not hash a local copy; leaving it as-is"
                ),
            }
        }

        state.counters.scanned = plan.stats.total_files;
        state.counters.skipped = plan.stats.to_skip;
        jobs::set_scanned(self.db, job_id, plan.stats.total_files, plan.stats.to_skip)?;
        sink.emit(JobEvent::Discovered {
            folders: folders.len(),
            files: plan.stats.total_files,
            bytes: plan.stats.total_bytes,
            to_copy: plan.stats.to_copy,
            to_copy_bytes: plan.stats.copy_bytes,
            skipped: plan.stats.to_skip,
        });

        // --- free space -------------------------------------------------------
        let needed = config.space_requirement(plan.stats.copy_bytes);
        if let Err(err) = super::destination::validate(self.probe, &config.root, Some(needed)) {
            let message = err.to_string();
            state.fatal = Some(message.clone());
            sink.emit(JobEvent::Fatal { message });
            return Ok(());
        }

        // --- copying ----------------------------------------------------------
        jobs::transition(self.db, job_id, JobStatus::Copying)?;
        sink.emit(JobEvent::StateChanged(JobStatus::Copying));

        let started = Instant::now();
        let mut progress = Progress {
            files_total: plan.stats.to_copy,
            bytes_total: plan.stats.copy_bytes,
            files_skipped: plan.stats.to_skip,
            ..Default::default()
        };
        let mut last_emit = Instant::now();

        for index in 0..plan.files.len() {
            if !plan.files[index].will_copy() {
                continue;
            }
            let Some(mapped) = mappings.get(index).cloned() else {
                continue;
            };

            // Cancellation and user pause are honoured before starting a file, so
            // no partial is ever left behind by a cancel.
            match self.await_running(job_id, sink, control, PauseReason::UserRequested, None) {
                WaitOutcome::Running => {}
                WaitOutcome::Cancelled => {
                    state.cancelled = true;
                    break;
                }
            }

            // A filling drive must not silently truncate a file.
            if !self.check_running_space(config, &plan, state, sink, control) {
                break;
            }

            let remote = plan.files[index].remote.clone();
            if let Some(parent) = mapped.destination.parent() {
                if let Err(err) = self.probe.ensure_dir(parent) {
                    self.fail_file(state, sink, &remote, err.to_string(), Some(&mapped));
                    progress.files_failed = state.counters.failed;
                    continue;
                }
            }
            let staged = match transfer::begin(
                self.probe,
                &config.root,
                &plan.files[index].rel,
                &mapped.destination,
            ) {
                Ok(staged) => staged,
                Err(err) => {
                    self.fail_file(state, sink, &remote, err.to_string(), Some(&mapped));
                    progress.files_failed = state.counters.failed;
                    continue;
                }
            };

            match self.pull_with_recovery(config, state, sink, control, &remote.path, &staged) {
                PullOutcome::Ok => {}
                PullOutcome::Cancelled => {
                    // Nothing half-written is left behind.
                    transfer::discard(&staged);
                    state.cancelled = true;
                    break;
                }
                PullOutcome::Failed(message) => {
                    transfer::discard(&staged);
                    self.fail_file(state, sink, &remote, message, Some(&mapped));
                    progress.files_failed = state.counters.failed;
                    continue;
                }
            }

            let archive = ArchiveRequest {
                destination_root: config.root.clone(),
                rel: plan.files[index].rel.clone(),
                stamp: state.stamp.clone(),
                known_hash: plan.files[index].known_hash.clone(),
            };

            // Hash mode asks the phone for its own digest of the source file; the
            // staged copy must reproduce it byte for byte. Without this, "hash
            // verification" could only ever say "these are the bytes we wrote".
            let expected_hash = if matches!(config.verify, VerifyMode::Hash) {
                match self.remote.hash(&config.serial, &remote.path) {
                    Ok(hash) => hash,
                    Err(err) => {
                        transfer::discard(&staged);
                        self.fail_file(state, sink, &remote, err.to_string(), Some(&mapped));
                        progress.files_failed = state.counters.failed;
                        continue;
                    }
                }
            } else {
                None
            };

            match transfer::commit(
                &staged,
                remote.size,
                config.verify,
                expected_hash.as_deref(),
                Some(archive),
            ) {
                Ok(outcome) => {
                    let record = FileRecord {
                        source_path: remote.path.clone(),
                        destination_path: mapped.destination.to_string_lossy().to_string(),
                        source_size: Some(remote.size),
                        source_modified_at: remote.modified_at,
                        content_hash: outcome.content_hash.clone(),
                    };
                    files::record_verified_now(self.db, config.device_id, job_id, &record)?;
                    state.counters.copied += 1;
                    state.counters.bytes_copied += outcome.bytes;
                    sink.emit(JobEvent::FileCopied {
                        source: record.source_path,
                        destination: record.destination_path,
                        bytes: outcome.bytes,
                        reason: plan.files[index].reason,
                        archived: outcome.archived,
                    });
                }
                Err(err) => {
                    transfer::discard(&staged);
                    self.fail_file(state, sink, &remote, err.to_string(), Some(&mapped));
                }
            }

            progress.files_done = state.counters.copied;
            progress.bytes_done = state.counters.bytes_copied;
            progress.files_failed = state.counters.failed;
            progress.current_file = Some(remote.path.clone());
            let is_last = !plan.files[index + 1..].iter().any(|file| file.will_copy());
            if is_last || last_emit.elapsed() >= PROGRESS_INTERVAL {
                Self::update_rates(&mut progress, started);
                sink.emit(JobEvent::Progress(progress.clone()));
                last_emit = Instant::now();
            }
        }

        if !state.cancelled && state.fatal.is_none() {
            Self::update_rates(&mut progress, started);
            sink.emit(JobEvent::Progress(progress));
        }
        Ok(())
    }

    /// Compare the counters this run kept against the manifest rows it wrote.
    ///
    /// They can only disagree if a manifest write failed halfway (the log says
    /// so). Trusting the manifest keeps the history honest: a job never claims to
    /// have copied a file that is not recorded.
    fn reconcile_counters(&self, job_id: i64, counted: JobCounters) -> JobCounters {
        let rows = match files::files_for_job(self.db, job_id) {
            Ok(rows) => rows,
            Err(err) => {
                tracing::warn!(job_id, error = %err, "could not re-read the manifest to reconcile counters");
                return counted;
            }
        };
        let verified = rows.iter().filter(|row| row.is_verified()).count() as u64;
        let failed = (rows.len() as u64).saturating_sub(verified);
        if verified == counted.copied && failed == counted.failed {
            return counted;
        }
        tracing::warn!(
            job_id,
            counted_copied = counted.copied,
            counted_failed = counted.failed,
            manifest_verified = verified,
            manifest_failed = failed,
            "job counters disagreed with the manifest; using the manifest"
        );
        let bytes = rows
            .iter()
            .filter(|row| row.is_verified())
            .filter_map(|row| row.size())
            .sum();
        JobCounters {
            copied: verified,
            failed,
            bytes_copied: if counted.copied == verified {
                counted.bytes_copied
            } else {
                bytes
            },
            ..counted
        }
    }

    /// Detect (or accept) the storage root for this device.
    fn resolve_storage_root(&self, config: &JobConfig) -> Result<String, BackupError> {
        if let Some(root) = &config.storage_root {
            return Ok(selection::normalize_remote(root));
        }
        let roots = self.remote.storage_roots(&config.serial)?;
        selection::pick_storage_root(&roots).ok_or(BackupError::NoUsableStorage)
    }

    fn build_selection(
        &self,
        config: &JobConfig,
        storage_root: &str,
    ) -> Result<Selection, BackupError> {
        let selection = selection::build(
            storage_root,
            config.folders.iter().cloned(),
            &config.custom_folders,
            config.whole_storage,
        )?;
        let pruned = selection::remove_redundant(selection.folders.clone());
        Ok(Selection {
            storage_root: selection.storage_root,
            folders: pruned,
            whole_storage: selection.whole_storage,
        })
    }

    /// List every file in the selection, tolerating per-folder failures.
    ///
    /// A folder the phone refuses to read must never look like an empty folder:
    /// it becomes a warning the user can act on (plan E05.1.3).
    fn enumerate(
        &self,
        serial: &str,
        selection: &Selection,
        folders: &[String],
        sink: &dyn EventSink,
    ) -> Vec<(RemoteEntry, RelPath)> {
        let mut out = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for folder in folders {
            match self.remote.list_files(serial, folder) {
                Ok(entries) => {
                    for entry in entries {
                        if entry.kind != EntryKind::File {
                            continue;
                        }
                        let rel = match RelPath::from_absolute(&entry.path, &selection.storage_root)
                        {
                            Ok(rel) => rel,
                            Err(err) => {
                                tracing::debug!(path = %entry.path, error = %err, "skipping unmappable path");
                                continue;
                            }
                        };
                        if seen.insert(entry.path.clone()) {
                            out.push((entry, rel));
                        }
                    }
                }
                Err(err) => {
                    let message = err.to_string();
                    tracing::warn!(folder, error = %message, "folder could not be listed");
                    sink.emit(JobEvent::FolderWarning {
                        folder: folder.clone(),
                        message,
                    });
                }
            }
        }
        out
    }

    /// Wait until the control source says "run", recording the pauses and resumes.
    fn await_running(
        &self,
        job_id: i64,
        sink: &dyn EventSink,
        control: &dyn ControlSource,
        reason: PauseReason,
        file: Option<String>,
    ) -> WaitOutcome {
        if control.signal() == ControlSignal::Running {
            return WaitOutcome::Running;
        }
        if let Err(err) = jobs::transition(self.db, job_id, JobStatus::Paused) {
            tracing::debug!(job_id, error = %err, "could not record the paused state");
        }
        sink.emit(JobEvent::Paused {
            reason,
            file: file.clone(),
        });
        loop {
            match control.wait_for_running(PAUSE_POLL) {
                ControlSignal::Running => {
                    if let Err(err) = jobs::transition(self.db, job_id, JobStatus::Copying) {
                        tracing::debug!(job_id, error = %err, "could not record the resumed state");
                    }
                    sink.emit(JobEvent::Resumed);
                    return WaitOutcome::Running;
                }
                ControlSignal::Cancelled => return WaitOutcome::Cancelled,
                ControlSignal::Paused => continue,
            }
        }
    }

    /// Pull one file, pausing and retrying while the failure looks transient.
    ///
    /// `adb pull` restarts the whole file, so the retry is a whole-file retry and
    /// the staging file is simply overwritten (plan E05.3.1).
    fn pull_with_recovery(
        &self,
        config: &JobConfig,
        state: &mut RunState,
        sink: &dyn EventSink,
        control: &dyn ControlSource,
        remote_path: &str,
        staged: &transfer::StagedTransfer,
    ) -> PullOutcome {
        let mut attempt = 0u32;
        loop {
            match self
                .remote
                .pull(&config.serial, remote_path, &staged.staging_path)
            {
                Ok(()) => return PullOutcome::Ok,
                Err(err) if err.is_transient() && attempt < MAX_TRANSIENT_RETRIES => {
                    attempt += 1;
                    let reason = if err.needs_authorization() {
                        PauseReason::AuthorizationNeeded
                    } else {
                        PauseReason::DeviceDisconnected
                    };
                    tracing::warn!(
                        path = remote_path,
                        attempt,
                        error = %err,
                        "transfer interrupted; waiting for the device"
                    );
                    match self.await_running(
                        state.job_id,
                        sink,
                        control,
                        reason,
                        Some(remote_path.to_string()),
                    ) {
                        WaitOutcome::Running => continue,
                        WaitOutcome::Cancelled => return PullOutcome::Cancelled,
                    }
                }
                Err(err) => return PullOutcome::Failed(err.to_string()),
            }
        }
    }

    /// Check free space before a file, pausing (and then failing) when the disk is
    /// full. Returns `false` when the copy loop must stop.
    fn check_running_space(
        &self,
        config: &JobConfig,
        plan: &Plan,
        state: &mut RunState,
        sink: &dyn EventSink,
        control: &dyn ControlSource,
    ) -> bool {
        let remaining = plan
            .stats
            .copy_bytes
            .saturating_sub(state.counters.bytes_copied);
        let needed = config.space_requirement(remaining);
        if self.read_space(config, state, needed) {
            return true;
        }
        // Not enough room: pause and let the user free some, then re-check once.
        tracing::warn!("destination is running out of space; pausing");
        match self.await_running(state.job_id, sink, control, PauseReason::LowSpace, None) {
            WaitOutcome::Cancelled => {
                state.cancelled = true;
                return false;
            }
            WaitOutcome::Running => {}
        }
        if self.read_space(config, state, needed) {
            return true;
        }
        state.fatal = Some(format!(
            "not enough free space in {}: need {} more, and the drive is still full after resuming",
            config.root.display(),
            transfer::human_bytes(needed)
        ));
        // The caller stops; `run` turns a fatal into a `failed` job.
        false
    }

    /// `true` when there is room. A platform that cannot report space is trusted.
    fn read_space(&self, config: &JobConfig, state: &mut RunState, needed: u64) -> bool {
        let free = match self.probe.free_bytes(&config.root) {
            Ok(free) => free,
            Err(err) => {
                state.fatal = Some(err.to_string());
                return false;
            }
        };
        match space_verdict(
            SpaceInfo {
                free_bytes: free,
                writable: true,
            },
            needed,
        ) {
            SpaceVerdict::Ok | SpaceVerdict::Unknown => true,
            SpaceVerdict::Low { available, .. } => {
                tracing::warn!(available, needed, "low on space");
                false
            }
        }
    }

    /// Record a per-file failure in the manifest and tell the UI.
    fn fail_file(
        &self,
        state: &mut RunState,
        sink: &dyn EventSink,
        remote: &RemoteEntry,
        message: String,
        mapped: Option<&MappedPath>,
    ) {
        let record = FileRecord {
            source_path: remote.path.clone(),
            destination_path: mapped
                .map(|m| m.destination.to_string_lossy().to_string())
                .unwrap_or_default(),
            source_size: Some(remote.size),
            source_modified_at: remote.modified_at,
            content_hash: None,
        };
        if let Err(err) = files::record_failed(self.db, state.device_id, state.job_id, &record) {
            tracing::error!(path = %remote.path, error = %err, "could not record a failed file");
        }
        state.record_failure(&remote.path, message.clone());
        sink.emit(JobEvent::FileFailed {
            source: remote.path.clone(),
            message,
        });
    }

    fn update_rates(progress: &mut Progress, started: Instant) {
        let elapsed = started.elapsed().as_secs_f64();
        if elapsed <= 0.0 {
            return;
        }
        let rate = (progress.bytes_done as f64 / elapsed) as u64;
        progress.bytes_per_sec = Some(rate);
        let remaining = progress.bytes_total.saturating_sub(progress.bytes_done);
        progress.eta_secs = remaining.checked_div(rate);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WaitOutcome {
    Running,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PullOutcome {
    Ok,
    Cancelled,
    Failed(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fraction_is_safe_with_no_files() {
        let progress = Progress::default();
        assert_eq!(progress.fraction(), 1.0);
    }

    #[test]
    fn progress_fraction_counts_failures_as_processed() {
        let progress = Progress {
            files_done: 4,
            files_failed: 1,
            files_total: 10,
            ..Default::default()
        };
        assert!((progress.fraction() - 0.5).abs() < f32::EPSILON);
        let over = Progress {
            files_done: 20,
            files_total: 10,
            ..Default::default()
        };
        assert_eq!(over.fraction(), 1.0);
    }

    #[test]
    fn run_control_reports_its_state() {
        let control = RunControl::new();
        assert_eq!(control.signal(), ControlSignal::Running);
        control.pause();
        assert_eq!(control.signal(), ControlSignal::Paused);
        assert_eq!(
            control.wait_for_running(Duration::from_millis(50)),
            ControlSignal::Paused
        );
        control.resume();
        assert_eq!(
            control.wait_for_running(Duration::from_millis(50)),
            ControlSignal::Running
        );
        control.cancel();
        assert_eq!(control.signal(), ControlSignal::Cancelled);
        assert_eq!(
            control.wait_for_running(Duration::from_millis(50)),
            ControlSignal::Cancelled
        );
    }

    #[test]
    fn wait_for_running_wakes_up_when_the_flag_is_cleared() {
        let control = RunControl::new();
        control.pause();
        let handle = control.clone();
        let waiter = std::thread::spawn(move || handle.wait_for_running(Duration::from_secs(30)));
        std::thread::sleep(Duration::from_millis(120));
        control.resume();
        assert_eq!(waiter.join().unwrap(), ControlSignal::Running);
    }

    #[test]
    fn uncontrolled_never_pauses() {
        let control = Uncontrolled;
        assert_eq!(control.signal(), ControlSignal::Running);
        assert_eq!(
            control.wait_for_running(Duration::from_millis(1)),
            ControlSignal::Running
        );
    }

    #[test]
    fn collecting_sink_records_events() {
        let sink = CollectingSink::new();
        sink.emit(JobEvent::StateChanged(JobStatus::Scanning));
        sink.emit(JobEvent::Progress(Progress::default()));
        assert_eq!(sink.events().len(), 2);
        assert_eq!(sink.count_of(|e| matches!(e, JobEvent::Progress(_))), 1);
        assert!(sink.outcomes().is_empty());
    }

    #[test]
    fn space_requirement_includes_the_floor() {
        let config = JobConfig {
            device_id: 1,
            serial: "S".into(),
            display_name: "Phone".into(),
            root: PathBuf::from("/backups"),
            folders: vec![],
            custom_folders: vec![],
            whole_storage: false,
            verify: VerifyMode::Size,
            storage_root: None,
            free_space_floor: 1024,
        };
        assert_eq!(config.space_requirement(500), 1524);
        assert_eq!(config.space_requirement(u64::MAX), u64::MAX);
    }

    #[test]
    fn pause_reasons_are_descriptive() {
        for reason in [
            PauseReason::UserRequested,
            PauseReason::AuthorizationNeeded,
            PauseReason::DeviceDisconnected,
            PauseReason::LowSpace,
        ] {
            assert!(!reason.message().is_empty());
        }
        assert!(
            PauseReason::LowSpace.is_automatic(),
            "only the user's own pause is not automatic"
        );
        assert!(PauseReason::AuthorizationNeeded.is_automatic());
        assert!(!PauseReason::UserRequested.is_automatic());
    }
}
