//! The `egui` rendering of [`crate::app::App`].
//!
//! Every widget here does exactly one thing: draw state, and push an
//! [`Intent`] when the user acts. No widget reads the database, spawns a thread,
//! or decides anything that [`crate::view`] could decide instead.
//!
//! Widgets cannot borrow `&mut App` while reading it, so edits are collected into
//! local buffers and flushed after the frame — the same discipline as the rest of
//! the application, applied to the borrow checker.

use std::time::Duration;

use egui::{Color32, RichText, Ui};

use android_backup_core::database::files::BackupFileRow;

use crate::app::{App, Intent, FILE_PAGE};
use crate::view::{
    self, format_bytes, format_duration, format_speed, progress_headline, Banner, JobView, Modal,
    StartBlock,
};

/// Text colour for a problem. `egui`'s own red is too dark on a light theme and
/// too dim on a dark one, so pick per theme.
fn bad(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0xff, 0x6b, 0x6b)
    } else {
        Color32::from_rgb(0xb0, 0x1c, 0x1c)
    }
}

fn good(ui: &Ui) -> Color32 {
    if ui.visuals().dark_mode {
        Color32::from_rgb(0x7e, 0xd3, 0x88)
    } else {
        Color32::from_rgb(0x1b, 0x6b, 0x2c)
    }
}

fn muted(ui: &Ui) -> Color32 {
    ui.visuals().weak_text_color()
}

/// Draw one frame and then perform whatever the user asked for.
pub fn draw(app: &mut App, ctx: &egui::Context) {
    let mut intents: Vec<Intent> = Vec::new();
    let mut name_edit = app.name_buffer().to_string();
    let mut folder_edit = app.folder_buffer().to_string();
    let mut edits = Edits::default();

    {
        let app_ref: &App = app;
        if let Some(banner) = app_ref.view().banner() {
            top_banner(ctx, banner, &mut intents);
        }
        status_bar(ctx, app_ref, &mut intents);
        egui::SidePanel::left("devices")
            .resizable(true)
            .default_width(300.0)
            .width_range(240.0..=420.0)
            .show(ctx, |ui| device_list(ui, app_ref, &mut intents));

        egui::CentralPanel::default().show(ctx, |ui| match app_ref.view().selected() {
            None => empty_state(ui, app_ref),
            Some(_) => device_panel(ui, app_ref, &mut name_edit, &mut folder_edit, &mut intents),
        });

        // Modals are drawn last so they sit above everything.
        if let Some(modal) = app_ref.view().modal() {
            modal_window(ctx, app_ref, modal, &mut intents);
        }
    }

    if name_edit != app.name_buffer() {
        edits.name = Some(name_edit);
    }
    if folder_edit != app.folder_buffer() {
        edits.folder = Some(folder_edit);
    }
    if let Some(name) = edits.name {
        app.set_name_buffer(name);
    }
    if let Some(folder) = edits.folder {
        app.set_folder_buffer(folder);
    }
    for intent in intents {
        app.handle(intent);
    }
}

/// Deferred text-box writes.
#[derive(Default)]
struct Edits {
    name: Option<String>,
    folder: Option<String>,
}

// ---------------------------------------------------------------------------
// Chrome
// ---------------------------------------------------------------------------

fn top_banner(ctx: &egui::Context, banner: &Banner, intents: &mut Vec<Intent>) {
    egui::TopBottomPanel::top("banner").show(ctx, |ui| {
        ui.horizontal_wrapped(|ui| {
            if banner.blocking {
                ui.colored_label(bad(ui), "⚠");
            } else {
                ui.colored_label(muted(ui), "•");
            }
            ui.label(&banner.message);
            if ui.small_button("Dismiss").clicked() {
                intents.push(Intent::DismissBanner);
            }
        });
    });
}

fn status_bar(ctx: &egui::Context, app: &App, intents: &mut Vec<Intent>) {
    egui::TopBottomPanel::top("status").show(ctx, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.heading("Android Backup");
            ui.separator();
            ui.label(RichText::new(app.view().adb_summary()).small());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .button("Backup location…")
                    .on_hover_text(app.default_destination_hint())
                    .clicked()
                {
                    intents.push(Intent::PickBackupLocation);
                }
                match app.config().global_backup_dir.as_ref() {
                    Some(dir) => {
                        ui.label(
                            RichText::new(view::truncate_middle(&dir.display().to_string(), 48))
                                .small(),
                        );
                    }
                    None => {
                        ui.colored_label(
                            muted(ui),
                            RichText::new("no backup location yet").small(),
                        );
                    }
                }
            });
        });
        if let Some(note) = app.adb_note() {
            ui.colored_label(muted(ui), RichText::new(note).small());
        }
        if let Some(error) = app.location_error() {
            ui.colored_label(bad(ui), error);
        }
        if let Some(error) = app.view().poll_failed() {
            ui.colored_label(bad(ui), format!("Device list unavailable: {error}"));
        }
    });
}

// ---------------------------------------------------------------------------
// Device list
// ---------------------------------------------------------------------------

fn device_list(ui: &mut Ui, app: &App, intents: &mut Vec<Intent>) {
    ui.add_space(4.0);
    ui.label(RichText::new("Devices").strong());
    ui.separator();

    let selected = app.view().selected_serial().map(str::to_string);
    let mut seen_any = false;
    for device in app.view().devices() {
        seen_any = true;
        let is_selected = selected.as_deref() == Some(device.serial.as_str());
        let response = ui.selectable_label(
            is_selected,
            format!("{}  ({})", device.name, device.state_label()),
        );
        if response.clicked() && !is_selected {
            intents.push(Intent::Select(device.serial.clone()));
        }
        // The state and the reason it cannot be used, in one small line.
        ui.horizontal_wrapped(|ui| {
            ui.add_space(12.0);
            if device.is_ready() {
                ui.colored_label(good(ui), RichText::new("ready").small());
            } else if let Some(message) = device.gate_message() {
                ui.colored_label(muted(ui), RichText::new(message).small());
            } else {
                ui.colored_label(muted(ui), RichText::new("not connected").small());
            }
        });
        ui.add_space(2.0);
    }
    if !seen_any {
        ui.add_space(6.0);
        ui.colored_label(
            muted(ui),
            "No devices yet. Connect a phone with USB debugging enabled.",
        );
    }

    ui.with_layout(egui::Layout::bottom_up(egui::Align::Min), |ui| {
        ui.add_space(6.0);
        ui.colored_label(
            muted(ui),
            RichText::new(format!("{} known device(s)", app.view().device_count())).small(),
        );
    });
}

fn empty_state(ui: &mut Ui, app: &App) {
    ui.add_space(24.0);
    ui.vertical_centered(|ui| {
        ui.heading("Select a device");
        ui.add_space(8.0);
        ui.label("Plug in a phone, enable USB debugging, and approve the prompt on its screen.");
        ui.label("Then pick it from the list on the left to choose what to back up.");
        if app.view().device_count() > 0 {
            ui.add_space(8.0);
            ui.colored_label(
                muted(ui),
                "Devices you backed up before are listed even when unplugged.",
            );
        }
    });
}

// ---------------------------------------------------------------------------
// Device panel
// ---------------------------------------------------------------------------

fn device_panel(
    ui: &mut Ui,
    app: &App,
    name_edit: &mut String,
    folder_edit: &mut String,
    intents: &mut Vec<Intent>,
) {
    let Some(device) = app.view().selected() else {
        return;
    };
    let device_id = device.row.as_ref().map(|row| row.id);

    ui.horizontal(|ui| {
        ui.heading(&device.name);
        ui.colored_label(muted(ui), device.state_label());
        if device.is_ready() {
            ui.colored_label(good(ui), "ready");
        }
    });
    if let Some(message) = device.gate_message() {
        ui.colored_label(bad(ui), message);
    }
    if let Some(model) = &device.model {
        let version = device.android_version.as_deref().unwrap_or("?");
        ui.colored_label(muted(ui), format!("{model} · Android {version}"));
    } else if device.row.is_none() {
        ui.colored_label(
            muted(ui),
            "Not in the manifest yet — waiting for the first poll.",
        );
    }

    ui.add_space(6.0);
    ui.separator();

    if device_id.is_none() {
        // Nothing below this point can work without a manifest row.
        return;
    }

    // -- name ------------------------------------------------------------
    ui.horizontal(|ui| {
        ui.label("Name");
        ui.add(
            egui::TextEdit::singleline(name_edit)
                .desired_width(220.0)
                .hint_text("e.g. Kitchen phone"),
        );
        if ui.button("Save").clicked() {
            intents.push(Intent::ApplyName);
        }
    });
    if let Some(error) = app.name_error() {
        ui.colored_label(bad(ui), error);
    }

    // -- destination ------------------------------------------------------
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        ui.label("Backup to");
        match app.selected_destination() {
            Some((path, own)) => {
                ui.label(view::truncate_middle(&path.display().to_string(), 60));
                if own {
                    ui.colored_label(muted(ui), "(this device's own folder)");
                }
            }
            None => {
                ui.colored_label(bad(ui), "no location configured");
            }
        }
    });
    ui.horizontal(|ui| {
        if ui.button("Change…").clicked() {
            intents.push(Intent::PickDeviceLocation);
        }
        if device
            .row
            .as_ref()
            .is_some_and(|row| row.destination_path.is_some())
            && ui.button("Use the shared location").clicked()
        {
            intents.push(Intent::ClearDeviceLocation);
        }
    });

    // -- folders ----------------------------------------------------------
    ui.add_space(6.0);
    ui.label(RichText::new("Folders to back up").strong());
    let mut any = false;
    for folder in app.folders() {
        any = true;
        ui.horizontal(|ui| {
            let mut enabled = folder.enabled;
            let response =
                ui.checkbox(&mut enabled, view::truncate_middle(&folder.source_path, 48));
            if response.changed() {
                intents.push(Intent::ToggleFolder {
                    source_path: folder.source_path.clone(),
                    enabled,
                });
            }
            let custom = !android_backup_core::database::devices::DEFAULT_FOLDERS
                .contains(&folder.source_path.as_str());
            if custom && ui.small_button("Remove").clicked() {
                intents.push(Intent::RemoveFolder {
                    source_path: folder.source_path.clone(),
                });
            }
        });
    }
    if !any {
        ui.colored_label(muted(ui), "No folders yet.");
    }
    ui.horizontal(|ui| {
        ui.add(
            egui::TextEdit::singleline(folder_edit)
                .desired_width(180.0)
                .hint_text("e.g. Podcasts"),
        );
        if ui.button("Add folder").clicked() {
            intents.push(Intent::AddFolder);
        }
    });
    if let Some(error) = app.folder_error() {
        ui.colored_label(bad(ui), error);
    }
    if !app.has_enabled_folder() {
        ui.colored_label(bad(ui), "Select at least one folder before starting.");
    }

    // -- backup -----------------------------------------------------------
    ui.add_space(8.0);
    ui.separator();
    backup_controls(ui, app, intents);

    // -- last backup -------------------------------------------------------
    ui.add_space(6.0);
    let latest = device_id.and_then(|id| app.latest_job(id));
    ui.label(RichText::new(view::last_backup_line(latest)).strong());

    // -- history ----------------------------------------------------------
    ui.add_space(4.0);
    ui.separator();
    history_section(ui, app, intents);
}

fn backup_controls(ui: &mut Ui, app: &App, intents: &mut Vec<Intent>) {
    let block = app.start_block();
    let live = app.job_is_live();
    let paused = app.is_paused();

    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(block.is_none() && !live, |ui| {
            if ui
                .add(egui::Button::new(RichText::new("Back up now").strong()))
                .clicked()
            {
                intents.push(Intent::Start);
            }
        });
        if live {
            if paused {
                if ui.button("Resume").clicked() {
                    intents.push(Intent::Resume);
                }
            } else if ui.button("Pause").clicked() {
                intents.push(Intent::Pause);
            }
            if ui.button("Cancel").clicked() {
                intents.push(Intent::Cancel);
            }
        }
    });
    if let Some(block) = block {
        ui.colored_label(muted(ui), block.message());
    }
    if let Some(note) = app.start_note() {
        ui.colored_label(bad(ui), note);
    }

    if let Some(job) = app.selected_job() {
        job_progress(ui, job);
    }
}

fn job_progress(ui: &mut Ui, job: &JobView) {
    ui.add_space(4.0);
    ui.add(egui::ProgressBar::new(job.progress_fraction()).show_percentage());

    ui.horizontal_wrapped(|ui| {
        ui.label(progress_headline(&job.progress));
        if let Some(speed) = format_speed(job.progress.bytes_per_sec) {
            ui.colored_label(muted(ui), speed);
        }
        if let Some(eta) = job.progress.eta_secs {
            ui.colored_label(
                muted(ui),
                format!("about {} left", format_duration(Duration::from_secs(eta))),
            );
        }
    });
    if let Some(line) = job.plan_line() {
        ui.colored_label(muted(ui), line);
    }
    if let Some(path) = &job.current_file {
        ui.colored_label(muted(ui), view::truncate_middle(path, 72));
    }
    if let Some((strip, problem)) = job.strip() {
        if problem {
            ui.colored_label(bad(ui), strip);
        } else {
            ui.colored_label(good(ui), strip);
        }
    }
    for warning in &job.warnings {
        ui.colored_label(muted(ui), format!("• {warning}"));
    }
    for (source, reason) in &job.failures {
        ui.colored_label(bad(ui), format!("{source}: {reason}"));
    }
    if job.failures_total > job.failures.len() as u64 {
        ui.colored_label(
            muted(ui),
            format!(
                "and {} more failure(s)",
                job.failures_total - job.failures.len() as u64
            ),
        );
    }
    if job.archived > 0 {
        ui.colored_label(
            muted(ui),
            format!("{} replaced file(s) moved to history", job.archived),
        );
    }
}

fn history_section(ui: &mut Ui, app: &App, intents: &mut Vec<Intent>) {
    let Some(device_id) = app
        .view()
        .selected()
        .and_then(|d| d.row.as_ref().map(|r| r.id))
    else {
        return;
    };
    let rows = app.history(device_id);
    ui.label(RichText::new(format!("History ({})", rows.len())).strong());
    if rows.is_empty() {
        ui.colored_label(muted(ui), "No backups recorded for this device yet.");
        return;
    }

    // A plain grid of labels: no sorting, no virtual scrolling — the list is
    // paged in the manifest instead (HISTORY_PAGE rows at a time).
    egui::Grid::new("history")
        .num_columns(5)
        .striped(true)
        .spacing([12.0, 4.0])
        .show(ui, |ui| {
            for heading in [
                "When",
                "Took",
                "Result",
                "Copied / unchanged / failed",
                "Size",
            ] {
                ui.label(RichText::new(heading).strong());
            }
            ui.end_row();

            for row in rows {
                let opened = app.open_job() == Some(row.id);
                let cells = view::history_cells(row);
                let arrow = if opened { "▾" } else { "▸" };
                let response = ui.selectable_label(opened, format!("{arrow} {}", cells[0]));
                if response.clicked() {
                    intents.push(Intent::ToggleHistoryRow(row.id));
                }
                // Column 0 is drawn as the button; the rest are plain text.
                for cell in cells.iter().skip(1) {
                    ui.label(cell);
                }
                ui.end_row();

                if opened {
                    ui.label("");
                    ui.vertical(|ui| {
                        file_list(ui, app, intents);
                    });
                    for _ in 0..3 {
                        ui.label("");
                    }
                    ui.end_row();
                }
            }
        });
}

/// The expanded file list of one job: what was actually copied.
fn file_list(ui: &mut Ui, app: &App, intents: &mut Vec<Intent>) {
    let files = app.job_files();
    if files.is_empty() {
        ui.colored_label(muted(ui), "This job recorded no files.");
        return;
    }
    let mut failures = 0u64;
    for file in files {
        file_row(ui, file);
        if !file.is_verified() {
            failures += 1;
        }
    }
    if app.has_more_files() {
        if ui.button("Load more").clicked() {
            intents.push(Intent::LoadMoreFiles);
        }
    } else {
        ui.colored_label(
            muted(ui),
            format!("{} file(s) shown · {} not verified", files.len(), failures),
        );
    }
}

fn file_row(ui: &mut Ui, file: &BackupFileRow) {
    ui.horizontal_wrapped(|ui| {
        if file.is_verified() {
            ui.colored_label(good(ui), "✓");
        } else {
            ui.colored_label(bad(ui), "✗");
        }
        ui.label(view::truncate_middle(&file.source_path, 56));
        match file.size() {
            Some(size) => {
                ui.colored_label(muted(ui), format_bytes(size));
            }
            None => {
                ui.colored_label(muted(ui), "—");
            }
        }
        if !file.is_verified() {
            ui.colored_label(bad(ui), &file.status);
        }
    });
}

// ---------------------------------------------------------------------------
// Modals
// ---------------------------------------------------------------------------

fn modal_window(ctx: &egui::Context, app: &App, modal: &Modal, intents: &mut Vec<Intent>) {
    let title = modal.title().to_string();
    egui::Window::new(title)
        .collapsible(false)
        .resizable(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            ui.set_max_width(460.0);
            match modal {
                Modal::Welcome { suggested_dir } => {
                    ui.label("Welcome. Two quick things before your first backup:");
                    ui.add_space(6.0);
                    ui.label(RichText::new("1. Where should backups be stored?").strong());
                    ui.colored_label(muted(ui), suggested_dir);
                    ui.add_space(6.0);
                    ui.label(RichText::new("2. adb").strong());
                    ui.colored_label(muted(ui), adb_status_text(app));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button("Use the suggested folder").clicked() {
                            intents.push(Intent::AcceptSuggestedLocation);
                        }
                        if ui.button("Choose another…").clicked() {
                            intents.push(Intent::PickBackupLocation);
                        }
                        if ui.button("Later").clicked() {
                            intents.push(Intent::DismissModal);
                        }
                    });
                }
                Modal::AdbMissing { message } => {
                    ui.label(message);
                    ui.add_space(8.0);
                    adb_actions(ui, app, intents);
                }
                Modal::AdbInstalling {
                    stage,
                    detail,
                    fraction,
                } => {
                    ui.label(stage);
                    ui.add_space(6.0);
                    // A bar when the total is known, a spinner when it is not:
                    // a bar with no fraction would sit at zero and look stalled.
                    match fraction {
                        Some(value) => {
                            ui.add(
                                egui::ProgressBar::new(*value)
                                    .show_percentage()
                                    .desired_width(320.0),
                            );
                        }
                        None => {
                            ui.horizontal(|ui| {
                                ui.spinner();
                                ui.colored_label(muted(ui), "working…");
                            });
                        }
                    }
                    if !detail.is_empty() {
                        ui.add_space(4.0);
                        ui.colored_label(muted(ui), detail);
                    }
                    ui.add_space(8.0);
                    ui.label(
                        "platform-tools goes into this app's data directory. \
                         Your existing adb, if any, is left alone until the \
                         download has been verified.",
                    );
                    ui.add_space(8.0);
                    if ui.button("Cancel").clicked() {
                        intents.push(Intent::CancelAdbInstall);
                    }
                }
                Modal::ServerConflict { message } => {
                    ui.colored_label(bad(ui), message);
                    ui.add_space(6.0);
                    ui.label("Stopping the local adb server usually clears this. Nothing else is affected.");
                    ui.add_space(8.0);
                    adb_actions(ui, app, intents);
                    if ui.button("Close").clicked() {
                        intents.push(Intent::DismissModal);
                    }
                }
                Modal::Error { message, .. } => {
                    ui.colored_label(bad(ui), message);
                    ui.add_space(8.0);
                    if ui.button("Close").clicked() {
                        intents.push(Intent::DismissModal);
                    }
                }
            }
        });
}

fn adb_status_text(app: &App) -> String {
    match app.adb_found() {
        Some(found) => format!(
            "Found: {} ({})",
            found.path.display(),
            found.source.describe()
        ),
        None => "Not found yet.".to_string(),
    }
}

fn adb_actions(ui: &mut Ui, app: &App, intents: &mut Vec<Intent>) {
    let (available, note) = app.install_adb_button();
    ui.horizontal_wrapped(|ui| {
        ui.add_enabled_ui(available, |ui| {
            if ui.button("Install automatically").clicked() {
                intents.push(Intent::InstallAdb);
            }
        });
        if ui.button("Choose adb…").clicked() {
            intents.push(Intent::ChooseAdbBinary);
        }
        if ui.button("Use the system adb").clicked() {
            intents.push(Intent::UseSystemAdb);
        }
        if ui.button("Try again").clicked() {
            intents.push(Intent::RetryAdb);
        }
    });
    if !available {
        ui.colored_label(muted(ui), note);
    }
}

// ---------------------------------------------------------------------------
// Unused-import guards: the page size is part of the UI contract, and the start
// gating is rendered by `backup_controls`, so both are referenced here.
// ---------------------------------------------------------------------------

/// Rows of history the manifest returns per query (kept visible to the UI layer
/// so the paging constant has exactly one definition).
pub const HISTORY_PAGE_SIZE: u32 = view::HISTORY_PAGE;
/// Files of one job fetched at a time.
pub const FILE_PAGE_SIZE: usize = FILE_PAGE;

/// A one-line description of a start block, used by the tooltip on the button.
pub fn start_block_hint(block: &StartBlock) -> String {
    block.message()
}
