//! Application configuration: a small TOML file, deliberately **separate** from
//! the SQLite manifest (plan §4.1: "ADB should be managed separately from the
//! backup database and user data").
//!
//! `load` never writes: a corrupt or missing file falls back to defaults with a
//! warning, and only an explicit [`AppConfig::save`] persists anything. This is
//! what stops the application from silently overwriting a user's manual ADB path.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::automation::Schedule;
use crate::clock;
use crate::paths::PathError;

/// How the application obtains the `adb` executable (plan §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum AdbMode {
    /// Downloaded and managed by the application (default).
    #[default]
    Managed,
    /// Use whatever `adb` is on `PATH`.
    System,
    /// Use an explicit executable chosen by the user.
    Manual { path: PathBuf },
}

pub const CONFIG_VERSION: u32 = 1;
pub const DEFAULT_POLL_INTERVAL_SECS: u64 = 4;
/// How often the automation clock wakes (E09.3). A minute is plenty: the
/// schedules it serves are measured in hours, and the tick is cheap enough that
/// being wrong about this costs nothing but a little idle CPU.
pub const DEFAULT_AUTOMATION_TICK_SECS: u64 = 60;
const CONFIG_FILE_NAME: &str = "config.toml";
const APP_DIR_NAME: &str = "android-backup";

/// Automation defaults (Wave 9).
///
/// Per-device intent lives in the manifest (E09.1's `auto_schedule_kind` /
/// `auto_schedule_hours`); this is only the schedule a *new* device is offered,
/// so a hand-edited file can never enrol a device behind the user's back.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutomationConfig {
    #[serde(default)]
    pub default_schedule: Schedule,
    /// How often the automation clock wakes, in seconds. Zero — which a
    /// hand-edited file can contain — means "as fast as is sane", not "spin".
    #[serde(default = "default_automation_tick")]
    pub tick_interval_secs: u64,
    /// Automation switched off from the tray, without forgetting anybody's
    /// schedule: a brake, not a reset. Persisted so that "pause" survives a
    /// restart — a brake that releases itself when the window is closed is not
    /// one, and the user pressed it precisely because the app runs unattended.
    #[serde(default)]
    pub paused: bool,
}

impl Default for AutomationConfig {
    fn default() -> Self {
        Self {
            default_schedule: Schedule::default(),
            tick_interval_secs: DEFAULT_AUTOMATION_TICK_SECS,
            paused: false,
        }
    }
}

fn default_automation_tick() -> u64 {
    DEFAULT_AUTOMATION_TICK_SECS
}

/// How the application behaves when nobody is looking at it (Wave 9, E09.5).
///
/// Separate from [`AutomationConfig`] on purpose: "do not back up behind my
/// back" and "keep running when I close the window" are different promises, and a
/// user who wants the tray but no automatic backups must not have to choose.
///
/// The defaults are the cautious ones for automation (`Off` above) and the
/// convenient ones here: closing the window has always kept the tray app alive for
/// a backup in flight, and a first run in 0.1.0 had no tray at all, so nothing that
/// ran before changes behaviour except by the user's own hand.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackgroundConfig {
    /// Closing the window hides it to the tray instead of quitting. Automation
    /// then keeps working, which is the whole point of the tray.
    #[serde(default = "default_true")]
    pub close_to_tray: bool,
    /// Show the tray icon at all. A user who wants the window to be the only
    /// surface can turn it off; closing then quits, as it did before Wave 9.
    #[serde(default = "default_true")]
    pub tray_icon: bool,
    /// Start the application when the user logs in. Off by default: an
    /// application that installs itself into a login session unasked is
    /// malware, however convenient it is.
    #[serde(default)]
    pub run_at_login: bool,
}

impl Default for BackgroundConfig {
    fn default() -> Self {
        Self {
            close_to_tray: true,
            tray_icon: true,
            run_at_login: false,
        }
    }
}

fn default_true() -> bool {
    true
}

/// Everything the application needs to know, persisted as TOML.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default = "default_config_version")]
    pub config_version: u32,
    #[serde(default)]
    pub adb_mode: AdbMode,
    /// Global default backup location; `None` means "not chosen yet".
    #[serde(default)]
    pub global_backup_dir: Option<PathBuf>,
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Directory where managed platform-tools are extracted.
    #[serde(default)]
    pub managed_adb_dir: Option<PathBuf>,
    /// Automation defaults; missing in a 0.1.0 file, hence the serde default.
    #[serde(default)]
    pub automation: AutomationConfig,
    /// Tray, close-to-tray and login behaviour; also missing in a 0.1.0 file.
    #[serde(default)]
    pub background: BackgroundConfig,
}

fn default_config_version() -> u32 {
    CONFIG_VERSION
}

fn default_poll_interval() -> u64 {
    DEFAULT_POLL_INTERVAL_SECS
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            config_version: CONFIG_VERSION,
            adb_mode: AdbMode::default(),
            global_backup_dir: None,
            poll_interval_secs: DEFAULT_POLL_INTERVAL_SECS,
            managed_adb_dir: None,
            automation: AutomationConfig::default(),
            background: BackgroundConfig::default(),
        }
    }
}

/// Errors from configuration I/O.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConfigError {
    #[error("could not determine the home directory for default paths")]
    NoHomeDir,
    #[error("i/o error accessing {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not parse configuration file {path}: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: Box<toml::de::Error>,
    },
    #[error("could not serialize configuration: {0}")]
    Serialize(#[from] toml::ser::Error),
    #[error(transparent)]
    Path(#[from] PathError),
}

/// Resolved filesystem locations used by the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppPaths {
    pub config_file: PathBuf,
    pub data_dir: PathBuf,
    pub log_dir: PathBuf,
    pub managed_adb_dir: PathBuf,
}

impl AppPaths {
    /// Discover platform-appropriate locations.
    ///
    /// * Linux: `~/.config/android-backup`, `~/.local/share/android-backup`
    /// * Windows: `%APPDATA%\android-backup`
    pub fn discover() -> Result<Self, ConfigError> {
        let config_dir = dirs::config_dir().ok_or(ConfigError::NoHomeDir)?;
        let data_dir = dirs::data_dir().unwrap_or_else(|| config_dir.clone());
        Ok(Self::with_root(&config_dir, &data_dir))
    }

    /// Build from explicit roots; used by tests and by callers that manage their
    /// own layout.
    pub fn with_root(config_root: &Path, data_root: &Path) -> Self {
        let config_dir = config_root.join(APP_DIR_NAME);
        let data_dir = data_root.join(APP_DIR_NAME);
        Self {
            config_file: config_dir.join(CONFIG_FILE_NAME),
            log_dir: data_dir.join("logs"),
            managed_adb_dir: data_dir.join("platform-tools"),
            data_dir,
        }
    }
}

/// Platform default backup location: `%USERPROFILE%/AndroidBackups` on Windows,
/// `~/AndroidBackups` elsewhere (plan §6.1).
pub fn default_backup_dir() -> Result<PathBuf, ConfigError> {
    dirs::home_dir()
        .map(|home| home.join("AndroidBackups"))
        .ok_or(ConfigError::NoHomeDir)
}

impl AppConfig {
    /// Load configuration from a file, falling back to defaults when the file is
    /// missing or unreadable. Never writes.
    pub fn load(path: &Path) -> Self {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!(path = %path.display(), "no configuration file yet, using defaults");
                return Self::default();
            }
            Err(err) => {
                tracing::warn!(path = %path.display(), error = %err, "cannot read configuration, using defaults");
                return Self::default();
            }
        };
        match toml::from_str::<AppConfig>(&raw) {
            Ok(mut cfg) => {
                cfg.normalize();
                cfg
            }
            Err(err) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %err,
                    "configuration file is corrupt, using defaults (file left untouched)"
                );
                Self::default()
            }
        }
    }

    /// Persist configuration to `path`, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<(), ConfigError> {
        let mut cfg = self.clone();
        cfg.normalize();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ConfigError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }
        let text = toml::to_string_pretty(&cfg)?;
        // Written whole or not at all: the file is read back by this application at
        // the next launch, and by the user in an editor in between, and half a line
        // of TOML is worse than none.
        crate::fsx::write_atomic(path, &text).map_err(|source| ConfigError::Io {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Clamp/normalize values that could otherwise break the app.
    fn normalize(&mut self) {
        if self.poll_interval_secs == 0 {
            self.poll_interval_secs = DEFAULT_POLL_INTERVAL_SECS;
        }
        if self.config_version == 0 {
            self.config_version = CONFIG_VERSION;
        }
        self.automation.default_schedule = self.automation.default_schedule.clamped();
    }

    /// Effective poll interval as a duration.
    pub fn poll_interval(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.poll_interval_secs.max(1))
    }

    /// Timestamp helper exposed for callers that log config changes.
    pub fn loaded_at(&self) -> String {
        clock::now_iso()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig {
            config_version: CONFIG_VERSION,
            adb_mode: AdbMode::Manual {
                path: PathBuf::from("/opt/adb"),
            },
            global_backup_dir: Some(PathBuf::from("/mnt/backups")),
            poll_interval_secs: 9,
            managed_adb_dir: Some(PathBuf::from("/data/pt")),
            automation: AutomationConfig {
                default_schedule: Schedule::EveryHours(6),
                tick_interval_secs: 30,
                paused: true,
            },
            background: BackgroundConfig {
                close_to_tray: true,
                tray_icon: true,
                run_at_login: true,
            },
        };
        cfg.save(&path).unwrap();
        let loaded = AppConfig::load(&path);
        assert_eq!(loaded, cfg);
    }

    #[test]
    fn a_0_1_0_config_file_loads_unchanged_with_automation_off() {
        // Verbatim what the previous release wrote: no `[automation]` section.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "config_version = 1\npoll_interval_secs = 4\n\n[adb_mode]\nmode = \"system\"\n",
        )
        .unwrap();
        let cfg = AppConfig::load(&path);
        assert_eq!(cfg.automation.default_schedule, Schedule::Off);
        assert_eq!(
            cfg.automation.tick_interval_secs, DEFAULT_AUTOMATION_TICK_SECS,
            "a file from before the clock existed still gets a sane cadence"
        );
        assert_eq!(cfg.adb_mode, AdbMode::System);
        assert_eq!(cfg.poll_interval_secs, 4);
    }

    #[test]
    fn automation_schedule_round_trips_and_is_clamped_on_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "[automation]\ndefault_schedule = { mode = \"every_hours\", hours = 6 }\n",
        )
        .unwrap();
        assert_eq!(
            AppConfig::load(&path).automation.default_schedule,
            Schedule::EveryHours(6)
        );

        // A hand-edited out-of-range interval is clamped, never saved back
        // verbatim, so the settings UI can always represent what it reads.
        std::fs::write(
            &path,
            "[automation]\ndefault_schedule = { mode = \"every_hours\", hours = 100000 }\n",
        )
        .unwrap();
        assert_eq!(
            AppConfig::load(&path).automation.default_schedule,
            Schedule::EveryHours(crate::automation::MAX_INTERVAL_HOURS)
        );

        // `on_connect` has no `hours` to carry.
        std::fs::write(
            &path,
            "[automation]\ndefault_schedule = { mode = \"on_connect\" }\n",
        )
        .unwrap();
        assert_eq!(
            AppConfig::load(&path).automation.default_schedule,
            Schedule::OnConnect
        );
    }

    #[test]
    fn missing_file_yields_defaults_without_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig::load(&path);
        assert_eq!(cfg, AppConfig::default());
        assert!(!path.exists(), "load must never create the file");
    }

    #[test]
    fn corrupt_file_falls_back_and_leaves_file_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "this is not toml {{{").unwrap();
        let cfg = AppConfig::load(&path);
        assert_eq!(cfg, AppConfig::default());
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "this is not toml {{{",
            "load must not rewrite a corrupt file"
        );
    }

    #[test]
    fn unknown_fields_are_ignored_and_partial_files_use_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "poll_interval_secs = 7\nfuture_option = true\n").unwrap();
        let cfg = AppConfig::load(&path);
        assert_eq!(cfg.poll_interval_secs, 7);
        assert_eq!(cfg.adb_mode, AdbMode::Managed);
        assert_eq!(cfg.config_version, CONFIG_VERSION);
        assert_eq!(cfg.automation, AutomationConfig::default());
    }

    #[test]
    fn zero_poll_interval_is_normalized() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "poll_interval_secs = 0\n").unwrap();
        let cfg = AppConfig::load(&path);
        assert_eq!(cfg.poll_interval_secs, DEFAULT_POLL_INTERVAL_SECS);
    }

    #[test]
    fn explicit_manual_adb_path_survives_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        let cfg = AppConfig {
            adb_mode: AdbMode::Manual {
                path: PathBuf::from("/usr/bin/adb"),
            },
            ..AppConfig::default()
        };
        cfg.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).adb_mode, cfg.adb_mode);
    }

    #[test]
    fn default_backup_dir_is_home_based() {
        let dir = default_backup_dir().unwrap();
        assert!(dir.ends_with("AndroidBackups"), "{dir:?}");
    }

    #[test]
    fn app_paths_layout() {
        let paths = AppPaths::with_root(Path::new("/cfg"), Path::new("/data"));
        assert_eq!(
            paths.config_file,
            PathBuf::from("/cfg/android-backup/config.toml")
        );
        assert_eq!(
            paths.managed_adb_dir,
            PathBuf::from("/data/android-backup/platform-tools")
        );
        assert_eq!(paths.log_dir, PathBuf::from("/data/android-backup/logs"));
    }
}
