# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet. New work starts from the wave documents, per the "deferred" list in
`docs/overview_plan.md` §13 (restore, code signing, aarch64 and macOS builds).

## [0.2.0] - 2026-10-13

Wave 9: automation. The engine, the manifest and the UI promise exactly what they
promised in 0.1.0; this release adds a second way to run them — unattended — and it
does nothing until a device is opted in. Everything here was planned and tracked in
`docs/plan/wave-9-automation.md`, and the acceptance run is `docs/acceptance.md` §8.

### Changed

- **Closing the window hides it, when a tray icon is available.** On Windows and on
  Linux desktops with an AppIndicator library, the close button now hides the window
  and the program keeps running, so a schedule can fire with nobody watching. Use
  **Quit** in the tray menu or **Quit now** in **Background…** to stop for good;
  `background.close_to_tray = false` restores "close means quit". Where no tray icon
  can be created, close still means quit, and the app says so rather than pretending.
- **Quitting cancels live jobs cooperatively** instead of leaving them to be recovered
  on the next launch: the job and driver threads are joined, the manifest is left
  consistent, and an exit no longer resembles a crash.
- The tray icon is an optional runtime dependency on Linux
  (`libayatana-appindicator3`); it is loaded at runtime and its absence is reported,
  never fatal.

### Added

- **Per-device schedules.** Each profile can be set to back up when the phone is
  connected or every N hours, or left off; automation is opt-in per device. The
  schedule is stored with the profile (`devices.auto_schedule_kind` /
  `auto_schedule_hours`, manifest migration v2) and 0.1.0 manifests upgrade in
  place without losing a row.
- **Automatic backup on connection and on the clock.** A phone that is connected
  and due starts the same job the Start button starts — through the same gate,
  with the same destination and folder resolution — recorded as
  `trigger = auto`. Manual backups always win: `DeviceBusy` means automation
  stays quiet rather than queueing behind a person's job.
- **Automatic resume, scoped to automation's own work.** A job that automation
  started and that was interrupted (cable out, phone asleep, USB debugging
  revoked) resumes when the phone is ready again, continuing the *same* job from
  the manifest, so only the remainder is pulled. A job a person started keeps
  waiting for that person's Resume.
- **Background operation from the tray.** Closing the window hides it to a tray
  icon; the device watcher and the automation driver keep working while the
  window is hidden. The tray menu offers Open, Back up all connected now,
  Pause/Resume automation and Quit, and the icon shows idle / running /
  needs-attention. Where no tray icon can be created the app still runs with its
  window, so the tray is never load-bearing.
- **Run at login and single instance.** An optional login entry (Windows Run key,
  Linux `.desktop` file) brings the tray app up hidden; a second launch is
  refused through a lock file next to the manifest, so two copies can never write
  the same manifest.
- **Schedules and status in the app.** Each device's panel shows and edits its own
  schedule and says what automation is doing with it — "Next automatic backup in
  2 h 05 m", "starts when the phone is connected", "an automatic backup is due now" —
  and a **Background** dialog covers close-to-tray, starting at login and quitting for
  good, including what the desktop actually did about the login entry. The first-run
  wizard asks once whether new phones should back up on their own, and the answer
  applies to phones plugged in later rather than rewriting what was already chosen.
- **OS notifications.** Toasts announce an automatic backup starting, a job
  finishing, and anything that needs attention, with the same wording the panel
  uses. Repeat toasts are coalesced per device and kind, and they are paced so a
  flapping phone cannot produce a toast storm.

### Fixed

- **Free space is readable on Windows again — the volume holding an existing
  directory no longer answers "unknown".** On Windows, `canonicalize` returns a
  verbatim path (`\\?\C:\…`), which is a different prefix component from the
  `C:\` a mount point is written with, so the volume lookup that matches a path
  to its disk refused the very volume the path lived on: every existing
  directory read as `None`, and the before-backup space check silently never ran.
  Both ends of the comparison go through the same canonicalization now. Found by
  the Windows round of the CI suite, on a test that had quietly allowed `None`
  until now — it asserts an actual free-space answer instead.
- **A large-output fixture test is spelled for the shell that runs it.** The
  no-deadlock test wrote an sh `for`/`seq` loop verbatim into the Windows `.bat`
  fixture, where cmd wants `%%i` — "i was unexpected at this time", exit 255, and
  a test that never produced its output at all. It has a batch spelling now
  (`for /L %%i in (1,1,40000)`), same 2 MiB, same line count.
- **The Linux tray icon is created on a thread of its own.** An AppIndicator cannot
  exist before GTK is initialised, and on the window's thread GTK is not — that
  thread belongs to winit — so gtk-rs asserted and the first 0.2.0 build died at
  startup, behind a crash dialog. The icon, its menu and a `gtk::main()` loop now
  live on a thread that does nothing else, and the window talks to them by channel
  (state, pause, stop). Nothing GTK crosses the boundary, and a host that cannot
  give the thread an icon still gets a window. The crash also exposed how little
  proved it: the window smoke test recognised the crash dialog by its title and
  called it a pass. It now insists on a *visible* window belonging to the process
  under test, and `scripts/smoke-tray.sh` tests the tray itself on a private session
  bus — `--hidden`, the menu, clicks reaching the app, and a clean quit — with CI
  running it on every push (`docs/testing.md`, `docs/release.md`).
- **A test double no longer calls an atomic method that rustc 1.99 deprecated.** The
  fake "text file busy" spawner in `crates/core/src/adb/runner.rs` used
  `AtomicU32::fetch_update`, which 1.99 deprecates in favour of `try_update` — and
  `try_update` does not exist in 1.89, the version this workspace declares as its
  minimum, so neither spelling builds everywhere. It is a compare-and-swap loop now.
  Test-only code: nothing in the shipped binary had that call, and no behaviour
  changed — CI was red because `clippy -D warnings` compiles the tests too. The
  release checklist gained the `rustup update stable` that would have caught it
  locally (`docs/release.md`, `docs/testing.md`).

### Known limitations

- **No restore.** Unchanged from 0.1.0: files go phone → computer, and putting them
  back is a manual `adb push`. Restore is still on the deferred list.
- **The tray icon and the toasts are best-effort on Linux.** The AppIndicator
  library is neither bundled nor required; without it the program runs windowed,
  close means quit, and the error text says which of the two reasons applies
  (`docs/setup.md` §3, `docs/troubleshooting.md` §18). GNOME additionally needs an
  extension before it will show a tray icon at all.
- **Automation runs while the program runs.** There is no service and no second
  process: closing to the tray and the login entry are what keep it up. A machine
  that is asleep at the scheduled time backs up when it wakes.
- **Schedules are intervals, not calendars.** "Every day at 03:00" is deliberately
  not in the model (timezone and missed-run semantics a phone backup does not need);
  "every 6 hours" counts from the last attempt.

## [0.1.0]

First release. Windows 10/11 x64 and Linux x86_64 desktops; phones running
Android 6 or newer, over ADB, with no SDK or Android Studio installed.

### Added

- **Device discovery.** Phone list with model, Android version, and live
  authorization state (`ready`, `unauthorized`, `offline`, …), refreshed on a
  poll while the app runs. Two phones with the same name are kept apart by a
  serial suffix, so their backups never share a folder.
- **Per-device profiles.** Display name, destination folder, and the set of phone
  folders to back up, all editable in the app and stored in the manifest.
- **Incremental backup engine.** Files are compared by size and modification time
  first; unchanged files are skipped. A local copy that has gone missing or is
  truncated is detected and re-copied, so "skipped" never means "silently wrong".
- **Verified, recoverable transfers.** Each file is pulled into
  `.incomplete/<path>.part` and only moved into place — and recorded in the
  manifest — once it is complete and its SHA-256 matches the phone's (via
  `adb shell sha256sum`; if the phone has no `sha256sum`, the local digest is
  recorded instead and the transfer still succeeds).
- **History instead of overwriting.** A file that changed on the phone has its
  previous copy moved into `.backup/history/<path>/<job time>-<hash>/` before the
  new one lands, so both versions exist and nothing is lost to a silent
  overwrite.
- **Nothing is ever deleted.** Deleting a file on the phone, or removing it from
  the folder selection, never deletes the copy on the computer. The manifest is
  append-only: history rows are never rewritten.
- **Pause, resume, cancel.** Pause and resume at any time from the UI, including
  automatically when the phone is unplugged or stops answering part-way through
  a file; the job names the file it was on. Interrupted jobs leave no manifest
  rows and their partial files are swept on the next run.
- **Crash recovery.** A job left open by an abrupt exit is closed on the next
  start, and stale staging partials are cleaned up, without touching verified
  files.
- **Managed ADB install.** On first launch the app offers to download Google's
  official `platform-tools`, checks it against the SHA-1/SHA-256 published in
  Google's own repository manifest, verifies the extracted `adb` reports a
  version, and only then swaps it into place. Existing installs are untouched;
  system and manually chosen `adb` binaries are equally supported, and a version
  mismatch with an already-running adb server is reported with the two fixes.
- **Corrupt-manifest quarantine.** A manifest that SQLite cannot read is kept
  as `manifest.db.corrupt-<timestamp>` (with its WAL/SHM sidecars) and the app
  starts fresh, telling the user that their backed-up files are unaffected.
- **Artifacts.** Portable tarball (recommended on Linux) and an AppImage
  wrapper for the same binary, a portable ZIP for Windows, each with SHA-256
  checksums, setup and troubleshooting documentation, and the licence texts. The
  AppImage is best-effort: how it starts depends on FUSE and on AppImageLauncher
  not being in the way.
- **CLI surface.** `--version` and `--help` report without starting a window,
  for scripts and for checking what a downloaded artifact actually is.

### Security

- Downloaded platform-tools are verified against Google's published digests
  before anything is executed; an `adb` that fails its version probe is never
  swapped in. Extraction is restricted to an allow-list of binaries (zip-slip
  guards included), so a tampered archive cannot write outside the install
  directory.

### Known limitations

- **No restore.** Files are copied from the phone to the computer; putting them
  back is a manual `adb push`. Restore is on the deferred list.
- **Unsigned Windows builds.** SmartScreen warns once on first launch; see
  `docs/release.md` for the reasoning and the published checksums.
- **Android-protected folders.** `Android/data`, `Android/obb` and some OEM
  camera folders cannot be read over ADB without root. Failures are reported per
  file rather than silently skipped.
- **One job per device at a time**, and no scheduling or background operation:
  a backup runs while the app is open.
- **x86_64 desktops only** for now (Windows x64, Linux x86_64).
