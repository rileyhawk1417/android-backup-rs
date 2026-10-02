# Changelog

All notable changes to this project are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and the project uses
[semantic versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

Nothing yet. New work starts from the wave documents in `docs/plan/`, per the
"deferred" list in `docs/overview_plan.md` §13 (scheduling, notifications,
restore).

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
