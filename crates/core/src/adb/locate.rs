//! Locating a usable `adb` executable (plan §4.1).
//!
//! Resolution order:
//!
//! 1. [`AdbMode::Manual`] — the exact path the user chose. Never substituted.
//! 2. [`AdbMode::Managed`] — the application-managed directory, then `PATH`.
//!    Falling back to `PATH` avoids forcing a ~1.5 GB download on users who
//!    already have platform-tools; the found [`AdbSource`] is reported so the UI
//!    can tell them which one is in use.
//! 3. [`AdbMode::System`] — only `PATH`.
//!
//! A user's existing installation is never modified or replaced.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::config::AdbMode;

use super::runner::{Adb, ProcessAdbRunner};

/// Where the found executable came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdbSource {
    Managed,
    System,
    Manual,
}

impl AdbSource {
    pub fn describe(&self) -> &'static str {
        match self {
            Self::Managed => "managed by this application",
            Self::System => "from your system PATH",
            Self::Manual => "chosen by you",
        }
    }
}

/// Parsed `adb version` output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
    pub raw: String,
}

impl std::fmt::Display for AdbVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

/// A usable adb executable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AdbFound {
    pub path: PathBuf,
    pub version: AdbVersion,
    pub source: AdbSource,
}

/// Why no usable adb could be found.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AdbLocateError {
    #[error(
        "no adb executable was found (checked the configured path, the managed directory and PATH)"
    )]
    NotFound,
    #[error("adb at {0} exists but could not be executed")]
    NotExecutable(PathBuf),
    #[error("could not understand the output of `adb version`: {0}")]
    InvalidOutput(String),
    #[error("i/o error while looking for adb: {0}")]
    Io(#[from] std::io::Error),
}

/// The platform-appropriate adb binary name.
pub fn adb_binary_name() -> &'static str {
    if cfg!(windows) {
        "adb.exe"
    } else {
        "adb"
    }
}

/// Parse `Android Debug Bridge version 1.0.41`.
pub fn parse_version(output: &str) -> Result<AdbVersion, AdbLocateError> {
    let line = output
        .lines()
        .find(|l| {
            l.to_ascii_lowercase()
                .contains("android debug bridge version")
        })
        .ok_or_else(|| AdbLocateError::InvalidOutput(output.trim().to_string()))?;
    let raw_version = line
        .split_whitespace()
        .last()
        .ok_or_else(|| AdbLocateError::InvalidOutput(output.trim().to_string()))?;
    let mut parts = raw_version.split('.').map(|p| {
        p.chars()
            .take_while(|c| c.is_ascii_digit())
            .collect::<String>()
    });
    let parse_part = |s: Option<String>| s.and_then(|s| s.parse::<u32>().ok());
    let major = parse_part(parts.next())
        .ok_or_else(|| AdbLocateError::InvalidOutput(raw_version.to_string()))?;
    let minor = parse_part(parts.next()).unwrap_or(0);
    let patch = parse_part(parts.next()).unwrap_or(0);
    Ok(AdbVersion {
        major,
        minor,
        patch,
        raw: line.trim().to_string(),
    })
}

/// Function used to prove a candidate binary works. Injectable for tests.
pub type VersionProbe<'a> = dyn Fn(&Path) -> Result<AdbVersion, AdbLocateError> + 'a;

/// Find adb using the given probe.
pub fn find_adb_with(
    mode: &AdbMode,
    managed_dir: &Path,
    probe: &VersionProbe<'_>,
) -> Result<AdbFound, AdbLocateError> {
    match mode {
        AdbMode::Manual { path } => {
            if !path.exists() {
                return Err(AdbLocateError::NotFound);
            }
            match probe(path) {
                Ok(version) => Ok(AdbFound {
                    path: path.clone(),
                    version,
                    source: AdbSource::Manual,
                }),
                Err(err) => Err(classify_probe_error(path, err)),
            }
        }
        AdbMode::System => {
            let path = which_adb()?;
            match probe(&path) {
                Ok(version) => Ok(AdbFound {
                    path,
                    version,
                    source: AdbSource::System,
                }),
                Err(err) => Err(classify_probe_error(&path, err)),
            }
        }
        AdbMode::Managed => {
            let managed = managed_dir.join(adb_binary_name());
            if managed.exists() {
                if let Ok(version) = probe(&managed) {
                    return Ok(AdbFound {
                        path: managed,
                        version,
                        source: AdbSource::Managed,
                    });
                }
            }
            let path = which_adb()?;
            match probe(&path) {
                Ok(version) => Ok(AdbFound {
                    path,
                    version,
                    source: AdbSource::System,
                }),
                Err(err) => Err(classify_probe_error(&path, err)),
            }
        }
    }
}

fn classify_probe_error(path: &Path, err: AdbLocateError) -> AdbLocateError {
    match err {
        AdbLocateError::InvalidOutput(_)
        | AdbLocateError::NotExecutable(_)
        | AdbLocateError::Io(_) => AdbLocateError::NotExecutable(path.to_path_buf()),
        other => other,
    }
}

/// Search `PATH` for the adb binary.
pub fn which_adb() -> Result<PathBuf, AdbLocateError> {
    let path_var = std::env::var_os("PATH").ok_or(AdbLocateError::NotFound)?;
    let name = adb_binary_name();
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(AdbLocateError::NotFound)
}

/// Convenience wrapper that probes with a real process runner.
pub fn find_adb(mode: &AdbMode, managed_dir: &Path) -> Result<AdbFound, AdbLocateError> {
    find_adb_with(mode, managed_dir, &|path| {
        let runner = ProcessAdbRunner::new(path.to_path_buf());
        probe_version(&runner)
    })
}

/// Ask a runner for its version.
pub fn probe_version(adb: &dyn Adb) -> Result<AdbVersion, AdbLocateError> {
    let out = adb
        .run(&["version"], Duration::from_secs(15))
        .map_err(|err| AdbLocateError::InvalidOutput(err.to_string()))?;
    parse_version(&out.stdout)
}

/// Build a runner for a discovered adb.
pub fn runner_for(found: &AdbFound) -> Arc<ProcessAdbRunner> {
    Arc::new(ProcessAdbRunner::new(found.path.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::write_script;

    fn probe_ok() -> impl Fn(&Path) -> Result<AdbVersion, AdbLocateError> {
        |_path: &Path| {
            Ok(AdbVersion {
                major: 1,
                minor: 0,
                patch: 41,
                raw: "Android Debug Bridge version 1.0.41".into(),
            })
        }
    }

    #[test]
    fn parses_version_line() {
        let v = parse_version("Android Debug Bridge version 1.0.41\nVersion 37.0.0-android-tools")
            .unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 0, 41));
    }

    #[test]
    fn parses_short_version_lines() {
        let v = parse_version("Android Debug Bridge version 1.0.39").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 0, 39));
        let v = parse_version("Android Debug Bridge version 1.0").unwrap();
        assert_eq!((v.major, v.minor, v.patch), (1, 0, 0));
    }

    #[test]
    fn rejects_garbage_version_output() {
        assert!(matches!(
            parse_version("not adb at all"),
            Err(AdbLocateError::InvalidOutput(_))
        ));
    }

    #[test]
    fn manual_path_is_used_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let adb = write_script(
            dir.path(),
            "adb",
            &["echo 'Android Debug Bridge version 1.0.41'"],
        );
        let mode = AdbMode::Manual { path: adb.clone() };
        let found = find_adb_with(&mode, dir.path(), &probe_ok()).unwrap();
        assert_eq!(found.path, adb);
        assert_eq!(found.source, AdbSource::Manual);
    }

    #[test]
    fn missing_manual_path_reports_not_found() {
        let dir = tempfile::tempdir().unwrap();
        let mode = AdbMode::Manual {
            path: dir.path().join("nope"),
        };
        assert!(matches!(
            find_adb_with(&mode, dir.path(), &probe_ok()),
            Err(AdbLocateError::NotFound)
        ));
    }

    #[test]
    fn managed_dir_is_preferred_over_path() {
        let dir = tempfile::tempdir().unwrap();
        let managed = dir.path().join("managed");
        std::fs::create_dir_all(&managed).unwrap();
        let managed_adb =
            crate::testutil::write_named_script(&managed, super::adb_binary_name(), &["echo x"]);
        let mode = AdbMode::Managed;
        let found = find_adb_with(&mode, &managed, &probe_ok()).unwrap();
        assert_eq!(found.path, managed_adb);
        assert_eq!(found.source, AdbSource::Managed);
    }

    #[test]
    fn managed_mode_falls_back_to_path() {
        // A "managed" dir without adb: falls through to PATH (which has real adb here,
        // but the probe is injected so this stays deterministic).
        let dir = tempfile::tempdir().unwrap();
        let found = find_adb_with(&AdbMode::Managed, dir.path(), &probe_ok());
        match found {
            Ok(found) => assert_eq!(found.source, AdbSource::System),
            Err(AdbLocateError::NotFound) => { /* no adb on PATH in this environment */ }
            Err(other) => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn probe_failure_maps_to_not_executable() {
        let dir = tempfile::tempdir().unwrap();
        let adb = write_script(dir.path(), "adb", &["echo x"]);
        let mode = AdbMode::Manual { path: adb.clone() };
        let probe = |_: &Path| Err(AdbLocateError::InvalidOutput("nope".into()));
        assert!(matches!(
            find_adb_with(&mode, dir.path(), &probe),
            Err(AdbLocateError::NotExecutable(_))
        ));
    }

    #[test]
    fn real_probe_against_fixture_script() {
        let dir = tempfile::tempdir().unwrap();
        let adb = write_script(
            dir.path(),
            "adb",
            &["echo 'Android Debug Bridge version 1.0.41'"],
        );
        let runner = ProcessAdbRunner::new(adb);
        let version = probe_version(&runner).unwrap();
        assert_eq!(version.major, 1);
        assert_eq!(version.patch, 41);
    }

    #[test]
    fn binary_name_matches_platform() {
        if cfg!(windows) {
            assert_eq!(adb_binary_name(), "adb.exe");
        } else {
            assert_eq!(adb_binary_name(), "adb");
        }
    }
}
