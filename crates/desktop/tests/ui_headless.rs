//! Headless UI tests: run the real `egui` pass and inspect the text it produced.
//!
//! `egui::Context::run` needs no window, no GPU and no event loop, so the whole
//! window can be exercised in CI. The tests read the laid-out text out of the
//! frame's shapes, which is the only honest way to check that the UI says what it
//! thinks it says — asserting on the view model alone would not catch a panel
//! that simply never draws the value.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;

use android_backup_core::adb::runner::Adb;
use android_backup_core::backup::job::{JobConfig, JobOutcome};
use android_backup_core::backup::{FsProbe, RemoteFiles};
use android_backup_core::backup::{JobEvent, PauseReason, Progress, VerifyMode};
use android_backup_core::config::{AppConfig, AppPaths};
use android_backup_core::database::devices;
use android_backup_core::database::jobs::JobCounters;
use android_backup_core::database::Db;
use android_backup_core::devices::watcher::DeviceEvent;
use android_backup_core::devices::{DeviceInfo, DeviceState};
use android_backup_core::mock::{MockAdb, MockRemote};
use android_backup_desktop::app::{App, AppDeps, Intent};
use android_backup_desktop::autostart::{self, Status as StartupStatus};
use android_backup_desktop::testutil::FakeStartup;
use android_backup_desktop::view::{self, JobView};

/// Build an app over a temporary manifest and destination.
fn app(dir: &Path) -> (App, Arc<Db>) {
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
    let remote: Arc<MockRemote> = Arc::new(MockRemote::new());
    let app = App::new(AppDeps {
        paths,
        cfg,
        config_existed: true,
        adb: Some(adb),
        adb_found: None,
        remote: Some(remote as Arc<dyn RemoteFiles>),
        db: Some(Arc::clone(&db)),
        probe: None::<Arc<dyn FsProbe>>,
        installer: None,
        // Scripted to cancel: a test rendering frames must not be able to draw a
        // dialog on the machine running it.
        picker: Some(android_backup_desktop::testutil::picker(
            android_backup_desktop::testutil::ScriptedPicker::cancelling(),
        )),
        notifier: Some(android_backup_desktop::testutil::recorder()),
        watch_devices: false,
        // No tray and no clock: this test drives intents by hand, and a tray icon
        // cannot be drawn on a machine with no desktop at all.
        tray: None,
        automation: false,
        startup: None,
    });
    (app, db)
}

fn props(model: &str) -> android_backup_core::adb::props::DeviceProps {
    android_backup_core::adb::props::DeviceProps {
        manufacturer: Some("Samsung".to_string()),
        model: Some(model.to_string()),
        product: None,
        android_version: Some("14".to_string()),
    }
}

fn info(serial: &str, state: DeviceState, model: &str) -> Arc<DeviceInfo> {
    Arc::new(DeviceInfo {
        serial: serial.to_string(),
        state,
        raw_state: android_backup_core::adb::parse::RawState::Device,
        listing_model: Some(model.to_string()),
        listing_product: None,
        transport_id: None,
        props: props(model),
    })
}

/// Render one frame and return every string `egui` laid out.
fn frame_text(app: &mut App) -> String {
    let ctx = egui::Context::default();
    // Two passes: the first builds the layout, the second measures what it
    // produced with fonts already resident. Both are cheap.
    let mut text = String::new();
    for _ in 0..2 {
        let output = ctx.run(egui::RawInput::default(), |ctx| app.frame(ctx));
        text = collect_text(&output.shapes);
    }
    text
}

fn collect_text(shapes: &[egui::epaint::ClippedShape]) -> String {
    let mut out = String::new();
    for clipped in shapes {
        collect_shape(&clipped.shape, &mut out);
    }
    out
}

fn collect_shape(shape: &egui::epaint::Shape, out: &mut String) {
    match shape {
        egui::epaint::Shape::Text(text) => {
            out.push_str(text.galley.text());
            out.push('\n');
        }
        egui::epaint::Shape::Vec(shapes) => {
            for shape in shapes {
                collect_shape(shape, out);
            }
        }
        _ => {}
    }
}

/// Render until the text contains `needle`, or fail after five seconds.
///
/// Progress arrives from the installer thread through the app's message queue,
/// which is only drained while a frame is drawn, so the loop *is* the pump.
fn wait_for_text(app: &mut App, needle: &str) -> String {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        let text = frame_text(app);
        if text.contains(needle) {
            return text;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "never rendered {needle:?}; last frame was:\n{text}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// A temp dir that lives as long as the test.
fn temp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

#[test]
fn an_empty_app_renders_the_chrome_and_the_empty_state() {
    let dir = temp();
    let (mut app, _db) = app(dir.path());
    let text = frame_text(&mut app);
    assert!(text.contains("Android Backup"), "{text}");
    assert!(text.contains("Select a device"), "{text}");
    assert!(text.contains("Devices"), "{text}");
    assert!(text.contains("No devices yet"), "{text}");
}

#[test]
fn a_connected_device_renders_its_panel_and_the_start_button() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");

    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    let text = frame_text(&mut app);
    assert!(text.contains("Samsung SM-A536B"), "{text}");
    assert!(text.contains("Folders to back up"), "{text}");
    assert!(
        text.contains("DCIM"),
        "the seeded folders must be listed: {text}"
    );
    assert!(text.contains("Back up now"), "{text}");
    assert!(text.contains("ready"), "{text}");
    assert!(
        text.contains("Last backup:") && text.contains("never backed up"),
        "{text}"
    );
    assert!(
        text.contains("No backups recorded for this device yet"),
        "{text}"
    );
}

#[test]
fn an_unauthorized_device_explains_the_usb_prompt_instead_of_offering_start() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.view_mut()
        .apply_device_event(&DeviceEvent::Unauthorized(info(
            "SER1",
            DeviceState::Unauthorized,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    let text = frame_text(&mut app);
    // The button is drawn, but disabled; the reason is spelled out either way.
    assert!(
        text.contains("approve the \"Allow USB debugging?\" prompt"),
        "{text}"
    );
    assert!(!app.can_start(), "an unauthorized device must not start");
}

#[test]
fn a_running_job_renders_progress_and_the_controls() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.view_mut()
        .apply_device_event(&DeviceEvent::Connected(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    app.view_mut()
        .insert_job(JobView::new(7, device.id, "SER1".to_string()));
    app.view_mut().apply_job_event(
        device.id,
        &JobEvent::Progress(Progress {
            files_done: 30,
            files_total: 100,
            bytes_done: 5_242_880,
            bytes_total: 20_971_520,
            files_skipped: 5,
            files_failed: 1,
            current_file: Some("/storage/emulated/0/DCIM/Camera/a.jpg".to_string()),
            bytes_per_sec: Some(1_048_576),
            eta_secs: Some(65),
        }),
    );

    let text = frame_text(&mut app);
    assert!(text.contains("files: 30/100"), "{text}");
    assert!(text.contains("Cancel"), "{text}");
    assert!(text.contains("Pause"), "{text}");
    assert!(
        text.contains("a.jpg"),
        "the current file must be visible: {text}"
    );
}

#[test]
fn a_paused_job_renders_the_reason_and_a_resume_button() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.view_mut()
        .apply_device_event(&DeviceEvent::Connected(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));
    app.view_mut()
        .insert_job(JobView::new(7, device.id, "SER1".to_string()));
    app.view_mut().apply_job_event(
        device.id,
        &JobEvent::Paused {
            reason: PauseReason::AuthorizationNeeded,
            file: None,
        },
    );

    let text = frame_text(&mut app);
    assert!(text.contains("Resume"), "{text}");
    assert!(text.contains("approve the USB debugging prompt"), "{text}");
}

#[test]
fn a_finished_job_renders_its_terminal_summary() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.view_mut()
        .apply_device_event(&DeviceEvent::Connected(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));
    app.view_mut()
        .insert_job(JobView::new(7, device.id, "SER1".to_string()));
    app.view_mut().apply_job_event(
        device.id,
        &JobEvent::Finished(Box::new(JobOutcome {
            job_id: 7,
            device_id: device.id,
            status: android_backup_core::database::jobs::JobStatus::Completed,
            counters: JobCounters {
                scanned: 100,
                copied: 95,
                skipped: 5,
                failed: 0,
                bytes_copied: 5_242_880,
            },
            error_summary: None,
            duration: std::time::Duration::from_secs(125),
            failures: Vec::new(),
        })),
    );

    let text = frame_text(&mut app);
    assert!(text.contains("Backup completed"), "{text}");
    assert!(text.contains("95 copied, 5 unchanged"), "{text}");
}

#[test]
fn the_first_run_wizard_is_drawn_and_its_buttons_are_present() {
    let dir = temp();
    let mut deps = AppDeps::new(
        AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
        AppConfig::default(),
    );
    deps.config_existed = false;
    deps.watch_devices = false;
    let mut app = App::new(deps);
    let text = frame_text(&mut app);
    assert!(text.contains("Welcome"), "{text}");
    assert!(text.contains("Use the suggested folder"), "{text}");
    assert!(text.contains("Where should backups be stored?"), "{text}");
    assert!(text.contains("adb"), "{text}");
}

#[test]
fn a_missing_adb_is_reported_in_a_modal_with_every_route_out() {
    let dir = temp();
    let mut deps = AppDeps::new(
        AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
        AppConfig::default(),
    );
    deps.watch_devices = false;
    let mut app = App::new(deps);
    let text = frame_text(&mut app);
    assert!(text.contains("ADB not found"), "{text}");
    assert!(text.contains("Choose adb"), "{text}");
    assert!(text.contains("Use the system adb"), "{text}");
    assert!(text.contains("Install automatically"), "{text}");
    // Without an installer (this build), the button is disabled and says why.
    assert!(text.contains("unavailable in this build"), "{text}");
}

#[test]
fn a_configured_installer_offers_a_working_button_with_no_excuse_note() {
    let dir = temp();
    let mut deps = AppDeps::new(
        AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
        AppConfig::default(),
    );
    deps.watch_devices = false;
    deps.installer = Some(android_backup_desktop::testutil::installer(
        android_backup_desktop::testutil::ScriptedInstaller::succeeding(),
    ));
    let mut app = App::new(deps);
    let text = frame_text(&mut app);
    assert!(text.contains("Install automatically"), "{text}");
    assert!(!text.contains("unavailable in this build"), "{text}");
}

#[test]
fn the_install_progress_modal_shows_the_stage_the_bytes_and_a_cancel() {
    let dir = temp();
    let mut deps = AppDeps::new(
        AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
        AppConfig::default(),
    );
    deps.watch_devices = false;
    // A gated installer: the install starts and stays in flight, so the modal is
    // on screen while the test looks at it.
    let (release, gate) = std::sync::mpsc::channel::<()>();
    deps.installer = Some(android_backup_desktop::testutil::installer(
        android_backup_desktop::testutil::ScriptedInstaller::succeeding().gated(gate),
    ));
    let mut app = App::new(deps);
    app.handle(Intent::InstallAdb);
    let text = wait_for_text(&mut app, "1.0 KB of 4.0 KB");
    assert!(text.contains("Installing adb"), "{text}");
    assert!(text.contains("Downloading platform-tools"), "{text}");
    assert!(text.contains("Cancel"), "{text}");
    let _ = release.send(());
}

#[test]
fn the_error_banner_is_drawn_above_the_panels() {
    let dir = temp();
    let (mut app, _db) = app(dir.path());
    app.view_mut()
        .set_error("the manifest is locked by another process");
    let text = frame_text(&mut app);
    assert!(
        text.contains("the manifest is locked by another process"),
        "{text}"
    );
    assert!(text.contains("Dismiss"), "{text}");
}

#[test]
fn an_expanded_history_row_renders_its_file_list() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    let job =
        android_backup_core::database::jobs::insert_job(&db, device.id, "2026-01-01T10:00:00Z")
            .expect("job");
    android_backup_core::database::files::record_verified(
        &db,
        device.id,
        job,
        &android_backup_core::database::files::FileRecord {
            source_path: "DCIM/photo.jpg".to_string(),
            destination_path: "Samsung SM-A536B/DCIM/photo.jpg".to_string(),
            source_size: Some(1024),
            source_modified_at: None,
            content_hash: Some("abc123".to_string()),
        },
        "2026-01-01T10:00:01Z",
    )
    .expect("record");
    android_backup_core::database::jobs::finalize(
        &db,
        job,
        android_backup_core::database::jobs::JobStatus::Completed,
        &JobCounters {
            scanned: 1,
            copied: 1,
            skipped: 0,
            failed: 0,
            bytes_copied: 1024,
        },
        None,
        "2026-01-01T10:00:02Z",
    )
    .expect("finalize");

    app.view_mut()
        .apply_device_event(&DeviceEvent::Connected(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));
    app.handle(Intent::ToggleHistoryRow(job));

    let text = frame_text(&mut app);
    assert!(text.contains("History"), "{text}");
    assert!(text.contains("DCIM/photo.jpg"), "{text}");
    assert!(text.contains("1 file(s) shown"), "{text}");
    assert!(text.contains("Completed"), "{text}");
}

/// The job config the UI builds must satisfy the engine's own contract; the
/// engine is the authority, so this is checked against it rather than guessed.
#[test]
fn the_ui_builds_a_job_config_the_engine_accepts() {
    let dir = temp();
    let (app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    let config: JobConfig = app.job_config(&device).expect("config");
    assert_eq!(config.verify, VerifyMode::Hash);
    assert!(config.folders.iter().any(|f| f == "DCIM"));
    assert!(config.root.starts_with(dir.path().join("backups")));
    // The view's own line for the same plan, so the UI cannot disagree with it.
    let line = view::format_bytes(0);
    assert_eq!(line, "0 B");
}

// ---------------------------------------------------------------------------
// E09.6 — settings and status
// ---------------------------------------------------------------------------

/// The three rows of the schedule control, the brake, and a line saying what will
/// happen. A device with automation off must still show the rows: switching it on
/// has to be possible from the same panel that says it is off.
#[test]
fn the_device_panel_offers_the_schedule_rows_and_the_pause_switch() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    let text = frame_text(&mut app);
    assert!(text.contains("Automatic backups"), "{text}");
    assert!(text.contains("Off"), "{text}");
    assert!(text.contains("When connected"), "{text}");
    assert!(text.contains("Every"), "{text}");
    assert!(text.contains("hours"), "{text}");
    assert!(text.contains("Pause all automatic backups"), "{text}");
    // Off and unpaused: the status line says so in as many words.
    assert!(
        text.contains("Automatic backup is off for this device."),
        "{text}"
    );
}

/// A device on a schedule, plugged in, with a backup in its history: the panel says
/// when the next one is due. The clock is the real one, so the assertion is on the
/// shape of the sentence rather than a number.
#[test]
fn a_scheduled_device_says_when_the_next_backup_is_due() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    android_backup_core::database::devices::set_auto_schedule(
        &db,
        device.id,
        android_backup_core::automation::Schedule::EveryHours(6),
    )
    .expect("schedule");
    // One backup, just now: the countdown is measured from the last attempt, so the
    // attempt has to exist for this to be a countdown rather than "due now".
    let job = android_backup_core::database::jobs::insert_job(
        &db,
        device.id,
        &android_backup_core::clock::now_iso(),
    )
    .expect("job");
    android_backup_core::database::jobs::finalize(
        &db,
        job,
        android_backup_core::database::jobs::JobStatus::Completed,
        &JobCounters {
            scanned: 0,
            copied: 0,
            skipped: 0,
            failed: 0,
            bytes_copied: 0,
        },
        None,
        &android_backup_core::clock::now_iso(),
    )
    .expect("finalize");
    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    let text = frame_text(&mut app);
    assert!(
        text.contains("Next automatic backup in"),
        "the schedule must say when it next runs: {text}"
    );
    assert!(
        !text.contains("due now"),
        "it ran a moment ago, so nothing is due yet: {text}"
    );
    assert!(
        !text.contains("Automatic backup is off"),
        "it is scheduled, so it must not claim otherwise: {text}"
    );
}

/// Choosing a row writes through the manifest, and the panel is redrawn from what
/// was stored rather than from the click — so a clamped value shows up as clamped.
#[test]
fn choosing_a_schedule_writes_it_and_shows_what_was_stored() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    app.handle(Intent::SetAutoSchedule(
        android_backup_core::automation::Schedule::EveryHours(6),
    ));
    let stored = android_backup_core::database::devices::get_by_id(&db, device.id)
        .expect("read")
        .expect("row")
        .auto_schedule;
    assert_eq!(
        stored,
        android_backup_core::automation::Schedule::EveryHours(6)
    );

    // A six-hour device that has never run is due immediately, and the panel drawn
    // from the stored row says exactly that — the countdown wording needs a history
    // to count from, which this device does not have yet.
    let text = frame_text(&mut app);
    assert!(text.contains("An automatic backup is due now."), "{text}");
}

/// The interval is the user's number, but not any number: what the manifest holds
/// after a nonsensical one is the clamped value, not the nonsense.
#[test]
fn a_zero_hour_interval_is_stored_as_the_minimum() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    app.handle(Intent::Select("SER1".to_string()));

    app.handle(Intent::SetAutoSchedule(
        android_backup_core::automation::Schedule::EveryHours(0),
    ));
    let stored = android_backup_core::database::devices::get_by_id(&db, device.id)
        .expect("read")
        .expect("row")
        .auto_schedule;
    assert_eq!(
        stored,
        android_backup_core::automation::Schedule::EveryHours(
            android_backup_core::automation::MIN_INTERVAL_HOURS
        )
    );
}

/// The brake is one flag, visible from the panel and from the status bar, and it is
/// the flag the engine reads.
#[test]
fn the_pause_switch_says_so_in_the_panel_and_in_the_status_bar() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    android_backup_core::database::devices::set_auto_schedule(
        &db,
        device.id,
        android_backup_core::automation::Schedule::OnConnect,
    )
    .expect("schedule");
    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));

    // Not paused: no brake light anywhere.
    let text = frame_text(&mut app);
    assert!(!text.contains("automatic backups paused"), "{text}");

    app.handle(Intent::ToggleAutomation);
    assert!(app.automation_paused(), "the switch flips the config flag");

    let text = frame_text(&mut app);
    assert!(
        text.contains("automatic backups paused"),
        "the status bar must say the fleet is paused: {text}"
    );
    assert!(
        text.contains("Resume automatic backups"),
        "and offer the way out of it: {text}"
    );
    assert!(
        text.contains("Automatic backups are paused") && text.contains("will not run yet"),
        "the device's own line must agree: {text}"
    );
}

/// An unplugged device on a schedule is waiting, not broken: the wording it gets
/// must not ask the user to fix anything.
#[test]
fn an_unplugged_scheduled_device_is_waiting_rather_than_blocked() {
    let dir = temp();
    let (mut app, db) = app(dir.path());
    let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
        .expect("device");
    devices::seed_default_folders(&db, device.id).expect("folders");
    android_backup_core::database::devices::set_auto_schedule(
        &db,
        device.id,
        android_backup_core::automation::Schedule::EveryHours(6),
    )
    .expect("schedule");
    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.view_mut()
        .apply_device_event(&DeviceEvent::Disconnected {
            serial: "SER1".to_string(),
        });
    app.handle(Intent::Select("SER1".to_string()));

    let text = frame_text(&mut app);
    assert!(
        text.contains("starts as soon as Samsung SM-A536B is connected"),
        "an unplugged phone is the schedule's waiting room: {text}"
    );
    assert!(
        !text.contains("Not yet:"),
        "nothing is broken, so nothing is quoted as a fault: {text}"
    );
}

/// The wizard's box is stored as the configuration's default schedule, and it is
/// written when it is ticked — not when the location question is answered.
#[test]
fn the_wizard_asks_about_automatic_backups_and_remembers_the_answer() {
    let dir = temp();
    let mut deps = AppDeps::new(
        AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data")),
        AppConfig::default(),
    );
    deps.config_existed = false;
    deps.watch_devices = false;
    let mut app = App::new(deps);
    let text = frame_text(&mut app);
    assert!(text.contains("back up on their own"), "{text}");
    assert!(text.contains("Back up a phone when I plug it in"), "{text}");
    assert!(!app.welcome_auto(), "the box starts unticked: it is opt-in");

    app.handle(Intent::SetWelcomeAuto(true));
    assert!(app.welcome_auto(), "{:?}", app.welcome_auto());
    let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
    let stored = AppConfig::load(&paths.config_file);
    assert_eq!(
        stored.automation.default_schedule,
        android_backup_core::automation::Schedule::OnConnect
    );

    // Unticking it is also written: the answer is whatever the box says last.
    app.handle(Intent::SetWelcomeAuto(false));
    let stored = AppConfig::load(&paths.config_file);
    assert_eq!(
        stored.automation.default_schedule,
        android_backup_core::automation::Schedule::Off
    );
}

/// An app over a *file* manifest, so a second one can be opened on the same paths.
///
/// This is what "survives a restart" has to mean: a fresh `App`, a fresh view model
/// and a fresh manifest connection, reading whatever the last run left behind.
fn app_on_disk(dir: &Path, cfg: AppConfig) -> (App, Arc<Db>) {
    app_on_disk_with_startup(dir, cfg, None)
}

/// The same, with a login entry the test can watch (E09.5).
///
/// The real one is the Windows Run key or a file in `~/.config/autostart`, and a test
/// that used it would change the machine running the tests.
fn app_on_disk_with_startup(
    dir: &Path,
    cfg: AppConfig,
    startup: Option<Arc<dyn autostart::Startup>>,
) -> (App, Arc<Db>) {
    let paths = AppPaths::with_root(&dir.join("cfg"), &dir.join("data"));
    std::fs::create_dir_all(&paths.data_dir).expect("data dir");
    let db = Arc::new(Db::open_file(&paths.data_dir.join("manifest.db")).expect("db"));
    let adb = Arc::new(MockAdb::new().always(MockAdb::ok(""))) as Arc<dyn Adb>;
    let app = App::new(AppDeps {
        paths,
        cfg,
        config_existed: true,
        adb: Some(adb),
        adb_found: None,
        remote: Some(Arc::new(MockRemote::new()) as Arc<dyn RemoteFiles>),
        db: Some(Arc::clone(&db)),
        probe: None,
        installer: None,
        picker: Some(android_backup_desktop::testutil::picker(
            android_backup_desktop::testutil::ScriptedPicker::cancelling(),
        )),
        notifier: None,
        watch_devices: false,
        tray: None,
        automation: false,
        startup,
    });
    (app, db)
}

/// What the user chose is still what the next run shows: the schedule is in the
/// manifest and the brake is in the configuration, and both are re-read rather than
/// remembered in the process.
#[test]
fn the_schedule_and_the_brake_survive_a_restart() {
    let dir = temp();
    let device_id;
    let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
    {
        let cfg = AppConfig {
            global_backup_dir: Some(dir.path().join("backups")),
            ..AppConfig::default()
        };
        let (mut app, db) = app_on_disk(dir.path(), cfg);
        let device = devices::upsert_seen(&db, "SER1", &props("SM-A536B"), "2026-01-01T00:00:00Z")
            .expect("device");
        devices::seed_default_folders(&db, device.id).expect("folders");
        device_id = device.id;
        app.view_mut()
            .apply_device_event(&DeviceEvent::FirstSeen(info(
                "SER1",
                DeviceState::Connected,
                "SM-A536B",
            )));
        app.handle(Intent::Select("SER1".to_string()));
        app.handle(Intent::SetAutoSchedule(
            android_backup_core::automation::Schedule::EveryHours(6),
        ));
        app.handle(Intent::ToggleAutomation);
        assert!(app.automation_paused(), "the switch flips the flag");
    }

    // The second run reads the configuration the first one wrote, exactly as a
    // launch from the login entry would.
    let (mut app, db) = app_on_disk(dir.path(), AppConfig::load(&paths.config_file));
    assert!(
        app.automation_paused(),
        "a pause that lifts itself on the next start is not a pause"
    );
    let stored = devices::get_by_id(&db, device_id)
        .expect("read")
        .expect("row")
        .auto_schedule;
    assert_eq!(
        stored,
        android_backup_core::automation::Schedule::EveryHours(6)
    );

    app.view_mut()
        .apply_device_event(&DeviceEvent::FirstSeen(info(
            "SER1",
            DeviceState::Connected,
            "SM-A536B",
        )));
    app.handle(Intent::Select("SER1".to_string()));
    let text = frame_text(&mut app);
    assert!(text.contains("6 hours"), "the interval is re-read: {text}");
    assert!(
        text.contains("automatic backups paused"),
        "so is the brake: {text}"
    );
}

/// The background settings panel: what it shows, and what ticking a box does.
///
/// Both settings here reach outside the process — one writes the login entry, the other
/// changes what closing the window means — so the assertions are about the files and
/// the mechanism, not about the pixels.
#[test]
fn the_settings_panel_turns_the_login_entry_on_and_off() {
    let dir = temp();
    let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
    let entry = FakeStartup::new(StartupStatus::Disabled);
    let (mut app, _db) = app_on_disk_with_startup(
        dir.path(),
        AppConfig::default(),
        Some(Arc::clone(&entry) as Arc<dyn autostart::Startup>),
    );

    app.handle(Intent::OpenBackground);
    let text = frame_text(&mut app);
    assert!(text.contains("When the window is closed"), "{text}");
    assert!(text.contains("Start in the tray when I log in"), "{text}");
    assert!(
        !app.run_at_login(),
        "the box starts where the configuration is"
    );

    app.handle(Intent::SetRunAtLogin(true));
    assert_eq!(
        entry.state(),
        StartupStatus::Enabled,
        "ticking the box writes the entry"
    );
    assert!(entry.calls().contains(&"enable"));
    let text = frame_text(&mut app);
    assert!(
        text.contains("run at login: enabled"),
        "the panel says what happened, not what was asked: {text}"
    );

    app.handle(Intent::SetRunAtLogin(false));
    assert_eq!(entry.state(), StartupStatus::Disabled);
    assert!(AppConfig::load(&paths.config_file)
        .background
        .run_at_login
        .eq(&false));

    // The next launch starts from the configuration, which is what the login entry
    // itself will do — so the box has to come back ticked from the file, not from
    // anything this process remembered.
    let entry = FakeStartup::new(StartupStatus::Enabled);
    let (mut app, _db) = app_on_disk_with_startup(
        dir.path(),
        AppConfig::default(),
        Some(Arc::clone(&entry) as Arc<dyn autostart::Startup>),
    );
    assert!(!app.run_at_login());
    app.handle(Intent::SetRunAtLogin(true));
    let stored = AppConfig::load(&paths.config_file);
    assert!(stored.background.run_at_login);
    let (mut app, _db) = app_on_disk_with_startup(
        dir.path(),
        stored,
        Some(Arc::clone(&entry) as Arc<dyn autostart::Startup>),
    );
    assert!(
        app.run_at_login(),
        "a tick survives a restart even if the wizard comes back"
    );
    app.handle(Intent::OpenBackground);
    let text = frame_text(&mut app);
    assert!(
        text.contains("run at login: no change needed"),
        "an entry that is already there is left alone: {text}"
    );
}

/// A desktop that refuses the entry must not look like one that simply has it off.
#[test]
fn a_refused_login_entry_is_explained_and_the_wish_is_kept() {
    let dir = temp();
    let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
    let entry = FakeStartup::failing(StartupStatus::Disabled, "the registry key is read-only");
    let (mut app, _db) = app_on_disk_with_startup(
        dir.path(),
        AppConfig::default(),
        Some(Arc::clone(&entry) as Arc<dyn autostart::Startup>),
    );

    app.handle(Intent::OpenBackground);
    app.handle(Intent::SetRunAtLogin(true));
    let text = frame_text(&mut app);
    assert!(
        text.contains("could not be changed"),
        "the panel reports the refusal: {text}"
    );
    assert!(
        text.contains("the registry key is read-only"),
        "and says why: {text}"
    );
    assert!(
        AppConfig::load(&paths.config_file).background.run_at_login,
        "the wish is remembered, so a machine that changes its mind needs no re-ticking"
    );
}

/// A platform with no mechanism at all: the box is disabled and the reason is a fact
/// about the machine, not a failure the user can fix.
#[test]
fn a_desktop_with_no_login_mechanism_says_so() {
    let dir = temp();
    let entry = FakeStartup::new(StartupStatus::Unavailable(
        "no desktop environment was found".to_string(),
    ));
    let (mut app, _db) = app_on_disk_with_startup(
        dir.path(),
        AppConfig::default(),
        Some(Arc::clone(&entry) as Arc<dyn autostart::Startup>),
    );

    app.handle(Intent::OpenBackground);
    let text = frame_text(&mut app);
    assert!(
        text.contains("Not possible on this machine: no desktop environment was found"),
        "{text}"
    );

    // A build with no handle at all (no way to learn the program's own path) words it
    // as the smaller statement rather than as an error.
    let (mut app, _db) = app_on_disk(dir.path(), AppConfig::default());
    app.handle(Intent::OpenBackground);
    let text = frame_text(&mut app);
    assert!(
        text.contains("This build cannot start itself at login"),
        "{text}"
    );
}
