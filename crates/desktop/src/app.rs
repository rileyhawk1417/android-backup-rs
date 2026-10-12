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
use std::time::{Duration, Instant};

use android_backup_core::adb::installer::{
    Control as InstallControl, InstallError, InstallReport, Progress as InstallProgress,
    Stage as InstallStage,
};
use android_backup_core::adb::locate::{AdbFound, AdbLocateError};
use android_backup_core::adb::runner::Adb;
use android_backup_core::automation::driver::{AutomationDriver, DriverConfig, SharedNoteSink};
use android_backup_core::automation::policy::Policy;
use android_backup_core::backup::controller::{BackupController, JobHandle};
use android_backup_core::backup::destination::{self, FsProbe, LocalFsProbe};
use android_backup_core::backup::enumerate::RemoteFiles;
use android_backup_core::backup::{JobConfig, JobEvent, RecoveryReport};
use android_backup_core::config::{AppConfig, AppPaths};
use android_backup_core::database::devices::{self, DeviceRow};
use android_backup_core::database::files::{self, BackupFileRow};
use android_backup_core::database::jobs::{self, JobRow};
use android_backup_core::database::Db;
use android_backup_core::devices::profile::ProfileService;
use android_backup_core::devices::watcher::{DeviceSnapshotHandle, DeviceWatcher, WatcherConfig};
use android_backup_core::devices::{DeviceEvent, DeviceInfo, DeviceSink, SharedSink};

use crate::automation::{AppHost, RepaintingNote};
use crate::jobconfig;
use crate::notifications::{AutomationToasts, Notice, Notifications, Notifier};
use crate::picker::{NativePicker, Picker};
use crate::startup::{self, AdbInstaller};
use crate::tray::{self, TrayCommand, TrayHandle, TrayState};
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
    /// The file and folder dialogs. `None` uses the real ones; a test injects a
    /// scripted picker so that no window is ever drawn (see [`crate::picker`]).
    pub picker: Option<Arc<dyn Picker>>,
    /// OS notifications. `None` drops them, which is what a headless run wants;
    /// the application installs the real one in `main`, and a test installs a
    /// recorder (see [`crate::notifications`]).
    pub notifier: Option<Arc<dyn Notifier>>,
    /// Start the device watcher. Tests turn this off when they drive events by
    /// hand; everything else goes through the real thread.
    pub watch_devices: bool,
    /// Builds the tray icon, on the thread that owns the event loop (E09.5). `None`
    /// means "this run has no tray" — which is what every headless test wants, and
    /// what the application passes when the user has turned `background.tray_icon`
    /// off. A factory rather than an icon because the icon binds to the thread it is
    /// created on, and because a desktop that cannot have one (a bare GNOME session)
    /// must be able to say so *before* the window decides how closing behaves.
    pub tray: Option<TrayFactory>,
    /// Builds the automation clock. `None` in a run with no controller or no
    /// manifest — nothing to schedule — and in tests that drive the policy by hand.
    pub automation: bool,
}

/// How the application gets a tray icon: handed a channel for menu clicks and the
/// current pause state, it either produces an icon or explains why it cannot.
///
/// A boxed closure so that `main` passes [`tray::spawn`] and a test passes a
/// recording double, with no trait object ceremony in between.
pub type TrayFactory = Box<dyn FnOnce(crate::tray::Wiring) -> Result<Box<dyn TrayHandle>, String>>;

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
            picker: None,
            notifier: None,
            watch_devices: true,
            tray: None,
            automation: false,
        }
    }
}

/// A modal or a picker result that arrived from a background thread.
pub(crate) enum UiMessage {
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
    /// Automation started a backup on its own thread (E09.5) and is handing the
    /// window the one thing it needs to show and control it.
    AutomationStarted {
        device_id: i64,
        serial: String,
        handle: JobHandle,
    },
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
    /// The dialogs. Real in the application, scripted in the tests.
    picker: Arc<dyn Picker>,
    /// Where the app speaks when nobody is looking at the window.
    notifications: Arc<Notifications>,
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

    // -- background operation (Wave 9, E09.5) ------------------------------
    /// The tray icon, when this desktop has one. `None` is a supported state:
    /// closing the window then quits, exactly as it did before Wave 9.
    tray: Option<Box<dyn TrayHandle>>,
    /// Menu clicks, forwarded from the shell's thread by [`crate::tray`].
    tray_rx: Receiver<TrayCommand>,
    /// The window is hidden in the tray and has stopped drawing.
    hidden: bool,
    /// The automation clock, kept alive here so that dropping the app stops it.
    automation: Option<AutomationDriver>,
    /// The policy the clock ticks, shared so that device events arriving on this
    /// thread go through the same rulebook the clock uses.
    policy: Option<Arc<Mutex<Policy>>>,
    /// The host both of them call. Kept as a field because the UI thread drives it
    /// for [`DeviceEvent`]s.
    host: Option<Arc<AppHost>>,
    /// The same toast sink the clock holds, so notes raised on this thread go
    /// through the same delivery policy (E09.4).
    toasts: Option<SharedNoteSink>,

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
    /// Set when the user asked to quit from the tray: the window must then really
    /// close, rather than hide itself as `close_to_tray` would have it do.
    quitting: bool,
    /// A `--hidden` start that has not been honoured yet (E09.5).
    start_hidden: bool,
}

/// The name the panel is showing for the device a job belongs to, so a toast says
/// what the window would have said.
///
/// A free function rather than a method: the job pump is walking the event channels
/// while it asks, and a `&self` method there would borrow the whole app.
fn device_label(view: &ViewState, device_id: i64) -> String {
    let serial = view.job(device_id).map(|job| job.serial.clone());
    serial
        .as_deref()
        .and_then(|serial| view.device(serial))
        .map(|device| device.name.clone())
        .filter(|name| !name.trim().is_empty())
        .or(serial)
        .unwrap_or_else(|| "the phone".to_string())
}

/// What one job event means to someone who may not be looking at the window.
///
/// `None` for what the panel is already saying in front of the user, and for the
/// news that is not news: a pause the user asked for, and a cancellation. Pure and
/// free-standing, so the wording and the name can be asserted without a job, a
/// channel or a desktop.
fn job_notice(view: &ViewState, device_id: i64, event: &JobEvent) -> Option<Notice> {
    match event {
        JobEvent::Finished(outcome) => {
            let kept = view.job(device_id).map(|job| job.archived).unwrap_or(0);
            Notice::finished(device_label(view, device_id), outcome, kept)
        }
        JobEvent::Paused { reason, file } => {
            Notice::paused(device_label(view, device_id), *reason, file.as_deref())
        }
        JobEvent::Fatal { message } => Some(Notice::fatal(device_label(view, device_id), message)),
        _ => None,
    }
}

/// Said once, when the desktop cannot show a tray icon but the configuration asks
/// for close-to-tray. Dismissible, and it does not come back for the rest of the
/// session — the situation does not change, but repeating it every launch would be
/// nagging, and the configuration file is the durable answer.
fn no_tray_banner() -> String {
    "This desktop has no place to put a tray icon, so closing the window will quit \
     rather than keep Android Backup running. On Linux, a panel extension that \
     supports status icons (appindicator) makes the tray work; otherwise set \
     background.close_to_tray = false to stop being told about it."
        .to_string()
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
            picker,
            notifier,
            watch_devices,
            tray: tray_factory,
            automation: run_automation,
        } = deps;
        let probe: Arc<dyn FsProbe> = probe.unwrap_or_else(|| Arc::new(LocalFsProbe::new()));
        let picker: Arc<dyn Picker> = picker.unwrap_or_else(|| Arc::new(NativePicker));
        let notifications = Arc::new(Notifications::new(notifier));
        let (tx, rx) = channel();
        let (tray_tx, tray_rx) = tray::channel();
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
            picker,
            notifications,
            installing: false,
            install_cancelled: Arc::new(AtomicBool::new(false)),
            ctx_slot,
            tray: None,
            tray_rx,
            hidden: false,
            automation: None,
            policy: None,
            host: None,
            toasts: None,
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
            quitting: false,
            start_hidden: false,
        };

        // The tray, then automation behind it (E09.5). The order matters: whether
        // there is a tray icon decides what closing the window means, so the icon is
        // created first and its absence is a fact the rest of the run can read.
        app.install_tray(tray_factory, tray_tx);
        if run_automation {
            app.start_automation();
        }

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

    // ---------------------------------------------------------------------
    // Background operation (Wave 9, E09.5)
    // ---------------------------------------------------------------------

    /// Build the tray icon, if this run may have one.
    ///
    /// Called once, at construction, on the thread that will own the event loop —
    /// the shell requires the icon to be created on the thread that pumps its
    /// messages, which is the thread `main` builds the app on.
    ///
    /// A failure is *not* fatal and is not silent either: without a tray, closing the
    /// window can only quit, because hiding a window the user cannot get back to is
    /// worse than quitting it. The banner says so once, which is the one message this
    /// feature owes anybody who asked for close-to-tray on a desktop that cannot do
    /// it.
    fn install_tray(&mut self, factory: Option<TrayFactory>, commands: Sender<TrayCommand>) {
        let Some(factory) = factory else {
            return;
        };
        if !self.cfg.background.tray_icon {
            tracing::info!("no tray icon: switched off in the configuration file");
            return;
        }
        let wiring = crate::tray::Wiring {
            commands,
            automation_paused: self.cfg.automation.paused,
            ctx: Arc::clone(&self.ctx_slot),
        };
        match factory(wiring) {
            Ok(tray) => {
                tray.set_state(self.tray_state());
                self.tray = Some(tray);
            }
            Err(err) => {
                tracing::warn!(%err, "no system tray icon");
                if self.cfg.background.close_to_tray {
                    self.view.set_banner(Some(Banner {
                        message: no_tray_banner(),
                        blocking: false,
                    }));
                }
            }
        }
    }

    /// Start the automation clock (E09.3) and the host it drives.
    ///
    /// Started after the tray, because the two are one feature: the clock is what
    /// makes closing the window meaningful. Absent when there is nothing to drive —
    /// no manifest, or no controller, both of which are supported states where
    /// automation simply cannot do anything and must not pretend otherwise.
    fn start_automation(&mut self) {
        let (Some(db), Some(controller)) = (self.db.clone(), self.controller.clone()) else {
            tracing::info!("automation is off: no manifest or no controller to drive it");
            return;
        };

        // With no watcher the clock has no device list to tick over, and is handed an
        // empty one: the *events* still work, because a caller feeding them by hand
        // supplies the device with them. Nothing else changes, and the alternative —
        // refusing to build the host at all — would make the policy untestable
        // without a real `adb` polling thread.
        let devices = match &self.watcher {
            Some(watcher) => watcher.snapshot_handle(),
            None => {
                tracing::info!("automation clock has no device watcher: only fed events will act");
                DeviceSnapshotHandle::empty()
            }
        };

        let host = Arc::new(AppHost::new(
            Arc::clone(&db),
            self.cfg.clone(),
            Arc::clone(&controller),
            Arc::clone(&self.probe),
            devices,
            self.tx.clone(),
        ));
        self.host = Some(Arc::clone(&host));

        // Toasts, with the manifest behind them so a note can name the phone the way
        // the panel does (E09.4). The same sink serves the clock's notes and the ones
        // device events produce on this thread, so both go through one delivery policy.
        let toasts: SharedNoteSink = Arc::new(AutomationToasts::new(
            Arc::clone(&self.notifications),
            Some(Arc::clone(&db)),
        ));
        let sink: SharedNoteSink = Arc::new(RepaintingNote::new(
            Arc::clone(&toasts),
            Arc::clone(&self.ctx_slot),
        ));
        self.toasts = Some(toasts);

        let policy = Arc::new(Mutex::new(Policy::new()));
        self.policy = Some(Arc::clone(&policy));
        let driver =
            AutomationDriver::spawn(host, policy, sink, DriverConfig::from_app_config(&self.cfg));
        driver.set_paused(self.cfg.automation.paused);
        tracing::info!(
            interval_secs = driver.interval().as_secs(),
            paused = driver.is_paused(),
            "automation clock started"
        );
        self.automation = Some(driver);

        // A device that is already plugged in when the app starts is not "new" to
        // the watcher, so no `FirstSeen` event will arrive for it. The clock covers it
        // (a tick is at most one interval away), which is why an immediate tick at
        // startup is not needed here.
    }

    /// What the tray icon should be showing. Pure, so a test can assert the mapping
    /// without a desktop.
    pub fn tray_state(&self) -> TrayState {
        crate::tray::state_of(&self.view)
    }

    /// Is there a tray icon? Closing the window means something different either
    /// way, and the UI says so.
    pub fn has_tray(&self) -> bool {
        self.tray.is_some()
    }

    /// Are automatic backups paused? The toggle's state, for the tray menu and the
    /// status strip.
    pub fn automation_paused(&self) -> bool {
        self.cfg.automation.paused
    }

    /// Is the window currently hidden in the tray?
    pub fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Start with the window hidden (the login entry's `--hidden`, E09.5).
    ///
    /// Only a *request*: the window is hidden on the first frame, which is the first
    /// moment the context exists to be told, and only if there is a tray icon to get
    /// it back. A hidden window with no way to show it would be a trap, so a run with
    /// no tray opens normally and says why in the log.
    pub fn start_hidden(&mut self) {
        self.start_hidden = true;
    }

    /// The first-frame half of [`App::start_hidden`].
    fn honour_start_hidden(&mut self) {
        if !std::mem::take(&mut self.start_hidden) {
            return;
        }
        if self.tray.is_some() {
            self.hide_to_tray();
        } else {
            tracing::info!(
                "--hidden was asked for, but there is no tray icon to restore the window from; \
                 opening it instead"
            );
        }
    }

    /// Does closing the window hide it rather than quit?
    pub fn close_to_tray(&self) -> bool {
        self.cfg.background.close_to_tray && self.tray.is_some() && !self.quitting
    }

    /// Hide the window, leaving the process (and automation) running.
    fn hide_to_tray(&mut self) {
        self.hidden = true;
        // The window stops drawing, so nobody is looking at it: the frame loop cannot
        // say so any more, and the notification policy has to be told by hand.
        self.notifications.set_window_visible(false);
        if let Some(ctx) = lock(&self.ctx_slot).clone() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(false));
        }
        if let Some(tray) = &self.tray {
            tray.set_state(self.tray_state());
        }
        tracing::info!("window hidden to the tray");
    }

    /// Bring the window back. Harmless when it is already up.
    pub fn show_window(&mut self) {
        self.hidden = false;
        if let Some(ctx) = lock(&self.ctx_slot).clone() {
            // Order matters: an invisible window is not focused, so it has to be shown
            // before it can be raised.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
    }

    /// Ask the event loop to close the window for good.
    ///
    /// The tray's Quit, and nothing else: closing the window by its own button goes
    /// through `close_to_tray` and may merely hide it. `closing` is set here as well
    /// as in the frame loop so that the frame which arrives from the `Close` command
    /// does not treat itself as a close *request* and hide the window on the way out.
    pub fn quit(&mut self) {
        self.quitting = true;
        self.closing = true;
        self.stop_background_work();
        if let Some(ctx) = lock(&self.ctx_slot).clone() {
            // A hidden window may not draw again on its own; showing it first makes
            // sure this command is processed.
            ctx.send_viewport_cmd(egui::ViewportCommand::Visible(true));
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }

    /// Drain the tray's channel: one menu click, one intent.
    fn drain_tray(&mut self, ctx: &egui::Context) -> bool {
        let mut changed = false;
        while let Ok(command) = self.tray_rx.try_recv() {
            changed = true;
            tracing::debug!(?command, "tray command");
            match command {
                // Showing the window is the window's business, not an intent: see
                // [`crate::tray::intents_of`].
                TrayCommand::Open => {
                    self.show_window();
                    ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
                }
                other => {
                    for intent in crate::tray::intents_of(other) {
                        self.handle(intent);
                    }
                }
            }
        }
        changed
    }

    /// Pause or resume every device's schedule at once (the tray's toggle).
    ///
    /// A brake, not a reset: per-device schedules are untouched, and a device that
    /// came due while automation was paused is still due when it resumes. The setting
    /// is persisted, because a pause that lifts itself on the next restart — which is
    /// exactly when an unattended app gets restarted — is not a pause.
    fn toggle_automation(&mut self) {
        let paused = !self.cfg.automation.paused;
        self.cfg.automation.paused = paused;
        if let Some(driver) = &self.automation {
            driver.set_paused(paused);
        }
        if let Some(tray) = &self.tray {
            tray.set_automation_paused(paused);
        }
        if let Err(err) = self.cfg.save(&self.paths.config_file) {
            tracing::warn!(%err, "could not save the paused setting");
            self.start_note = Some(format!("the setting could not be saved: {err}"));
            return;
        }
        // The status strip, until E09.6 gives automation a settings panel of its own.
        self.start_note = Some(if paused {
            "Automatic backups are paused. Devices keep their schedules; nothing is forgotten."
                .to_string()
        } else {
            "Automatic backups are on again.".to_string()
        });
    }

    /// Take over a job the automation clock started on its own thread.
    ///
    /// The handle arrives by channel because the clock cannot touch the view. From
    /// here the job is indistinguishable from a manual one: same panel, same Pause and
    /// Cancel, same history. The manifest already says it was automatic (the
    /// [`JobTrigger`](android_backup_core::database::jobs::JobTrigger) column), so
    /// nothing about the row has to be rewritten.
    fn adopt_job(&mut self, device_id: i64, serial: String, handle: JobHandle) {
        if let Some(existing) = self.handles.get(&device_id) {
            // The controller allows one job per device, so this cannot happen through
            // it; if it ever does, the newer job is the one with a live sender, and the
            // older handle is dropped rather than allowed to shadow it.
            tracing::warn!(
                device_id,
                old_job = existing.id,
                new_job = handle.id,
                "adopting an automatic job while another handle existed"
            );
        }
        tracing::info!(job_id = handle.id, %serial, "automatic backup adopted by the window");
        self.view
            .insert_job(JobView::new(handle.id, device_id, serial));
        self.handles.insert(device_id, handle);
        self.reload_history(device_id);
    }

    /// Feed device events to the scheduling policy (E09.2), on this thread.
    ///
    /// The policy is shared with the clock; this is the half of its input that comes
    /// from the watcher rather than from the passage of time. The lock ordering the
    /// core documents is kept: the policy lock is taken first, and the host is called
    /// underneath it.
    fn feed_automation(&mut self, events: &[DeviceEvent]) {
        let (Some(policy), Some(host)) = (self.policy.clone(), self.host.clone()) else {
            return;
        };
        if self.cfg.automation.paused {
            // A pause silences starts *and* resumptions. Device state is still folded
            // into the policy so that unpausing needs no replug.
            let mut guard = lock(&policy);
            for event in events {
                let _ = guard.on_event(host.as_ref(), event, Instant::now());
            }
            return;
        }
        let notes = {
            let mut guard = lock(&policy);
            let mut notes = Vec::new();
            for event in events {
                notes.extend(guard.on_event(host.as_ref(), event, Instant::now()));
            }
            notes
        };
        let Some(toasts) = &self.toasts else {
            return;
        };
        for note in notes {
            toasts.note(note);
        }
    }

    /// One frame: drain, react, render.
    pub fn frame(&mut self, ctx: &egui::Context) {
        if lock(&self.ctx_slot).is_none() {
            *lock(&self.ctx_slot) = Some(ctx.clone());
        }
        // A login start (`--hidden`) hides the window before the first draw when —
        // and only when — there is an icon to bring it back.
        self.honour_start_hidden();

        if !self.closing && ctx.input(|input| input.viewport().close_requested()) {
            if self.close_to_tray() {
                // The close button hides the window; the process, the watcher and the
                // clock all keep running. `CancelClose` is what makes that possible: the
                // window is not destroyed, so nothing has to be rebuilt when it comes
                // back.
                ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
                self.hide_to_tray();
            } else {
                self.closing = true;
                self.stop_background_work();
            }
        }

        // Tell the notification policy whether anybody is watching (E09.4). A hidden
        // window stops drawing frames, which is why `hide_to_tray` says so itself
        // rather than relying on this line forever being reached.
        let watching = ctx.input(|input| {
            let viewport = input.viewport();
            viewport.focused.unwrap_or(true) && !viewport.minimized.unwrap_or(false)
        });
        self.notifications
            .set_window_visible(watching && !self.hidden);

        let changed = self.drain_events() | self.drain_tray(ctx);
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

        // The tray icon follows the view, once per frame and only when it changed:
        // `TrayHandle::set_state` is expected to be cheap and idempotent, and this is
        // also how the clock's work gets reflected in an icon nobody clicked.
        if let Some(tray) = &self.tray {
            tray.set_state(self.tray_state());
        }

        crate::ui::draw(self, ctx);
    }

    /// Stop the watcher and join its thread. Also called on drop.
    ///
    /// The automation clock stops too, and does so *first*: it is the one thing that
    /// starts work, and it must not start a job on its way out of a database this
    /// function is about to have pruned out from under it.
    pub fn stop_background_work(&mut self) {
        if let Some(mut driver) = self.automation.take() {
            driver.stop();
        }
        if let Some(mut watcher) = self.watcher.take() {
            watcher.stop();
        }
        // Running jobs next: the clock has been stopped, so nothing new can start
        // while they are being unwound.
        self.cancel_live_jobs();
        if let Some(tray) = self.tray.take() {
            // A crashed icon lingers in the notification area until the user hovers
            // over it; removing it on the way out is the difference between a tidy
            // process and a ghost.
            tray.close();
        }
        if let Some(controller) = self.controller.clone() {
            controller.prune_finished();
        }
    }

    /// Cancel every job still running and wait for its thread (E09.5).
    ///
    /// Quitting is the one moment the application can still choose how a live backup
    /// ends. Without this the rows for those jobs would be left saying "running", and
    /// the next launch would have to recover them as interrupted (E05.6) — which is
    /// the right answer for a crash and the wrong one for a user who pressed Quit:
    /// they asked for the backup to stop, not for it to be reported as a failure. The
    /// cancel is cooperative and the transfer checks it between files, so the wait
    /// here is bounded by one file rather than by the whole job.
    fn cancel_live_jobs(&self) {
        let Some(controller) = self.controller.clone() else {
            return;
        };
        for (job_id, device_id) in controller.active_jobs() {
            match controller.cancel(job_id) {
                Ok(()) => {}
                // Finished in the moment between listing it and asking: nothing to do,
                // and nothing wrong.
                Err(err) => {
                    tracing::debug!(job = job_id, %err, "no need to cancel a job on exit");
                    continue;
                }
            }
            if let Err(err) = controller.join(job_id) {
                tracing::warn!(job = job_id, %err, "could not join a cancelled job");
                continue;
            }
            tracing::info!(
                job = job_id,
                device_id,
                "cancelled a running backup on exit"
            );
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
                // The same events drive automation (E09.2): a phone that has just
                // been plugged in and enrolled is exactly what an `OnConnect`
                // schedule is waiting for.
                self.feed_automation(&events);
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
            UiMessage::AutomationStarted {
                device_id,
                serial,
                handle,
            } => self.adopt_job(device_id, serial, handle),
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
        // Notices are collected and shown after the loop: announcing borrows all of
        // `self`, and the loop is holding the event channels.
        let mut notices: Vec<Notice> = Vec::new();
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
                if let Some(notice) = job_notice(&self.view, *device_id, event) {
                    notices.push(notice);
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
        for notice in notices {
            self.notifications.announce(notice);
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
        let picker = Arc::clone(&self.picker);
        let start_dir = self
            .cfg
            .global_backup_dir
            .clone()
            .unwrap_or_else(|| startup::suggested_backup_dir(&self.cfg));
        spawn_picker(
            move || {
                picker
                    .folder("Choose a backup folder", Some(&start_dir))
                    .map(UiMessage::FolderPicked)
            },
            tx,
        );
    }

    fn pick_adb_async(&self) {
        let tx = self.tx.clone();
        let picker = Arc::clone(&self.picker);
        // Pre-filled, because the file is called `adb` (or `adb.exe`) and the dialog
        // is looking at whatever folder it last remembered.
        let suggested = android_backup_core::adb::locate::adb_binary_name();
        spawn_picker(
            move || {
                picker
                    .file("Choose the adb executable", None, Some(suggested))
                    .map(UiMessage::AdbPicked)
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
    ///
    /// Thin wrapper over [`jobconfig::for_device`], which is the single source of
    /// truth for this decision because the automation driver asks the same
    /// question with no window in front of anyone (E09.5).
    pub fn job_config(&self, device: &DeviceRow) -> Result<JobConfig, String> {
        let Some(db) = self.db.as_ref() else {
            return Err("the manifest is unavailable, so no backup can run".to_string());
        };
        jobconfig::for_device(db, &self.cfg, &self.probe, device)
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
        if let Err(message) = self.start_device(&device) {
            self.start_note = Some(message);
        }
    }

    /// Start one device's backup, by its manifest row.
    ///
    /// The body of the Start button, extracted because the tray's "back up all
    /// connected now" needs exactly the same thing for each device it finds.
    fn start_device(&mut self, device: &DeviceRow) -> Result<(), String> {
        let Some(controller) = self.controller.clone() else {
            return Err(StartBlock::NoController.message());
        };
        let config = self.job_config(device)?;
        match controller.start(device, config) {
            Ok(handle) => {
                tracing::info!(job_id = handle.id, device = %device.serial, "backup started");
                self.view
                    .insert_job(JobView::new(handle.id, device.id, device.serial.clone()));
                self.handles.insert(device.id, handle);
                self.reload_history(device.id);
                Ok(())
            }
            Err(err) => Err(format!("the backup did not start: {err}")),
        }
    }

    /// Back up every connected device that can be backed up right now.
    ///
    /// The tray's one-click version of "back up my phone". Devices that cannot start
    /// are *named* rather than silently skipped: the user asked for all of them, and
    /// "nothing happened" is not an answer. Failures do not stop the others — one
    /// phone with an unplugged destination must not cancel the rest of the batch.
    fn start_all_connected(&mut self) {
        self.start_note = None;
        let candidates: Vec<DeviceRow> = self
            .view
            .devices()
            .filter(|device| device.is_ready())
            .filter_map(|device| device.row.clone())
            .collect();
        if candidates.is_empty() {
            self.start_note = Some(
                "No phone is ready to back up right now. See the device list for why.".to_string(),
            );
            return;
        }

        let mut started = Vec::new();
        let mut refused = Vec::new();
        for device in &candidates {
            let name = self
                .view
                .device(&device.serial)
                .map(|view| view.name.clone())
                .unwrap_or_else(|| device.serial.clone());
            match self.start_device(device) {
                Ok(()) => started.push(name),
                Err(message) => refused.push(format!("{name}: {message}")),
            }
        }

        self.start_note = Some(match (started.len(), refused.is_empty()) {
            (0, _) => format!("Nothing started. {}", refused.join(" ")),
            (n, true) => format!("Backing up {n} device(s)."),
            (n, false) => format!(
                "Backing up {n} device(s). Not started — {}",
                refused.join(" ")
            ),
        });
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
    ToggleFolder {
        source_path: String,
        enabled: bool,
    },
    AddFolder,
    RemoveFolder {
        source_path: String,
    },
    Start,
    Pause,
    Resume,
    Cancel,
    /// Back up every connected device that can be backed up, now (the tray's item).
    StartAllConnected,
    /// Pause or resume automation as a whole (the tray's item).
    ToggleAutomation,
    /// Leave the application for good, from the tray.
    Quit,
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
            Intent::StartAllConnected => self.start_all_connected(),
            Intent::ToggleAutomation => self.toggle_automation(),
            Intent::Quit => self.quit(),
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
    use crate::notifications::NoticeKind;
    use crate::testutil::{self, PickerCall, RecordingNotifier, RecordingTray, ScriptedPicker};
    use crate::tray::TrayCommand;
    use crate::view::ModalKind;
    use android_backup_core::adb::locate::AdbSource;
    use android_backup_core::adb::props::DeviceProps;
    use android_backup_core::automation::Schedule;
    use android_backup_core::backup::compare::Reason;
    use android_backup_core::backup::enumerate::RemoteFiles;
    use android_backup_core::backup::transfer::VerifyMode;
    use android_backup_core::backup::{JobOutcome, PauseReason, DEFAULT_FREE_SPACE_FLOOR};
    use android_backup_core::config::AdbMode;
    use android_backup_core::database::jobs::{JobCounters, JobStatus, JobTrigger};
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
    ///
    /// Its dialogs are scripted to cancel: nothing a test can do draws a window.
    fn app_with(dir: &Path, remote: Arc<dyn RemoteFiles>) -> App {
        app_deps(dir, remote, testutil::picker(ScriptedPicker::cancelling()))
    }

    /// An app whose dialogs answer with `picker` — the only way to drive
    /// `ChooseAdbBinary`/`PickBackupLocation` without a desktop appearing.
    fn app_with_picker(dir: &Path, picker: Arc<dyn Picker>) -> App {
        app_deps(dir, Arc::new(MockRemote::new()), picker)
    }

    /// An app whose toasts are recorded rather than shown.
    fn app_with_toasts(dir: &Path) -> (App, Arc<RecordingNotifier>) {
        let recorder = testutil::recorder();
        let mut app = app_deps(
            dir,
            Arc::new(MockRemote::new()),
            testutil::picker(ScriptedPicker::cancelling()),
        );
        app.notifications = Arc::new(crate::notifications::Notifications::new(Some(Arc::clone(
            &recorder,
        )
            as Arc<dyn Notifier>)));
        (app, recorder)
    }

    /// An app with a recording tray icon and the automation clock running
    /// (E09.5).
    ///
    /// The tray is a double because a real icon needs a desktop, but everything else
    /// is the real thing: the real policy, the real driver, the real host, the real
    /// controller. `watch_devices` is off and events are fed by hand, which is the
    /// only difference from a phone being plugged in.
    fn app_with_tray(dir: &Path) -> (App, std::rc::Rc<RecordingTray>, Arc<RecordingNotifier>) {
        app_with_tray_and(dir, Arc::new(MockRemote::new()))
    }

    /// The same, with a remote that behaves the way a test needs it to — a slow
    /// pull, a deleted file, a phone that says no.
    fn app_with_tray_and(
        dir: &Path,
        remote: Arc<dyn RemoteFiles>,
    ) -> (App, std::rc::Rc<RecordingTray>, Arc<RecordingNotifier>) {
        let recorder = testutil::recorder();
        let (factory, tray) = testutil::tray();
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
        let app = App::new(AppDeps {
            paths,
            cfg,
            config_existed: true,
            adb: Some(adb),
            adb_found: None,
            remote: Some(remote),
            db: Some(db),
            probe: Some(Arc::new(LocalFsProbe::new())),
            installer: None,
            picker: Some(testutil::picker(ScriptedPicker::cancelling())),
            notifier: Some(Arc::clone(&recorder) as Arc<dyn Notifier>),
            watch_devices: false,
            tray: Some(factory),
            automation: true,
        });
        (app, tray, recorder)
    }

    /// One frame, as the event loop would run it.
    fn frame(app: &mut App) {
        frame_with(app, egui::RawInput::default());
    }

    /// One frame with the window's close button pressed, which is the only way to
    /// exercise close-to-tray: the flag arrives as a viewport event.
    fn close_button(app: &mut App) {
        let mut input = egui::RawInput::default();
        let mut viewport = egui::ViewportInfo::default();
        viewport.events.push(egui::ViewportEvent::Close);
        input.viewports.insert(egui::ViewportId::ROOT, viewport);
        frame_with(app, input);
    }

    fn frame_with(app: &mut App, input: egui::RawInput) {
        let ctx = egui::Context::default();
        // The output is where the viewport commands land, which is what the window
        // thread would act on; asserting on them is `ui_headless`'s job, not this one.
        let _ = ctx.run(input, |ctx| app.frame(ctx));
    }

    /// A device the way the watcher reports one that can be backed up.
    fn connected(serial: &str) -> Arc<android_backup_core::devices::DeviceInfo> {
        Arc::new(android_backup_core::devices::DeviceInfo {
            serial: serial.to_string(),
            state: android_backup_core::devices::DeviceState::Connected,
            raw_state: android_backup_core::adb::parse::RawState::Device,
            listing_model: Some("SM-A536B".to_string()),
            listing_product: None,
            transport_id: None,
            props: props(),
        })
    }

    fn app_deps(dir: &Path, remote: Arc<dyn RemoteFiles>, picker: Arc<dyn Picker>) -> App {
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
            picker: Some(picker),
            notifier: None,
            watch_devices: false,
            tray: None,
            automation: false,
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
            picker: Some(testutil::picker(ScriptedPicker::cancelling())),
            notifier: None,
            watch_devices: false,
            tray: None,
            automation: false,
        })
    }

    /// Pump the message queue the way the frame loop does, until `predicate` holds.
    ///
    /// A picker answers from its own thread — in the application it is sitting behind
    /// a window — so a test has to pump the same queue the window pumps.
    fn wait_until(app: &mut App, mut predicate: impl FnMut(&App) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            app.drain_events();
            if predicate(app) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the dialog's answer never arrived"
            );
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
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
    fn a_finished_backup_is_announced_when_nobody_is_looking() {
        let dir = temp();
        let (mut app, toasts) = app_with_toasts(dir.path());
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
        // Behind the tray: the panel is not on screen, which is the whole reason a
        // toast exists.
        app.notifications.set_window_visible(false);

        app.handle(Intent::Start);
        assert!(app.selected_job().is_some(), "{:?}", app.start_note());
        wait_until(&mut app, |app| !app.view().any_live_job());

        let notices = toasts.notices();
        assert_eq!(notices.len(), 1, "{notices:?}");
        assert!(
            matches!(notices[0].kind, NoticeKind::Completed | NoticeKind::Stopped),
            "{:?}",
            notices[0]
        );
        assert!(
            notices[0].device.contains("Samsung SM-A536B"),
            "named the way the panel names it: {}",
            notices[0].device
        );
    }

    #[test]
    fn the_toast_for_a_finished_job_uses_the_panel_s_name_and_counts() {
        let dir = temp();
        let mut app = app(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        // The user's own name for the phone is what the panel shows, so it is what
        // the toast has to say.
        devices::rename(&db, device.id, "Ailena's phone").expect("name");
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
        app.refresh_rows_from_db();
        app.view_mut()
            .insert_job(JobView::new(7, device.id, "SER1".to_string()));
        app.view_mut().apply_job_event(
            device.id,
            &JobEvent::FileCopied {
                source: "DCIM/photo.jpg".to_string(),
                destination: "Ailena's phone/DCIM/photo.jpg".to_string(),
                bytes: 1024,
                reason: Reason::New,
                archived: Some(PathBuf::from("Ailena's phone/.history/photo.jpg")),
            },
        );

        let event = JobEvent::Finished(Box::new(JobOutcome {
            job_id: 7,
            device_id: device.id,
            status: JobStatus::Completed,
            counters: JobCounters {
                scanned: 4,
                copied: 3,
                skipped: 1,
                failed: 0,
                bytes_copied: 4096,
            },
            error_summary: None,
            duration: std::time::Duration::from_secs(61),
            failures: Vec::new(),
        }));

        let notice = job_notice(&app.view, device.id, &event).expect("news");
        assert_eq!(notice.kind, NoticeKind::Completed);
        assert_eq!(notice.device, "Ailena's phone");
        assert_eq!(notice.title, "Backup complete");
        assert!(
            notice.body.contains("3 copied, 1 unchanged"),
            "{}",
            notice.body
        );
        assert!(
            notice.body.contains("1 kept as an earlier version"),
            "the panel counts them, so the toast does too: {}",
            notice.body
        );
        assert!(notice.body.contains("1m 01s"), "{}", notice.body);
    }

    #[test]
    fn a_user_requested_pause_is_not_news() {
        let dir = temp();
        let mut app = app(dir.path());
        app.view_mut()
            .insert_job(JobView::new(7, 1, "SER1".to_string()));

        // The engine pauses for its own reasons; the user pauses with the button.
        let asked = JobEvent::Paused {
            reason: PauseReason::UserRequested,
            file: None,
        };
        assert!(job_notice(&app.view, 1, &asked).is_none());

        let full = JobEvent::Paused {
            reason: PauseReason::LowSpace,
            file: Some("DCIM/very/long/path/to/a/video.mp4".to_string()),
        };
        let notice = job_notice(&app.view, 1, &full).expect("news");
        assert_eq!(notice.kind, NoticeKind::Paused);
        assert!(
            notice.body.contains("a/video.mp4") && notice.body.contains("space"),
            "the reason and the file survive intact: {}",
            notice.body
        );
    }

    #[test]
    fn a_fatal_stop_is_announced_with_the_engine_s_wording() {
        let dir = temp();
        let mut app = app(dir.path());
        app.view_mut()
            .insert_job(JobView::new(7, 1, "SER1".to_string()));
        let event = JobEvent::Fatal {
            message: "no storage root: the phone is not unlocked".to_string(),
        };
        let notice = job_notice(&app.view, 1, &event).expect("news");
        assert_eq!(notice.kind, NoticeKind::Stopped);
        assert_eq!(notice.title, "Backup stopped");
        assert!(
            notice.body.contains("the phone is not unlocked"),
            "{}",
            notice.body
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
        let binary = dir.path().join("adb");
        std::fs::write(&binary, b"#!/bin/sh\n").expect("write script");
        let scripted = Arc::new(ScriptedPicker::accepting_file(binary.clone()));
        let mut app = app_with_picker(dir.path(), scripted.clone() as Arc<dyn Picker>);

        app.handle(Intent::ChooseAdbBinary);
        let mode = startup::manual_adb_mode(&binary);
        wait_until(&mut app, |app| app.config().adb_mode == mode);
        assert_eq!(app.config().adb_mode, mode, "the chosen binary is stored");
        assert_eq!(
            scripted.calls(),
            vec![PickerCall::File {
                title: "Choose the adb executable".to_string(),
                start: None,
                suggested: Some(android_backup_core::adb::locate::adb_binary_name().to_string()),
            }],
            "one dialog, pre-filled with the name this platform calls adb"
        );
    }

    #[test]
    fn a_chosen_folder_becomes_the_backup_location() {
        let dir = temp();
        let chosen = dir.path().join("chosen");
        std::fs::create_dir_all(&chosen).expect("dir");
        let scripted = Arc::new(ScriptedPicker::accepting_folder(chosen.clone()));
        let mut app = app_with_picker(dir.path(), scripted.clone() as Arc<dyn Picker>);
        assert!(app.selected_device().is_none(), "so the global one it is");

        app.handle(Intent::PickBackupLocation);
        wait_until(&mut app, |app| {
            app.config().global_backup_dir.as_deref() == Some(chosen.as_path())
        });
        assert_eq!(
            scripted.calls(),
            vec![PickerCall::Folder {
                title: "Choose a backup folder".to_string(),
                start: Some(dir.path().join("backups")),
            }],
            "the dialog opens where backups already go"
        );
    }

    #[test]
    fn a_cancelled_folder_dialog_changes_nothing() {
        let dir = temp();
        let scripted = Arc::new(ScriptedPicker::cancelling());
        let mut app = app_with_picker(dir.path(), scripted.clone() as Arc<dyn Picker>);
        let before = app.config().global_backup_dir.clone();

        app.handle(Intent::PickBackupLocation);
        wait_until(&mut app, |_| scripted.call_count() == 1);
        assert_eq!(app.config().global_backup_dir, before);
        assert!(app.location_error().is_none(), "nothing was wrong with it");
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

    // -----------------------------------------------------------------------
    // The tray and close-to-tray (E09.5)
    // -----------------------------------------------------------------------

    #[test]
    fn closing_the_window_hides_it_when_a_tray_can_bring_it_back() {
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        assert!(app.has_tray());
        assert!(!app.is_hidden());

        close_button(&mut app);

        assert!(
            app.is_hidden(),
            "the close button must hide the window, not end the process"
        );
        assert!(
            !app.closing,
            "a hidden window is not a closing one: the event loop keeps running"
        );
        assert!(
            app.automation.is_some(),
            "the automation clock must survive the window being hidden"
        );
        // The frame loop cannot tell the notification policy anything while the
        // window is hidden, so hiding has to say it.
        assert!(!app.notifications.window_visible());
        assert_eq!(
            tray.state(),
            Some(TrayState::Idle),
            "hiding must leave the icon saying what is true"
        );
    }

    #[test]
    fn closing_the_window_quits_when_there_is_no_tray() {
        let dir = temp();
        // The default test app has no tray, which is the state a bare GNOME session
        // produces too: closing must quit rather than hide the window somewhere the
        // user cannot reach it.
        let mut app = app(dir.path());
        assert!(!app.has_tray());
        close_button(&mut app);
        assert!(app.closing);
        assert!(!app.is_hidden());
    }

    #[test]
    fn closing_the_window_quits_when_close_to_tray_is_switched_off() {
        let dir = temp();
        let (mut app, _, _) = app_with_tray(dir.path());
        app.cfg.background.close_to_tray = false;
        close_button(&mut app);
        assert!(app.closing, "the setting is the user's, and it is obeyed");
        assert!(!app.is_hidden());
    }

    #[test]
    fn a_login_start_hides_the_window_only_when_there_is_a_tray() {
        // With a tray: the window waits in the notification area, which is the whole
        // point of `--hidden`.
        let dir = temp();
        let (mut trayed, _, _) = app_with_tray(dir.path());
        trayed.start_hidden();
        frame(&mut trayed);
        assert!(trayed.is_hidden());
        assert!(!trayed.closing);

        // Without one: the window opens. A hidden window with no way to show it
        // would be an application the user cannot reach at all.
        let dir = temp();
        let mut tray_less = app(dir.path());
        tray_less.start_hidden();
        frame(&mut tray_less);
        assert!(!tray_less.is_hidden());
        assert!(!tray_less.closing);
    }

    #[test]
    fn the_tray_menu_brings_a_hidden_window_back() {
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        close_button(&mut app);
        assert!(app.is_hidden());

        tray.click(TrayCommand::Open).expect("click");
        // A hidden window is not guaranteed a redraw, which is exactly why the icon
        // is handed the context: without a frame, the command would never be read.
        assert!(
            tray.repaint_context().is_some(),
            "the tray must be able to wake a hidden window"
        );
        frame(&mut app);

        assert!(!app.is_hidden(), "Open must show the window");
        assert!(!app.closing);
    }

    #[test]
    fn the_tray_menu_quits_a_hidden_window_for_good() {
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        close_button(&mut app);
        assert!(app.automation.is_some());

        tray.click(TrayCommand::Quit).expect("click");
        frame(&mut app);

        assert!(app.quitting);
        assert!(app.closing);
        assert!(
            app.automation.is_none(),
            "quitting must join the automation thread, not leak it"
        );
        assert!(tray.is_closed(), "the icon must be removed on the way out");
    }

    #[test]
    fn quitting_closes_the_tray_even_when_the_window_was_never_hidden() {
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        app.handle(Intent::Quit);
        assert!(app.quitting && app.closing);
        assert!(tray.is_closed());
    }

    #[test]
    fn quitting_stops_a_running_backup_cleanly_instead_of_leaving_it_to_recovery() {
        let dir = temp();
        // Three files behind a slow pull, so the job is still in flight when Quit
        // arrives. That is the only moment the decision matters: a job that has
        // already finished needs no cancelling.
        let remote = Arc::new(
            MockRemote::new()
                .with_file("/storage/emulated/0/DCIM/a.jpg", b"aaaa")
                .with_file("/storage/emulated/0/DCIM/b.jpg", b"bbbb")
                .with_file("/storage/emulated/0/DCIM/c.jpg", b"cccc")
                .with_pull_delay(std::time::Duration::from_millis(150)),
        );
        let (mut app, _, _) = app_with_tray_and(dir.path(), remote);
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        app.view_mut()
            .apply_device_event(&DeviceEvent::FirstSeen(connected("SER1")));
        app.handle(Intent::Select("SER1".to_string()));
        app.handle(Intent::Start);
        wait_until(&mut app, |app| app.selected_job().is_some());
        let job_id = app.selected_job().expect("a job").id;

        app.handle(Intent::Quit);

        let row = jobs::get_job(&db, job_id)
            .expect("query")
            .expect("the row survives");
        assert_eq!(
            row.status,
            JobStatus::Cancelled,
            "Quit is an instruction to stop, not a crash to recover from"
        );
        assert!(
            jobs::unfinished_jobs(&db).expect("query").is_empty(),
            "nothing may be left live for the next launch to recover"
        );
        // The panel reads the same events it always did, so the cancelled job leaves
        // it rather than sitting there as a job nobody can stop any more.
        wait_until(&mut app, |app| !app.view().any_live_job());
    }

    #[test]
    fn the_tray_toggle_pauses_automation_and_writes_it_down() {
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        let config_file = app.paths.config_file.clone();
        assert!(!app.automation_paused());

        tray.click(TrayCommand::ToggleAutomation).expect("click");
        frame(&mut app);

        assert!(app.automation_paused());
        assert_eq!(tray.pauses(), vec![true], "the menu item must follow");
        assert!(
            config_file.is_file() && AppConfig::load(&config_file).automation.paused,
            "a pause that lifts itself on the next restart is not a pause"
        );

        // And back, without forgetting anything.
        tray.click(TrayCommand::ToggleAutomation).expect("click");
        frame(&mut app);
        assert!(!app.automation_paused());
        assert_eq!(tray.pauses(), vec![true, false]);
    }

    #[test]
    fn a_phone_that_arrives_while_the_window_is_hidden_backs_itself_up() {
        let dir = temp();
        let (mut app, _tray, toasts) = app_with_tray(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        // Opted in: an automatic backup is never something that happens *to* a device.
        devices::set_auto_schedule(&db, device.id, Schedule::OnConnect).expect("schedule");
        app.refresh_rows_from_db();

        // Behind the tray, where nothing is drawing frames and the only channel left
        // is the notification.
        close_button(&mut app);
        assert!(app.is_hidden());

        app.feed_automation(&[DeviceEvent::FirstSeen(connected("SER1"))]);

        // The job is started by the host and handed back through the message queue,
        // which is what keeps the panel able to pause and cancel it.
        wait_until(&mut app, |app| app.view().job(device.id).is_some());
        let job_id = app.view().job(device.id).expect("a job").id;
        assert_eq!(
            jobs::get_job(&db, job_id)
                .expect("query")
                .expect("row")
                .trigger,
            JobTrigger::Auto,
            "the row has to remember that nobody asked for this one"
        );

        wait_until(&mut app, |app| !app.view().any_live_job());
        let titles = toasts.titles();
        assert!(
            titles.iter().any(|title| title.starts_with("Backing up")),
            "an automatic start must be announced: {titles:?}"
        );
        assert!(
            titles.iter().any(|title| title == "Backup complete"),
            "and so must the end of it: {titles:?}"
        );
    }

    #[test]
    fn a_phone_with_no_schedule_is_left_alone_even_when_it_appears() {
        let dir = temp();
        let (mut app, _tray, toasts) = app_with_tray(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        app.refresh_rows_from_db();

        app.feed_automation(&[DeviceEvent::FirstSeen(connected("SER1"))]);

        assert!(
            app.view().job(device.id).is_none(),
            "automation is opt-in per device, and this one never opted in"
        );
        wait_until(&mut app, |app| app.view().jobs().count() == 0);
        assert!(
            toasts.titles().is_empty(),
            "nothing happened, so there is nothing to announce: {:?}",
            toasts.titles()
        );
    }

    #[test]
    fn the_tray_backs_up_every_connected_device_at_once() {
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        for serial in ["SER1", "SER2"] {
            let device = devices::upsert_seen(&db, serial, &props(), "2026-01-01T00:00:00Z")
                .expect("device");
            devices::seed_default_folders(&db, device.id).expect("folders");
            // Enrolled in the session, which is what the tray's menu asks about. No
            // schedule: this is a person pressing a button, not automation.
            app.view_mut()
                .apply_device_event(&DeviceEvent::FirstSeen(connected(serial)));
        }
        app.refresh_rows_from_db();

        tray.click(TrayCommand::BackupAll).expect("click");
        frame(&mut app);

        assert_eq!(app.view().jobs().count(), 2, "both connected devices");
        for serial in ["SER1", "SER2"] {
            let row = devices::get_by_serial(&db, serial)
                .expect("query")
                .expect("row");
            let job = jobs::latest_job(&db, row.id)
                .expect("query")
                .expect("a job row");
            assert_eq!(
                job.trigger,
                JobTrigger::Manual,
                "the tray's button is a person asking, so the row says so"
            );
        }
        assert!(
            app.start_note()
                .is_some_and(|note| note.contains("2 device(s)")),
            "the tray's button reports what it did: {:?}",
            app.start_note()
        );
    }

    #[test]
    fn the_icon_says_what_the_panel_says() {
        // The tray reads the view model and nothing else, so the two cannot disagree.
        let dir = temp();
        let (mut app, tray, _) = app_with_tray(dir.path());
        let db = Arc::clone(app.db().expect("db"));
        let device =
            devices::upsert_seen(&db, "SER1", &props(), "2026-01-01T00:00:00Z").expect("device");
        app.refresh_rows_from_db();
        app.view_mut()
            .insert_job(JobView::new(1, device.id, "SER1".to_string()));

        frame(&mut app);
        assert_eq!(tray.state(), Some(TrayState::Running));

        app.view_mut().apply_job_event(
            device.id,
            &JobEvent::Paused {
                reason: PauseReason::DeviceDisconnected,
                file: None,
            },
        );
        frame(&mut app);
        assert_eq!(
            tray.state(),
            Some(TrayState::Attention),
            "a job needing a person outranks one that is merely busy"
        );
    }

    #[test]
    fn a_desktop_that_cannot_have_a_tray_says_so_and_keeps_closing_meaning_exit() {
        let dir = temp();
        let (factory, reasons) = testutil::failing_tray();
        let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
        let mut deps = AppDeps::new(paths, AppConfig::default());
        deps.watch_devices = false;
        deps.tray = Some(factory);
        let mut app = App::new(deps);

        assert!(!app.has_tray());
        assert_eq!(reasons.lock().expect("lock").len(), 1, "asked exactly once");
        assert!(!app.close_to_tray());
        // The banner is the difference between "there is no tray here" and "backups
        // are not running": a user who asked for close-to-tray is told why not.
        let banner = app.view().banner().map(|b| b.message.clone());
        assert!(
            banner
                .as_deref()
                .is_some_and(|message| message.contains("tray")),
            "the window must explain the missing icon: {banner:?}"
        );

        close_button(&mut app);
        assert!(app.closing, "without an icon, closing still means quitting");
    }

    #[test]
    fn a_tray_that_is_switched_off_is_not_created_at_all() {
        let dir = temp();
        let (factory, _tray) = testutil::tray();
        let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
        let cfg = AppConfig {
            background: android_backup_core::config::BackgroundConfig {
                tray_icon: false,
                ..AppConfig::default().background
            },
            ..AppConfig::default()
        };
        let mut deps = AppDeps::new(paths, cfg);
        deps.watch_devices = false;
        deps.tray = Some(factory);
        let app = App::new(deps);

        assert!(!app.has_tray());
        assert!(!app.close_to_tray());
        // No icon means no banner either: the user turned it off on purpose, and
        // there is nothing to explain.
        assert!(app.view().banner().is_none());
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
