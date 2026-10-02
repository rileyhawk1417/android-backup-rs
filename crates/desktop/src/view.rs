//! The view model: everything the window renders, and nothing else.
//!
//! This module is deliberately free of `egui`: it folds core events into state,
//! formats that state for display, and decides which controls are available. The
//! widget code in [`crate::ui`] then has no logic left to get wrong, and the rules
//! that matter (button gating, terminal-state strips, history summaries) are
//! testable without a window.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use android_backup_core::backup::controller::{JobPhase, StartError};
use android_backup_core::backup::{JobEvent, JobOutcome, PauseReason, Progress};
use android_backup_core::database::devices::DeviceRow;
use android_backup_core::database::jobs::{self, JobRow, JobStatus};
use android_backup_core::devices::backup_gate::{check, GateError};
use android_backup_core::devices::watcher::DeviceEvent;
use android_backup_core::devices::{DeviceInfo, DeviceState};

/// Failures kept in a [`JobView`] for the expandable error list.
pub const MAX_LISTED_FAILURES: usize = 50;
/// Warnings kept for the folder-warning list.
pub const MAX_LISTED_WARNINGS: usize = 20;
/// History rows fetched in one page.
pub const HISTORY_PAGE: u32 = 500;

// ---------------------------------------------------------------------------
// Formatting helpers (pure; the tests pin every one of them)
// ---------------------------------------------------------------------------

/// Human label for a device state, as shown in the list.
pub fn state_label(state: DeviceState) -> &'static str {
    match state {
        DeviceState::Connected => "Connected",
        DeviceState::Unauthorized => "Unauthorized — approve on phone",
        DeviceState::Offline => "Offline",
        DeviceState::Disconnected => "Disconnected",
        DeviceState::Error => "Error",
    }
}

/// Human label for a job status.
pub fn status_label(status: JobStatus) -> &'static str {
    match status {
        JobStatus::Planning => "Planning",
        JobStatus::Scanning => "Scanning",
        JobStatus::Copying => "Copying",
        JobStatus::Paused => "Paused",
        JobStatus::Cancelled => "Cancelled",
        JobStatus::Completed => "Completed",
        JobStatus::Failed => "Failed",
    }
}

/// Byte count in binary units, one decimal above KB.
pub fn format_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else if value >= MB {
        format!("{:.1} MB", value / MB)
    } else if value >= KB {
        format!("{:.1} KB", value / KB)
    } else {
        format!("{bytes} B")
    }
}

/// Transfer speed, or `None` when the engine has not measured one yet.
pub fn format_speed(bytes_per_sec: Option<u64>) -> Option<String> {
    bytes_per_sec
        .filter(|rate| *rate > 0)
        .map(|rate| format!("{}/s", format_bytes(rate)))
}

/// `mm:ss`, or `h:mm:ss` past an hour. `None` renders as `--:--`.
pub fn format_clock(secs: Option<u64>) -> String {
    let Some(secs) = secs else {
        return "--:--".to_string();
    };
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m:02}:{s:02}")
    }
}

/// `1.2 s`, `12 s`, `3m 05s` — how long something took.
pub fn format_duration(duration: Duration) -> String {
    let secs = duration.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

/// Shorten `text` in the middle so the tail (usually the interesting part of a
/// path) stays visible.
pub fn truncate_middle(text: &str, max: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    if chars.len() <= max || max < 7 {
        return text.to_string();
    }
    // Round the front *up* so the ellipsis lands just before the tail: the tail
    // is the part that identifies the file.
    let keep_front = (max - 3).div_ceil(2);
    let keep_back = max - 3 - keep_front;
    let front: String = chars[..keep_front].iter().collect();
    let back: String = chars[chars.len() - keep_back..].iter().collect();
    format!("{front}...{back}")
}

/// The one-line progress headline of the progress panel.
pub fn progress_headline(progress: &Progress) -> String {
    let mut line = format!(
        "files: {}/{} | {} copied",
        progress.files_done,
        progress.files_total,
        format_bytes(progress.bytes_done)
    );
    if progress.eta_secs.is_some() {
        line.push_str(&format!(" | ETA {}", format_clock(progress.eta_secs)));
    }
    if let Some(speed) = format_speed(progress.bytes_per_sec) {
        line.push_str(&format!(" | {speed}"));
    }
    if progress.files_failed > 0 {
        line.push_str(&format!(" | {} failed", progress.files_failed));
    }
    line
}

/// Status-bar summary of the most recent job, from the manifest (E02.3).
pub fn last_backup_summary(row: &JobRow) -> String {
    format!(
        "Last backup: {} — {} copied, {} unchanged, {} failed",
        status_label(row.status),
        row.files_copied,
        row.files_skipped,
        row.files_failed
    )
}

/// "no backups yet" is a different message from "the last one failed".
pub fn last_backup_line(row: Option<&JobRow>) -> String {
    match row {
        Some(row) => last_backup_summary(row),
        None => "Last backup: — never backed up".to_string(),
    }
}

/// Cells of one history row: started, duration, status, copied/unchanged/failed,
/// bytes. Kept as data so the table and the tests see the same strings.
pub fn history_cells(row: &JobRow) -> [String; 5] {
    let duration = match &row.completed_at {
        Some(end) => match (
            android_backup_core::clock::iso_to_unix(&row.started_at),
            android_backup_core::clock::iso_to_unix(end),
        ) {
            (Some(start), Some(finish)) => {
                format_duration(Duration::from_secs(finish.saturating_sub(start) as u64))
            }
            _ => "—".to_string(),
        },
        None => "—".to_string(),
    };
    [
        row.started_at.replace('T', " ").replace('Z', ""),
        duration,
        status_label(row.status).to_string(),
        format!(
            "{} / {} / {}",
            row.files_copied, row.files_skipped, row.files_failed
        ),
        format_bytes(row.bytes_copied.max(0) as u64),
    ]
}

// ---------------------------------------------------------------------------
// Devices
// ---------------------------------------------------------------------------

/// What the UI knows about one device.
#[derive(Debug, Clone, PartialEq)]
pub struct DeviceView {
    pub serial: String,
    pub state: DeviceState,
    /// Name shown in the list: the manifest's display name once the device is
    /// known, otherwise the best name `adb` can offer right now.
    pub name: String,
    pub model: Option<String>,
    pub android_version: Option<String>,
    /// The last event payload, kept so the gate can be re-evaluated.
    pub info: Option<Arc<DeviceInfo>>,
    /// The manifest row, once the database knows this serial.
    pub row: Option<DeviceRow>,
    /// Why this device cannot be backed up right now, if it cannot.
    pub gate: Option<GateError>,
}

impl DeviceView {
    fn new(serial: &str) -> Self {
        Self {
            serial: serial.to_string(),
            state: DeviceState::Disconnected,
            name: serial.to_string(),
            model: None,
            android_version: None,
            info: None,
            row: None,
            gate: None,
        }
    }

    /// Fold a fresh snapshot from the watcher.
    fn apply_info(&mut self, info: &Arc<DeviceInfo>, first_seen: bool) {
        let props = info.effective_props();
        self.serial = info.serial.clone();
        self.state = info.state;
        self.model = props.model.clone();
        self.android_version = props.android_version.clone();
        self.gate = check(info).err();
        // A user-chosen name (the manifest row) always wins over the automatic one.
        if self.row.is_none() {
            let automatic =
                android_backup_core::database::devices::default_display_name(&info.serial, &props);
            if first_seen || self.info.is_none() || self.name == self.serial {
                self.name = automatic;
            }
        }
        self.info = Some(Arc::clone(info));
    }

    /// Attach or refresh the manifest row.
    pub fn apply_row(&mut self, row: DeviceRow) {
        self.name = row.display_name.clone();
        self.model = row.model.clone().or_else(|| self.model.clone());
        self.android_version = row
            .android_version
            .clone()
            .or_else(|| self.android_version.clone());
        self.serial = row.serial.clone();
        self.row = Some(row);
    }

    pub fn state_label(&self) -> &'static str {
        state_label(self.state)
    }

    /// True when this device can start a backup right now.
    pub fn is_ready(&self) -> bool {
        self.gate.is_none() && self.state == DeviceState::Connected
    }

    /// Actionable help for a device that cannot be backed up.
    pub fn gate_message(&self) -> Option<String> {
        self.gate.as_ref().map(|err| err.to_string())
    }
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// What the plan turned out to be, once enumeration finished.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiscoveredSummary {
    pub folders: usize,
    pub files: u64,
    pub bytes: u64,
    pub to_copy: u64,
    pub to_copy_bytes: u64,
    pub skipped: u64,
}

/// Everything the progress panel and the summary strips render.
#[derive(Debug, Clone, PartialEq)]
pub struct JobView {
    pub id: i64,
    pub device_id: i64,
    pub serial: String,
    pub phase: JobPhase,
    pub status: Option<JobStatus>,
    pub progress: Progress,
    pub discovered: Option<DiscoveredSummary>,
    pub pause: Option<PauseReason>,
    pub current_file: Option<String>,
    pub warnings: Vec<String>,
    pub failures: Vec<(String, String)>,
    /// Failures seen in total (the list above is capped).
    pub failures_total: u64,
    pub archived: u64,
    pub fatal: Option<String>,
    pub outcome: Option<Box<JobOutcome>>,
}

impl JobView {
    pub fn new(id: i64, device_id: i64, serial: String) -> Self {
        Self {
            id,
            device_id,
            serial,
            phase: JobPhase::Starting,
            status: None,
            progress: Progress::default(),
            discovered: None,
            pause: None,
            current_file: None,
            warnings: Vec::new(),
            failures: Vec::new(),
            failures_total: 0,
            archived: 0,
            fatal: None,
            outcome: None,
        }
    }

    /// Fold one engine event into the view. Pure: same event, same result.
    pub fn apply(&mut self, event: &JobEvent) {
        match event {
            JobEvent::StateChanged(status) => {
                self.status = Some(*status);
                self.phase = match status {
                    JobStatus::Paused => JobPhase::Paused,
                    other if other.is_terminal() => JobPhase::Finished(*other),
                    _ => JobPhase::Running,
                };
            }
            JobEvent::Discovered {
                folders,
                files,
                bytes,
                to_copy,
                to_copy_bytes,
                skipped,
            } => {
                self.discovered = Some(DiscoveredSummary {
                    folders: *folders,
                    files: *files,
                    bytes: *bytes,
                    to_copy: *to_copy,
                    to_copy_bytes: *to_copy_bytes,
                    skipped: *skipped,
                });
            }
            JobEvent::FolderWarning { folder, message } => {
                if self.warnings.len() < MAX_LISTED_WARNINGS {
                    self.warnings.push(format!("{folder}: {message}"));
                }
            }
            JobEvent::FileCopied {
                destination,
                archived,
                ..
            } => {
                if archived.is_some() {
                    self.archived += 1;
                }
                self.current_file = Some(destination.clone());
            }
            JobEvent::FileFailed { source, message } => {
                self.failures_total += 1;
                if self.failures.len() < MAX_LISTED_FAILURES {
                    self.failures.push((source.clone(), message.clone()));
                }
            }
            JobEvent::Paused { reason, file } => {
                self.pause = Some(*reason);
                self.phase = JobPhase::Paused;
                if file.is_some() {
                    self.current_file = file.clone();
                }
            }
            JobEvent::Resumed => {
                self.pause = None;
                self.phase = JobPhase::Running;
            }
            JobEvent::Progress(progress) => {
                self.progress = progress.clone();
                if progress.current_file.is_some() {
                    self.current_file = progress.current_file.clone();
                }
            }
            JobEvent::Fatal { message } => {
                self.fatal = Some(message.clone());
            }
            JobEvent::Finished(outcome) => {
                self.status = Some(outcome.status);
                self.progress = progress_from_counters(&outcome.counters);
                self.phase = JobPhase::Finished(outcome.status);
                self.pause = None;
                self.outcome = Some(outcome.clone());
            }
        }
    }

    pub fn is_live(&self) -> bool {
        self.phase.is_live()
    }

    pub fn is_paused(&self) -> bool {
        self.phase.is_paused()
    }

    pub fn progress_fraction(&self) -> f32 {
        self.progress.fraction()
    }

    /// Strip under the progress bar: the terminal summary, the pause reason, or
    /// the fatal message. `true` marks it as a problem (drawn in red).
    pub fn strip(&self) -> Option<(String, bool)> {
        if let Some(outcome) = &self.outcome {
            let counters = &outcome.counters;
            let detail = format!(
                "{} copied, {} unchanged, {} failed",
                counters.copied, counters.skipped, counters.failed
            );
            if outcome.succeeded() {
                return Some((
                    format!(
                        "Backup completed — {detail} in {}",
                        format_duration(outcome.duration)
                    ),
                    false,
                ));
            }
            let reason = outcome
                .error_summary
                .clone()
                .unwrap_or_else(|| status_label(outcome.status).to_string());
            return Some((
                format!(
                    "Backup {} — {detail} ({reason})",
                    status_label(outcome.status).to_lowercase()
                ),
                true,
            ));
        }
        if let Some(fatal) = &self.fatal {
            return Some((format!("Backup stopped: {fatal}"), true));
        }
        if let Some(pause) = self.pause {
            return Some((pause.message().to_string(), pause.is_automatic()));
        }
        None
    }

    /// A one-line summary of the plan, once it is known.
    pub fn plan_line(&self) -> Option<String> {
        let plan = self.discovered?;
        Some(format!(
            "{} folders, {} files ({} → {} to copy)",
            plan.folders,
            plan.files,
            format_bytes(plan.bytes),
            format_bytes(plan.to_copy_bytes)
        ))
    }
}

/// The engine's final counters, as progress: the bar ends where the numbers say
/// it should, instead of wherever the last throttled update left it.
pub fn progress_from_counters(counters: &jobs::JobCounters) -> Progress {
    let done = counters.copied + counters.skipped + counters.failed;
    Progress {
        files_done: done,
        files_total: done,
        bytes_done: counters.bytes_copied,
        bytes_total: counters.bytes_copied,
        files_skipped: counters.skipped,
        files_failed: counters.failed,
        current_file: None,
        bytes_per_sec: None,
        eta_secs: None,
    }
}

// ---------------------------------------------------------------------------
// Banners and modals
// ---------------------------------------------------------------------------

/// A problem banner above the panels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Banner {
    pub message: String,
    pub blocking: bool,
}

/// Which modal is on screen. Used so a modal that was dismissed once is not
/// re-derived on the next frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModalKind {
    Welcome,
    AdbMissing,
    ServerConflict,
    Error,
}

/// A modal dialog. Content is data; [`crate::ui`] renders and resolves it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Modal {
    /// First launch: pick a backup location before anything else.
    Welcome { suggested_dir: String },
    /// `adb` could not be located.
    AdbMissing { message: String },
    /// `adb` reported a server problem (version mismatch, dead socket).
    ServerConflict { message: String },
    /// Anything else that needs the user's attention.
    Error { title: String, message: String },
}

impl Modal {
    pub fn kind(&self) -> ModalKind {
        match self {
            Self::Welcome { .. } => ModalKind::Welcome,
            Self::AdbMissing { .. } => ModalKind::AdbMissing,
            Self::ServerConflict { .. } => ModalKind::ServerConflict,
            Self::Error { .. } => ModalKind::Error,
        }
    }

    pub fn title(&self) -> &str {
        match self {
            Self::Welcome { .. } => "Welcome",
            Self::AdbMissing { .. } => "ADB not found",
            Self::ServerConflict { .. } => "ADB server problem",
            Self::Error { title, .. } => title,
        }
    }
}

// ---------------------------------------------------------------------------
// Start gating
// ---------------------------------------------------------------------------

/// Why the Start button is unavailable for the selected device.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartBlock {
    /// Nothing selected yet.
    NoDevice,
    /// Selected, but the database has no profile for it (the watcher sink has
    /// not caught up, or the device has never been seen).
    NoProfile,
    /// An automatic job (or one started before the restart) is already running.
    Running(String),
    /// A device-specific path or the global directory is missing.
    NoDestination,
    /// No controller: adb could not be located, so nothing can be started.
    NoController,
    /// The device is not in an authorized, connected state.
    Gate(String),
}

impl StartBlock {
    /// The inline explanation shown next to the disabled button.
    pub fn message(&self) -> String {
        match self {
            Self::NoDevice => "Select a device first.".to_string(),
            Self::NoProfile => {
                "This device is not in the manifest yet — waiting for the first poll.".to_string()
            }
            Self::Running(what) => format!("A backup is already running ({what})."),
            Self::NoDestination => {
                "No backup location is configured — choose one in the device panel.".to_string()
            }
            Self::NoController => {
                "adb is not available, so backups cannot start. See the ADB panel.".to_string()
            }
            Self::Gate(message) => message.clone(),
        }
    }
}

/// Everything the window renders and the state it renders from.
#[derive(Debug, Default)]
pub struct ViewState {
    devices: BTreeMap<String, DeviceView>,
    /// Insertion order, so the list does not jump around as devices reconnect.
    order: Vec<String>,
    selected: Option<String>,
    /// One live [`JobView`] per device id.
    jobs: HashMap<i64, JobView>,
    /// Set when the device listing itself failed.
    poll_failed: Option<String>,
    banner: Option<Banner>,
    modal: Option<Modal>,
    /// Modal kinds the user already dismissed in this run.
    dismissed: HashSet<ModalKind>,
    /// adb description for the status bar (path + version).
    adb_summary: String,
    /// Counters for the status bar.
    pub events_applied: u64,
}

impl ViewState {
    pub fn new(adb_summary: impl Into<String>) -> Self {
        Self {
            adb_summary: adb_summary.into(),
            ..Self::default()
        }
    }

    pub fn adb_summary(&self) -> &str {
        &self.adb_summary
    }

    pub fn set_adb_summary(&mut self, summary: impl Into<String>) {
        self.adb_summary = summary.into();
    }

    // -- devices -----------------------------------------------------------

    pub fn devices(&self) -> impl Iterator<Item = &DeviceView> {
        self.order
            .iter()
            .filter_map(|serial| self.devices.get(serial))
    }

    pub fn device(&self, serial: &str) -> Option<&DeviceView> {
        self.devices.get(serial)
    }

    pub fn selected_serial(&self) -> Option<&str> {
        self.selected.as_deref()
    }

    pub fn selected(&self) -> Option<&DeviceView> {
        self.selected.as_ref().and_then(|s| self.devices.get(s))
    }

    pub fn select(&mut self, serial: &str) {
        if self.devices.contains_key(serial) {
            self.selected = Some(serial.to_string());
        }
    }

    pub fn device_count(&self) -> usize {
        self.devices.len()
    }

    pub fn poll_failed(&self) -> Option<&str> {
        self.poll_failed.as_deref()
    }

    pub fn apply_row(&mut self, row: DeviceRow) {
        let serial = row.serial.clone();
        if let Some(view) = self.devices.get_mut(&serial) {
            view.apply_row(row);
            return;
        }
        // A device the manifest knows but adb does not list right now (unplugged
        // since the last run): still worth showing, greyed out.
        let mut view = DeviceView::new(&serial);
        view.apply_row(row);
        self.devices.insert(serial.clone(), view);
        self.order.push(serial);
    }

    /// Fold one watcher event. Returns `true` when something changed, which is
    /// the signal to repaint.
    pub fn apply_device_event(&mut self, event: &DeviceEvent) -> bool {
        self.events_applied += 1;
        match event {
            DeviceEvent::FirstSeen(info) => {
                let first_seen = !self.devices.contains_key(&info.serial);
                let had_selection = self.selected.is_some();
                let view = self
                    .devices
                    .entry(info.serial.clone())
                    .or_insert_with(|| DeviceView::new(&info.serial));
                view.apply_info(info, first_seen);
                if first_seen {
                    self.order.push(info.serial.clone());
                }
                if !had_selection {
                    self.selected = Some(info.serial.clone());
                }
            }
            DeviceEvent::Connected(info)
            | DeviceEvent::Unauthorized(info)
            | DeviceEvent::Offline(info) => {
                let entry = self
                    .devices
                    .entry(info.serial.clone())
                    .or_insert_with(|| DeviceView::new(&info.serial));
                entry.apply_info(info, false);
            }
            DeviceEvent::StateChanged { serial, to, .. } => {
                if let Some(view) = self.devices.get_mut(serial) {
                    view.state = *to;
                    if let Some(info) = view.info.clone() {
                        let mut updated = (*info).clone();
                        updated.state = *to;
                        view.gate = check(&updated).err();
                    }
                }
            }
            DeviceEvent::Disconnected { serial } => {
                if let Some(view) = self.devices.get_mut(serial) {
                    view.state = DeviceState::Disconnected;
                    view.gate = Some(GateError::Disconnected {
                        serial: serial.clone(),
                    });
                }
            }
            DeviceEvent::PollFailed { message } => {
                self.poll_failed = Some(message.clone());
                self.banner = Some(Banner {
                    message: format!("Device list unavailable: {message}"),
                    blocking: false,
                });
            }
            DeviceEvent::PollRecovered => {
                self.poll_failed = None;
                self.banner = None;
            }
        }
        true
    }

    // -- jobs --------------------------------------------------------------

    pub fn jobs(&self) -> impl Iterator<Item = &JobView> {
        self.jobs.values()
    }

    pub fn job(&self, device_id: i64) -> Option<&JobView> {
        self.jobs.get(&device_id)
    }

    pub fn live_job(&self, device_id: i64) -> Option<&JobView> {
        self.jobs.get(&device_id).filter(|job| job.is_live())
    }

    pub fn any_live_job(&self) -> bool {
        self.jobs.values().any(JobView::is_live)
    }

    pub fn insert_job(&mut self, job: JobView) {
        self.jobs.insert(job.device_id, job);
    }

    /// Fold one job event for a device. Returns `true` when something changed.
    pub fn apply_job_event(&mut self, device_id: i64, event: &JobEvent) -> bool {
        self.events_applied += 1;
        if let Some(job) = self.jobs.get_mut(&device_id) {
            job.apply(event);
        } else if let JobEvent::Finished(outcome) = event {
            // A job that was started before this process existed (recovered run):
            // show its outcome rather than dropping it.
            let mut job = JobView::new(outcome.job_id, device_id, String::new());
            job.apply(event);
            self.jobs.insert(device_id, job);
        }
        true
    }

    /// Drop a job view once its thread has been joined and the user has seen the
    /// summary. Until then the panel keeps showing the terminal strip (E06.4).
    pub fn forget_job(&mut self, device_id: i64) {
        if self.jobs.get(&device_id).is_some_and(|job| !job.is_live()) {
            self.jobs.remove(&device_id);
        }
    }

    // -- gating ------------------------------------------------------------

    /// Whether a backup may start for `serial`, with the reason it may not.
    pub fn start_block(
        &self,
        serial: Option<&str>,
        controller_available: bool,
    ) -> Option<StartBlock> {
        let Some(serial) = serial else {
            return Some(StartBlock::NoDevice);
        };
        let Some(device) = self.devices.get(serial) else {
            return Some(StartBlock::NoDevice);
        };
        if let Some(job) = device.row.as_ref().and_then(|row| self.jobs.get(&row.id)) {
            if job.is_live() {
                return Some(StartBlock::Running(
                    job.status
                        .map(status_label)
                        .unwrap_or("starting")
                        .to_string(),
                ));
            }
        }
        if let Some(gate) = device.gate_message() {
            return Some(StartBlock::Gate(gate));
        }
        // A device the manifest remembers but that is not connected right now has
        // no snapshot to gate on: treat it as disconnected rather than as ready.
        if device.info.is_none() {
            return Some(StartBlock::Gate(
                GateError::Disconnected {
                    serial: serial.to_string(),
                }
                .to_string(),
            ));
        }
        if device.row.is_none() {
            return Some(StartBlock::NoProfile);
        }
        if !controller_available {
            return Some(StartBlock::NoController);
        }
        None
    }

    /// The same gate the core enforces, expressed as a result for callers that
    /// just want to know.
    pub fn can_start(
        &self,
        serial: Option<&str>,
        controller_available: bool,
    ) -> Result<(), StartBlock> {
        match self.start_block(serial, controller_available) {
            Some(block) => Err(block),
            None => Ok(()),
        }
    }

    // -- banners and modals ------------------------------------------------

    pub fn banner(&self) -> Option<&Banner> {
        self.banner.as_ref()
    }

    pub fn set_banner(&mut self, banner: Option<Banner>) {
        self.banner = banner;
    }

    pub fn set_error(&mut self, message: impl Into<String>) {
        self.banner = Some(Banner {
            message: message.into(),
            blocking: false,
        });
    }

    pub fn clear_banner(&mut self) {
        self.banner = None;
    }

    pub fn modal(&self) -> Option<&Modal> {
        self.modal.as_ref()
    }

    /// Offer a modal, unless this kind was already dismissed in this run.
    pub fn show_modal(&mut self, modal: Modal) {
        if !self.dismissed.contains(&modal.kind()) {
            self.modal = Some(modal);
        }
    }

    /// Is this kind of modal allowed to appear?
    pub fn modal_allowed(&self, kind: ModalKind) -> bool {
        !self.dismissed.contains(&kind)
    }

    pub fn dismiss_modal(&mut self) {
        if let Some(modal) = self.modal.take() {
            self.dismissed.insert(modal.kind());
        }
    }

    pub fn has_modal(&self) -> bool {
        self.modal.is_some()
    }
}

/// Map the controller's start failure to the UI's wording.
impl From<&StartError> for StartBlock {
    fn from(err: &StartError) -> Self {
        Self::Running(err.to_string())
    }
}

/// A short, human summary of the plan for the log and the status bar.
pub fn summarize_outcome(outcome: &JobOutcome) -> String {
    format!(
        "{}: {} copied, {} unchanged, {} failed in {}",
        status_label(outcome.status),
        outcome.counters.copied,
        outcome.counters.skipped,
        outcome.counters.failed,
        format_duration(outcome.duration)
    )
}

/// Failures worth listing for a job row (already capped by the engine).
pub fn failure_lines(outcome: &JobOutcome) -> Vec<String> {
    outcome
        .failures
        .iter()
        .map(|(path, message)| format!("{path}: {message}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use android_backup_core::adb::parse::RawState;
    use android_backup_core::adb::props::DeviceProps;
    use android_backup_core::backup::Reason;
    use android_backup_core::database::jobs::JobCounters;

    fn info(serial: &str, state: DeviceState, model: Option<&str>) -> Arc<DeviceInfo> {
        Arc::new(DeviceInfo {
            serial: serial.to_string(),
            state,
            raw_state: RawState::Device,
            listing_model: model.map(str::to_string),
            listing_product: None,
            transport_id: None,
            props: DeviceProps {
                manufacturer: Some("Samsung".to_string()),
                model: model.map(str::to_string),
                product: None,
                android_version: Some("14".to_string()),
            },
        })
    }

    /// A manifest row, for the gating tests.
    fn row(id: i64, serial: &str, name: &str) -> DeviceRow {
        DeviceRow {
            id,
            serial: serial.to_string(),
            display_name: name.to_string(),
            manufacturer: Some("Samsung".to_string()),
            model: Some("SM-A536B".to_string()),
            android_version: Some("14".to_string()),
            destination_path: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_seen_at: "2026-01-01T00:00:00Z".to_string(),
        }
    }

    #[test]
    fn formats_bytes_with_binary_units() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(2048), "2.0 KB");
        assert_eq!(format_bytes(5 * 1024 * 1024), "5.0 MB");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
    }

    #[test]
    fn formats_clock_and_duration() {
        assert_eq!(format_clock(None), "--:--");
        assert_eq!(format_clock(Some(5)), "00:05");
        assert_eq!(format_clock(Some(305)), "05:05");
        assert_eq!(format_clock(Some(3725)), "1:02:05");
        assert_eq!(format_duration(Duration::from_secs(9)), "9s");
        assert_eq!(format_duration(Duration::from_secs(185)), "3m 05s");
    }

    #[test]
    fn truncates_in_the_middle_keeping_the_tail() {
        assert_eq!(truncate_middle("short", 20), "short");
        let long = "/storage/emulated/0/DCIM/Camera/20260101_120000.jpg";
        let cut = truncate_middle(long, 24);
        assert_eq!(cut.chars().count(), 24, "{cut}");
        assert!(cut.starts_with("/storage/em"), "{cut}");
        assert!(cut.ends_with("120000.jpg"), "{cut}");
    }

    #[test]
    fn progress_headline_carries_every_number_the_panel_shows() {
        let progress = Progress {
            files_done: 3,
            files_total: 10,
            bytes_done: 1_258_291,
            bytes_total: 9_000_000,
            files_skipped: 1,
            files_failed: 0,
            current_file: None,
            bytes_per_sec: Some(460_800),
            eta_secs: Some(65),
        };
        assert_eq!(
            progress_headline(&progress),
            "files: 3/10 | 1.2 MB copied | ETA 01:05 | 450.0 KB/s"
        );
    }

    #[test]
    fn progress_headline_omits_unknown_eta_and_reports_failures() {
        let progress = Progress {
            files_done: 4,
            files_total: 4,
            files_failed: 2,
            ..Progress::default()
        };
        let line = progress_headline(&progress);
        assert!(line.contains("files: 4/4"), "{line}");
        assert!(!line.contains("ETA"), "{line}");
        assert!(line.contains("2 failed"), "{line}");
    }

    #[test]
    fn device_events_build_and_fold_the_list() {
        let mut view = ViewState::new("adb 35.0.0");
        view.apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            Some("SM-A536B"),
        )));
        assert_eq!(view.device_count(), 1);
        // The first device is auto-selected: a single phone on the desk needs no
        // click before the Start button works.
        assert_eq!(view.selected_serial(), Some("SER1"));
        let device = view.device("SER1").expect("device");
        assert_eq!(device.name, "Samsung SM-A536B");
        assert_eq!(device.state_label(), "Connected");
        assert!(device.is_ready());
        // A backup needs a device id, so the Start button stays blocked until the
        // manifest has recorded this serial.
        assert!(matches!(
            view.can_start(Some("SER1"), true),
            Err(StartBlock::NoProfile)
        ));
        view.apply_row(row(1, "SER1", "Samsung SM-A536B"));
        assert!(view.can_start(Some("SER1"), true).is_ok());

        view.apply_device_event(&DeviceEvent::Unauthorized(info(
            "SER1",
            DeviceState::Unauthorized,
            Some("SM-A536B"),
        )));
        let device = view.device("SER1").expect("device");
        assert_eq!(device.state_label(), "Unauthorized — approve on phone");
        let block = view.start_block(Some("SER1"), true).expect("blocked");
        assert!(matches!(block, StartBlock::Gate(_)), "{block:?}");
        assert!(
            block.message().contains("approve"),
            "the gate must tell the user what to do: {}",
            block.message()
        );

        view.apply_device_event(&DeviceEvent::PollFailed {
            message: "adb server died".to_string(),
        });
        assert_eq!(view.poll_failed(), Some("adb server died"));
        assert!(view.banner().is_some());
        view.apply_device_event(&DeviceEvent::PollRecovered);
        assert!(view.poll_failed().is_none());
        assert!(view.banner().is_none());
    }

    #[test]
    fn a_second_device_does_not_steal_the_selection() {
        let mut view = ViewState::new("");
        view.apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            None,
        )));
        view.select("SER1");
        view.apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER2",
            DeviceState::Connected,
            None,
        )));
        assert_eq!(view.selected_serial(), Some("SER1"));
        assert_eq!(view.device_count(), 2);
    }

    #[test]
    fn job_events_drive_the_progress_panel() {
        let mut job = JobView::new(7, 1, "SER1".to_string());
        job.apply(&JobEvent::StateChanged(JobStatus::Copying));
        job.apply(&JobEvent::Discovered {
            folders: 2,
            files: 12,
            bytes: 900,
            to_copy: 10,
            to_copy_bytes: 800,
            skipped: 2,
        });
        job.apply(&JobEvent::Progress(Progress {
            files_done: 2,
            files_total: 10,
            bytes_done: 4096,
            bytes_total: 800,
            files_skipped: 0,
            files_failed: 0,
            current_file: Some("/storage/emulated/0/DCIM/a.jpg".to_string()),
            bytes_per_sec: Some(1024),
            eta_secs: Some(30),
        }));
        assert!(job.is_live());
        assert_eq!(job.status, Some(JobStatus::Copying));
        assert_eq!(
            job.current_file.as_deref(),
            Some("/storage/emulated/0/DCIM/a.jpg")
        );
        assert_eq!(job.progress_fraction(), 0.2);
        assert_eq!(
            job.plan_line().as_deref(),
            Some("2 folders, 12 files (900 B → 800 B to copy)")
        );

        job.apply(&JobEvent::Paused {
            reason: PauseReason::AuthorizationNeeded,
            file: None,
        });
        assert!(job.is_paused());
        let (strip, problem) = job.strip().expect("strip");
        assert!(
            problem,
            "a pause the engine chose is a problem the user must fix"
        );
        assert!(
            strip.contains("approve the USB debugging prompt"),
            "{strip}"
        );

        job.apply(&JobEvent::Resumed);
        assert!(!job.is_paused());
        job.apply(&JobEvent::FileCopied {
            source: "a".to_string(),
            destination: "b".to_string(),
            bytes: 10,
            reason: Reason::New,
            archived: Some("/backup/.history/old.jpg".into()),
        });
        assert_eq!(job.archived, 1);
        job.apply(&JobEvent::FileFailed {
            source: "/storage/emulated/0/DCIM/locked.jpg".to_string(),
            message: "permission denied".to_string(),
        });
        assert_eq!(job.failures_total, 1);
        assert_eq!(job.failures.len(), 1);
    }

    #[test]
    fn a_finished_job_shows_a_summary_strip_not_a_live_bar() {
        let mut job = JobView::new(9, 3, "SER1".to_string());
        let outcome = JobOutcome {
            job_id: 9,
            device_id: 3,
            status: JobStatus::Completed,
            counters: JobCounters {
                scanned: 452,
                copied: 430,
                skipped: 20,
                failed: 2,
                bytes_copied: 1_048_576,
            },
            error_summary: None,
            duration: Duration::from_secs(185),
            failures: vec![("/x".to_string(), "denied".to_string())],
        };
        job.apply(&JobEvent::Finished(Box::new(outcome)));
        assert!(!job.is_live());
        let (strip, problem) = job.strip().expect("strip");
        assert!(!problem, "{strip}");
        assert_eq!(
            strip,
            "Backup completed — 430 copied, 20 unchanged, 2 failed in 3m 05s"
        );
        assert_eq!(job.progress_fraction(), 1.0);
    }

    #[test]
    fn a_failed_job_strip_carries_the_reason() {
        let mut job = JobView::new(9, 3, "SER1".to_string());
        let outcome = JobOutcome {
            job_id: 9,
            device_id: 3,
            status: JobStatus::Failed,
            counters: JobCounters::default(),
            error_summary: Some("destination is full".to_string()),
            duration: Duration::from_secs(4),
            failures: Vec::new(),
        };
        job.apply(&JobEvent::Finished(Box::new(outcome)));
        let (strip, problem) = job.strip().expect("strip");
        assert!(problem);
        assert!(strip.contains("destination is full"), "{strip}");
    }

    #[test]
    fn a_fatal_event_is_shown_even_without_an_outcome() {
        let mut job = JobView::new(1, 1, "SER1".to_string());
        job.apply(&JobEvent::Fatal {
            message: "the backup drive disappeared".to_string(),
        });
        let (strip, problem) = job.strip().expect("strip");
        assert!(problem);
        assert!(strip.contains("backup drive disappeared"), "{strip}");
    }

    #[test]
    fn the_start_gate_reports_the_first_thing_that_blocks() {
        let mut view = ViewState::new("");
        assert!(matches!(
            view.start_block(None, true),
            Some(StartBlock::NoDevice)
        ));

        view.apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            Some("Pixel 8"),
        )));
        // No manifest row yet: the watcher's sink has not recorded it.
        assert!(matches!(
            view.start_block(Some("SER1"), true),
            Some(StartBlock::NoProfile)
        ));

        view.apply_row(DeviceRow {
            id: 42,
            serial: "SER1".to_string(),
            display_name: "Pixel 8".to_string(),
            manufacturer: Some("Google".to_string()),
            model: Some("Pixel 8".to_string()),
            android_version: Some("14".to_string()),
            destination_path: Some("/backups/pixel".into()),
            created_at: "2026-01-01T00:00:00Z".to_string(),
            last_seen_at: "2026-01-01T00:00:00Z".to_string(),
        });
        assert!(view.can_start(Some("SER1"), true).is_ok());
        assert!(matches!(
            view.start_block(Some("SER1"), false),
            Some(StartBlock::NoController)
        ));

        // A live job blocks the same device but not the others.
        view.insert_job(JobView::new(1, 42, "SER1".to_string()));
        assert!(matches!(
            view.start_block(Some("SER1"), true),
            Some(StartBlock::Running(_))
        ));
    }

    #[test]
    fn modals_are_offered_once_per_run() {
        let mut view = ViewState::new("");
        view.show_modal(Modal::Welcome {
            suggested_dir: "/home/u/AndroidBackups".to_string(),
        });
        assert!(view.has_modal());
        assert_eq!(view.modal().map(Modal::title), Some("Welcome"));
        view.dismiss_modal();
        assert!(!view.has_modal());

        // Re-derived next frame (still unresolved): must not come back.
        view.show_modal(Modal::Welcome {
            suggested_dir: "/home/u/AndroidBackups".to_string(),
        });
        assert!(!view.has_modal());
        assert!(!view.modal_allowed(ModalKind::Welcome));
        // A different modal is unaffected.
        assert!(view.modal_allowed(ModalKind::AdbMissing));
    }

    #[test]
    fn history_cells_and_summaries_render_from_rows() {
        let row = JobRow {
            id: 1,
            device_id: 1,
            started_at: "2026-01-01T10:00:00Z".to_string(),
            completed_at: Some("2026-01-01T10:03:05Z".to_string()),
            status: JobStatus::Completed,
            files_scanned: 450,
            files_copied: 430,
            files_skipped: 20,
            files_failed: 2,
            bytes_copied: 1_073_741_824,
            error_summary: None,
        };
        assert_eq!(
            last_backup_summary(&row),
            "Last backup: Completed — 430 copied, 20 unchanged, 2 failed"
        );
        let cells = history_cells(&row);
        assert_eq!(cells[0], "2026-01-01 10:00:00");
        assert_eq!(cells[1], "3m 05s");
        assert_eq!(cells[2], "Completed");
        assert_eq!(cells[3], "430 / 20 / 2");
        assert_eq!(cells[4], "1.0 GB");
        assert_eq!(last_backup_line(None), "Last backup: — never backed up");
    }

    #[test]
    fn summarize_outcome_is_one_line() {
        let outcome = JobOutcome {
            job_id: 1,
            device_id: 1,
            status: JobStatus::Cancelled,
            counters: JobCounters {
                scanned: 4,
                copied: 3,
                skipped: 1,
                failed: 0,
                bytes_copied: 300,
            },
            error_summary: Some("cancelled by the user".to_string()),
            duration: Duration::from_secs(12),
            failures: vec![("a".to_string(), "b".to_string())],
        };
        assert_eq!(
            summarize_outcome(&outcome),
            "Cancelled: 3 copied, 1 unchanged, 0 failed in 12s"
        );
        assert_eq!(failure_lines(&outcome), vec!["a: b".to_string()]);
    }

    #[test]
    fn labels_cover_every_state_and_reason() {
        for state in [
            DeviceState::Connected,
            DeviceState::Unauthorized,
            DeviceState::Offline,
            DeviceState::Disconnected,
            DeviceState::Error,
        ] {
            assert!(!state_label(state).is_empty());
        }
        for status in [
            JobStatus::Planning,
            JobStatus::Scanning,
            JobStatus::Copying,
            JobStatus::Paused,
            JobStatus::Cancelled,
            JobStatus::Completed,
            JobStatus::Failed,
        ] {
            assert!(!status_label(status).is_empty());
        }
        for reason in [
            Reason::New,
            Reason::SizeChanged,
            Reason::ModifiedTimeChanged,
            Reason::PreviousAttemptFailed,
            Reason::LocalCopyMissing,
            Reason::LocalSizeChanged,
            Reason::LocalContentChanged,
            Reason::Unchanged,
            Reason::UnchangedUnverifiable,
        ] {
            assert!(!reason.describe().is_empty());
        }
    }
}
