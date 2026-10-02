# Testing

Two kinds of evidence live here:

1. **Automated** — run in CI on every commit (`cargo fmt --check`,
   `cargo clippy --workspace --all-targets`, `cargo test --workspace`).
2. **Manual runs** — anything that needs a window, a real phone, or another OS.
   These are recorded with the machine, the date, and what was actually seen. A
   row is never pre-filled with "pass".

Rule 3 of `docs/plan/index.md` applies: when hardware is unavailable, the
automated mock harness is used and the commit is tagged `hw-verify-needed`; the
manual rows below stay **PENDING** until a human with hardware fills them in.

---

## 1. Automated suites

| Suite | Command | What it proves |
|-------|---------|----------------|
| Core unit | `cargo test -p android-backup-core --lib` (259) | ADB runner/parser/locator, manifest repositories, path sanitization, watcher, gate, engine internals |
| Device fixtures | `cargo test -p android-backup-core --test device_listing_fixtures` (8) | `adb devices -l` parsing across CRLF, spacing, daemon chatter and 11 real-world fixtures |
| Engine E2E | `cargo test -p android-backup-core --test e2e_backup` (25) | The whole backup engine on a scripted phone (Wave 5 hard gate) |
| UI unit | `cargo test -p android-backup-desktop --lib` (40) | View model folding, gating, intents, startup planning, logging |
| UI headless | `cargo test -p android-backup-desktop --test ui_headless` (11) | Real `egui::Context::run` frames, asserting the text actually laid out |
| Architecture | `cargo test -p android-backup-desktop --test architecture` (4) | No backup logic in the UI, no `egui` in core, one `eframe::App`, UI folds core types |

Last full run: **347 tests, 0 failed** (Linux, rustc 1.97.0). Also verified:
`cargo build --release -p android-backup-desktop` succeeds, and the release
binary, started with no display available, exits 0 after logging
`could not open the window: … neither WAYLAND_DISPLAY nor WAYLAND_SOCKET nor DISPLAY is set`
and writing a complete log file — i.e. the no-display path is graceful, not a crash.

One flake was found while stress-testing this suite and fixed rather than
re-run: `adb::runner::tests::large_output_does_not_deadlock` failed roughly once in
five runs *under CPU load* with `ETXTBSY` ("Text file busy"). That is a real
condition, not a test bug — `exec` fails while any process still holds the file
open for writing, including a child of ours forked microseconds earlier, and the
managed adb installer (Wave 7) replaces the binary in place. The runner now
retries a busy spawn up to 20 times (100 ms total) and only for
`ErrorKind::ExecutableFileBusy`; every other spawn error still fails immediately.
Covered by three tests in `adb/runner.rs`. 10/10 full-suite runs pass with eight
CPU hogs spinning.

The log file for the no-display run, for reference:

```
INFO  android_backup_desktop::logging: logging started file=…/logs/android-backup.log
DEBUG android_backup_core::config: no configuration file yet, using defaults path=…/config.toml
INFO  android_backup: configuration loaded config_existed=false
INFO  android_backup_core::database::migrations: applying database migration version=1
ERROR android_backup: startup failed could not open the window: …
ERROR android_backup: reporting to the user title="Android Backup could not start" …
```

---

## 2. Manual run matrix (Wave 6 gate)

The Wave 6 gate asks for "mock device + (if available) real device". Nothing in
this environment has a display or a phone, so the mock-driven rows are covered by
`ui_headless.rs` and the rest are **PENDING**.

### 2.1 No display / no device (automated substitute)

| # | Check | Status | Evidence |
|---|-------|--------|----------|
| 1 | App starts, creates the manifest and a log, exits cleanly with no display | **PASS (Linux, headless)** | §1 last run |
| 2 | Device list, detail panel, progress, history and modals render | **PASS (headless `egui`)** | `ui_headless.rs` |
| 3 | Start button gating (unauthorized, no profile, running job, no controller) | **PASS** | `ui_headless.rs`, `view.rs` tests |

### 2.2 Linux + real phone

PENDING — needs the hardware. Steps:

1. `cargo run -p android-backup-desktop --release` (or the release artifact).
2. Plug in a phone with USB debugging; approve the prompt on the phone.
3. Record: does the device appear within one poll interval (default 5 s) with a
   real model name? Does "Unauthorized" show the prompt text before approval?
4. Unlock and approve; press **Back up now**. Record: does the progress panel show
   `files: done/total`, speed and ETA, and the current file name?
5. Press **Pause**, then **Resume**; then start again and press **Cancel**.
   Record whether the job stops and the summary strip matches what is on the phone.
6. Run a second backup with nothing changed: it must report everything unchanged
   and copy nothing.
7. Delete a file on the phone, run again: the PC copy must remain.
8. Change a file on the phone, run again: the old copy must appear under
   `<destination>/.backup/history/…`.
9. Close the window mid-copy. Re-open: the interrupted transfer must be cleaned up
   and the job must be reported as interrupted, not as completed.
10. While the app is open, run `adb devices` in a terminal: it must keep working
    (the app never kills the adb server).

### 2.3 Windows

PENDING — needs a Windows machine. Additionally record:

1. `%APPDATA%\android-backup` and `%LOCALAPPDATA%\android-backup` layout, and that
   `%USERPROFILE%\AndroidBackups` is the suggested destination.
2. A path containing a space and a non-ASCII character (e.g. `C:\Users\Ana García\…`).
3. A phone file whose name contains a character Windows forbids (`:`, `?`, `*`):
   it must be stored under a sanitized name and still verified afterwards.
4. Pulling the cable mid-backup: the job must pause with the USB-debugging help
   text, then resume when it is plugged back in.
5. Closing the window while a backup runs: `adb.exe` must still be running
   (`tasklist | findstr adb`).

### 2.4 Cross-checks that are not OS-specific

| # | Check | Status |
|---|-------|--------|
| 1 | No silent overwrite: a changed file is archived under `.backup/history/` | PASS (E2E) |
| 2 | No deletion: a file deleted on the phone stays on the PC | PASS (E2E) |
| 3 | Interrupted transfer leaves no truncated file at the destination | PASS (E2E) |
| 4 | `last_successful_backup` only advances on a verified copy | PASS (unit) |
| 5 | Modals re-appear next launch while unresolved | PASS (unit) |
| 6 | Closing the app does not kill the adb server | PASS (design: nothing in the codebase runs `adb kill-server`; verify manually in 2.2/2.3 step 10) |

---

## 3. Known gaps entering Wave 7

- **"Install automatically" is disabled** and says "coming in a later build". This
  is the planned graceful degradation (E06.6.1b) until Wave 7 lands.
- **Managed ADB is located but never installed** — `adb_mode = Managed` falls back
  to the same search as `System` until E07.1.
- **Unauthorized mid-backup**: the engine pauses and the UI explains, but the
  automatic re-arm once the phone is re-authorized is E07.2.
