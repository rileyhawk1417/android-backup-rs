//! Architecture gates, enforced instead of documented.
//!
//! The W0 invariant is that `crates/core` has no UI dependency, and the W6 gate
//! adds the reverse: the UI is a *subscriber* — it may call into `core`, but it
//! must not contain backup logic of its own. Both are checked here, so a later
//! wave cannot quietly move a hash or a `rename` into the UI layer.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    // crates/desktop/tests/architecture.rs → the workspace root.
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("workspace root")
}

fn source_files(crate_dir: &Path) -> Vec<(PathBuf, String)> {
    let mut files = Vec::new();
    let mut stack = vec![crate_dir.join("src")];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("readable directory") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let text = std::fs::read_to_string(&path).expect("readable source");
                files.push((path, text));
            }
        }
    }
    assert!(
        !files.is_empty(),
        "no sources found under {}",
        crate_dir.display()
    );
    files
}

/// Strip comments, so a doc-comment that *mentions* `sha2` does not fail the gate.
fn code_only(text: &str) -> String {
    text.lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn crate_manifest(crate_dir: &Path) -> String {
    std::fs::read_to_string(crate_dir.join("Cargo.toml")).expect("manifest")
}

#[test]
fn the_ui_contains_no_backup_logic() {
    let root = repo_root();
    let desktop = root.join("crates/desktop");
    let manifest = crate_manifest(&desktop);
    for forbidden in ["sha2", "rusqlite", "hex ", "walkdir"] {
        assert!(
            !manifest.contains(forbidden),
            "the UI must not depend on `{forbidden}`: that work belongs in core"
        );
    }

    // These are the calls that *are* backup logic. Each is allowed in core only.
    let forbidden = [
        "sha2::",
        "Sha256::",
        "rusqlite::",
        "std::fs::rename",
        "fs::rename",
        "hard_link",
        "File::create(",
    ];
    for (path, text) in source_files(&desktop) {
        let code = code_only(&text);
        for needle in forbidden {
            assert!(
                !code.contains(needle),
                "{} contains `{needle}` — the UI may only call into android-backup-core",
                path.display()
            );
        }
    }
}

#[test]
fn core_has_no_ui_dependency() {
    let root = repo_root();
    let core = root.join("crates/core");
    let manifest = crate_manifest(&core);
    for forbidden in ["egui", "eframe", "rfd"] {
        assert!(
            !manifest.contains(forbidden),
            "core must not depend on `{forbidden}`"
        );
    }
    for (path, text) in source_files(&core) {
        let code = code_only(&text);
        for needle in ["egui::", "eframe::", "rfd::"] {
            assert!(
                !code.contains(needle),
                "{} refers to `{needle}` — core must stay UI-free",
                path.display()
            );
        }
    }
}

#[test]
fn there_is_exactly_one_eframe_entry_point() {
    let root = repo_root();
    let mut found = Vec::new();
    for (path, text) in source_files(&root.join("crates/desktop")) {
        if code_only(&text).contains("impl eframe::App for") {
            found.push(path.display().to_string());
        }
    }
    assert_eq!(
        found.len(),
        1,
        "want exactly one eframe::App implementation (window.rs), found {found:?}"
    );
    assert!(found[0].ends_with("window.rs"), "{found:?}");
}

/// The UI's job is to render what `core` reports, so the engine types it names
/// must be the ones core exports — no local `JobStatus`-like duplicates.
#[test]
fn the_ui_uses_core_types_rather_than_its_own() {
    let root = repo_root();
    let mut view = String::new();
    for (path, text) in source_files(&root.join("crates/desktop")) {
        if path.file_name().is_some_and(|name| name == "view.rs") {
            view.push_str(&text);
        }
    }
    for needle in [
        "android_backup_core::database::jobs::",
        "android_backup_core::backup::controller::",
        "android_backup_core::backup::",
    ] {
        assert!(
            view.contains(needle),
            "view.rs must fold the real event/status types (`{needle}` missing)"
        );
    }
    assert!(
        !view.contains("enum JobStatus"),
        "the view must not define its own job status"
    );
}
