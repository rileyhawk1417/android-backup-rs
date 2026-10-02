//! The eframe window. The **only** module in this crate that knows about
//! `eframe`: everything below it is `App`, which talks in `egui` and in core
//! types, and is therefore testable headlessly (see `tests/ui_headless.rs`).

use android_backup_core::config::AppPaths;
use eframe::egui;

use crate::app::App;

/// Window title, also used as the app name for the window manager.
pub const TITLE: &str = "Android Backup";
/// The smallest window in which the three panels still fit (plan §10).
pub const MIN_SIZE: [f32; 2] = [800.0, 620.0];
/// The size a fresh window opens at: wide enough for the history table.
pub const OPEN_SIZE: [f32; 2] = [1080.0, 760.0];

/// `App` as an `eframe` application.
pub struct Window {
    app: App,
}

impl Window {
    pub fn new(app: App) -> Self {
        Self { app }
    }

    /// The application, for tests that want to drive one frame.
    pub fn app(&mut self) -> &mut App {
        &mut self.app
    }
}

impl eframe::App for Window {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.app.frame(ctx);
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        // The watcher thread and any running job's thread are joined here; the
        // adb *server* is deliberately left running (plan §2.4: other tools use it).
        self.app.stop_background_work();
    }
}

/// The `eframe` options for this application.
pub fn options() -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(TITLE)
            .with_app_id("android-backup")
            .with_inner_size(OPEN_SIZE)
            .with_min_inner_size(MIN_SIZE),
        ..Default::default()
    }
}

/// Open the window and run until it closes.
///
/// Blocks. Returns an `eframe` error when the window could not even be created
/// (no display, no GPU) — the caller logs it and shows it to the user.
pub fn run_native(app: App) -> Result<(), eframe::Error> {
    let window = Window::new(app);
    eframe::run_native(
        TITLE,
        options(),
        Box::new(move |_cc| Ok(Box::new(window) as Box<dyn eframe::App>)),
    )
}

/// Where the application's own files live, or a message explaining why not.
pub fn discover_paths() -> Result<AppPaths, String> {
    AppPaths::discover().map_err(|err| {
        format!("could not work out where to keep the manifest and configuration: {err}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_is_big_enough_for_the_panels() {
        let options = options();
        assert_eq!(
            options.viewport.min_inner_size,
            Some(egui::Vec2::new(MIN_SIZE[0], MIN_SIZE[1]))
        );
        assert_eq!(
            options.viewport.inner_size,
            Some(egui::Vec2::new(OPEN_SIZE[0], OPEN_SIZE[1]))
        );
        assert!(
            options.viewport.min_inner_size.expect("min set").x >= 800.0,
            "the device list and the detail panel must fit side by side"
        );
    }

    #[test]
    fn the_title_is_the_product_name() {
        assert_eq!(TITLE, "Android Backup");
        assert_eq!(options().viewport.title.as_deref(), Some(TITLE));
    }
}
