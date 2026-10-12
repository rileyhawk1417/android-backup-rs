//! The system tray (Wave 9, E09.5).
//!
//! # What the tray is for
//!
//! Once closing the window means "keep running" (see [`crate::app`]), a running
//! process needs a visible handle: an icon that says what the app is doing, a menu
//! that can stop it, and a way back to the window. Without one, close-to-tray would
//! be a way to lose an application.
//!
//! # What it is not
//!
//! **Not a second control surface.** Every menu item is translated into an
//! [`Intent`] — the same intent the window's widgets emit — and goes through
//! [`crate::app::App::handle`]. A tray item that poked the database, or the
//! controller, directly would be a second implementation of a decision the window
//! already makes, and the two would drift.
//!
//! **Not a second source of truth.** The icon is a *rendering* of the view model:
//! [`state_of`] reads [`ViewState`] and nothing else. There is no "the tray thinks a
//! backup is running" flag that could be right while the panel is wrong.
//!
//! # Testability
//!
//! All of the above is pure: the state, the icon pixels, the menu text and the
//! command-to-intent mapping are functions over the view model and plain values.
//! The one part that touches an operating system — [`TrayIcon`] and its menu — sits
//! behind [`TrayHandle`], so a headless test drives the whole feature with a
//! scripted double ([`crate::testutil::RecordingTray`]) and never needs a desktop
//! environment. That is not a luxury: a tray cannot be drawn in CI at all, which is
//! exactly why nothing but the drawing may live there.

use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use crate::app::Intent;
use crate::view::ViewState;

/// What the tray icon is currently saying.
///
/// Three states, because three are actionable: nothing happening, work in flight,
/// and something wanting a person. "Attention" outranks "running" — a backup that is
/// progressing is not news, whereas a job that is stuck is the only thing on the
/// machine the user must act on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayState {
    Idle,
    Running,
    Attention,
}

impl TrayState {
    /// The tooltip, which is the only place the tray can explain itself without
    /// being clicked. Windows shows it on hover; Linux appindicators usually do not
    /// show it at all, which is why it is never the *only* place something is said.
    pub fn tooltip(self) -> &'static str {
        match self {
            Self::Idle => "Android Backup — watching for phones",
            Self::Running => "Android Backup — a backup is running",
            Self::Attention => "Android Backup — a backup needs your attention",
        }
    }

    /// The state, in words, for the menu's status line.
    pub fn label(self) -> &'static str {
        match self {
            Self::Idle => "Idle — watching for phones",
            Self::Running => "Backing up…",
            Self::Attention => "Needs attention",
        }
    }
}

/// Read the tray's state out of the view model.
///
/// Attention means a job is paused, or one has stopped with an error: both are
/// states a person has to clear. A job *running* is only interesting when nothing
/// needs attention, because a stuck job and a busy one can be the same click away.
pub fn state_of(view: &ViewState) -> TrayState {
    let mut running = false;
    for job in view.jobs() {
        if job.is_paused() {
            return TrayState::Attention;
        }
        if let Some(outcome) = &job.outcome {
            if !outcome.succeeded() {
                return TrayState::Attention;
            }
        }
        if job.fatal.is_some() {
            return TrayState::Attention;
        }
        running |= job.is_live();
    }
    if running {
        TrayState::Running
    } else {
        TrayState::Idle
    }
}

/// A square RGBA image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IconRgba {
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// How big the tray icon is drawn.
///
/// Windows wants a 16 px icon and scales it; Linux panels ask for 22 px. Drawing
/// once at 32 keeps both crisp and costs nothing (this happens when the state
/// changes, not per frame).
pub const ICON_SIZE: u32 = 32;

/// The colours, in one place, because the icon is the only surface that cannot
/// show a sentence. Shape changes with the colour so the three states stay apart in
/// greyscale, which colour alone would not.
mod colour {
    pub const OUTLINE: [u8; 4] = [0x5f, 0x63, 0x68, 0xff];
    pub const RUNNING: [u8; 4] = [0x1a, 0x73, 0xe8, 0xff];
    pub const RUNNING_BACK: [u8; 4] = [0xb3, 0xcf, 0xf5, 0xff];
    pub const ATTENTION: [u8; 4] = [0xb2, 0x6a, 0x00, 0xff];
    pub const PAPER: [u8; 4] = [0xff, 0xff, 0xff, 0xff];
}

/// Draw the icon for a state.
///
/// Hand-drawn rather than loaded from a file: a release is a zip with a binary in
/// it, and an icon that has to be found on disk is an icon that is sometimes
/// missing. A phone silhouette with a progress ring is all a 32 pixel square can
/// carry.
pub fn icon(state: TrayState) -> IconRgba {
    let mut rgba = vec![0u8; (ICON_SIZE * ICON_SIZE * 4) as usize];
    let mut put = |x: u32, y: u32, pixel: [u8; 4]| {
        if x < ICON_SIZE && y < ICON_SIZE {
            let i = ((y * ICON_SIZE + x) * 4) as usize;
            rgba[i..i + 4].copy_from_slice(&pixel);
        }
    };

    // The phone: a rounded rectangle from (8,3) to (23,28), with rounded corners
    // knocked off by a two-pixel chamfer.
    let (left, right, top, bottom) = (8, 24, 3, 29);
    let body = match state {
        TrayState::Idle => colour::OUTLINE,
        TrayState::Running | TrayState::Attention => colour::PAPER,
    };
    for y in top..bottom {
        for x in left..right {
            let corner = (x < left + 2 || x >= right - 2) && (y < top + 2 || y >= bottom - 2);
            if !corner {
                put(x, y, body);
            }
        }
    }

    // The screen: filled while running, an outline while idle, and left alone for
    // attention (whose badge says the news).
    let (screen_left, screen_right, screen_top, screen_bottom) = (10, 22, 7, 24);
    match state {
        TrayState::Idle => {
            for y in screen_top..screen_bottom {
                for x in screen_left..screen_right {
                    let edge = x == screen_left
                        || x == screen_right - 1
                        || y == screen_top
                        || y == screen_bottom - 1;
                    if edge {
                        put(x, y, colour::OUTLINE);
                    }
                }
            }
        }
        TrayState::Running => {
            // Two thirds filled: a bar, not a spinner, because a still image cannot
            // spin and a partial bar reads as "in progress" at any size.
            let filled_to = screen_top + (screen_bottom - screen_top) * 2 / 3;
            for y in screen_top..screen_bottom {
                for x in screen_left..screen_right {
                    put(
                        x,
                        y,
                        if y < filled_to {
                            colour::RUNNING
                        } else {
                            colour::RUNNING_BACK
                        },
                    );
                }
            }
        }
        TrayState::Attention => {}
    }

    if state == TrayState::Attention {
        // The badge: a disc in the bottom-right corner, with a white exclamation
        // mark. Both shapes at once, so the icon is legible at 16 px.
        let centre = (23.0f32, 23.0f32);
        for y in 0..ICON_SIZE {
            for x in 0..ICON_SIZE {
                let dx = x as f32 + 0.5 - centre.0;
                let dy = y as f32 + 0.5 - centre.1;
                if dx * dx + dy * dy <= 49.0 {
                    put(x, y, colour::ATTENTION);
                }
            }
        }
        for y in 19..25 {
            put(22, y, colour::PAPER);
            put(23, y, colour::PAPER);
        }
        for y in 26..28 {
            put(22, y, colour::PAPER);
            put(23, y, colour::PAPER);
        }
    }

    IconRgba {
        width: ICON_SIZE,
        height: ICON_SIZE,
        rgba,
    }
}

/// What a menu item asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayCommand {
    /// Show and focus the window.
    Open,
    /// Back up every connected device that can be backed up right now.
    BackupAll,
    /// Turn automatic backups on or off, without forgetting any schedule.
    ToggleAutomation,
    /// Quit, joining the worker threads.
    Quit,
}

/// The menu item identifiers, in one place so the shell and the mapper cannot
/// disagree about them.
pub mod ids {
    pub const OPEN: &str = "android-backup.open";
    /// The disabled status line. A label, not a button: it exists because Linux
    /// panels ignore tray tooltips, so "what is happening" needs somewhere else to
    /// be written down.
    pub const STATUS: &str = "android-backup.status";
    pub const BACKUP_ALL: &str = "android-backup.backup-all";
    pub const TOGGLE_AUTOMATION: &str = "android-backup.toggle-automation";
    pub const QUIT: &str = "android-backup.quit";
}

/// Map a menu item id back to what it means. `None` for an item this build does not
/// know (a stale menu after an upgrade, or another library's channel).
pub fn command_for(id: &str) -> Option<TrayCommand> {
    match id {
        ids::OPEN => Some(TrayCommand::Open),
        ids::BACKUP_ALL => Some(TrayCommand::BackupAll),
        ids::TOGGLE_AUTOMATION => Some(TrayCommand::ToggleAutomation),
        ids::QUIT => Some(TrayCommand::Quit),
        _ => None,
    }
}

/// The label for the automation toggle: it says what pressing it *does*, not what
/// is currently true, which is the mistake every "Enabled/Disabled" menu item makes.
pub fn automation_item_label(paused: bool) -> &'static str {
    if paused {
        "Resume automatic backups"
    } else {
        "Pause automatic backups"
    }
}

/// Translate a menu click into the intents the window would have emitted.
///
/// `Open` is the one command that is not an [`Intent`] because the window, not the
/// application state, owns visibility — the caller handles it directly.
pub fn intents_of(command: TrayCommand) -> Vec<Intent> {
    match command {
        TrayCommand::Open => Vec::new(),
        TrayCommand::BackupAll => vec![Intent::StartAllConnected],
        TrayCommand::ToggleAutomation => vec![Intent::ToggleAutomation],
        TrayCommand::Quit => vec![Intent::Quit],
    }
}

/// The tray's own channel to the window: the menu handler runs on the shell's
/// thread, so it must not touch the application.
pub fn channel() -> (Sender<TrayCommand>, Receiver<TrayCommand>) {
    mpsc::channel()
}

/// A live tray icon, as the window needs it.
///
/// Implemented by the native icon and by [`crate::testutil::RecordingTray`].
///
/// Deliberately **not** `Send`/`Sync`: the menu handles inside the real icon are
/// reference-counted to the thread that owns the event loop, which is the same
/// property that makes the menu handler forward commands over a channel instead of
/// reaching into the application. Nothing else in the program is allowed to hold
/// one, so the trait says so.
pub trait TrayHandle {
    /// Change the icon, tooltip and status line. Cheap and idempotent; callers may
    /// call it every frame, and the implementation is expected to skip the work when
    /// nothing changed.
    fn set_state(&self, state: TrayState);
    /// Update the automation toggle's label and checkmark.
    fn set_automation_paused(&self, paused: bool);
    /// Remove the icon. Called when the process is going away, so that a crashed
    /// icon does not linger in the notification area until the user hovers over it.
    fn close(&self);
}

/// Create the tray icon and its menu, if this desktop can have one.
///
/// Returns `Err` with a sentence fit for the log when it cannot: a Linux session
/// without an appindicator host (a bare GNOME, for instance) has nowhere to put a
/// tray icon, and the application then behaves as it did before Wave 9 — closing
/// the window quits — rather than hiding itself where nobody can reach it.
pub fn spawn(wiring: Wiring) -> Result<Box<dyn TrayHandle>, String> {
    NativeTray::create(wiring)
        .map(|tray| Box::new(tray) as Box<dyn TrayHandle>)
        .map_err(|err| err.to_string())
}

/// Everything the tray builder needs from the application.
///
/// Assembled by [`crate::app::App::install_tray`] and consumed by [`spawn`], so the
/// signature of the "make me a tray" seam stays one argument wide as it grows.
pub struct Wiring {
    /// Where menu clicks go. The window drains it (see [`crate::app::App`]).
    pub commands: Sender<TrayCommand>,
    /// The state the automation toggle starts in.
    pub automation_paused: bool,
    /// Used by the menu handler to make the window draw a frame, which is the only
    /// way the command above is ever read. Necessary because the window may be
    /// *hidden* when a menu item is clicked, and a hidden window is not guaranteed a
    /// synthetic redraw on every desktop.
    pub ctx: Arc<Mutex<Option<egui::Context>>>,
}

// ---------------------------------------------------------------------------
// The native icon
// ---------------------------------------------------------------------------

use tray_icon::menu::{CheckMenuItem, Menu, MenuId, MenuItem, PredefinedMenuItem};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

struct NativeTray {
    icon: TrayIcon,
    state: std::sync::Mutex<TrayState>,
    status: MenuItem,
    toggle: CheckMenuItem,
}

impl NativeTray {
    fn create(wiring: Wiring) -> Result<Self, String> {
        let Wiring {
            commands,
            automation_paused,
            ctx,
        } = wiring;
        let menu = Menu::new();
        menu.append(&MenuItem::with_id(
            MenuId::new(ids::OPEN),
            "Open Android Backup",
            true,
            None,
        ))
        .map_err(|err| format!("could not build the tray menu: {err}"))?;

        // The status line is an item rather than part of the tooltip because
        // Linux panels usually ignore tooltips.
        let status = MenuItem::with_id(
            MenuId::new(ids::STATUS),
            TrayState::Idle.label(),
            false,
            None,
        );
        menu.append(&status)
            .map_err(|err| format!("could not build the tray menu: {err}"))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("could not build the tray menu: {err}"))?;
        menu.append(&MenuItem::with_id(
            MenuId::new(ids::BACKUP_ALL),
            "Back up all connected now",
            true,
            None,
        ))
        .map_err(|err| format!("could not build the tray menu: {err}"))?;

        let toggle = CheckMenuItem::with_id(
            MenuId::new(ids::TOGGLE_AUTOMATION),
            automation_item_label(automation_paused),
            true,
            !automation_paused,
            None,
        );
        menu.append(&toggle)
            .map_err(|err| format!("could not build the tray menu: {err}"))?;
        menu.append(&PredefinedMenuItem::separator())
            .map_err(|err| format!("could not build the tray menu: {err}"))?;
        menu.append(&MenuItem::with_id(
            MenuId::new(ids::QUIT),
            "Quit",
            true,
            None,
        ))
        .map_err(|err| format!("could not build the tray menu: {err}"))?;

        let icon = Icon::from_rgba(icon(TrayState::Idle).rgba, ICON_SIZE, ICON_SIZE)
            .map_err(|err| format!("could not draw the tray icon: {err}"))?;

        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(icon)
            .with_tooltip(TrayState::Idle.tooltip())
            .with_menu_on_left_click(true)
            .build()
            .map_err(|err| format!("this desktop would not take a tray icon: {err}"))?;

        // The menu's handler runs on the shell's thread (a GTK callback on Linux,
        // the window procedure on Windows). It must not touch the application, so it
        // forwards the command and the window drains the channel on its own thread
        // (see `App::drain_tray`). Unknown ids — the status label, or another
        // library's menu — are ignored.
        //
        // The wake-up is not a nicety: a click on "Quit" from a *hidden* window has
        // to reach the window before the window can act on it, and some desktops do
        // not deliver a synthetic redraw to a window nobody can see.
        tray_icon::menu::MenuEvent::set_event_handler(Some(
            move |event: tray_icon::menu::MenuEvent| {
                if let Some(command) = command_for(event.id().as_ref()) {
                    let _ = commands.send(command);
                    if let Some(ctx) = ctx
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .as_ref()
                    {
                        ctx.request_repaint();
                    }
                }
            },
        ));

        tracing::info!("tray icon installed");
        Ok(Self {
            icon: tray,
            state: std::sync::Mutex::new(TrayState::Idle),
            status,
            toggle,
        })
    }
}

impl TrayHandle for NativeTray {
    fn set_state(&self, state: TrayState) {
        {
            let mut current = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if *current == state {
                return;
            }
            *current = state;
        }
        if let Ok(icon) = Icon::from_rgba(icon(state).rgba, ICON_SIZE, ICON_SIZE) {
            if let Err(err) = self.icon.set_icon(Some(icon)) {
                tracing::warn!(%err, "could not update the tray icon");
            }
        }
        if let Err(err) = self.icon.set_tooltip(Some(state.tooltip())) {
            tracing::debug!(%err, "could not update the tray tooltip");
        }
        self.status.set_text(state.label());
    }

    fn set_automation_paused(&self, paused: bool) {
        self.toggle.set_text(automation_item_label(paused));
        self.toggle.set_checked(!paused);
    }

    fn close(&self) {
        let _ = self.icon.set_visible(false);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use android_backup_core::backup::job::{JobEvent, JobOutcome};
    use android_backup_core::backup::PauseReason;
    use android_backup_core::database::jobs::{JobCounters, JobStatus};
    use std::time::Duration;

    use crate::view::{JobView, ViewState};

    /// A view holding exactly one job, which is all the tray state depends on: the
    /// tray reads jobs and nothing else.
    fn view_with(job: JobView) -> ViewState {
        let mut view = ViewState::new("adb 1.0");
        view.insert_job(job);
        view
    }

    fn live(id: i64, device_id: i64) -> JobView {
        JobView::new(id, device_id, format!("SER{device_id}"))
    }

    fn finished(id: i64, device_id: i64, status: JobStatus) -> JobView {
        let mut job = live(id, device_id);
        job.apply(&JobEvent::Finished(Box::new(JobOutcome {
            job_id: id,
            device_id,
            status,
            counters: JobCounters::default(),
            error_summary: None,
            duration: Duration::from_secs(1),
            failures: Vec::new(),
        })));
        job
    }

    #[test]
    fn no_jobs_is_idle() {
        assert_eq!(state_of(&ViewState::new("adb 1.0")), TrayState::Idle);
    }

    #[test]
    fn a_live_job_is_running() {
        assert_eq!(state_of(&view_with(live(1, 7))), TrayState::Running);
    }

    #[test]
    fn a_paused_job_wants_a_person() {
        let mut job = live(1, 7);
        job.apply(&JobEvent::Paused {
            reason: PauseReason::DeviceDisconnected,
            file: None,
        });
        assert_eq!(state_of(&view_with(job)), TrayState::Attention);
    }

    #[test]
    fn a_failed_job_wants_a_person_too() {
        let job = finished(1, 7, JobStatus::Failed);
        assert_eq!(state_of(&view_with(job)), TrayState::Attention);
    }

    /// A backup that finished cleanly leaves nothing to do, so the icon goes quiet
    /// again rather than staying lit.
    #[test]
    fn a_successful_job_goes_back_to_idle() {
        let job = finished(1, 7, JobStatus::Completed);
        assert_eq!(state_of(&view_with(job)), TrayState::Idle);
    }

    #[test]
    fn attention_outranks_running() {
        let mut view = view_with(live(1, 7));
        let mut paused = live(2, 9);
        paused.apply(&JobEvent::Paused {
            reason: PauseReason::AuthorizationNeeded,
            file: None,
        });
        view.insert_job(paused);
        assert_eq!(state_of(&view), TrayState::Attention);
    }

    #[test]
    fn a_stopped_job_is_attention_even_without_a_status() {
        let mut job = live(1, 7);
        job.apply(&JobEvent::Fatal {
            message: "destination not found".to_string(),
        });
        assert_eq!(state_of(&view_with(job)), TrayState::Attention);
    }

    #[test]
    fn every_state_draws_a_visible_different_icon() {
        let idle = icon(TrayState::Idle);
        let running = icon(TrayState::Running);
        let attention = icon(TrayState::Attention);

        for image in [&idle, &running, &attention] {
            assert_eq!(image.width, ICON_SIZE);
            assert_eq!(image.height, ICON_SIZE);
            assert_eq!(image.rgba.len(), (ICON_SIZE * ICON_SIZE * 4) as usize);
            let opaque = image.rgba.chunks(4).filter(|p| p[3] > 0).count();
            assert!(opaque > 100, "an icon nobody can see is not an icon");
        }

        let differing = |a: &IconRgba, b: &IconRgba| {
            a.rgba
                .chunks(4)
                .zip(b.rgba.chunks(4))
                .filter(|(x, y)| x != y)
                .count()
        };
        // Not "one pixel": the three states must differ in a way that survives
        // being scaled down to a 16 px panel.
        assert!(differing(&idle, &running) > 50);
        assert!(differing(&idle, &attention) > 50);
        assert!(differing(&running, &attention) > 50);
    }

    #[test]
    fn every_menu_id_maps_back_to_its_command() {
        assert_eq!(command_for(ids::OPEN), Some(TrayCommand::Open));
        assert_eq!(command_for(ids::BACKUP_ALL), Some(TrayCommand::BackupAll));
        assert_eq!(
            command_for(ids::TOGGLE_AUTOMATION),
            Some(TrayCommand::ToggleAutomation)
        );
        assert_eq!(command_for(ids::QUIT), Some(TrayCommand::Quit));
        assert_eq!(command_for("someone.elses.item"), None);
    }

    #[test]
    fn the_automation_item_says_what_it_will_do() {
        assert!(automation_item_label(true).starts_with("Resume"));
        assert!(automation_item_label(false).starts_with("Pause"));
    }

    #[test]
    fn tray_commands_become_the_same_intents_the_window_emits() {
        assert!(intents_of(TrayCommand::Open).is_empty());
        assert_eq!(
            intents_of(TrayCommand::BackupAll),
            vec![Intent::StartAllConnected]
        );
        assert_eq!(
            intents_of(TrayCommand::ToggleAutomation),
            vec![Intent::ToggleAutomation]
        );
        assert_eq!(intents_of(TrayCommand::Quit), vec![Intent::Quit]);
    }
}
