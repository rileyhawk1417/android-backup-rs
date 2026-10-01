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

Early development. See [`docs/plan/index.md`](docs/plan/index.md) for the executable
build plan and [`docs/overview_plan.md`](docs/overview_plan.md) for the product plan.

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
cargo build --release -p android-backup-desktop
cargo test --workspace
```

## License

MIT OR Apache-2.0
