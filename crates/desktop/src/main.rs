//! `android-backup` — the desktop application.
//!
//! This binary is deliberately thin: it decides *where* things live, installs
//! logging and a panic hook, builds the dependency bundle, and hands it to
//! [`android_backup_desktop::window::run_native`]. Every decision it could get
//! wrong is a library function with tests.

// No console window on Windows in release builds; debug builds keep stderr so a
// developer can read the log while it happens.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::panic::{self, AssertUnwindSafe};
use std::sync::{Arc, Mutex};

use android_backup_core::backup::destination::{FsProbe, LocalFsProbe};
use android_backup_core::backup::enumerate::AdbRemoteFiles;
use android_backup_core::backup::RemoteFiles;
use android_backup_core::config::AppConfig;
use android_backup_core::database::open_or_quarantine;
use android_backup_desktop::app::AppDeps;
use android_backup_desktop::startup::{AdbInstaller, ManagedInstaller};
use android_backup_desktop::window;

fn main() {
    let paths = match window::discover_paths() {
        Ok(paths) => paths,
        Err(message) => {
            // Nothing is set up yet, so this is the only channel available.
            eprintln!("android-backup: {message}");
            report("Android Backup cannot start", &message);
            return;
        }
    };
    let _log_guard = android_backup_desktop::logging::init(&paths.log_dir);

    let crash = Arc::new(Mutex::new(None));
    install_panic_hook(Arc::clone(&crash));

    // A panic inside a callback would otherwise take the window down with no
    // explanation: catch it, log it, and tell the user what to send us.
    let outcome = panic::catch_unwind(AssertUnwindSafe(|| run(paths.clone())));
    match outcome {
        Ok(Ok(())) => {}
        Ok(Err(message)) => {
            tracing::error!(%message, "startup failed");
            report("Android Backup could not start", &message);
        }
        Err(_) => {
            let message = lock(&crash)
                .clone()
                .unwrap_or_else(|| "the application stopped unexpectedly".to_string());
            report("Android Backup stopped unexpectedly", &message);
        }
    }
}

/// Build the dependency bundle and open the window.
fn run(paths: android_backup_core::config::AppPaths) -> Result<(), String> {
    std::fs::create_dir_all(&paths.data_dir)
        .map_err(|err| format!("could not create {}: {err}", paths.data_dir.display()))?;

    let config_existed = paths.config_file.exists();
    let cfg = AppConfig::load(&paths.config_file);
    tracing::info!(config_existed, "configuration loaded");

    // A missing adb is a normal state here, not a startup failure: the UI shows
    // the "ADB not found" modal and offers the three routes out (E06.6).
    let found = android_backup_desktop::startup::locate(&cfg, &paths)
        .map_err(|err| {
            tracing::warn!(%err, "no usable adb at startup");
            err
        })
        .ok();
    let adb = found.as_ref().map(android_backup_desktop::startup::runner);
    let adb_trait: Option<Arc<dyn android_backup_core::adb::runner::Adb>> = adb
        .clone()
        .map(|adb| adb as Arc<dyn android_backup_core::adb::runner::Adb>);
    let remote: Option<Arc<dyn RemoteFiles>> = adb_trait
        .clone()
        .map(|adb| Arc::new(AdbRemoteFiles::new(adb)) as Arc<dyn RemoteFiles>);
    let probe: Arc<dyn FsProbe> = Arc::new(LocalFsProbe::new());

    // A manifest that cannot be read is not a startup failure: it is an index,
    // never the only copy of anything (plan §9), so the app starts with a fresh
    // one and keeps the damaged file for the user to inspect (E07.3).
    let manifest_path = paths.data_dir.join("manifest.db");
    let (db, quarantined) = open_or_quarantine(&manifest_path)
        .map_err(|err| format!("could not open the backup manifest: {err}"))?;
    if let Some(quarantined) = &quarantined {
        tracing::error!(summary = %quarantined.summary(), "manifest replaced");
        report(
            "The backup manifest could not be read",
            &quarantined.guidance,
        );
    }
    let db = Arc::new(db);
    db.migrate()
        .map_err(|err| format!("could not prepare the backup manifest: {err}"))?;

    // Managed platform-tools (E07.1). Unavailable is a supported state: the modal
    // then only offers the routes that need an adb the user already has.
    let installer = match ManagedInstaller::new(&paths) {
        Ok(installer) => {
            tracing::info!(dest = %installer.destination().display(), "managed adb install available");
            Some(Arc::new(installer) as Arc<dyn AdbInstaller>)
        }
        Err(err) => {
            tracing::warn!(%err, "managed adb install unavailable");
            None
        }
    };

    let deps = AppDeps {
        paths,
        cfg,
        config_existed,
        adb: adb_trait,
        adb_found: found,
        remote,
        db: Some(db),
        probe: Some(probe),
        installer,
        watch_devices: true,
    };

    let app = android_backup_desktop::app::App::new(deps);
    window::run_native(app).map_err(|err| format!("could not open the window: {err}"))
}

/// Log panics and keep the message for the dialog shown after unwinding.
fn install_panic_hook(slot: Arc<Mutex<Option<String>>>) {
    let previous = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        let message = info.to_string();
        tracing::error!(%message, "panic");
        *lock(&slot) = Some(message);
        // Keep the default behaviour (a backtrace on stderr) in debug builds.
        if cfg!(debug_assertions) {
            previous(info);
        }
    }));
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Show a native message box. Used only when the application is not running yet
/// or not running any more, so it can block.
fn report(title: &str, message: &str) {
    tracing::error!(title, %message, "reporting to the user");
    rfd::MessageDialog::new()
        .set_level(rfd::MessageLevel::Error)
        .set_title(title)
        .set_description(format!(
            "{message}\n\nA log file was written next to the backup manifest."
        ))
        .show();
}
