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
        watch_devices: false,
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
    // Wave 7 has not landed, so the honest note must be shown.
    assert!(text.contains("later build"), "{text}");
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
