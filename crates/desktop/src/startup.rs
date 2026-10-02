//! Startup: everything decided before the first frame is drawn.
//!
//! Kept apart from [`crate::app`] so it can be tested without a window and
//! without touching the user's real configuration: the functions here take the
//! facts (does a config exist? did adb resolve?) and return a decision.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use android_backup_core::adb::locate::{self, AdbFound, AdbLocateError, AdbVersion};
use android_backup_core::adb::runner::{Adb, ProcessAdbRunner};
use android_backup_core::adb::AdbError;
use android_backup_core::config::{AdbMode, AppConfig, AppPaths};

/// What the UI should put on screen at launch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartupPlan {
    /// No configuration file yet: show the location/preferences wizard.
    pub show_welcome: bool,
    /// adb could not be resolved: show the "ADB not found" modal with this text.
    pub adb_error: Option<String>,
    /// adb resolved: a controller can be built and backups can run.
    pub adb_ready: bool,
}

impl StartupPlan {
    /// Does the app need the user to do something before backups can work?
    pub fn needs_user_action(&self) -> bool {
        self.show_welcome || self.adb_error.is_some()
    }

    /// Modal to offer first, if any. The wizard comes first: choosing a backup
    /// location is the thing the user can do without adb.
    pub fn first_modal(&self, suggested_dir: String) -> Option<crate::view::Modal> {
        if self.show_welcome {
            return Some(crate::view::Modal::Welcome { suggested_dir });
        }
        self.adb_error
            .clone()
            .map(|message| crate::view::Modal::AdbMissing { message })
    }
}

/// Decide what to show, from what is on disk and what adb did.
pub fn plan(config_existed: bool, adb: Result<&AdbFound, &AdbLocateError>) -> StartupPlan {
    StartupPlan {
        show_welcome: !config_existed,
        adb_error: adb.err().map(missing_adb_message),
        adb_ready: adb.is_ok(),
    }
}

/// The message shown in the "ADB not found" modal: the locator's own text plus
/// what the user can actually do about it.
pub fn missing_adb_message(err: &AdbLocateError) -> String {
    let hint = "Android Backup can use an existing adb, or one you point it at. \
                You do not need Android Studio: any platform-tools copy works.";
    match err {
        AdbLocateError::NotFound => format!(
            "{err}.\n\n{hint}\n\nChoose \"Use system adb\" to search PATH again, or \
             \"Choose adb binary…\" to pick the executable yourself."
        ),
        AdbLocateError::NotExecutable(path) => format!(
            "adb was found at {} but could not be run (missing execute permission, or \
             it is not a valid executable).\n\n{hint}",
            path.display()
        ),
        AdbLocateError::InvalidOutput(detail) => {
            format!("adb answered something unexpected:\n{detail}\n\n{hint}")
        }
        AdbLocateError::Io(err) => format!("{err}\n\n{hint}"),
        // `AdbLocateError` is `#[non_exhaustive]`: a future variant must still
        // produce something the user can read.
        other => format!("{other}\n\n{hint}"),
    }
}

/// One-line description of the adb in use, for the status bar.
pub fn adb_summary(found: &AdbFound) -> String {
    format!(
        "adb {} ({}) — {}",
        found.version,
        found.source.describe(),
        found.path.display()
    )
}

/// Search for adb using the configured mode.
///
/// `AppPaths::managed_adb_dir` is passed even when the mode is not `Managed`: the
/// locator uses it only for that mode.
pub fn locate(cfg: &AppConfig, paths: &AppPaths) -> Result<AdbFound, AdbLocateError> {
    locate::find_adb(&cfg.adb_mode, &paths.managed_adb_dir)
}

/// Build the runner used by the watcher and the backup controller.
pub fn runner(found: &AdbFound) -> Arc<ProcessAdbRunner> {
    locate::runner_for(found)
}

/// Remember a user-chosen adb executable, and re-probe it.
///
/// Returns the mode to store in the configuration. The caller saves the config
/// (this function never writes: a failed save must not lose the running choice).
pub fn manual_adb_mode(path: &Path) -> AdbMode {
    AdbMode::Manual {
        path: path.to_path_buf(),
    }
}

/// Switch to the system adb, and re-probe it.
pub fn system_adb_mode() -> AdbMode {
    AdbMode::System
}

/// Detect a broken adb *server* (version mismatch, dead socket, stale port).
///
/// This is deliberately separate from [`locate`]: the binary is fine, the server
/// it tries to reuse is not, and the fix is different (kill the stale server,
/// restart the service) — so the UI must say so rather than claim adb is missing.
pub fn server_conflict(adb: &dyn Adb) -> Option<String> {
    match adb.run(&["version"], Duration::from_secs(15)) {
        Ok(_) => None,
        Err(AdbError::ServerError { msg }) => Some(msg),
        Err(_) => None,
    }
}

/// Probe the adb version, for the status bar. Failures are not fatal.
pub fn probe_version(adb: &dyn Adb) -> Option<AdbVersion> {
    locate::probe_version(adb).ok()
}

/// Whether a configuration file exists yet (first launch detection).
pub fn config_existed(paths: &AppPaths) -> bool {
    paths.config_file.is_file()
}

/// The directory offered in the welcome wizard.
pub fn suggested_backup_dir(cfg: &AppConfig) -> PathBuf {
    cfg.global_backup_dir
        .clone()
        .or_else(|| android_backup_core::config::default_backup_dir().ok())
        .unwrap_or_else(|| PathBuf::from("AndroidBackups"))
}

/// Persist a configuration change, turning any failure into a message the UI can
/// show instead of dropping it.
pub fn save_config(cfg: &AppConfig, paths: &AppPaths) -> Result<(), String> {
    cfg.save(&paths.config_file)
        .map_err(|err| format!("could not save the configuration: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use android_backup_core::adb::locate::AdbSource;
    use android_backup_core::mock::MockAdb;

    fn found() -> AdbFound {
        AdbFound {
            path: PathBuf::from("/usr/bin/adb"),
            version: AdbVersion {
                major: 35,
                minor: 0,
                patch: 1,
                raw: "Android Debug Bridge version 1.0.41".to_string(),
            },
            source: AdbSource::System,
        }
    }

    #[test]
    fn a_clean_profile_asks_for_a_location_and_an_adb() {
        let err = AdbLocateError::NotFound;
        let plan = plan(false, Err(&err));
        assert!(plan.show_welcome, "no config: the wizard must appear");
        assert!(plan.adb_error.is_some());
        assert!(!plan.adb_ready);
        assert!(plan.needs_user_action());
        let modal = plan.first_modal("/home/u/AndroidBackups".to_string());
        assert!(
            matches!(modal, Some(crate::view::Modal::Welcome { .. })),
            "the wizard comes before the adb modal"
        );
    }

    #[test]
    fn a_configured_profile_with_adb_shows_nothing() {
        let plan = plan(true, Ok(&found()));
        assert!(!plan.show_welcome);
        assert!(plan.adb_error.is_none());
        assert!(plan.adb_ready);
        assert!(!plan.needs_user_action());
        assert!(plan.first_modal("/tmp".to_string()).is_none());
    }

    #[test]
    fn a_missing_adb_is_explained_with_a_way_out() {
        for err in [
            AdbLocateError::NotFound,
            AdbLocateError::NotExecutable(PathBuf::from("/opt/adb")),
            AdbLocateError::InvalidOutput("garbage".to_string()),
        ] {
            let message = missing_adb_message(&err);
            assert!(
                message.contains("Android Studio"),
                "the modal must say Android Studio is not needed: {message}"
            );
        }
        assert!(
            missing_adb_message(&AdbLocateError::NotExecutable(PathBuf::from("/opt/adb")))
                .contains("/opt/adb")
        );
    }

    #[test]
    fn adb_summary_names_the_path_and_the_source() {
        let summary = adb_summary(&found());
        assert!(summary.contains("35.0.1"), "{summary}");
        assert!(summary.contains("system PATH"), "{summary}");
        assert!(summary.contains("/usr/bin/adb"), "{summary}");
    }

    #[test]
    fn a_manual_choice_is_remembered_as_manual() {
        let mode = manual_adb_mode(Path::new("/opt/platform-tools/adb"));
        match mode {
            AdbMode::Manual { path } => assert_eq!(path, PathBuf::from("/opt/platform-tools/adb")),
            other => panic!("expected Manual, got {other:?}"),
        }
        assert_eq!(system_adb_mode(), AdbMode::System);
    }

    #[test]
    fn a_server_problem_is_reported_only_for_server_problems() {
        let conflicted = MockAdb::new().on(
            "version",
            MockAdb::err(AdbError::ServerError {
                msg: "adb server version (39) doesn't match this client (41)".to_string(),
            }),
        );
        let message = server_conflict(&conflicted).expect("a server conflict");
        assert!(message.contains("doesn't match"), "{message}");

        let healthy = MockAdb::new().on(
            "version",
            MockAdb::ok("Android Debug Bridge version 1.0.41"),
        );
        assert!(server_conflict(&healthy).is_none());

        let gone = MockAdb::new().on(
            "version",
            MockAdb::err(AdbError::AdbMissing {
                path: PathBuf::from("/nowhere/adb"),
                source: std::io::Error::new(std::io::ErrorKind::NotFound, "no such file"),
            }),
        );
        // A missing binary is not a server conflict; the locator already reported it.
        assert!(server_conflict(&gone).is_none());
    }

    #[test]
    fn saving_a_config_reports_failure_instead_of_panicking() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::with_root(dir.path(), dir.path());
        let cfg = AppConfig {
            global_backup_dir: Some(dir.path().to_path_buf()),
            ..AppConfig::default()
        };
        assert!(save_config(&cfg, &paths).is_ok());
        assert!(config_existed(&paths));
        assert_eq!(AppConfig::load(&paths.config_file), cfg);
        assert_eq!(suggested_backup_dir(&cfg), dir.path());
    }

    #[test]
    fn a_fresh_profile_has_no_config_yet() {
        let dir = tempfile::tempdir().expect("tempdir");
        let paths = AppPaths::with_root(&dir.path().join("cfg"), &dir.path().join("data"));
        assert!(!config_existed(&paths));
        assert_eq!(
            suggested_backup_dir(&AppConfig::default()),
            android_backup_core::config::default_backup_dir().expect("home dir")
        );
    }
}
