//! Phone-path → backup-path mapping. Pure, no I/O, fully unit-testable.
//!
//! Rules (see plan §7 "Important limitation" and §9 "Database requirements"):
//!
//! * A phone-provided name is **never** used as a trusted filesystem path.
//! * Absolute paths, `..` traversal and control characters are rejected outright.
//! * Characters that are legal on Android but illegal on Windows (`< > : " | ? * \`)
//!   are replaced with `_` so that a backup tree created on Linux stays usable on
//!   Windows. This replacement is deterministic and idempotent; the resulting
//!   destination path is recorded in the manifest, so the mapping is never lost.
//!   It also neutralizes Windows drive prefixes: a phone file named `a:b.jpg`
//!   becomes `a_b.jpg` instead of being mistaken for (or rejected as) a path on
//!   drive `a`.
//! * Windows-reserved device names (`CON`, `NUL`, `COM1`, …) get a `_` suffix.
//! * Trailing dots/spaces (legal on Android, stripped by Windows) become `_`.

use std::fmt;
use std::path::{Path, PathBuf};

/// Errors produced while validating a phone-provided path.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum PathError {
    #[error("path is empty")]
    Empty,
    #[error("path must be relative, got an absolute path: {0}")]
    Absolute(String),
    #[error("path contains a `..` component, which is not allowed: {0}")]
    Traversal(String),
    #[error("path contains a control character at byte offset {1}: {0}")]
    ControlChar(String, usize),
    #[error("path {0} is not inside root {1}")]
    NotUnderRoot(String, String),
}

const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// A validated, sanitized, `/`-separated relative path.
///
/// The only way to build one is [`RelPath::parse`] or [`RelPath::from_absolute`],
/// so any `RelPath` value in the program is guaranteed free of traversal and
/// unsafe characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct RelPath(String);

impl RelPath {
    /// Validate and sanitize an already-relative `/`-separated path.
    pub fn parse(source: &str) -> Result<Self, PathError> {
        if source.is_empty() {
            return Err(PathError::Empty);
        }
        if let Some(off) = source.find(|c: char| c.is_control()) {
            return Err(PathError::ControlChar(source.to_string(), off));
        }
        let trimmed = source.trim_end_matches('/');
        if trimmed.is_empty() {
            return Err(PathError::Empty);
        }
        if trimmed.starts_with('/') || trimmed.starts_with('\\') {
            return Err(PathError::Absolute(source.to_string()));
        }
        let mut segments = Vec::new();
        for seg in trimmed.split('/') {
            if seg.is_empty() || seg == "." {
                // Collapse repeated separators and `.` segments.
                continue;
            }
            if seg == ".." {
                return Err(PathError::Traversal(source.to_string()));
            }
            segments.push(sanitize_component(seg));
        }
        if segments.is_empty() {
            return Err(PathError::Empty);
        }
        Ok(Self(segments.join("/")))
    }

    /// Convert an absolute phone path into a relative path under `root`.
    ///
    /// `root` is normally the shared-storage root detected on the device, e.g.
    /// `/storage/emulated/0`. The root itself yields no path (it is not a file);
    /// paths outside the root are rejected.
    pub fn from_absolute(source: &str, root: &str) -> Result<Self, PathError> {
        let source = source.trim_end_matches('/');
        let root = root.trim_end_matches('/');
        let rest = match source.strip_prefix(root) {
            Some(rest) => rest,
            None => {
                return Err(PathError::NotUnderRoot(
                    source.to_string(),
                    root.to_string(),
                ))
            }
        };
        let rest = rest.strip_prefix('/').unwrap_or(rest);
        Self::parse(rest)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// File name (final segment, already sanitized).
    pub fn file_name(&self) -> &str {
        match self.0.rsplit_once('/') {
            Some((_, name)) => name,
            None => &self.0,
        }
    }

    /// Parent directory as a `/`-separated string, if any.
    pub fn parent_str(&self) -> Option<&str> {
        self.0.rsplit_once('/').map(|(parent, _)| parent)
    }

    pub fn parent(&self) -> Option<RelPath> {
        self.parent_str().map(|p| RelPath(p.to_string()))
    }

    pub fn child(&self, segment: &str) -> RelPath {
        RelPath(format!("{}/{}", self.0, sanitize_component(segment)))
    }

    /// Join under a base directory, re-validating every segment (defense in depth).
    pub fn join_to(&self, base: &Path) -> PathBuf {
        let mut out = base.to_path_buf();
        for seg in self.0.split('/') {
            debug_assert!(!seg.is_empty() && seg != ".." && seg != ".");
            out.push(seg);
        }
        out
    }
}

impl fmt::Display for RelPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Map a phone-relative path to its destination under a device backup root.
pub fn map_to_destination(root: &Path, rel: &RelPath) -> PathBuf {
    rel.join_to(root)
}

/// Deterministic, idempotent replacement of characters that are unsafe in a
/// cross-platform backup tree.
pub fn sanitize_component(seg: &str) -> String {
    let mut out = String::with_capacity(seg.len());
    for ch in seg.chars() {
        if ch.is_control() {
            continue; // control chars are rejected earlier; belt and braces
        }
        if matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*' | '\\') {
            out.push('_');
        } else {
            out.push(ch);
        }
    }
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
        out.push('_');
    }
    if out.is_empty() {
        out.push('_');
    }
    if is_reserved(&out) {
        out.push('_');
    }
    out
}

fn is_reserved(name: &str) -> bool {
    let stem = match name.split_once('.') {
        Some((stem, _)) => stem,
        None => name,
    };
    WINDOWS_RESERVED
        .iter()
        .any(|r| stem.eq_ignore_ascii_case(r))
}

/// A destination path produced for a source path, kept together so callers can
/// never mix them up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MappedPath {
    pub source: String,
    pub rel: RelPath,
    pub destination: PathBuf,
}

/// Map many source paths, disambiguating cases where two distinct source names
/// sanitize to the same destination path (e.g. `a:b.jpg` and `a?b.jpg` on Linux).
///
/// `case_insensitive` should be `true` on Windows so that destinations that
/// differ only by case are disambiguated before they collide on disk.
pub fn map_all(
    root: &Path,
    sources: impl IntoIterator<Item = (String, RelPath)>,
    case_insensitive: bool,
) -> Vec<MappedPath> {
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut out = Vec::new();
    for (source, rel) in sources {
        let key = if case_insensitive {
            rel.as_str().to_lowercase()
        } else {
            rel.as_str().to_string()
        };
        let rel = match seen.get(&key) {
            Some(existing) if existing != &source => disambiguate(&rel, &source),
            _ => {
                seen.insert(key, source.clone());
                rel
            }
        };
        let final_key = if case_insensitive {
            rel.as_str().to_lowercase()
        } else {
            rel.as_str().to_string()
        };
        seen.insert(final_key, source.clone());
        out.push(MappedPath {
            destination: map_to_destination(root, &rel),
            source,
            rel,
        });
    }
    out
}

/// Append a short hash of the original source path before the extension.
fn disambiguate(rel: &RelPath, source: &str) -> RelPath {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let digest = hex::encode(hasher.finalize());
    let tag = &digest[..8];
    let name = rel.file_name();
    let new_name = match name.rsplit_once('.') {
        Some((stem, ext)) => format!("{stem}~{tag}.{ext}"),
        None => format!("{name}~{tag}"),
    };
    match rel.parent_str() {
        Some(parent) => RelPath(format!("{parent}/{new_name}")),
        None => RelPath(new_name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(s: &str) -> RelPath {
        RelPath::parse(s).expect("should parse")
    }

    #[test]
    fn rejects_traversal() {
        assert_eq!(
            RelPath::parse("../../../etc/passwd"),
            Err(PathError::Traversal("../../../etc/passwd".into()))
        );
        assert!(matches!(
            RelPath::parse("DCIM/../../etc"),
            Err(PathError::Traversal(_))
        ));
    }

    #[test]
    fn rejects_absolute() {
        assert!(matches!(
            RelPath::parse("/etc/passwd"),
            Err(PathError::Absolute(_))
        ));
        assert!(matches!(
            RelPath::parse("\\\\server\\share"),
            Err(PathError::Absolute(_))
        ));
    }

    #[test]
    fn drive_like_names_are_neutralized_not_rejected() {
        // Android allows `:` in file names, so `a:b.jpg` is a real file, not a
        // Windows path. Sanitizing removes the ambiguity entirely.
        assert_eq!(ok("a:b.jpg").as_str(), "a_b.jpg");
        assert_eq!(ok("C:/x").as_str(), "C_/x");
    }

    #[test]
    fn rejects_control_chars_and_empty() {
        assert!(matches!(RelPath::parse(""), Err(PathError::Empty)));
        assert!(matches!(RelPath::parse("/"), Err(PathError::Empty)));
        assert!(matches!(
            RelPath::parse("a\0b"),
            Err(PathError::ControlChar(_, 1))
        ));
    }

    #[test]
    fn collapses_redundant_separators() {
        assert_eq!(ok("sdcard/DCIM//x").as_str(), "sdcard/DCIM/x");
        assert_eq!(ok("a/./b").as_str(), "a/b");
        assert_eq!(ok("a/b/").as_str(), "a/b");
    }

    #[test]
    fn sanitizes_windows_illegal_chars() {
        assert_eq!(ok("a:b?c|d*e.jpg").as_str(), "a_b_c_d_e.jpg");
        assert_eq!(ok("back\\slash.jpg").as_str(), "back_slash.jpg");
        let p = ok("IMG 001.jpg");
        assert_eq!(p.as_str(), "IMG 001.jpg", "spaces are legal");
    }

    #[test]
    fn sanitizes_reserved_names() {
        assert_eq!(ok("CON").as_str(), "CON_");
        assert_eq!(ok("con.txt").as_str(), "con.txt_");
        assert_eq!(ok("NUL.log").as_str(), "NUL.log_");
        assert_eq!(ok("COM1").as_str(), "COM1_");
        assert_eq!(ok("CONSOLE").as_str(), "CONSOLE");
    }

    #[test]
    fn sanitizes_trailing_dot_and_space() {
        assert_eq!(ok("weird.").as_str(), "weird_");
        assert_eq!(ok("weird ").as_str(), "weird_");
        assert_eq!(ok("a.b.").as_str(), "a.b_");
    }

    #[test]
    fn sanitize_is_idempotent() {
        for name in ["a_b.jpg", "CON_", "weird_", "plain.jpg", "CONSOLE"] {
            let once = sanitize_component(name);
            assert_eq!(sanitize_component(&once), once, "{name}");
        }
    }

    #[test]
    fn unicode_is_preserved() {
        assert_eq!(ok("Фото/IMG_001.jpg").as_str(), "Фото/IMG_001.jpg");
    }

    #[test]
    fn from_absolute_strips_root() {
        let rel = RelPath::from_absolute(
            "/storage/emulated/0/DCIM/Camera/IMG.jpg",
            "/storage/emulated/0",
        )
        .unwrap();
        assert_eq!(rel.as_str(), "DCIM/Camera/IMG.jpg");
        assert!(matches!(
            RelPath::from_absolute("/sdcard/x", "/storage/emulated/0"),
            Err(PathError::NotUnderRoot(_, _))
        ));
    }

    #[test]
    fn join_to_builds_expected_tree() {
        let base = Path::new("/backups/Phone");
        let rel = ok("DCIM/Camera/IMG_001.jpg");
        assert_eq!(
            map_to_destination(base, &rel),
            PathBuf::from("/backups/Phone/DCIM/Camera/IMG_001.jpg")
        );
        assert_eq!(rel.file_name(), "IMG_001.jpg");
        assert_eq!(rel.parent_str(), Some("DCIM/Camera"));
    }

    #[test]
    fn map_all_disambiguates_sanitized_collisions() {
        let root = Path::new("/b");
        let sources = vec![
            ("/d/a:b.jpg".to_string(), ok("a:b.jpg")),
            ("/d/a?b.jpg".to_string(), ok("a?b.jpg")),
        ];
        let mapped = map_all(root, sources, false);
        assert_eq!(mapped.len(), 2);
        assert_ne!(mapped[0].destination, mapped[1].destination);
        assert!(mapped[1].destination.to_string_lossy().contains('~'));
        // Both keep the .jpg extension.
        assert!(mapped[1].destination.to_string_lossy().ends_with(".jpg"));
    }

    #[test]
    fn map_all_distinct_names_are_untouched() {
        let root = Path::new("/b");
        let sources = vec![
            ("/d/a/IMG_1.jpg".to_string(), ok("a/IMG_1.jpg")),
            ("/d/b/IMG_1.jpg".to_string(), ok("b/IMG_1.jpg")),
        ];
        let mapped = map_all(root, sources, false);
        assert_eq!(mapped[0].destination, PathBuf::from("/b/a/IMG_1.jpg"));
        assert_eq!(mapped[1].destination, PathBuf::from("/b/b/IMG_1.jpg"));
    }

    #[test]
    fn map_all_case_insensitive_only_when_asked() {
        let root = Path::new("/b");
        let sources = vec![
            ("/d/A.jpg".to_string(), ok("A.jpg")),
            ("/d/a.jpg".to_string(), ok("a.jpg")),
        ];
        let linux = map_all(root, sources.clone(), false);
        assert_ne!(linux[0].destination, linux[1].destination);
        let windows = map_all(root, sources, true);
        assert_ne!(windows[0].destination, windows[1].destination);
    }
}
