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
| Core unit | `cargo test -p android-backup-core --lib` (289) | ADB runner/parser/locator, manifest repositories, path sanitization, watcher, gate, engine internals, managed-installer manifest/zip/swap |
| Device fixtures | `cargo test -p android-backup-core --test device_listing_fixtures` (8) | `adb devices -l` parsing across CRLF, spacing, daemon chatter and 11 real-world fixtures |
| Engine E2E | `cargo test -p android-backup-core --test e2e_backup` (25) | The whole backup engine on a scripted phone (Wave 5 hard gate) |
| Reliability scenarios | `cargo test -p android-backup-core --test scenario_*` (23) | Disconnect, unauthorized, crash mid-write, duplicate names, corrupt manifest, 50k files, history/versioning |
| UI unit | `cargo test -p android-backup-desktop --lib` (50) | View model folding, gating, intents, startup planning, logging, managed-install flow |
| UI headless | `cargo test -p android-backup-desktop --test ui_headless` (13) | Real `egui::Context::run` frames, asserting the text actually laid out (incl. install progress + cancel) |
| Architecture | `cargo test -p android-backup-desktop --test architecture` (4) | No backup logic in the UI, no `egui` in core, one `eframe::App`, UI folds core types |
| Live installer (network) | `cargo test -p android-backup-core --test installer_network` (2 + 1 ignored) | The managed install against Google itself: live manifest, live digest, extracted adb really runs. See §3 |

CI (`.github/workflows/ci.yml`) runs fmt, clippy (`-D warnings`) and the whole
suite on `ubuntu-latest` **and** `windows-latest`, so the Windows-only paths — the
`adb.exe`/`AdbWin*.dll` allow-list, drive-letter sanitization, path separators —
are compiled and tested from the first commit rather than discovered at release
time. `.github/workflows/nightly.yml` runs the live installer test with
`continue-on-error`, so a Google outage is a red nightly run and never a blocked
pull request.

**PENDING: the first green CI run on GitHub.** The workflows are in the tree
(added after the Wave 7 audit caught that E00.3 had been marked done without the
file ever existing), but this checkout has no remote, so they have never executed.
The `apt-get` list for the Linux GUI build is the one line most likely to need an
edit on that first run.

Last full run: **414 tests, 0 failed, 1 ignored** (Linux, rustc 1.97.1), also green under 8-way CPU load. Also verified:
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

### 2.2 Window under a virtual display (attempted, inconclusive)

A stronger check than the headless pass was attempted on Linux: run the release
binary under `Xvfb :99 -screen 0 1400x900x24` with `LIBGL_ALWAYS_SOFTWARE=1`.

Recorded facts (2026-10-02, Linux):

- The process stayed alive in its event loop (`/proc/<pid>/wchan`:
  `do_epoll_wait`, 38 threads) and logged **no** error — so `eframe` created its
  window and the GL context, and `on_exit`/`stop_background_work` never had to run.
- `xdotool search --onlyvisible --name ".*"` found the mapped window.
- Capturing it came back **blank**: `import -window <id>` produced a uniform
  two-colour image, and `ffmpeg -f x11grab` produced a black frame
  (mean 5.5e-5, std 1.4). Xvfb has no compositor, so the GL surface is not
  readable from the X framebuffer — this is a capture limitation, not evidence
  that the UI is blank.

So: **the window opens and the loop runs (verified); the UI being visible on
screen is still PENDING**, and is covered for now only by `ui_headless.rs`,
which asserts the text `egui` really laid out. A human on a real desktop should
confirm the pixels.

### 2.3 Linux + real phone

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

### 2.4 Windows

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

### 2.5 Cross-checks that are not OS-specific

| # | Check | Status |
|---|-------|--------|
| 1 | No silent overwrite: a changed file is archived under `.backup/history/` | PASS (E2E) |
| 2 | No deletion: a file deleted on the phone stays on the PC | PASS (E2E) |
| 3 | Interrupted transfer leaves no truncated file at the destination | PASS (E2E) |
| 4 | `last_successful_backup` only advances on a verified copy | PASS (unit) |
| 5 | Modals re-appear next launch while unresolved | PASS (unit) |
| 6 | Closing the app does not kill the adb server | PASS (design: nothing in the codebase runs `adb kill-server`; verify manually in 2.3/2.4 step 10) |

---

## 3. Managed ADB install (Wave 7, E07.1)

The installer downloads platform-tools from Google's pinned manifest
(`repository2-3.xml`, `dl.google.com` only), verifies the archive against the
published digest *before* extracting, extracts only the files adb needs, and
swaps the result into `<data>/platform-tools` only after the new binary answers
`adb version`. Interruption safety is structural: everything happens in a staging
directory beside the target, and the previous installation is untouched until the
rename.

What is proven automatically:

| Scenario | Evidence |
|---|---|
| Manifest parse: latest revision per OS, sizes, digests | `adb::installer` unit tests (fixture manifest) |
| Archive installs; only the allow-listed files land | `adb::installer` unit tests (`platform_tools_zip` fixture) |
| Checksum mismatch → nothing installed, previous binary intact | `adb::installer` unit tests |
| Truncated download → size check + retry | `adb::installer` unit tests |
| Zip-slip / absolute entry → rejected before any write | `adb::installer` unit tests |
| No free space → typed error, no partial install | `adb::installer` unit tests (`MockFsProbe`) |
| UI: progress modal, byte counts, Cancel, failure keeps the routes out open | `ui_headless.rs` (2 tests) + `app.rs` (5 tests) |
| A cancel changes nothing; a failed install writes no config | `app.rs::cancelling_an_install_says_nothing_was_changed` |
| One click = one download | `app.rs::a_second_install_request_while_one_is_running_is_ignored` |

**PASS (Linux, 2026-10-02, `Linux 7.2.6-1-cachyos`, rustc 1.97.1 — real network, real download).**
`cargo test -p android-backup-core --test installer_network -- --ignored --nocapture`
ran the whole production path (no fixtures, no `MockFetcher`) against
`dl.google.com`:

```
installed adb 1.0.41 (platform-tools 37.0.1) from
  https://dl.google.com/android/repository/platform-tools_r37.0.1-linux.zip
  — 9054187 bytes, 3 files kept                       (3.84 s)
```

That single run covers the four things fixtures cannot: the live manifest still
parses, the published digest still matches the served archive, the allow-list
keeps the extraction to `adb` + `NOTICE` + `source.properties`, and the extracted
binary **executes on this machine** (`adb version` exits 0 and reports the same
version the installer's own probe saw). The test is `#[ignore]`d and runs nightly
via `continue-on-error`, so a Google outage can never block a PR.

Still PENDING: the same run on **Windows** (where `AdbWinApi.dll`/`AdbWinUsbApi.dll`
must survive the allow-list), the proxy/offline case (what the modal says when the
manifest host is unreachable), and a network drop mid-download. Tagged
`hw-verify-needed` for E08.4.

---

## 4. Reliability scenarios (Wave 7, E07.3)

| Scenario | What it proves |
|---|---|
| `scenario_disconnect.rs` | A cable pull pauses the job, names the interrupted file, and the next job copies only the remainder (no re-copy of verified files); an unauthorized phone pauses with USB-debugging guidance and records nothing as failed |
| `scenario_kill_db_write.rs` | A kill between `rename` and the manifest write is recovered by `recovery::recover_device`: unfinished jobs are closed, partials swept, and a committed file without its row is versioned into history rather than overwritten |
| `scenario_duplicate_names.rs` | Same display name → disambiguated by serial tail; same file name in different folders → distinct destinations; overlapping selections → copied once |
| `scenario_corrupt_db.rs` | A truncated manifest is quarantined and replaced, and the next backup versions existing files into history instead of overwriting |
| `scenario_large.rs` | 50,000 files: scan+compare ≈ 0.7 s (budget 60 s) with bounded allocator growth (~30 MB) |
| `scenario_overlap_and_history.rs` | 10 versions of one file, a rename, and sanitized-name collisions all keep the history tree correct |

---

## 5. Known gaps entering Wave 8

- **A real phone has never been used.** Everything is mock-driven; the Wave 6/8
  human matrix in §2 is still PENDING.
- **Managed ADB is installed but never exercised end-to-end** (see §3).
- **Windows is unverified**: nothing has been built or run there yet (E08.1).
- **Resume after re-authorization is manual** by design: the engine pauses and the
  UI explains, the user presses Resume (automatic re-arm is out of scope, plan §13).
