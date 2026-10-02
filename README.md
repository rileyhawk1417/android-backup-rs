Android Backup Utility
======================

A lightweight, cross-platform (Windows / Linux) desktop application that backs up
files from Android devices over ADB — no Android Studio, no cloud account.

* Reliable: incremental backups, never deletes previously saved copies, never
  records a failed transfer as a success.
* Understandable: native desktop UI, device list with live authorization state,
  per-device destinations and folder selection.
* Local: SQLite manifest lives with your backups; nothing leaves your machine.

## Status

Wave 6 of 9: the desktop UI is in place and the backup engine passes its mock
end-to-end gate. The app can be run today; what is still missing is the managed ADB
installer (Wave 7) and the release artifacts (Wave 8).

* [`docs/plan/index.md`](docs/plan/index.md) — the executable build plan (progress)
* [`docs/overview_plan.md`](docs/overview_plan.md) — the product plan
* [`docs/testing.md`](docs/testing.md) — what is tested automatically, and the
  manual run matrix

## Architecture

```
crates/
├── core/       backup engine, ADB integration, SQLite manifest  (no UI deps)
├── platform/   OS-specific: app dirs, disk space, process control
├── desktop/    egui/eframe UI — subscribes to core events, holds no backup logic
└── cli/        placeholder (deferred, see plan §13)
```

`core` never depends on a UI crate, so a CLI or background service can be added later
without touching the engine.

## Building

Requires a stable Rust toolchain.

```bash
cargo run --release -p android-backup-desktop   # open the app
cargo test --workspace                          # 410 tests
cargo fmt --check && cargo clippy --workspace --all-targets
```

The app keeps its manifest and its log under the platform data directory
(`~/.local/share/android-backup/` on Linux, `%APPDATA%\android-backup\` on
Windows). `ANDROID_BACKUP_LOG=debug` raises the log level; the log file is
`logs/android-backup.log`.

On first launch, pick a backup folder and point the app at `adb` if it is not on
`PATH` — or press **Install automatically**, which downloads Google's
`platform-tools`, verifies it against the published checksum and only then swaps
it in (nothing is replaced until the new `adb` runs). Then plug in a phone with
USB debugging on, approve the prompt on the phone, choose the folders to back up,
and press **Back up now**.

## License

MIT OR Apache-2.0
