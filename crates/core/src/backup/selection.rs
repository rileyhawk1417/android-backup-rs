//! Which phone folders a backup covers (plan F1.2, F7.2).
//!
//! Selection is expressed **relative to one storage root** (normally
//! `/storage/emulated/0`). That is what keeps the engine simple and what makes
//! [`crate::paths::RelPath`] — and therefore the cross-platform destination
//! mapping — sound.

use crate::paths::{PathError, RelPath};

/// Storage roots to try, most likely first. The first one that exists on the
/// device is used; `/sdcard` and `/storage/self/primary` are symlinks on most
/// devices, so the canonical path is preferred.
pub const CANDIDATE_STORAGE_ROOTS: &[&str] =
    &["/storage/emulated/0", "/sdcard", "/storage/self/primary"];

/// Selection problems.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum SelectionError {
    #[error("no folders are selected for backup")]
    Empty,

    #[error("folder {path} is not inside the storage root {root}")]
    OutsideStorage { path: String, root: String },

    #[error("folder path {path} is not usable: {message}")]
    Invalid { path: String, message: String },
}

/// One folder to back up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectedFolder {
    /// Phone-absolute path, normalized, e.g. `/storage/emulated/0/DCIM`.
    pub absolute: String,
    /// Destination-relative path, e.g. `DCIM`.
    pub rel: RelPath,
    /// True when the user typed this path rather than picking a default.
    pub custom: bool,
}

/// The resolved selection for a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub storage_root: String,
    pub folders: Vec<SelectedFolder>,
    /// Back up the whole storage root rather than individual folders.
    pub whole_storage: bool,
}

impl Selection {
    pub fn is_empty(&self) -> bool {
        !self.whole_storage && self.folders.is_empty()
    }

    /// Folder paths to enumerate.
    pub fn enumerable_folders(&self) -> Vec<String> {
        if self.whole_storage {
            vec![self.storage_root.clone()]
        } else {
            self.folders.iter().map(|f| f.absolute.clone()).collect()
        }
    }
}

/// Pick the storage root present on the device.
pub fn pick_storage_root(available: &[String]) -> Option<String> {
    for candidate in CANDIDATE_STORAGE_ROOTS {
        if let Some(found) = available.iter().find(|p| p.as_str() == *candidate) {
            return Some(found.clone());
        }
    }
    // Unknown layout: accept a device-reported root that looks like shared storage.
    available
        .iter()
        .find(|p| p.starts_with("/storage/") || p.starts_with("/mnt/"))
        .cloned()
}

/// Collapse duplicate separators and normalize separators to `/`.
pub fn normalize_remote(path: &str) -> String {
    let unified = path.replace('\\', "/");
    let mut out = String::with_capacity(unified.len());
    let mut last_was_slash = false;
    for ch in unified.chars() {
        if ch == '/' {
            if last_was_slash {
                continue;
            }
            last_was_slash = true;
        } else {
            last_was_slash = false;
        }
        out.push(ch);
    }
    let trimmed = out.trim_end_matches('/');
    if trimmed.is_empty() {
        "/".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Turn a configured folder entry into a phone-absolute path under `root`.
///
/// Accepts `DCIM`, `/storage/emulated/0/DCIM`, `/sdcard/DCIM` and Windows-style
/// backslashes; anything outside `root` is rejected rather than silently mapped
/// somewhere else.
pub fn to_absolute(root: &str, entry: &str) -> Result<String, SelectionError> {
    let root = normalize_remote(root);
    let entry = entry.trim();
    if entry.is_empty() {
        return Err(SelectionError::Invalid {
            path: entry.to_string(),
            message: "empty path".into(),
        });
    }
    let normalized = normalize_remote(entry);
    if !normalized.starts_with('/') {
        return Ok(normalize_remote(&format!("{root}/{normalized}")));
    }
    // Absolute: rewrite a symlinked alias onto the canonical root.
    for alias in CANDIDATE_STORAGE_ROOTS {
        if *alias == root.as_str() {
            continue;
        }
        if let Some(rest) = normalized.strip_prefix(alias) {
            if rest.is_empty() || rest.starts_with('/') {
                return Ok(normalize_remote(&format!("{root}{rest}")));
            }
        }
    }
    Ok(normalized)
}

/// Build a selection from configured folder entries.
pub fn build(
    storage_root: &str,
    entries: impl IntoIterator<Item = String>,
    custom: &[String],
    whole_storage: bool,
) -> Result<Selection, SelectionError> {
    let root = normalize_remote(storage_root);
    let mut folders: Vec<SelectedFolder> = Vec::new();

    for (entry, is_custom) in entries
        .into_iter()
        .map(|e| (e, false))
        .chain(custom.iter().cloned().map(|e| (e, true)))
    {
        let absolute = to_absolute(&root, &entry)?;
        if absolute == root {
            // Selecting the root itself means "everything".
            return Ok(Selection {
                storage_root: root,
                folders: Vec::new(),
                whole_storage: true,
            });
        }
        let rel = RelPath::from_absolute(&absolute, &root).map_err(|err| match err {
            PathError::NotUnderRoot(path, root) => SelectionError::OutsideStorage { path, root },
            other => SelectionError::Invalid {
                path: absolute.clone(),
                message: other.to_string(),
            },
        })?;
        folders.push(SelectedFolder {
            absolute,
            rel,
            custom: is_custom,
        });
    }

    folders.sort_by(|a, b| a.absolute.cmp(&b.absolute));
    folders.dedup_by(|a, b| a.absolute == b.absolute);

    if folders.is_empty() && !whole_storage {
        return Err(SelectionError::Empty);
    }
    Ok(Selection {
        storage_root: root,
        folders,
        whole_storage,
    })
}

/// Folders that are direct children of `parent`, used when a parent folder is
/// selected: their files are already covered, so they need not be enumerated
/// twice.
pub fn is_covered_by_ancestor(folder: &SelectedFolder, all: &[SelectedFolder]) -> bool {
    all.iter().any(|candidate| {
        candidate.absolute != folder.absolute
            && folder
                .absolute
                .starts_with(&format!("{}/", candidate.absolute))
    })
}

/// Drop folders already covered by a selected ancestor.
pub fn remove_redundant(folders: Vec<SelectedFolder>) -> Vec<SelectedFolder> {
    folders
        .iter()
        .filter(|folder| !is_covered_by_ancestor(folder, &folders))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOT: &str = "/storage/emulated/0";

    fn entries(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn relative_entries_are_anchored_at_the_root() {
        let sel = build(ROOT, entries(&["DCIM", "Download"]), &[], false).unwrap();
        assert_eq!(sel.folders[0].absolute, "/storage/emulated/0/DCIM");
        assert_eq!(sel.folders[0].rel.as_str(), "DCIM");
        assert_eq!(sel.folders[1].absolute, "/storage/emulated/0/Download");
    }

    #[test]
    fn absolute_entries_are_accepted() {
        let sel = build(
            ROOT,
            entries(&["/storage/emulated/0/DCIM/Camera"]),
            &[],
            false,
        )
        .unwrap();
        assert_eq!(sel.folders[0].rel.as_str(), "DCIM/Camera");
    }

    #[test]
    fn sdcard_alias_is_rewritten_to_the_canonical_root() {
        let sel = build(ROOT, entries(&["/sdcard/Pictures"]), &[], false).unwrap();
        assert_eq!(sel.folders[0].absolute, "/storage/emulated/0/Pictures");
    }

    #[test]
    fn custom_folders_are_marked_and_merged() {
        let sel = build(
            ROOT,
            entries(&["DCIM"]),
            &entries(&["Android/media/com.whatsapp"]),
            false,
        )
        .unwrap();
        assert_eq!(sel.folders.len(), 2);
        let custom = sel.folders.iter().find(|f| f.custom).unwrap();
        assert_eq!(custom.rel.as_str(), "Android/media/com.whatsapp");
    }

    #[test]
    fn paths_outside_the_storage_root_are_rejected() {
        let err = build(ROOT, entries(&["/data/local/tmp"]), &[], false).unwrap_err();
        match err {
            SelectionError::OutsideStorage { path, root } => {
                assert_eq!(path, "/data/local/tmp");
                assert_eq!(root, ROOT);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn traversal_is_rejected() {
        let err = build(ROOT, entries(&["../secret"]), &[], false).unwrap_err();
        assert!(matches!(
            err,
            SelectionError::OutsideStorage { .. } | SelectionError::Invalid { .. }
        ));
    }

    #[test]
    fn selecting_the_root_means_everything() {
        let sel = build(ROOT, entries(&[ROOT]), &[], false).unwrap();
        assert!(sel.whole_storage);
        assert_eq!(sel.enumerable_folders(), vec![ROOT.to_string()]);
    }

    #[test]
    fn no_folders_is_an_error_unless_whole_storage() {
        assert!(matches!(
            build(ROOT, entries(&[]), &[], false),
            Err(SelectionError::Empty)
        ));
        let sel = build(ROOT, entries(&[]), &[], true).unwrap();
        assert!(sel.whole_storage);
        assert!(!sel.is_empty());
        assert!(build(ROOT, entries(&[]), &[], false).is_err());
    }

    #[test]
    fn duplicates_are_collapsed() {
        let sel = build(
            ROOT,
            entries(&["DCIM", "DCIM/", "/storage/emulated/0/DCIM"]),
            &[],
            false,
        )
        .unwrap();
        assert_eq!(sel.folders.len(), 1);
    }

    #[test]
    fn duplicate_separators_and_backslashes_are_normalized() {
        assert_eq!(
            normalize_remote("/storage//emulated/0/"),
            "/storage/emulated/0"
        );
        assert_eq!(normalize_remote("/sdcard\\DCIM"), "/sdcard/DCIM");
        assert_eq!(normalize_remote("/"), "/");
        let sel = build(ROOT, entries(&["DCIM//Camera///"]), &[], false).unwrap();
        assert_eq!(sel.folders[0].rel.as_str(), "DCIM/Camera");
    }

    #[test]
    fn nested_selections_drop_the_redundant_child() {
        let sel = build(
            ROOT,
            entries(&["DCIM", "DCIM/Camera", "Download"]),
            &[],
            false,
        )
        .unwrap();
        let pruned = remove_redundant(sel.folders);
        let paths: Vec<&str> = pruned.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(paths, vec!["DCIM", "Download"]);
    }

    #[test]
    fn storage_root_detection_prefers_the_canonical_path() {
        assert_eq!(
            pick_storage_root(&entries(&["/sdcard", "/storage/emulated/0"])),
            Some("/storage/emulated/0".to_string())
        );
        assert_eq!(
            pick_storage_root(&entries(&["/sdcard"])),
            Some("/sdcard".to_string())
        );
        assert_eq!(pick_storage_root(&[]), None);
    }

    #[test]
    fn selection_is_empty_only_when_nothing_is_selected() {
        let sel = build(ROOT, entries(&["DCIM"]), &[], false).unwrap();
        assert!(!sel.is_empty());
    }
}
