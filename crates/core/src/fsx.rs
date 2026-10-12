//! Small file helpers that more than one part of the app needs.
//!
//! Currently one: writing a file so that a crash halfway through cannot leave a
//! half-written one behind. That matters for the two files whose corruption is
//! invisible until the next launch — the configuration and the login-startup
//! entry — because both are read back by something other than this application
//! (the user's editor, the desktop session), which will not know the difference.

use std::io;
use std::path::Path;

/// Write `contents` to `path`, replacing it only once the whole text is on disk.
///
/// The write goes to a temporary file next to the target (same directory, so the
/// final step is a rename within one filesystem) and is then moved into place.
/// A reader therefore sees either the old file or the complete new one, never a
/// prefix of the new one.
pub fn write_atomic(path: &Path, contents: &str) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let temporary = temporary_path(path);
    std::fs::write(&temporary, contents)?;
    match std::fs::rename(&temporary, path) {
        Ok(()) => Ok(()),
        Err(err) => {
            // Leaving the scratch file behind would be litter, and the caller is
            // about to report the failure anyway.
            let _ = std::fs::remove_file(&temporary);
            Err(err)
        }
    }
}

/// Where the scratch file goes. Kept beside the target, and named so that a
/// leftover from a killed process is recognisable as ours rather than as a
/// second copy of the user's file.
fn temporary_path(path: &Path) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    path.with_file_name(format!(".{name}.tmp"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;

    fn temp() -> tempfile::TempDir {
        tempfile::tempdir().expect("temp dir")
    }

    #[test]
    fn a_write_replaces_the_previous_contents() {
        let dir = temp();
        let path = dir.path().join("file.txt");
        write_atomic(&path, "one").expect("first write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "one");
        write_atomic(&path, "two").expect("second write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "two");
    }

    #[test]
    fn a_write_creates_the_folders_it_needs() {
        let dir = temp();
        let path = dir.path().join("a").join("b").join("file.txt");
        write_atomic(&path, "hello").expect("write");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "hello");
    }

    #[test]
    fn a_write_leaves_no_scratch_file_behind() {
        let dir = temp();
        let path = dir.path().join("file.txt");
        write_atomic(&path, "hello").expect("write");
        let names: Vec<String> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["file.txt".to_string()], "{names:?}");
    }

    #[test]
    fn the_scratch_file_is_hidden_and_beside_the_target() {
        let temporary = temporary_path(Path::new("/tmp/dir/entry.desktop"));
        assert_eq!(temporary, Path::new("/tmp/dir/.entry.desktop.tmp"));
    }
}
