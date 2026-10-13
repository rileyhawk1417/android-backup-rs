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
//!
//! # What the drawing costs on Linux
//!
//! A Linux tray icon is GTK: `tray-icon` builds it from GTK widgets through
//! `libayatana-appindicator`, and gtk-rs refuses to build one before `gtk::init` — by
//! *asserting*, not by returning an error. This window is an `eframe`/`winit` window and
//! never initialises GTK, so building the tray on the window's thread ended the process
//! at startup with "GTK has not been initialized" in the log — and the smoke test that
//! should have caught it watched the crash report appear and called it a pass (it now
//! checks the process and the log instead of the first window title it finds). The tray
//! therefore lives on a thread of its own, which GTK also needs in order to be *served*:
//! the panel asks the icon for its menu over the session bus, and those requests are
//! delivered to a GTK main loop. See [`linux`].

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
/// without a display or without an appindicator library (a bare GNOME, for instance)
/// has nowhere to put a tray icon, and the application then behaves as it did before
/// Wave 9 — closing the window quits — rather than hiding itself where nobody can
/// reach it.
pub fn spawn(wiring: Wiring) -> Result<Box<dyn TrayHandle>, String> {
    platform::spawn(wiring)
}

/// Which of the two implementations this platform gets.
///
/// A platform gets one or the other. On Linux the tray has to be built on a thread of
/// its own and served from a GTK main loop (see [`linux`]); Windows and macOS want it
/// built on the window's thread, inside the message loop that is already running
/// (see [`native`]). Deciding that here keeps [`spawn`] — the seam the application
/// calls — one line wide on every platform.
#[cfg(target_os = "linux")]
use linux as platform;
#[cfg(not(target_os = "linux"))]
use native as platform;

/// Whether this desktop can have a tray icon, as a sentence when it cannot.
///
/// Two things have to be true, and neither is fatal to the application: the session
/// needs a display for an icon to live in, and the appindicator library that
/// `tray-icon` loads lazily has to be findable — because the crate *panics* when it
/// is missing instead of returning an error to log. Asking both questions here is
/// what turns "this desktop cannot do it" into a message rather than a dead process.
#[cfg(target_os = "linux")]
fn tray_availability(display: Option<&str>, indicator: Option<&str>) -> Result<(), String> {
    if display.is_none() {
        return Err(
            "there is no display for a tray icon (neither DISPLAY nor WAYLAND_DISPLAY is set); \
             the application runs without one"
                .to_string(),
        );
    }
    match indicator {
        Some(name) => {
            tracing::debug!(library = name, "appindicator library found");
            Ok(())
        }
        None => Err(format!(
            "this desktop has no appindicator library ({}); install libayatana-appindicator3-1 \
             to get a tray icon — the application runs without one",
            LIBS.join(" or ")
        )),
    }
}

/// The display this session is on, if any: either backend counts, because GTK ends up
/// on the same one the window is on. A variable that is set while its server is gone
/// is not caught here — `gtk::init` is what finds that out, which is why its answer is
/// still reported back to the window.
#[cfg(target_os = "linux")]
fn display_name() -> Option<String> {
    ["WAYLAND_DISPLAY", "DISPLAY"]
        .into_iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
}

/// The Linux indicator library, found **before** the tray is built, in the order the
/// tray crate itself tries the sonames.
#[cfg(target_os = "linux")]
fn indicator_library() -> Option<&'static str> {
    LIBS.iter().copied().find(|name| can_load(name))
}

/// Sonames, in the order `tray-icon`'s Linux backend tries them.
#[cfg(target_os = "linux")]
const LIBS: [&str; 2] = ["libayatana-appindicator3.so.1", "libappindicator3.so.1"];

/// Whether the dynamic loader can find a library by name — the same question the
/// tray crate asks, asked in a way that answers instead of dying.
#[cfg(target_os = "linux")]
fn can_load(name: &str) -> bool {
    // SAFETY: nothing is called through the handle. It is dropped immediately, which
    // is exactly what the loader expects of a caller who only wanted to know.
    unsafe { libloading::Library::new(name) }.is_ok()
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

/// The menu and the two items that change, built in one place so that the GTK thread
/// (Linux) and the window's own thread (Windows, macOS) cannot disagree about an id, a
/// label or an order.
struct MenuParts {
    menu: Menu,
    status: MenuItem,
    toggle: CheckMenuItem,
}

fn build_menu(automation_paused: bool) -> Result<MenuParts, String> {
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

    Ok(MenuParts {
        menu,
        status,
        toggle,
    })
}

/// Send menu clicks to the window, and make it draw a frame to read them.
///
/// The handler runs on whichever thread the shell owns the menu on — the GTK thread on
/// Linux, the window procedure on Windows — and must not touch the application from
/// there, so it forwards the command and asks for a repaint. The wake-up is not a
/// nicety: a click on "Quit" from a *hidden* window has to reach the window before the
/// window can act on it, and some desktops do not deliver a synthetic redraw to a
/// window nobody can see. Unknown ids — the status label, or another library's menu —
/// are ignored.
fn watch_menu_clicks(commands: Sender<TrayCommand>, ctx: Arc<Mutex<Option<egui::Context>>>) {
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
}

/// Draw a state onto a live icon: its image, its tooltip and its status line.
fn apply_state(tray: &TrayIcon, status: &MenuItem, state: TrayState) {
    if let Ok(image) = Icon::from_rgba(icon(state).rgba, ICON_SIZE, ICON_SIZE) {
        if let Err(err) = tray.set_icon(Some(image)) {
            tracing::warn!(%err, "could not update the tray icon");
        }
    }
    if let Err(err) = tray.set_tooltip(Some(state.tooltip())) {
        tracing::debug!(%err, "could not update the tray tooltip");
    }
    status.set_text(state.label());
}

/// Point the automation toggle at the state it currently describes.
fn apply_paused(toggle: &CheckMenuItem, paused: bool) {
    toggle.set_text(automation_item_label(paused));
    toggle.set_checked(!paused);
}

// ---------------------------------------------------------------------------
// Linux: the tray on a thread of its own
// ---------------------------------------------------------------------------

/// The tray on its own thread, because GTK insists on having one (Linux).
///
/// `tray-icon` makes a Linux icon out of GTK widgets through `libayatana-appindicator`,
/// and gtk-rs will not let those widgets be created or touched before `gtk::init` has
/// run, nor from a thread other than the one that ran it — and it *asserts* rather
/// than returning an error, so a window that builds its tray itself dies at startup
/// with "GTK has not been initialized" instead of opening. The GTK thread is also the
/// thread GTK has to be *served* from: the icon registers on the session bus and the
/// panel's requests for its menu are delivered to a GTK main loop, which an egui
/// window does not run. This thread runs `gtk::main` and keeps running it while the
/// window is hidden, which is the state the tray exists for.
///
/// So the tray is a small service with a handle: nothing GTK crosses the thread
/// boundary, only requests one way (state, pause, stop) and menu clicks the other (the
/// commands the menu handler already forwards). The window's [`TrayHandle`] is the
/// handle; if the thread never gets as far as a usable icon, `spawn` says so and the
/// application carries on without a tray, as it does on a desktop that has no
/// appindicator at all.
#[cfg(target_os = "linux")]
mod linux {
    use super::*;
    use std::sync::mpsc::{self, Receiver, SyncSender};
    use std::thread::JoinHandle;
    use std::time::Duration;

    /// How often the GTK thread looks for a request from the window.
    ///
    /// A poll rather than a wake-up, because the queue is nearly always empty, this
    /// thread is idle anyway, and asking a GLib loop every tenth of a second costs less
    /// than the machinery that would avoid asking. The delay cannot be seen: the icon
    /// renders a view model that changes at the speed of a backup, not of a frame.
    const POLL: Duration = Duration::from_millis(100);

    /// How long [`spawn`] waits for the thread to say whether it worked. GTK either
    /// starts in milliseconds or not at all, and a window with a late tray beats a
    /// window that never opens — after this, the thread is left to itself.
    const READY_TIMEOUT: Duration = Duration::from_secs(5);

    /// What the window asks of the tray thread. The other direction needs no inventing:
    /// a menu click is already forwarded by the handler the menu was built with.
    enum Request {
        State(TrayState),
        Paused(bool),
        /// Take the icon down and let the thread end.
        Stop,
    }

    /// What the thread has already been told, so that a per-frame call is not a
    /// per-frame message.
    #[derive(Default)]
    struct Told {
        state: Option<TrayState>,
        paused: Option<bool>,
    }

    /// The handle the window holds. Nothing GTK is in here — which is exactly why a
    /// thread GTK has never heard of may hold it.
    pub struct GtkTray {
        requests: Sender<Request>,
        told: Mutex<Told>,
        thread: Mutex<Option<JoinHandle<()>>>,
    }

    pub fn spawn(wiring: Wiring) -> Result<Box<dyn TrayHandle>, String> {
        tray_availability(display_name().as_deref(), indicator_library())?;

        let (requests, queue) = mpsc::channel();
        let (ready, answer) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("android-backup-tray".to_string())
            .spawn(move || {
                // The tray is a nicety and the window is the application. gtk-rs's one
                // way of reporting "the GTK I was initialised with is not the GTK I was
                // built against" is to assert, so the assert is caught here: the icon
                // is refused and the run continues without one.
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run(queue, wiring, ready.clone())
                }));
                if outcome.is_err() {
                    let _ = ready.try_send(Err("the tray icon could not be built".to_string()));
                    tracing::warn!(
                        "the tray thread panicked; the application carries on without a tray"
                    );
                }
            })
            .map_err(|err| format!("could not start the tray thread: {err}"))?;

        match answer.recv_timeout(READY_TIMEOUT) {
            Ok(Ok(())) => Ok(Box::new(GtkTray {
                requests,
                told: Mutex::new(Told::default()),
                thread: Mutex::new(Some(thread)),
            })),
            Ok(Err(err)) => Err(err),
            Err(_) => Err("the tray icon did not appear in time".to_string()),
        }
    }

    /// The GTK thread: initialise, build, serve, and apply requests until told to stop.
    ///
    /// The outcome is reported exactly once, before the loop, so that the window is
    /// never handed a handle to a tray that is not there.
    fn run(queue: Receiver<Request>, wiring: Wiring, ready: SyncSender<Result<(), String>>) {
        // The thread's first act, and the reason this module exists.
        if let Err(err) = gtk::init() {
            let _ = ready.send(Err(format!("GTK would not start: {err}")));
            return;
        }

        let MenuParts {
            menu,
            status,
            toggle,
        } = match build_menu(wiring.automation_paused) {
            Ok(parts) => parts,
            Err(err) => {
                let _ = ready.send(Err(err));
                return;
            }
        };
        let image = match Icon::from_rgba(icon(TrayState::Idle).rgba, ICON_SIZE, ICON_SIZE) {
            Ok(image) => image,
            Err(err) => {
                let _ = ready.send(Err(format!("could not draw the tray icon: {err}")));
                return;
            }
        };
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_icon(image)
            .with_tooltip(TrayState::Idle.tooltip())
            .with_menu_on_left_click(true)
            .build()
        {
            Ok(tray) => tray,
            Err(err) => {
                let _ = ready.send(Err(format!(
                    "this desktop would not take a tray icon: {err}"
                )));
                return;
            }
        };

        watch_menu_clicks(wiring.commands, Arc::clone(&wiring.ctx));

        // The icon, the menu and the loop that serves them live in this closure,
        // because this is the only thread allowed to touch them. Requests arrive here,
        // "stop" among them — which is what lets the loop, and the thread, end.
        gtk::glib::timeout_add_local(POLL, move || {
            while let Ok(request) = queue.try_recv() {
                match request {
                    Request::State(state) => apply_state(&tray, &status, state),
                    Request::Paused(paused) => apply_paused(&toggle, paused),
                    Request::Stop => {
                        let _ = tray.set_visible(false);
                        gtk::main_quit();
                        return gtk::glib::ControlFlow::Break;
                    }
                }
            }
            gtk::glib::ControlFlow::Continue
        });

        // Only now is there an icon to speak of, and only now does anything else run on
        // this thread.
        let _ = ready.send(Ok(()));
        tracing::info!("tray icon installed");
        gtk::main();
        tracing::debug!("tray thread finished");
    }

    impl TrayHandle for GtkTray {
        fn set_state(&self, state: TrayState) {
            {
                let mut told = self
                    .told
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if told.state == Some(state) {
                    return;
                }
                told.state = Some(state);
            }
            if self.requests.send(Request::State(state)).is_err() {
                tracing::debug!("the tray thread is gone; the icon stays as it was");
            }
        }

        fn set_automation_paused(&self, paused: bool) {
            {
                let mut told = self
                    .told
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if told.paused == Some(paused) {
                    return;
                }
                told.paused = Some(paused);
            }
            if self.requests.send(Request::Paused(paused)).is_err() {
                tracing::debug!("the tray thread is gone; the toggle stays as it was");
            }
        }

        fn close(&self) {
            // Ignore a send that fails: a thread that has already finished is exactly
            // what the caller wanted. The join is what matters — it is what makes
            // quitting wait for the icon to go away.
            let _ = self.requests.send(Request::Stop);
            if let Some(thread) = self
                .thread
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .take()
            {
                let _ = thread.join();
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Windows and macOS: the tray on the window's own thread
// ---------------------------------------------------------------------------

/// The tray on the window's own thread, which is where these shells want it: the icon
/// is driven by the window procedure's message loop, and the window is already running
/// one. There is no second thread here because there is nothing that needs one.
///
/// This one is built on Linux too — it asks for nothing Linux cannot compile — so that
/// code written for a platform the author is *not* sitting at is still type-checked
/// where it is written. It is never used there, which is all the allow says; on Windows
/// and macOS it must be used, and being unused is a warning like any other. (The Linux
/// thread above cannot offer the same, because GTK is not a dependency elsewhere.)
#[cfg_attr(target_os = "linux", allow(dead_code))]
mod native {
    use super::*;

    pub fn spawn(wiring: Wiring) -> Result<Box<dyn TrayHandle>, String> {
        NativeTray::create(wiring)
            .map(|tray| Box::new(tray) as Box<dyn TrayHandle>)
            .map_err(|err| err.to_string())
    }

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
            let MenuParts {
                menu,
                status,
                toggle,
            } = build_menu(automation_paused)?;

            let icon = Icon::from_rgba(icon(TrayState::Idle).rgba, ICON_SIZE, ICON_SIZE)
                .map_err(|err| format!("could not draw the tray icon: {err}"))?;

            let tray = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_icon(icon)
                .with_tooltip(TrayState::Idle.tooltip())
                .with_menu_on_left_click(true)
                .build()
                .map_err(|err| format!("this desktop would not take a tray icon: {err}"))?;

            watch_menu_clicks(commands, ctx);

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
            apply_state(&self.icon, &self.status, state);
        }

        fn set_automation_paused(&self, paused: bool) {
            apply_paused(&self.toggle, paused);
        }

        fn close(&self) {
            let _ = self.icon.set_visible(false);
        }
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

    /// The check that stands between a bare GNOME and a process that dies while
    /// building its tray icon. It has to answer, not guess: a name that is not there
    /// must come back `false`, and one that is (on a machine that has it) `true`.
    #[test]
    #[cfg(target_os = "linux")]
    fn the_indicator_probe_answers_rather_than_assuming() {
        assert!(
            !can_load("libdefinitely-not-installed-4f3a.so.9"),
            "a missing library must not look present"
        );
        // Either the machine has an appindicator (the tray works) or it does not (the
        // tray is skipped); both are outcomes this function is allowed to report.
        let found: Vec<&str> = LIBS.iter().copied().filter(|name| can_load(name)).collect();
        assert_eq!(
            indicator_library().is_some(),
            !found.is_empty(),
            "the verdict and the probe disagree about {LIBS:?}"
        );
    }

    /// The table that decides whether GTK is asked for a menu at all. Both answers are
    /// needed to build an icon, and the message for each has to name the thing that is
    /// missing — this string is what a user sees in the log when their desktop cannot
    /// show a tray.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_tray_needs_both_a_display_and_an_indicator_library() {
        assert!(tray_availability(Some(":0"), Some(LIBS[0])).is_ok());
        assert!(tray_availability(Some("wayland-0"), Some(LIBS[1])).is_ok());

        let no_display = tray_availability(None, Some(LIBS[0])).unwrap_err();
        assert!(no_display.contains("no display"), "{no_display}");

        let no_library = tray_availability(Some(":0"), None).unwrap_err();
        assert!(no_library.contains("appindicator"), "{no_library}");
        assert!(no_library.contains(LIBS[0]), "{no_library}");
    }

    /// The shape of the bug this module was written for, at the only place it can be
    /// reproduced without a desktop: a session with no display must be *refused*, not
    /// attempted. Asking GTK for a menu there panics (and did, in-process, killing the
    /// window during startup), so this is the one test that runs [`spawn`] itself, and
    /// it only runs where the answer is decided before any GTK call is made.
    #[test]
    #[cfg(target_os = "linux")]
    fn without_a_display_the_tray_is_refused_instead_of_killing_the_app() {
        if display_name().is_some() || indicator_library().is_none() {
            // Either this machine has a display — where building a tray would put an
            // icon in the user's panel, which a test has no business doing — or it has
            // no appindicator library, so the reason would be the other one, which the
            // table above already covers.
            return;
        }

        let (commands, _events) = channel();
        let err = spawn(Wiring {
            commands,
            automation_paused: false,
            ctx: Arc::new(Mutex::new(None)),
        })
        .err()
        .expect("a session with no display cannot have a tray icon");
        assert!(err.contains("no display"), "unexpected reason: {err}");
    }

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
