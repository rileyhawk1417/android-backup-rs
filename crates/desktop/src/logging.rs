//! Logging setup: one file in the data directory, plus stderr while developing.
//!
//! The file is what a user can send you after something went wrong, so it is
//! always written and always plain text (`with_ansi(false)`). The filter is
//! `ANDROID_BACKUP_LOG` when set, `info` otherwise, with this crate's own modules
//! at `debug` so a support log explains what the UI decided.

use std::path::Path;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

/// Log file name inside [`AppPaths::log_dir`](android_backup_core::config::AppPaths).
pub const LOG_FILE: &str = "android-backup.log";

/// The environment variable that overrides the log level.
pub const LOG_ENV: &str = "ANDROID_BACKUP_LOG";

fn filter() -> EnvFilter {
    EnvFilter::try_from_env(LOG_ENV).unwrap_or_else(|_| {
        EnvFilter::new("info,android_backup_core=debug,android_backup_desktop=debug")
    })
}

/// Install the global subscriber.
///
/// Returns the guard that keeps the background writer alive: **hold it for the
/// lifetime of the process**, or the last lines are lost on exit. When the log
/// directory cannot be used, logging falls back to stderr and this returns
/// `None` — a missing log file must never stop the application from starting.
pub fn init(log_dir: &Path) -> Option<WorkerGuard> {
    let file = std::fs::create_dir_all(log_dir)
        .map_err(
            |err| tracing::warn!(dir = %log_dir.display(), %err, "cannot create the log directory"),
        )
        .ok()
        .and_then(|()| {
            std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(log_dir.join(LOG_FILE))
                .map_err(|err| {
                    eprintln!(
                        "android-backup: cannot write {}: {err}",
                        log_dir.join(LOG_FILE).display()
                    )
                })
                .ok()
        });

    let Some(file) = file else {
        let _ = tracing_subscriber::registry()
            .with(filter())
            .with(tracing_subscriber::fmt::layer())
            .try_init();
        return None;
    };

    let (writer, guard) = tracing_appender::non_blocking(file);
    let subscriber = tracing_subscriber::registry().with(filter()).with(
        tracing_subscriber::fmt::layer()
            .with_ansi(false)
            .with_writer(writer),
    );
    // While developing, seeing the log in the terminal beats tailing a file.
    #[cfg(debug_assertions)]
    let subscriber = subscriber.with(tracing_subscriber::fmt::layer());

    let _ = subscriber.try_init();
    tracing::info!(file = %log_dir.join(LOG_FILE).display(), "logging started");
    Some(guard)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_file_is_appended_and_keeps_its_previous_lines() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(LOG_FILE);
        std::fs::write(&path, "an earlier run\n").expect("seed");
        // Exercise the same open flags `init` uses.
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .expect("open");
        use std::io::Write as _;
        writeln!(file, "this run").expect("write");
        let text = std::fs::read_to_string(&path).expect("read");
        assert_eq!(text, "an earlier run\nthis run\n");
    }

    #[test]
    fn an_unusable_log_directory_falls_back_instead_of_failing() {
        let dir = tempfile::tempdir().expect("tempdir");
        // A *file* where the directory should be: `create_dir_all` fails.
        let blocked = dir.path().join("not-a-dir");
        std::fs::write(&blocked, "x").expect("seed");
        assert!(init(&blocked).is_none());
    }
}
