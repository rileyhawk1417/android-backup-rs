//! Application state and the intent handlers behind every widget.
//!
//! The rule this module exists to enforce: **the UI never does the work**. It
//! folds events into [`ViewState`], forwards intents into `android-backup-core`,
//! and nothing else. No comparisons, no transfers, no hashing — grep this
//! directory for `sha2`/`compare`/`transfer` and you will find only calls, never
//! logic.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use android_backup_core::adb::installer::{
    Control as InstallControl, InstallError, InstallReport, Progress as InstallProgress,
    Stage as InstallStage,
};
use android_backup_core::adb::locate::{AdbFound, AdbLocateError};
use android_backup_core::adb::runner::Adb;
use android_backup_core::backup::controller::{BackupController, JobHandle};
use android_backup_core::backup::destination::{self, FsProbe, LocalFsProbe};
use android_backup_core::backup::enumerate::RemoteFiles;
use android_backup_core::backup::{
    JobConfig, JobEvent, RecoveryReport, VerifyMode, DEFAULT_FREE_SPACE_FLOOR,
};
use android_backup_core::config::{AppConfig, AppPaths};
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::files::{self, BackupFileRow};
use android_backup_core::database::jobs::{self, JobRow};
use android_backup_core::database::Db;
use android_backup_core::devices::profile::ProfileService;
use android_backup_core::devices::watcher::{DeviceWatcher, WatcherConfig};
use android_backup_core::devices::{DeviceInfo, DeviceSink, SharedSink};

use crate::startup::{self, AdbInstaller};
use crate::view::{Banner, JobView, Modal, StartBlock, ViewState, HISTORY_PAGE};

/// Rows of a job's file list loaded per "load more" click.
pub const FILE_PAGE: usize = 500;

/// Everything the application needs before it can show a window. Built by
/// `main`, and by the tests with mocks.
pub struct AppDeps {
    pub paths: AppPaths,
    pub cfg: AppConfig,
    /// Was there a configuration file on disk? Drives the welcome wizard.
    pub config_existed: bool,
    /// adb runner, when one was located.
    pub adb: Option<Arc<dyn Adb>>,
    pub adb_found: Option<AdbFound>,
    /// Remote file access for the backup engine (real adb, or a mock in tests).
    pub remote: Option<Arc<dyn RemoteFiles>>,
    pub db: Option<Arc<Db>>,
    /// Filesystem probe used for destination validation and recovery.
    pub probe: Option<Arc<dyn FsProbe>>,
    /// Managed platform-tools installer. `None` disables the button, and the UI
    /// says why rather than offering a dead end.
    pub installer: Option<Arc<dyn AdbInstaller>>,
    /// Start the device watcher. Tests turn this off when they drive events by
    /// hand; everything else goes through the real thread.
    pub watch_devices: bool,
}

impl AppDeps {
    /// Deps with the real filesystem probe, no watcher.
    pub fn new(paths: AppPaths, cfg: AppConfig) -> Self {
        Self {
            paths,
            cfg,
            config_existed: true,
            adb: None,
            adb_found: None,
            remote: None,
            db: None,
            probe: Some(Arc::new(LocalFsProbe::new())),
            installer: None,
            watch_devices: true,
        }
    }
}

/// A modal or a picker result that arrived from a background thread.
enum UiMessage {
    /// A folder chosen by the user (backup location or device destination).
    FolderPicked(PathBuf),
    /// An adb executable chosen by the user.
    AdbPicked(PathBuf),
    /// A background adb locate attempt finished.
    AdbLocated(Result<AdbFound, AdbLocateError>),
    /// The installer is still working.
    AdbInstallProgress(InstallProgress),
    /// The installer finished, one way or the other.
    AdbInstalled(Result<InstallReport, InstallError>),
    /// Startup recovery finished for one device.
    Recovered {
        device_id: i64,
        report: RecoveryReport,
    },
    /// Recovery failed (the destination is gone, the manifest is locked, …).
    RecoveryFailed { device_id: i64, message: String },
}

/// Wakes egui when a device event arrives, then forwards it to the real sink.
///
/// Without this the window would only repaint on the next input event: the
/// watcher thread has no way to reach the `Context` itself.
struct RepaintingSink {
    inner: SharedSink,
    ctx: Arc<Mutex<Option<egui::Context>>>,
}

impl DeviceSink for RepaintingSink {
    fn upsert(&self, device: &DeviceInfo) {
        self.inner.upsert(device);
        if let Some(ctx) = lock(&self.ctx).as_ref() {
            ctx.request_repaint();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The whole application.
pub struct App {
    // -- infrastructure ---------------------------------------------------
    paths: AppPaths,
    cfg: AppConfig,
    db: Option<Arc<Db>>,
    profiles: Option<ProfileService>,
    controller: Option<Arc<BackupController>>,
    remote: Option<Arc<dyn RemoteFiles>>,
    probe: Arc<dyn FsProbe>,
    watcher: Option<DeviceWatcher>,
    adb: Option<Arc<dyn Adb>>,
    adb_found: Option<AdbFound>,
    /// Managed installer (E07.1); `None` when the platform cannot install or the
    /// HTTP client could not be built.
    installer: Option<Arc<dyn AdbInstaller>>,
    /// An installation is in flight: the progress modal is up and a second
    /// request must not start another download.
    installing: bool,
    /// Set by the Cancel button, read by the installer thread's progress
    /// callback on the next tick.
    install_cancelled: Arc<AtomicBool>,
    /// Shared with the watcher's sink so it can wake the UI.
    ctx_slot: Arc<Mutex<Option<egui::Context>>>,

    // -- view state -------------------------------------------------------
    view: ViewState,
    /// Live job handles, keyed by device id.
    handles: HashMap<i64, JobHandle>,
    /// History rows per device, read from the manifest (never from memory).
    history: HashMap<i64, Vec<JobRow>>,
    latest: HashMap<i64, Option<JobRow>>,
    /// Files of the expanded history job, and how many are visible.
    open_job: Option<i64>,
    job_files: Vec<BackupFileRow>,
    files_visible: usize,
    /// Devices whose startup recovery already ran in this session.
    recovered: HashSet<i64>,

    // -- edit buffers -----------------------------------------------------
    name_buffer: String,
    name_serial: Option<String>,
    name_error: Option<String>,
    folder_buffer: String,
    folder_error: Option<String>,
    location_error: Option<String>,
    start_note: Option<String>,
    /// Folders of the selected device, as last read from the manifest.
    folders: Vec<devices::FolderRow>,

    // -- channels ---------------------------------------------------------
    tx: Sender<UiMessage>,
    rx: Receiver<UiMessage>,

    /// Set once the window asked to close, so the watcher is stopped exactly once.
    closing: bool,
}

impl App {
    pub fn new(deps: AppDeps) -> Self {
        let AppDeps {
            paths,
            cfg,
            config_existed,
            adb,
            adb_found,
            remote,
            db,
            probe,
            installer,
            watch_devices,
        } = deps;
        let probe: Arc<dyn FsProbe> = probe.unwrap_or_else(|| Arc::new(LocalFsProbe::new()));
        let (tx, rx) = channel();
        let ctx_slot = Arc::new(Mutex::new(None));

        let profiles = db.as_ref().map(|db| ProfileService::new(Arc::clone(db)));
        let controller = match (db.as_ref(), remote.as_ref()) {
            (Some(db), Some(remote)) => Some(Arc::new(BackupController::new(
                Arc::clone(db),
                Arc::clone(remote),
                Arc::clone(&probe),
            ))),
            _ => None,
        };

        let adb_summary = match (&adb_found, adb.is_some()) {
            (Some(found), _) => startup::adb_summary(found),
            (None, true) => "adb (path unknown)".to_string(),
            (None, false) => "adb: not found".to_string(),
        };
        let mut view = ViewState::new(adb_summary);

        // The manifest's devices are shown before adb says anything: a phone that
        // was unplugged since the last run must not vanish from the list.
        if let Some(db) = db.as_ref() {
            match devices::list(db) {
                Ok(rows) => {
                    for row in rows {
                        view.apply_row(row);
                    }
                }
                Err(err) => view.set_error(format!("could not read the device list: {err}")),
            }
        }

        let watcher = match (&adb, &db, watch_devices) {
            (Some(adb), Some(db), true) => {
                let sink: SharedSink = Arc::new(RepaintingSink {
                    inner: Arc::clone(db) as SharedSink,
                    ctx: Arc::clone(&ctx_slot),
                });
                Some(DeviceWatcher::spawn(
                    Arc::clone(adb),
                    Some(sink),
                    WatcherConfig::from_app_config(&cfg),
                ))
            }
            _ => None,
        };

        let mut app = Self {
            paths,
            cfg,
            db,
            profiles,
            controller,
            remote,
            probe,
            watcher,
            adb,
            adb_found,
            installer,
            installing: false,
            install_cancelled: Arc::new(AtomicBool::new(false)),
            ctx_slot,
            view,
            handles: HashMap::new(),
            history: HashMap::new(),
            latest: HashMap::new(),
            open_job: None,
            job_files: Vec::new(),
            files_visible: FILE_PAGE,
            recovered: HashSet::new(),
            name_buffer: String::new(),
            name_serial: None,
            name_error: None,
            folder_buffer: String::new(),
            folder_error: None,
            location_error: None,
            start_note: None,
            folders: Vec::new(),
            tx,
            rx,
            closing: false,
        };

        // Startup state: the wizard and the adb modal, both re-derived every launch
        // (E06.6: they come back until they are resolved).
        let adb_result: Result<&AdbFound, &AdbLocateError> = match app.adb_found.as_ref() {
            Some(found) => Ok(found),
            None => Err(&AdbLocateError::NotFound),
        };
        let plan = startup::plan(config_existed, adb_result);
        if let Some(modal) = plan.first_modal(
            startup::suggested_backup_dir(&app.cfg)
                .display()
                .to_string(),
        ) {
            app.view.show_modal(modal);
        }
        if !plan.adb_ready && app.adb.is_none() && app.adb_found.is_none() {
            tracing::warn!("no usable adb at startup; backups stay disabled");
        }
        // A server conflict is reported separately from a missing binary.
        if let Some(adb) = app.adb.clone() {
            if let Some(message) = startup::server_conflict(adb.as_ref()) {
                app.view.show_modal(Modal::ServerConflict { message });
            }
        }
        app.select_first_device();
        app
    }

    // ---------------------------------------------------------------------
    // Accessors used by the renderer and the tests
    // ---------------------------------------------------------------------

    pub fn view(&self) -> &ViewState {
        &self.view
    }

    pub fn view_mut(&mut self) -> &mut ViewState {
        &mut self.view
    }

    pub fn config(&self) -> &AppConfig {
        &self.cfg
    }

    pub fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn adb_found(&self) -> Option<&AdbFound> {
        self.adb_found.as_ref()
    }

    pub fn controller(&self) -> Option<&Arc<BackupController>> {
        self.controller.as_ref()
    }

    pub fn db(&self) -> Option<&Arc<Db>> {
        self.db.as_ref()
    }

    pub fn history(&self, device_id: i64) -> &[JobRow] {
        self.history
            .get(&device_id)
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    pub fn latest_job(&self, device_id: i64) -> Option<&JobRow> {
        self.latest.get(&device_id).and_then(Option::as_ref)
    }

    pub fn folders(&self) -> &[devices::FolderRow] {
        &self.folders
    }

    pub fn open_job(&self) -> Option<i64> {
        self.open_job
    }

    pub fn job_files(&self) -> &[BackupFileRow] {
        &self.job_files[..self.files_visible.min(self.job_files.len())]
    }

    pub fn has_more_files(&self) -> bool {
        self.files_visible < self.job_files.len()
    }

    pub fn name_buffer(&self) -> &str {
        &self.name_buffer
    }

    pub fn name_error(&self) -> Option<&str> {
        self.name_error.as_deref()
    }

    pub fn folder_buffer(&self) -> &str {
        &self.folder_buffer
    }

    pub fn folder_error(&self) -> Option<&str> {
        self.folder_error.as_deref()
    }

    pub fn location_error(&self) -> Option<&str> {
        self.location_error.as_deref()
    }

    pub fn start_note(&self) -> Option<&str> {
        self.start_note.as_deref()
    }

    pub fn name_buffer_mut(&mut self) -> &mut String {
        &mut self.name_buffer
    }

    pub fn folder_buffer_mut(&mut self) -> &mut String {
        &mut self.folder_buffer
    }

    /// Replace the name buffer with what the user typed. The value is only stored
    /// when [`Intent::ApplyName`] runs; this merely keeps the text box in sync.
    pub fn set_name_buffer(&mut self, text: String) {
        self.name_buffer = text;
    }

    pub fn set_folder_buffer(&mut self, text: String) {
        self.folder_buffer = text;
    }

    /// Why the Start button is unavailable, if it is.
    pub fn start_block(&self) -> Option<StartBlock> {
        self.view
            .start_block(self.view.selected_serial(), self.controller.is_some())
    }

    /// The effective destination of the selected device, for display.
    pub fn destination_of(&self, device: &DeviceRow) -> Result<PathBuf, String> {
        let others: Vec<DeviceRow> = self
            .view
            .devices()
            .filter_map(|view| view.row.clone())
            .collect();
        destination::resolve_with_config(device, &others, &self.cfg)
            .map(|dest| dest.root)
            .map_err(|err| err.to_string())
    }

    /// Is there somewhere to write to right now?
    ///
    /// The device's own subfolder is *allowed to be missing*: the engine creates
    /// it on the first backup. What must exist is the folder it lives in, because
    /// that is the one the user chose — if the drive holding it is gone, creating
    /// the path would silently write to a plain directory on another disk.
    /// A destination the device owns is checked in full, since the user named it.
    fn check_destination_ready(
        &self,
        dest: &destination::EffectiveDestination,
    ) -> Result<(), String> {
        let check = if dest.device_subfolder {
            dest.root.parent().unwrap_or(&dest.root)
        } else {
            &dest.root
        };
        destination::validate(&*self.probe, check, None)
            .map(|_| ())
            .map_err(|err| format!("{} cannot be used: {err}", check.display()))
    }

    // ---------------------------------------------------------------------
    // Frame entry point
    // ---------------------------------------------------------------------

    /// One frame: drain, react, render.
    pub fn frame(&mut self, ctx: &egui::Context) {
        if lock(&self.ctx_slot).is_none() {
            *lock(&self.ctx_slot) = Some(ctx.clone());
        }
        if !self.closing && ctx.input(|input| input.viewport().close_requested()) {
            self.closing = true;
            self.stop_background_work();
        }

        let changed = self.drain_events();
        if changed {
            ctx.request_repaint();
        }
        // Keep polling: a job in flight needs its progress drawn even when no
        // event arrived this frame, and the device list must never look frozen.
        let interval = if self.view.any_live_job() {
            Duration::from_millis(200)
        } else {
            self.cfg.poll_interval()
        };
        ctx.request_repaint_after(interval);

        crate::ui::draw(self, ctx);
    }

    /// Stop the watcher and join its thread. Also called on drop.
    pub fn stop_background_work(&mut self) {
        if let Some(mut watcher) = self.watcher.take() {
            watcher.stop();
        }
        if let Some(controller) = self.controller.clone() {
            controller.prune_finished();
        }
    }

    fn drain_events(&mut self) -> bool {
        let mut changed = false;

        // 1. Messages from background threads (pickers, recovery, adb probe).
        while let Ok(message) = self.rx.try_recv() {
            changed = true;
            self.apply_message(message);
        }

        // 2. Device events from the watcher.
        if let Some(watcher) = self.watcher.as_ref() {
            let events = watcher.drain();
            for event in &events {
                self.view.apply_device_event(event);
            }
            if !events.is_empty() {
                changed = true;
                self.refresh_rows_from_db();
                self.recover_new_devices();
            }
        }

        // 3. Job events from the controller's handles.
        changed |= self.drain_jobs();
        changed
    }

    fn apply_message(&mut self, message: UiMessage) {
        match message {
            UiMessage::FolderPicked(path) => self.on_folder_picked(path),
            UiMessage::AdbPicked(path) => self.on_adb_picked(path),
            UiMessage::AdbLocated(result) => self.on_adb_located(result),
            UiMessage::AdbInstallProgress(progress) => self.on_install_progress(progress),
            UiMessage::AdbInstalled(result) => self.on_installed(result),
            UiMessage::Recovered { device_id, report } => {
                self.on_recovered(device_id, report);
            }
            UiMessage::RecoveryFailed { device_id, message } => {
                tracing::warn!(device_id, %message, "startup recovery failed");
                self.view.set_error(format!(
                    "could not check the last backup of a device: {message}"
                ));
            }
        }
    }

    fn drain_jobs(&mut self) -> bool {
        let mut changed = false;
        let mut finished: Vec<i64> = Vec::new();
        for (device_id, handle) in &self.handles {
            let events = handle.drain();
            if events.is_empty() {
                continue;
            }
            changed = true;
            for event in &events {
                if matches!(event, JobEvent::Finished(_)) {
                    finished.push(*device_id);
                }
                self.view.apply_job_event(*device_id, event);
            }
        }
        for device_id in finished {
            self.handles.remove(&device_id);
            if let Some(job) = self.view.job(device_id) {
                if let Some(outcome) = &job.outcome {
                    tracing::info!(job_id = outcome.job_id, "backup finished");
                }
            }
            self.reload_history(device_id);
        }
        if let Some(controller) = self.controller.as_ref() {
            // Joins the threads of jobs that are done (never blocks on a live one).
            controller.prune_finished();
        }
        changed
    }

    // ---------------------------------------------------------------------
    // Devices
    // ---------------------------------------------------------------------

    /// Re-read the manifest rows for the devices currently on screen, so a device
    /// the watcher just recorded gets its profile (and therefore its folders).
    fn refresh_rows_from_db(&mut self) {
        let mut serials: Vec<String> = self
            .view
            .devices()
            .map(|view| view.serial.clone())
            .collect();
        // The selected serial counts too: it can be selected before it is listed.
        if let Some(selected) = self.view.selected_serial() {
            if !serials.iter().any(|serial| serial == selected) {
                serials.push(selected.to_string());
            }
        }
        for serial in serials {
            if self
                .view
                .device(&serial)
                .is_some_and(|view| view.row.is_some())
            {
                continue;
            }
            self.apply_row_for(&serial);
        }
        self.load_folders_for_selection();
    }

    /// Attach the manifest row for `serial`, if there is one.
    fn apply_row_for(&mut self, serial: &str) {
        let Some(db) = self.db.clone() else {
            return;
        };
        match devices::resolve_active_device(&db, serial) {
            Ok(Some(row)) => self.view.apply_row(row),
            Ok(None) => {}
            Err(err) => tracing::warn!(%serial, %err, "could not read the device profile"),
        }
    }

    fn select_device(&mut self, serial: &str) {
        // A serial can be selected (from the list, or restored at startup) before
        // the view knows anything about it. Record the row first, otherwise
        // `select` finds no entry to select and the click is silently lost.
        if self.view.device(serial).is_none() {
            self.apply_row_for(serial);
        }
        self.view.select(serial);
        // The view may know the serial before the manifest does (the watcher sink
        // writes asynchronously), so look the row up again here.
        self.refresh_rows_from_db();
        self.name_serial = None;
        self.name_error = None;
        self.folder_error = None;
        self.location_error = None;
        self.start_note = None;
        self.open_job = None;
        self.job_files.clear();
        self.load_folders_for_selection();
        // History belongs to the device, so it is re-read from the manifest on
        // every selection change: the list always reflects what is on disk.
        if let Some(device_id) = self.selected_device_id() {
            self.reload_history(device_id);
        }
    }

    fn load_folders_for_selection(&mut self) {
        let Some(profiles) = self.profiles.as_ref() else {
            self.folders.clear();
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            self.folders.clear();
            return;
        };
        match profiles.folders(device_id) {
            Ok(folders) => self.folders = folders,
            Err(err) => {
                tracing::warn!(device_id, %err, "could not read the folder list");
                self.folders.clear();
            }
        }
        // The name buffer follows the selection until the user types in it.
        if let Some(view) = self.view.selected() {
            if self.name_serial.as_deref() != Some(view.serial.as_str()) {
                self.name_buffer = view.name.clone();
                self.name_serial = Some(view.serial.clone());
            }
        }
    }

    fn selected_device(&self) -> Option<DeviceRow> {
        self.view.selected().and_then(|view| view.row.clone())
    }

    fn selected_device_id(&self) -> Option<i64> {
        self.view
            .selected()
            .and_then(|view| view.row.as_ref().map(|r| r.id))
    }

    /// Run startup recovery once per device, off the UI thread.
    fn recover_new_devices(&mut self) {
        let Some(controller) = self.controller.clone() else {
            return;
        };
        let rows: Vec<DeviceRow> = self
            .view
            .devices()
            .filter_map(|device| device.row.clone())
            .collect();
        for row in rows {
            if !self.recovered.insert(row.id) {
                continue;
            }
            let root = match self.destination_of(&row) {
                Ok(root) => root,
                Err(message) => {
                    tracing::debug!(device_id = row.id, %message, "no destination to recover yet");
                    // Not resolved yet (no global folder chosen): try again later.
                    self.recovered.remove(&row.id);
                    continue;
                }
            };
            let tx = self.tx.clone();
            let device_id = row.id;
            let worker = Arc::clone(&controller);
            let spawn = std::thread::Builder::new()
                .name(format!("recover-{device_id}"))
                .spawn(move || {
                    let message = match worker.recover(device_id, &root) {
                        Ok(report) => UiMessage::Recovered { device_id, report },
                        Err(err) => UiMessage::RecoveryFailed {
                            device_id,
                            message: err.to_string(),
                        },
                    };
                    if tx.send(message).is_err() {
                        tracing::debug!("recovery result dropped: the UI is gone");
                    }
                });
            if let Err(err) = spawn {
                tracing::warn!(device_id, %err, "could not start the recovery thread");
                self.recovered.remove(&device_id);
            }
        }
    }

    fn on_recovered(&mut self, device_id: i64, report: RecoveryReport) {
        if report.had_interrupted_work() {
            let mut parts = Vec::new();
            if !report.partials_removed.is_empty() {
                parts.push(format!(
                    "{} interrupted transfer(s) cleaned up",
                    report.partials_removed.len()
                ));
            }
            if !report.unfinished_jobs_closed.is_empty() {
                parts.push(format!(
                    "{} unfinished backup(s) closed",
                    report.unfinished_jobs_closed.len()
                ));
            }
            if !report.stale_rows.is_empty() {
                parts.push(format!(
                    "{} file(s) will be re-copied",
                    report.stale_rows.len()
                ));
            }
            self.view.set_banner(Some(Banner {
                message: format!("Recovered from an interrupted run: {}", parts.join(", ")),
                blocking: false,
            }));
        }
        self.reload_history(device_id);
    }

    // ---------------------------------------------------------------------
    // Intents: names, destinations, folders
    // ---------------------------------------------------------------------

    fn apply_name(&mut self) {
        self.name_error = None;
        let Some(profiles) = self.profiles.as_ref() else {
            self.name_error = Some("the manifest is unavailable".to_string());
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            return;
        };
        let typed = self.name_buffer.clone();
        match profiles.rename(device_id, &typed) {
            Ok(stored) => {
                self.name_buffer = stored;
                self.refresh_rows_from_db();
                self.load_folders_for_selection();
            }
            Err(err) => self.name_error = Some(err.to_string()),
        }
    }

    /// Ask for a folder with the native dialog, on a background thread: `rfd`
    /// blocks, and blocking here would freeze the window behind the dialog.
    fn pick_folder_async(&self) {
        let tx = self.tx.clone();
        let start_dir = self
            .cfg
            .global_backup_dir
            .clone()
            .unwrap_or_else(|| startup::suggested_backup_dir(&self.cfg));
        spawn_picker(
            move || {
                let mut dialog = rfd::FileDialog::new().set_directory(&start_dir);
                dialog = dialog.set_title("Choose a backup folder");
                dialog.pick_folder().map(UiMessage::FolderPicked)
            },
            tx,
        );
    }

    fn pick_adb_async(&self) {
        let tx = self.tx.clone();
        let name = android_backup_core::adb::locate::adb_binary_name();
        spawn_picker(
            move || {
                rfd::FileDialog::new()
                    .set_title("Choose the adb executable")
                    .pick_file()
                    .map(UiMessage::AdbPicked)
                    .or_else(|| {
                        let _ = name;
                        None
                    })
            },
            tx,
        );
    }

    fn on_folder_picked(&mut self, path: PathBuf) {
        self.location_error = None;
        // Which picker was this? The welcome wizard and the device panel share it,
        // so the answer depends on the selected device.
        if self.view.modal().is_some() {
            self.set_global_backup_dir(path, false);
            return;
        }
        if self.selected_device().is_some() {
            self.set_device_destination(path);
            return;
        }
        self.set_global_backup_dir(path, false);
    }

    /// The backup location: validated before it is stored, so an unwritable drive
    /// never becomes the configured destination.
    ///
    /// `create` is set only for the wizard's suggestion (`~/AndroidBackups` does
    /// not exist on a fresh machine yet). A folder the user picked already exists.
    fn set_global_backup_dir(&mut self, path: PathBuf, create: bool) {
        if create && !self.probe.exists(&path) {
            if let Err(err) = self.probe.ensure_dir(&path) {
                self.location_error = Some(format!("{} cannot be used: {err}", path.display()));
                return;
            }
        }
        if let Err(err) = destination::validate(&*self.probe, &path, None) {
            self.location_error = Some(format!("{} cannot be used: {err}", path.display()));
            return;
        }
        self.cfg.global_backup_dir = Some(path);
        if let Err(message) = startup::save_config(&self.cfg, &self.paths) {
            self.location_error = Some(message);
        }
        self.recover_new_devices();
    }

    fn set_device_destination(&mut self, path: PathBuf) {
        if let Err(err) = destination::validate(&*self.probe, &path, None) {
            self.location_error = Some(format!("{} cannot be used: {err}", path.display()));
            return;
        }
        let Some(profiles) = self.profiles.as_ref() else {
            self.location_error = Some("the manifest is unavailable".to_string());
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            return;
        };
        match profiles.set_destination(device_id, Some(&path)) {
            Ok(()) => self.refresh_rows_from_db(),
            Err(err) => self.location_error = Some(err.to_string()),
        }
    }

    fn clear_device_destination(&mut self) {
        let Some(profiles) = self.profiles.as_ref() else {
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            return;
        };
        match profiles.set_destination(device_id, None) {
            Ok(()) => {
                self.location_error = None;
                self.refresh_rows_from_db();
            }
            Err(err) => self.location_error = Some(err.to_string()),
        }
    }

    fn toggle_folder(&mut self, source_path: &str, enabled: bool) {
        let Some(profiles) = self.profiles.as_ref() else {
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            return;
        };
        if let Err(err) = profiles.set_folder(device_id, source_path, enabled) {
            self.folder_error = Some(err.to_string());
            return;
        }
        self.load_folders_for_selection();
    }

    fn add_custom_folder(&mut self) {
        self.folder_error = None;
        let typed = self.folder_buffer.clone();
        let Some(profiles) = self.profiles.as_ref() else {
            self.folder_error = Some("the manifest is unavailable".to_string());
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            return;
        };
        match profiles.add_folder(device_id, &typed) {
            Ok(_) => {
                self.folder_buffer.clear();
                self.load_folders_for_selection();
            }
            Err(err) => self.folder_error = Some(err.to_string()),
        }
    }

    fn remove_folder(&mut self, source_path: &str) {
        let Some(profiles) = self.profiles.as_ref() else {
            return;
        };
        let Some(device_id) = self.selected_device_id() else {
            return;
        };
        if let Err(err) = profiles.remove_folder(device_id, source_path) {
            self.folder_error = Some(err.to_string());
        }
        self.load_folders_for_selection();
    }

    // ---------------------------------------------------------------------
    // Intents: backups
    // ---------------------------------------------------------------------

    /// The job configuration for one device: destination resolved *and*
    /// validated here, so a dead drive is reported before a job row exists.
    pub fn job_config(&self, device: &DeviceRow) -> Result<JobConfig, String> {
        let Some(profiles) = self.profiles.as_ref() else {
            return Err("the manifest is unavailable, so no backup can run".to_string());
        };
        let others: Vec<DeviceRow> = self
            .view
            .devices()
            .filter_map(|view| view.row.clone())
            .collect();
        let dest = destination::resolve_with_config(device, &others, &self.cfg)
            .map_err(|err| format!("no backup location is configured: {err}"))?;
        self.check_destination_ready(&dest)?;
        let folders = profiles
            .enabled_folders(device.id)
            .map_err(|err| format!("could not read the folder list: {err}"))?;
        if folders.is_empty() {
            return Err(
                "no folders are selected for this device — select at least one".to_string(),
            );
        }
        Ok(JobConfig {
            device_id: device.id,
            serial: device.serial.clone(),
            display_name: device.display_name.clone(),
            root: dest.root,
            folders,
            custom_folders: Vec::new(),
            whole_storage: false,
            // Hash mode is the strongest check; a device without `sha256sum`
            // degrades to recording the local digest rather than failing.
            verify: VerifyMode::Hash,
            // Let the engine detect it: it knows the phone better than the UI.
            storage_root: None,
            free_space_floor: DEFAULT_FREE_SPACE_FLOOR,
        })
    }

    fn start_backup(&mut self) {
        self.start_note = None;
        if let Some(block) = self.start_block() {
            self.start_note = Some(block.message());
            return;
        }
        let Some(device) = self.selected_device() else {
            self.start_note = Some(StartBlock::NoDevice.message());
            return;
        };
        let Some(controller) = self.controller.clone() else {
            self.start_note = Some(StartBlock::NoController.message());
            return;
        };
        let config = match self.job_config(&device) {
            Ok(config) => config,
            Err(message) => {
                self.start_note = Some(message);
                return;
            }
        };
        match controller.start(&device, config) {
            Ok(handle) => {
                tracing::info!(job_id = handle.id, device = %device.serial, "backup started");
                self.view
                    .insert_job(JobView::new(handle.id, device.id, device.serial.clone()));
                self.handles.insert(device.id, handle);
                self.reload_history(device.id);
            }
            Err(err) => self.start_note = Some(format!("the backup did not start: {err}")),
        }
    }

    fn current_job_id(&self) -> Option<i64> {
        let device_id = self.selected_device_id()?;
        if let Some(handle) = self.handles.get(&device_id) {
            return Some(handle.id);
        }
        let job = self.view.job(device_id)?;
        (!job.is_live()).then_some(job.id)
    }

    fn pause_backup(&mut self) {
        let (Some(controller), Some(id)) = (self.controller.clone(), self.current_job_id()) else {
            return;
        };
        match controller.pause(id) {
            Ok(()) => {}
            Err(err) => self.start_note = Some(format!("could not pause: {err}")),
        }
    }

    fn resume_backup(&mut self) {
        let (Some(controller), Some(id)) = (self.controller.clone(), self.current_job_id()) else {
            return;
        };
        match controller.resume(id) {
            Ok(()) => {}
            Err(err) => self.start_note = Some(format!("could not resume: {err}")),
        }
    }

    fn cancel_backup(&mut self) {
        let (Some(controller), Some(id)) = (self.controller.clone(), self.current_job_id()) else {
            return;
        };
        match controller.cancel(id) {
            Ok(()) => {}
            Err(err) => self.start_note = Some(format!("could not cancel: {err}")),
        }
    }

    // ---------------------------------------------------------------------
    // Intents: history
    // ---------------------------------------------------------------------

    /// Select the first device the manifest knows about.
    ///
    /// One phone on the desk needs no click before the Start button is usable, and
    /// a phone that is unplugged right now still shows its history.
    fn select_first_device(&mut self) {
        if self.view.selected().is_some() {
            self.reload_history_for_selection();
            return;
        }
        let first = self
            .view
            .devices()
            .map(|device| device.serial.clone())
            .next();
        if let Some(serial) = first {
            self.select_device(&serial);
        }
    }

    fn reload_history_for_selection(&mut self) {
        let device_id = self
            .view
            .selected()
            .and_then(|view| view.row.as_ref().map(|row| row.id));
        if let Some(device_id) = device_id {
            self.reload_history(device_id);
        }
    }

    /// Re-read a device's history from the manifest. The UI never invents rows:
    /// this is why history survives a restart.
    fn reload_history(&mut self, device_id: i64) {
        let Some(db) = self.db.clone() else {
            return;
        };
        match jobs::history(&db, device_id, HISTORY_PAGE) {
            Ok(rows) => {
                self.history.insert(device_id, rows);
            }
            Err(err) => {
                tracing::warn!(device_id, %err, "could not read the backup history");
                self.view
                    .set_error(format!("could not read the backup history: {err}"));
            }
        }
        match jobs::latest_job(&db, device_id) {
            Ok(row) => {
                self.latest.insert(device_id, row);
            }
            Err(err) => tracing::warn!(device_id, %err, "could not read the last job"),
        }
    }

    fn toggle_history_row(&mut self, job_id: i64) {
        if self.open_job == Some(job_id) {
            self.open_job = None;
            self.job_files.clear();
            return;
        }
        self.open_job = Some(job_id);
        self.files_visible = FILE_PAGE;
        self.job_files.clear();
        let Some(db) = self.db.clone() else {
            return;
        };
        match files::files_for_job(&db, job_id) {
            Ok(rows) => self.job_files = rows,
            Err(err) => {
                tracing::warn!(job_id, %err, "could not read the job's file list");
                self.view
                    .set_error(format!("could not read the file list: {err}"));
            }
        }
    }

    fn load_more_files(&mut self) {
        self.files_visible = self.files_visible.saturating_add(FILE_PAGE);
    }

    // ---------------------------------------------------------------------
    // Intents: adb and first launch
    // ---------------------------------------------------------------------

    fn on_adb_picked(&mut self, path: PathBuf) {
        self.cfg.adb_mode = startup::manual_adb_mode(&path);
        self.relocate_adb();
    }

    fn use_system_adb(&mut self) {
        self.cfg.adb_mode = startup::system_adb_mode();
        self.relocate_adb();
    }

    /// Re-run the locator off the UI thread: it spawns `adb version`.
    fn relocate_adb(&mut self) {
        let cfg = self.cfg.clone();
        let paths = self.paths.clone();
        let tx = self.tx.clone();
        let spawn = std::thread::Builder::new()
            .name("locate-adb".to_string())
            .spawn(move || {
                let result = startup::locate(&cfg, &paths);
                if tx.send(UiMessage::AdbLocated(result)).is_err() {
                    tracing::debug!("adb locate result dropped: the UI is gone");
                }
            });
        if let Err(err) = spawn {
            tracing::warn!(%err, "could not start the adb locator thread");
        }
    }

    fn on_adb_located(&mut self, result: Result<AdbFound, AdbLocateError>) {
        match result {
            Ok(found) => {
                if let Err(message) = startup::save_config(&self.cfg, &self.paths) {
                    self.view.set_error(message);
                }
                self.view.set_adb_summary(startup::adb_summary(&found));
                let runner = startup::runner(&found);
                self.adb = Some(runner.clone() as Arc<dyn Adb>);
                self.remote = Some(Arc::new(
                    android_backup_core::backup::enumerate::AdbRemoteFiles::new(runner.clone()),
                ));
                self.adb_found = Some(found);
                // Rebuild the pieces that needed adb.
                if let (Some(db), Some(remote)) = (self.db.as_ref(), self.remote.as_ref()) {
                    self.controller = Some(Arc::new(BackupController::new(
                        Arc::clone(db),
                        Arc::clone(remote),
                        Arc::clone(&self.probe),
                    )));
                }
                if let Some(db) = self.db.as_ref() {
                    let sink: SharedSink = Arc::new(RepaintingSink {
                        inner: Arc::clone(db) as SharedSink,
                        ctx: Arc::clone(&self.ctx_slot),
                    });
                    self.watcher = Some(DeviceWatcher::spawn(
                        runner as Arc<dyn Adb>,
                        Some(sink),
                        WatcherConfig::from_app_config(&self.cfg),
                    ));
                }
                self.view.dismiss_modal();
                if let Some(adb) = self.adb.clone() {
                    if let Some(message) = startup::server_conflict(adb.as_ref()) {
                        self.view.show_modal(Modal::ServerConflict { message });
                    }
                }
            }
            Err(err) => {
                self.adb = None;
                self.adb_found = None;
                self.controller = None;
                self.remote = None;
                self.view.show_modal(Modal::AdbMissing {
                    message: startup::missing_adb_message(&err),
                });
            }
        }
    }

    /// Accept the suggested backup location from the welcome wizard.
    fn accept_suggested_location(&mut self) {
        let path = startup::suggested_backup_dir(&self.cfg);
        self.set_global_backup_dir(path, true);
        self.view.dismiss_modal();
    }

    fn dismiss_modal(&mut self) {
        self.view.dismiss_modal();
    }

    // ---------------------------------------------------------------------
    // Intents: managed adb installation (E07.1)
    // ---------------------------------------------------------------------

    /// "Install automatically": download platform-tools on a worker thread.
    ///
    /// Everything that takes time or can fail — the manifest fetch, 9 MB of
    /// archive, the checksum, the extraction, `adb version` — happens off the UI
    /// thread, and the only thing this method does is put a progress modal up.
    fn install_adb(&mut self) {
        if self.installing {
            tracing::debug!("an installation is already running");
            return;
        }
        let Some(installer) = self.installer.clone() else {
            // Unreachable from the UI (the button is disabled), but a silent
            // no-op here would be a bug nobody notices.
            tracing::warn!("asked to install adb with no installer configured");
            return;
        };

        self.installing = true;
        self.install_cancelled.store(false, Ordering::SeqCst);
        self.view.set_modal(Modal::installing(&InstallProgress {
            stage: InstallStage::Manifest,
            done: 0,
            total: None,
        }));

        let tx = self.tx.clone();
        let ctx_slot = Arc::clone(&self.ctx_slot);
        let cancelled = Arc::clone(&self.install_cancelled);
        let spawn = std::thread::Builder::new()
            .name("install-adb".to_string())
            .spawn(move || {
                let mut on_progress = |progress: InstallProgress| {
                    // Wake the UI. It only drains messages while it is drawing,
                    // and a download takes minutes on a slow link.
                    if let Some(ctx) = lock(&ctx_slot).as_ref() {
                        ctx.request_repaint();
                    }
                    if tx.send(UiMessage::AdbInstallProgress(progress)).is_err() {
                        return InstallControl::Cancel;
                    }
                    if cancelled.load(Ordering::SeqCst) {
                        InstallControl::Cancel
                    } else {
                        InstallControl::Continue
                    }
                };
                let result = installer.install(&mut on_progress);
                if tx.send(UiMessage::AdbInstalled(result)).is_err() {
                    tracing::debug!("install result dropped: the UI is gone");
                }
            });
        if let Err(err) = spawn {
            self.installing = false;
            self.view
                .set_error(format!("could not start the installer thread: {err}"));
        }
    }

    /// A progress tick: redraw the modal.
    fn on_install_progress(&mut self, progress: InstallProgress) {
        if self.installing {
            self.view.set_modal(Modal::installing(&progress));
        }
    }

    /// The Cancel button. The installer notices on its next progress tick and
    /// takes its staging directory with it.
    fn cancel_adb_install(&mut self) {
        if self.installing {
            tracing::info!("adb install cancelled by the user");
            self.install_cancelled.store(true, Ordering::SeqCst);
        }
    }

    /// The installation thread finished.
    fn on_installed(&mut self, result: Result<InstallReport, InstallError>) {
        self.installing = false;
        match result {
            Ok(report) => {
                tracing::info!(
                    path = %report.adb_path.display(),
                    version = %report.version,
                    revision = %report.revision,
                    "platform-tools installed"
                );
                // Point the configuration at what we installed, then probe it
                // like any other candidate: the installation is only "done"
                // once the binary answers `adb version`.
                report.apply_to(&mut self.cfg);
                if let Err(message) = startup::save_config(&self.cfg, &self.paths) {
                    self.view.set_error(message);
                }
                self.view.set_banner(Some(Banner {
                    message: startup::installed_note(&report),
                    blocking: false,
                }));
                self.relocate_adb();
            }
            Err(InstallError::Cancelled) => {
                tracing::info!("adb install cancelled");
                self.view.dismiss_modal();
                self.view.set_banner(Some(Banner {
                    message: "Installation cancelled — nothing was changed.".to_string(),
                    blocking: false,
                }));
            }
            Err(err) => {
                tracing::warn!(%err, "automatic adb installation failed");
                // Back to the modal the user came from, with the reason and the
                // two ways out (choose a binary, search PATH again).
                self.view.set_modal(Modal::AdbMissing {
                    message: startup::install_failed_message(&err),
                });
            }
        }
    }
}

/// Why the install button is disabled: no installer was configured at startup
/// (an unsupported platform, or the HTTP client could not be built).
const INSTALL_UNAVAILABLE: &str =
    "Automatic installation is unavailable in this build — it cannot \
     download platform-tools. Point the app at an existing adb instead.";

/// Run a native picker on its own thread and deliver the result as a message.
///
/// `rfd` blocks until the dialog is answered; doing that on the UI thread would
/// freeze the whole window, and `egui` would not even repaint behind the dialog.
fn spawn_picker<F>(pick: F, tx: Sender<UiMessage>)
where
    F: FnOnce() -> Option<UiMessage> + Send + 'static,
{
    let spawn = std::thread::Builder::new()
        .name("file-picker".to_string())
        .spawn(move || {
            if let Some(message) = pick() {
                if tx.send(message).is_err() {
                    tracing::debug!("picker result dropped: the UI is gone");
                }
            }
        });
    if let Err(err) = spawn {
        tracing::warn!(%err, "could not start the file picker thread");
    }
}

impl Drop for App {
    fn drop(&mut self) {
        self.stop_background_work();
    }
}

/// Intent names, so the renderer reads as a list of actions rather than a pile of
/// closures. Every one of these is a method call, never work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    Select(String),
    ApplyName,
    PickBackupLocation,
    PickDeviceLocation,
    ClearDeviceLocation,
    ToggleFolder { source_path: String, enabled: bool },
    AddFolder,
    RemoveFolder { source_path: String },
    Start,
    Pause,
    Resume,
    Cancel,
    ToggleHistoryRow(i64),
    LoadMoreFiles,
    DismissModal,
    AcceptSuggestedLocation,
    UseSystemAdb,
    ChooseAdbBinary,
    RetryAdb,
    InstallAdb,
    CancelAdbInstall,
    DismissBanner,
}

impl App {
    /// Perform an intent. The single entry point the widgets call.
    pub fn handle(&mut self, intent: Intent) {
        match intent {
            Intent::Select(serial) => self.select_device(&serial),
            Intent::ApplyName => self.apply_name(),
            Intent::PickBackupLocation | Intent::PickDeviceLocation => self.pick_folder_async(),
            Intent::ClearDeviceLocation => self.clear_device_destination(),
            Intent::ToggleFolder {
                source_path,
                enabled,
            } => self.toggle_folder(&source_path, enabled),
            Intent::AddFolder => self.add_custom_folder(),
            Intent::RemoveFolder { source_path } => self.remove_folder(&source_path),
            Intent::Start => self.start_backup(),
            Intent::Pause => self.pause_backup(),
            Intent::Resume => self.resume_backup(),
            Intent::Cancel => self.cancel_backup(),
            Intent::ToggleHistoryRow(job_id) => self.toggle_history_row(job_id),
            Intent::LoadMoreFiles => self.load_more_files(),
            Intent::DismissModal => self.dismiss_modal(),
            Intent::AcceptSuggestedLocation => self.accept_suggested_location(),
            Intent::UseSystemAdb => self.use_system_adb(),
            Intent::ChooseAdbBinary => self.pick_adb_async(),
            Intent::RetryAdb => self.relocate_adb(),
            Intent::InstallAdb => self.install_adb(),
            Intent::CancelAdbInstall => self.cancel_adb_install(),
            Intent::DismissBanner => self.view.clear_banner(),
        }
    }

    /// Public accessors the renderer needs that are not worth an intent.
    pub fn install_adb_button(&self) -> (bool, &'static str) {
        (self.installer.is_some(), INSTALL_UNAVAILABLE)
    }

    pub fn can_start(&self) -> bool {
        self.start_block().is_none()
    }

    pub fn is_paused(&self) -> bool {
        self.selected_device_id()
            .and_then(|id| self.view.job(id))
            .is_some_and(JobView::is_paused)
    }

    pub fn job_is_live(&self) -> bool {
        self.selected_device_id()
            .and_then(|id| self.view.job(id))
            .is_some_and(JobView::is_live)
    }

    pub fn selected_job(&self) -> Option<&JobView> {
        self.selected_device_id().and_then(|id| self.view.job(id))
    }

    /// The location shown in the device panel, and whether it came from the
    /// device's own setting.
    pub fn selected_destination(&self) -> Option<(PathBuf, bool)> {
        let device = self.selected_device()?;
        let own = device.destination_path.clone();
        let resolved = self.destination_of(&device).ok()?;
        Some((resolved, own.is_some()))
    }

    /// Where the picker starts for this device.
    pub fn default_destination_hint(&self) -> String {
        match self.cfg.global_backup_dir.as_ref() {
            Some(dir) => dir.display().to_string(),
            None => startup::suggested_backup_dir(&self.cfg)
                .display()
                .to_string(),
        }
    }

    /// Does this device have a folder selected at all?
    pub fn has_enabled_folder(&self) -> bool {
        self.folders.iter().any(|folder| folder.enabled)
    }

    pub fn adb_note(&self) -> Option<&'static str> {
        if self.controller.is_some() {
            None
        } else {
            Some("Backups are disabled until adb is located.")
        }
    }

    /// True when the window should show the first-run wizard's own layout.
    pub fn welcome_open(&self) -> bool {
        matches!(self.view.modal(), Some(Modal::Welcome { .. }))
    }
}

/// The default locations list, for the welcome wizard's hint.
pub fn default_locations() -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if let Ok(path) = android_backup_core::config::default_backup_dir() {
        out.push(("Home folder", path.display().to_string()));
    }
    if let Some(dir) = dirs::download_dir() {
        out.push((
            "Downloads",
            dir.join("AndroidBackups").display().to_string(),
        ));
    }
    out
}

/// A device's destination path as text, or the reason there is none.
pub fn destination_text(path: Option<&Path>) -> String {
    match path {
        Some(path) => path.display().to_string(),
        None => "(not set)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::view::ModalKind;
    use android_backup_core::adb::locate::AdbSource;
    use android_backup_core::adb::props::DeviceProps;
    use android_backup_core::backup::enumerate::RemoteFiles;
    use android_backup_core::config::AdbMode;
    use android_backup_core::database::jobs::{JobCounters, JobStatus};
    use android_backup_core::devices::watcher::DeviceEvent;
    use android_backup_core::mock::{MockAdb, MockRemote};
    use std::sync::Arc;

    fn props() -> DeviceProps {
        DeviceProps {
            manufacturer: Some("Samsung".to_string()),
            model: Some("SM-A536B".to_string()),
            product: Some("a53x".to_string()),
            android_version: Some("14".to_string()),
        }
    }

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("tempdir")
    }

    /// An app wired to a mock adb, an in-memory manifest and a temp destination.
    fn app_with(dir: &Path, remote: Arc<dyn RemoteFiles>) -> App {
        let paths = AppPaths::with_root(&dir.join("cfg"), &dir.join("data"));
        let db = Arc::new(Db::open_in_memory().expect("db"));
        db.migrate().expect("migrate");
        let backup_dir = dir.join("backups");
        std::fs::create_dir_all(&backup_dir).expect("backup dir");
        let cfg = AppConfig {
            global_backup_dir: Some(backup_dir),
            ..AppConfig::default()
        };
        let adb = Arc::new(MockAdb::new().always(MockAdb::ok(""))) as Arc<dyn Adb>;
        let found = AdbFound {
            path: PathBuf::from("/usr/bin/adb"),
            version: android_backup_core::adb::locate::AdbVersion {
                major: 35,
                minor: 0,
                patch: 1,
                raw: String::new(),
            },
            source: AdbSource::System,
        };
        App::new(AppDeps {
            paths,
            cfg,
            config_existed: true,
            adb: Some(adb),
            adb_found: Some(found),
            remote: Some(remote),
            db: Some(db),
            probe: Some(Arc::new(LocalFsProbe::new())),
            installer: None,
            watch_devices: false,
        })
    }

    fn app(dir: &Path) -> App {
        app_with(dir, Arc::new(MockRemote::new()))
    }

    /// An app over a *file* manifest, so a second one can be opened on it (a
    /// restart) in the same test.
    fn app_file(dir: &Path) -> App {
        let paths = AppPaths::with_root(&dir.join("cfg"), &dir.join("data"));
        std::fs::create_dir_all(&paths.data_dir).expect("data dir");
        let db = Arc::new(Db::open_file(&paths.data_dir.join("manifest.db")).expect("db"));
        db.migrate().expect("migrate");
        let backup_dir = dir.join("backups");
        std::fs::create_dir_all(&backup_dir).expect("backup dir");
        let cfg = AppConfig {
            global_backup_dir: Some(backup_dir),
            ..AppConfig::default()
        };
        let adb = Arc::new(MockAdb::new().always(MockAdb::ok(""))) as Arc<dyn Adb>;
        App::new(AppDeps {
            paths,
            cfg,
            config_existed: true,
            adb: Some(adb),
            adb_found: None,
            remote: Some(Arc::new(MockRemote::new())),
            db: Some(db),
            probe: Some(Arc::new(LocalFsProbe::new())),
            installer: None,
            watch_devices: false,
        })
    }

    #[test]
    fn a_clean_profile_opens_the_wizard_and_stays_usable() {
        let dir = temp();
        let mut deps = AppDeps::new(
            AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
            AppConfig::default(),
        );
        deps.config_existed = false;
        deps.watch_devices = false;
        let app = App::new(deps);
        assert!(
            app.welcome_open(),
            "the wizard must be the first thing shown"
        );
        assert!(app.view().modal().is_some());
        // No adb, no controller: the app still opens and says so.
        assert!(app.controller().is_none());
        assert!(app.adb_note().is_some());
        assert_eq!(app.view().device_count(), 0);
    }

    #[test]
    fn the_wizard_is_dismissed_once_and_stays_dismissed() {
        let dir = temp();
        let mut deps = AppDeps::new(
            AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
            AppConfig::default(),
        );
        deps.config_existed = false;
        deps.watch_devices = false;
        let mut app = App::new(deps);
        app.handle(Intent::DismissModal);
        assert!(!app.view().has_modal());
        // Re-deriving the same modal must not bring it back this run.
        app.view_mut().show_modal(Modal::Welcome {
            suggested_dir: "/tmp".to_string(),
        });
        assert!(!app.view().has_modal());
        assert!(!app.view().modal_allowed(ModalKind::Welcome));
    }

    #[test]
    fn accepting_the_suggested_location_saves_the_config() {
        let dir = temp();
        let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
        let mut deps = AppDeps::new(paths.clone(), AppConfig::default());
        deps.config_existed = false;
        deps.watch_devices = false;
        let mut app = App::new(deps);
        app.handle(Intent::AcceptSuggestedLocation);
        assert!(paths.config_file.is_file(), "the choice must stick");
        let saved = AppConfig::load(&paths.config_file);
        assert!(saved.global_backup_dir.is_some());
    }

    #[test]
    fn a_backup_cannot_start_without_a_device_or_a_controller() {
        let dir = temp();
        let mut app = app(dir.path());
        app.handle(Intent::Start);
        assert!(app.start_note().is_some());
        assert!(app.selected_job().is_none());
    }

    #[test]
    fn selecting_a_device_loads_its_profile_folders_and_name() {
        let dir = temp();
        let mut app = app(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        app.view_mut()
            .apply_device_event(&DeviceEvent::FirstSeen(Arc::new(
                android_backup_core::devices::DeviceInfo {
                    serial: "SER1".to_string(),
                    state: android_backup_core::devices::DeviceState::Connected,
                    raw_state: android_backup_core::adb::parse::RawState::Device,
                    listing_model: Some("SM-A536B".to_string()),
                    listing_product: None,
                    transport_id: None,
                    props: props(),
                },
            )));
        app.handle(Intent::Select("SER1".to_string()));
        assert_eq!(app.name_buffer(), "Samsung SM-A536B");
        assert!(app.folders().len() > 3);
        assert!(app.has_enabled_folder());
        assert!(
            app.can_start(),
            "an authorized device with folders may start"
        );
    }

    #[test]
    fn a_hostile_name_is_rejected_with_an_inline_message() {
        let dir = temp();
        let mut app = app(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        app.view_mut()
            .apply_device_event(&DeviceEvent::FirstSeen(Arc::new(
                android_backup_core::devices::DeviceInfo {
                    serial: "SER1".to_string(),
                    state: android_backup_core::devices::DeviceState::Connected,
                    raw_state: android_backup_core::adb::parse::RawState::Device,
                    listing_model: None,
                    listing_product: None,
                    transport_id: None,
                    props: props(),
                },
            )));
        app.handle(Intent::Select("SER1".to_string()));
        app.name_buffer_mut().clear();
        app.name_buffer_mut().push_str("Bad/Name");
        app.handle(Intent::ApplyName);
        assert!(
            app.name_error().is_some(),
            "the UI must explain the refusal"
        );
        let row = devices::get_by_id(&db, device.id)
            .expect("query")
            .expect("row");
        assert_ne!(row.display_name, "Bad/Name", "nothing may be written");

        app.name_buffer_mut().clear();
        app.name_buffer_mut().push_str("Kitchen phone");
        app.handle(Intent::ApplyName);
        assert!(app.name_error().is_none());
        let row = devices::get_by_id(&db, device.id)
            .expect("query")
            .expect("row");
        assert_eq!(row.display_name, "Kitchen phone");
    }

    #[test]
    fn a_custom_folder_is_validated_then_stored() {
        let dir = temp();
        let mut app = app(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        app.view_mut().apply_row(device.clone());
        app.handle(Intent::Select("SER1".to_string()));

        app.folder_buffer_mut().clear();
        app.folder_buffer_mut().push_str("../escape");
        app.handle(Intent::AddFolder);
        assert!(app.folder_error().is_some());
        assert!(!app.folders().iter().any(|f| f.source_path == "../escape"));

        app.folder_buffer_mut().clear();
        app.folder_buffer_mut().push_str("Podcasts");
        app.handle(Intent::AddFolder);
        assert!(app.folder_error().is_none());
        assert!(app.folders().iter().any(|f| f.source_path == "Podcasts"));
        assert!(
            app.folder_buffer().is_empty(),
            "the input is cleared on success"
        );

        app.handle(Intent::ToggleFolder {
            source_path: "Podcasts".to_string(),
            enabled: false,
        });
        assert!(
            !app.folders()
                .iter()
                .find(|f| f.source_path == "Podcasts")
                .expect("folder")
                .enabled
        );
        app.handle(Intent::RemoveFolder {
            source_path: "Podcasts".to_string(),
        });
        assert!(!app.folders().iter().any(|f| f.source_path == "Podcasts"));
    }

    #[test]
    fn the_job_config_carries_the_resolved_destination_and_folders() {
        let dir = temp();
        let app = app(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        let config = app.job_config(&device).expect("config");
        assert_eq!(config.device_id, device.id);
        assert_eq!(config.serial, "SER1");
        assert!(config.root.starts_with(dir.path()), "{:?}", config.root);
        assert!(config.folders.contains(&"DCIM".to_string()));
        assert!(config.custom_folders.is_empty());
        assert!(config.storage_root.is_none(), "the engine detects the root");
        assert_eq!(config.free_space_floor, DEFAULT_FREE_SPACE_FLOOR);
        assert_eq!(config.verify, VerifyMode::Hash);
    }

    #[test]
    fn a_missing_destination_stops_the_backup_before_a_job_row_exists() {
        let dir = temp();
        let mut app = app(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        // Point the device at a path whose parent does not exist: an unplugged drive.
        let missing = dir.path().join("gone").join("drive").join("backup");
        devices::set_destination(&db, device.id, Some(&missing)).expect("set destination");
        let device = devices::get_by_id(&db, device.id)
            .expect("query")
            .expect("row");
        let err = app.job_config(&device).expect_err("must be refused");
        assert!(err.contains("cannot be used"), "{err}");
        assert!(app.controller().is_some());
        app.handle(Intent::Start);
        assert!(app.selected_job().is_none());
        assert!(app.start_note().is_some());
        assert_eq!(
            jobs::history(&db, device.id, 10).expect("history").len(),
            0,
            "no job row may be created for a refused start"
        );
    }

    #[test]
    fn history_comes_from_the_manifest_and_survives_a_restart() {
        let dir = temp();
        let device_id;
        {
            let mut app = app_file(dir.path());
            let db = Arc::clone(app.db().expect("db"));
            let device = devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z")
                .expect("device");
            devices::seed_default_folders(&db, device.id).expect("folders");
            device_id = device.id;
            let job = jobs::insert_job(&db, device_id, "2026-01-01T10:00:00Z").expect("job");
            jobs::finalize(
                &db,
                job,
                JobStatus::Completed,
                &JobCounters {
                    scanned: 452,
                    copied: 430,
                    skipped: 20,
                    failed: 2,
                    bytes_copied: 1_073_741_824,
                },
                None,
                "2026-01-01T10:03:05Z",
            )
            .expect("finalize");
            app.handle(Intent::Select("SER1".to_string()));
            app.handle(Intent::ToggleHistoryRow(job));
            assert_eq!(app.history(device_id).len(), 1);
            assert_eq!(app.job_files().len(), 0, "a job with no recorded files");
        }

        // A second app over the same files (a restart) sees the same history.
        let app = app_file(dir.path());
        assert_eq!(app.history(device_id).len(), 1);
        assert_eq!(
            app.latest_job(device_id).map(|row| row.files_copied),
            Some(430)
        );
    }

    #[test]
    fn adb_can_be_re_located_and_the_choice_is_remembered() {
        let dir = temp();
        let mut app = app(dir.path());
        let binary = dir.path().join("adb");
        std::fs::write(&binary, b"#!/bin/sh\n").expect("write script");
        app.handle(Intent::ChooseAdbBinary);
        // The picker runs on another thread and cannot be driven from a test; the
        // message path is what matters here.
        app.tx
            .send(UiMessage::AdbPicked(binary.clone()))
            .expect("send");
        app.drain_events();
        assert_eq!(app.config().adb_mode, startup::manual_adb_mode(&binary));
    }

    #[test]
    fn destination_text_handles_both_cases() {
        assert_eq!(destination_text(None), "(not set)");
        assert_eq!(destination_text(Some(Path::new("/mnt/x"))), "/mnt/x");
    }

    // -----------------------------------------------------------------
    // Managed installation (E07.1)
    // -----------------------------------------------------------------

    /// An app with no adb and a scripted installer, plus the receiver that says
    /// when the installer thread has finished.
    fn app_with_installer(
        dir: &Path,
        script: crate::testutil::ScriptedInstaller,
    ) -> (App, std::sync::mpsc::Receiver<()>) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let mut deps = AppDeps::new(
            AppPaths::with_root(&dir.join("cfg"), &dir.join("data")),
            AppConfig::default(),
        );
        deps.watch_devices = false;
        deps.installer = Some(crate::testutil::installer(script.announcing(done_tx)));
        (App::new(deps), done_rx)
    }

    /// Wait (bounded) for the installer thread to report, then fold the message.
    fn wait_for_install(app: &mut App, done: &std::sync::mpsc::Receiver<()>) {
        done.recv_timeout(Duration::from_secs(5))
            .expect("the installer thread finished");
        app.drain_events();
    }

    #[test]
    fn installing_adb_points_the_config_at_the_managed_binary() {
        let dir = temp();
        let (mut app, done) =
            app_with_installer(dir.path(), crate::testutil::ScriptedInstaller::succeeding());
        assert!(app.install_adb_button().0, "an installer is configured");

        app.handle(Intent::InstallAdb);
        // The modal goes up synchronously, before a single byte is fetched.
        assert!(matches!(
            app.view().modal(),
            Some(Modal::AdbInstalling { .. })
        ));
        wait_for_install(&mut app, &done);

        let report = crate::testutil::sample_report();
        assert_eq!(app.config().adb_mode, AdbMode::Managed);
        assert_eq!(app.config().managed_adb_dir.as_deref(), Some(&*report.dir));
        // The choice is persisted, so the next launch finds the managed adb.
        let saved = AppConfig::load(&app.paths().config_file);
        assert_eq!(saved.adb_mode, AdbMode::Managed);
        assert_eq!(saved.managed_adb_dir, Some(report.dir.clone()));
        // And the user is told what happened, with the version that was verified.
        let banner = app.view().banner().expect("a banner").message.clone();
        assert!(banner.contains("1.0.41"), "{banner}");
        // The modal stays up until the *new* binary answers `adb version`: the
        // install is not done when the bytes land, but when adb runs.
        assert!(matches!(
            app.view().modal(),
            Some(Modal::AdbInstalling { .. })
        ));
    }

    #[test]
    fn a_failed_install_explains_itself_and_keeps_every_route_out_open() {
        let dir = temp();
        let (mut app, done) = app_with_installer(
            dir.path(),
            crate::testutil::ScriptedInstaller::failing(crate::testutil::digest_mismatch()),
        );
        let before = app.config().clone();

        app.handle(Intent::InstallAdb);
        wait_for_install(&mut app, &done);

        match app.view().modal() {
            Some(Modal::AdbMissing { message }) => {
                assert!(message.contains("sha256"), "{message}");
                assert!(message.contains("Choose adb"), "{message}");
            }
            other => panic!("expected the adb modal back, got {other:?}"),
        }
        // Nothing is switched over on a failure, and nothing is written.
        assert_eq!(app.config().adb_mode, before.adb_mode);
        assert!(!app.paths().config_file.exists());
    }

    #[test]
    fn cancelling_an_install_says_nothing_was_changed() {
        let dir = temp();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let installer = Arc::new(
            crate::testutil::ScriptedInstaller::succeeding()
                .gated(gate)
                .announcing(done_tx),
        );
        let mut deps = AppDeps::new(
            AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
            AppConfig::default(),
        );
        deps.watch_devices = false;
        deps.installer = Some(Arc::clone(&installer) as Arc<dyn AdbInstaller>);
        let mut app = App::new(deps);

        app.handle(Intent::InstallAdb);
        app.handle(Intent::CancelAdbInstall);
        // The gate is the download; the installer only notices on the tick after
        // it, which is exactly the behaviour being tested.
        let _ = release.send(());
        wait_for_install(&mut app, &done_rx);

        assert!(installer.was_cancelled(), "the installer was told to stop");
        let banner = app.view().banner().expect("a banner").message.clone();
        assert!(banner.contains("cancelled"), "{banner}");
        assert!(app.view().modal().is_none(), "the modal is gone");
        // A cancel changes nothing: no managed directory claimed, no config file
        // written, so the next launch starts where this one did.
        assert!(app.config().managed_adb_dir.is_none());
        assert!(!app.paths().config_file.exists());
    }

    #[test]
    fn a_second_install_request_while_one_is_running_is_ignored() {
        let dir = temp();
        let (release, gate) = std::sync::mpsc::channel::<()>();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let installer = Arc::new(
            crate::testutil::ScriptedInstaller::succeeding()
                .gated(gate)
                .announcing(done_tx),
        );
        let mut deps = AppDeps::new(
            AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
            AppConfig::default(),
        );
        deps.watch_devices = false;
        deps.installer = Some(Arc::clone(&installer) as Arc<dyn AdbInstaller>);
        let mut app = App::new(deps);

        app.handle(Intent::InstallAdb);
        // The same click again, mid-download.
        app.handle(Intent::InstallAdb);
        let _ = release.send(());
        wait_for_install(&mut app, &done_rx);

        assert_eq!(installer.installs(), 1, "one download, not two");
    }

    #[test]
    fn the_install_button_says_why_when_there_is_no_installer() {
        let dir = temp();
        let app = app(dir.path());
        let (available, note) = app.install_adb_button();
        assert!(!available);
        assert!(note.contains("unavailable"), "{note}");

        // Clicking anyway must not panic or put a stray modal up.
        let mut app = app;
        app.handle(Intent::InstallAdb);
        assert!(app.view().modal().is_none());
    }
}
