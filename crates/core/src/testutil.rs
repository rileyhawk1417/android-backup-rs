//! Test helpers shared by unit tests. Compiled only for test builds.
//!
//! Everything here is a fixture: panicking loudly on a broken fixture is the
//! point, so the workspace's no-`unwrap` lints do not apply.
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

/// Write an executable script into `dir`.
///
/// * unix: a `#!/bin/sh` script with mode 0755.
/// * windows: a `.bat` file (spawned through `cmd /C` by the runner).
///
/// Returns the path to pass to [`crate::adb::ProcessAdbRunner::new`].
pub fn write_script(dir: &Path, name: &str, lines: &[&str]) -> PathBuf {
    write_script_raw(dir, name, lines, 0)
}

/// Like [`write_script`] but exits with `exit_code`.
pub fn write_failing_script(dir: &Path, name: &str, lines: &[&str], exit_code: i32) -> PathBuf {
    write_script_raw(dir, name, lines, exit_code)
}

fn write_script_raw(dir: &Path, name: &str, lines: &[&str], exit_code: i32) -> PathBuf {
    #[cfg(unix)]
    {
        let path = dir.join(format!("{name}.sh"));
        let mut body = String::from("#!/bin/sh\n");
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str(&format!("exit {exit_code}\n"));
        std::fs::write(&path, body).expect("write fixture script");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .expect("stat fixture")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod fixture");
        path
    }
    #[cfg(windows)]
    {
        let path = dir.join(format!("{name}.bat"));
        let mut body = String::from("@echo off\r\n");
        for line in lines {
            body.push_str(line);
            body.push_str("\r\n");
        }
        body.push_str(&format!("exit /b {exit_code}\r\n"));
        std::fs::write(&path, body).expect("write fixture script");
        path
    }
}

/// A script that echoes `payload` (possibly multi-line) and exits 0.
pub fn write_output_script(dir: &Path, name: &str, payload: &str) -> PathBuf {
    #[cfg(unix)]
    {
        let path = dir.join(format!("{name}.sh"));
        let body =
            format!("#!/bin/sh\ncat <<'ADB_FIXTURE_EOF'\n{payload}\nADB_FIXTURE_EOF\nexit 0\n");
        std::fs::write(&path, body).expect("write fixture script");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .expect("stat fixture")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod fixture");
        path
    }
    #[cfg(windows)]
    {
        let path = dir.join(format!("{name}.bat"));
        let mut body = String::from("@echo off\r\n");
        for line in payload.lines() {
            body.push_str("echo ");
            body.push_str(
                &line
                    .replace('|', "^|")
                    .replace('<', "^<")
                    .replace('>', "^>")
                    .replace('&', "^&"),
            );
            body.push_str("\r\n");
        }
        body.push_str("exit /b 0\r\n");
        std::fs::write(&path, body).expect("write fixture script");
        path
    }
}

/// Like [`write_script`] but writes exactly `file_name` (no `.sh`/`.bat` suffix).
///
/// Locate/installer tests need a fixture with the real binary name, because the
/// code under test looks for `adb` (or `adb.exe`), not a test-only file name.
pub fn write_named_script(dir: &Path, file_name: &str, lines: &[&str]) -> PathBuf {
    let path = dir.join(file_name);
    #[cfg(unix)]
    {
        let mut body = String::from("#!/bin/sh\n");
        for line in lines {
            body.push_str(line);
            body.push('\n');
        }
        body.push_str("exit 0\n");
        std::fs::write(&path, body).expect("write fixture script");
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)
            .expect("stat fixture")
            .permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&path, perms).expect("chmod fixture");
    }
    #[cfg(windows)]
    {
        let mut body = String::from("@echo off\r\n");
        for line in lines {
            body.push_str(line);
            body.push_str("\r\n");
        }
        body.push_str("exit /b 0\r\n");
        std::fs::write(&path, body).expect("write fixture script");
    }
    path
}

/// Path of a fixture script without creating it (for locate tests).
pub fn script_path(dir: &Path, name: &str) -> PathBuf {
    #[cfg(unix)]
    {
        dir.join(format!("{name}.sh"))
    }
    #[cfg(windows)]
    {
        dir.join(format!("{name}.bat"))
    }
}
