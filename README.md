Android Backup Utility
======================

A lightweight, cross-platform (Windows / Linux) desktop application that backs up
files from Android devices over ADB — no Android Studio, no cloud account.

* Reliable: incremental backups, never deletes previously saved copies, never
  records a failed transfer as a success.
* Understandable: native desktop UI, device list with live authorization state,
  per-device destinations and folder selection.
* Local: SQLite manifest lives with your backups; nothing leaves your machine.
* Unattended, if you want it: per-device schedules (when the phone is plugged in,
  or every *n* hours), automatic resume of interrupted jobs automation started, a
  tray icon that keeps it running with the window closed, and system notifications
  when something happens while nobody is looking. Off by default.
* One-way, by design: copies from the phone to the computer and never back, and
  never writes anything to the phone.

## Status

Wave 9 of 9: the plan is complete. The backup engine, the SQLite manifest, the
desktop UI and the automation layer (scheduling, background operation, tray icon,
notifications) are implemented and green (574 automated checks), and the release
pipeline builds a portable Windows ZIP and a Linux tarball — plus a best-effort
AppImage — from a version tag.

The engine and the UI were the MVP; automation is the layer that lets the program
run without being watched. It is opt-in per device, and a phone whose schedule is
**Off** behaves exactly as it did in 0.1.0.

[`docs/acceptance.md`](docs/acceptance.md) is the item-by-item record of the MVP
definition of done, with the evidence for each row. What is left is *observation*
rather than code — a real phone, a real Windows desktop, and one person who is not
the author following the docs — and each of those has a short procedure in that
document.

* [`docs/setup.md`](docs/setup.md) — install, first launch, USB debugging, udev rules
* [`docs/troubleshooting.md`](docs/troubleshooting.md) — when the device is not there
* [`docs/release.md`](docs/release.md) — how a release is cut, and why the builds are unsigned
* [`docs/plan/index.md`](docs/plan/index.md) — the executable build plan (progress)
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
cargo test --workspace                          # 574 tests (1 ignored, live-network)
cargo fmt --check && cargo clippy --workspace --all-targets
```

CI (`.github/workflows/ci.yml`) runs exactly that on `ubuntu-latest` and
`windows-latest`. `.github/workflows/nightly.yml` additionally runs the one test
that touches the real world — installing `platform-tools` from Google — as a
non-blocking job.

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

To have it back up on its own, tick the welcome panel's *back up a phone when I
plug it in* box, or set a schedule per device later; closing the window then hides
it to the tray rather than quitting. [setup.md §10](docs/setup.md#10-let-it-back-up-on-its-own)
is the whole story, including what the tray menu does and how to stop it.

## License

MIT OR Apache-2.0
