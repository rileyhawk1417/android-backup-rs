# Android Backup Utility — Features → Actionable Steps

Source: `docs/overview_plan.md` §12 (MVP scope). Organized by feature; each feature
lists the concrete steps to deliver it. **F-numbers are the stable reference IDs** —
steps inside a feature are ordered in build sequence.

Legend: 🔧 core logic · 🖥 UI · 💾 data · ⚙️ infra

---

## F1 — Windows & Linux builds

Goal: the app compiles and runs on both target platforms from day one.

1. 🔧 Create Rust workspace with `core`, `platform`, `desktop` crates; ensure `cargo build` passes (S)
2. 🔧 Guard all platform-specific code behind `#[cfg(windows)]` / `#[cfg(unix)]` in `crates/platform` (S)
3. ⚙️ CI job matrix (ubuntu + windows): fmt, clippy, test, build on every push (S)
4. 🔧 Abstract filesystem/paths differences behind a `Platform` trait (e.g. default backup dir: `%USERPROFILE%/AndroidBackups` vs `~/AndroidBackups`) (S)
5. 🔧 Manual smoke run on Windows 10/11 and Linux (incl. one Arch distro); record environment quirks in `docs/testing.md` (S)

**Definition of done:** both binaries build in CI and launch on real machines of each OS.

---

## F2 — ADB detection & managed installation

Goal: app finds adb, or installs official Platform Tools itself, without Android Studio. Plan §4.

1. 🔧 Implement ADB locator: configured path → app-managed dir → system PATH; return typed result (`Found`, `NotFound`, `NotExecutable`, `ParseFailed`) (M)
2. 🔧 Validate adb by running `adb version`; parse version string (S)
3. 🔧 Persist ADB preference in config: `managed` (default) | `system` | `manual: <path>` (S)
4. 🖥 First-launch prompt when adb missing, with three options: auto-install / use existing / browse for binary (M)
5. 🔧 Downloader (reqwest, streamed) for official Platform Tools zip per platform; progress callback; retry on interruption (M)
6. 🔧 Checksum verification against Google-published hash **before** extraction — refuse to execute unverified binaries (S)
7. 🔧 Safe extraction: allow-list of expected entries, zip-slip protection, extract into app-managed dir (M)
8. 🔧 Re-verify installed adb runs; save path in config; never touch a user's existing adb installation (S)
9. 🔧 ADB server lifecycle: reuse running server (`start-server` idempotent), never kill on app exit, meaningful error on version/server conflicts (M)

**Definition of done:** clean machine with no adb → app guides install → adb usable; corrupt download rejected; existing user adb never replaced.

---

## F3 — Automatic device discovery

Goal: app shows connected phones live, no manual refresh. Plan §5.

1. 🔧 Parse `adb devices -l` into device structs (serial, state, model, transport) with unit tests on real output samples (M)
2. 🔧 Map raw ADB states to app states: Connected / Unauthorized / Offline (S)
3. 🔧 Derive Disconnected (known serial gone) and Error (query failed) states (S)
4. 🔧 Polling watcher task (default 3–5s, configurable) emitting `DevicesChanged` events (M)
5. 🔧 Per-device property queries: manufacturer, model, Android version (`shell getprop`), first-seen/last-seen timestamps (S)
6. 🖥 Device list panel with live state badges and empty state (M)

**Definition of done:** plug/unplug phone → list updates within one poll interval on both OSes.

---

## F4 — Device authorization & connection states

Goal: authorization problems are visible and actionable, not mysterious failures.

1. 🔧 Explicit `Unauthorized` state surfaced as its own UI state, not a generic error (S)
2. 🖥 Help hint on unauthorized devices ("check the phone screen and approve the prompt") (S)
3. 🔧 Block backup start while device is not Authorized (S)
4. 🔧 Unauthorized-during-backup → pause safely with clear report (part of job state machine, F8 step 6) (M)
5. 🔧 State-change events propagate to UI and running jobs (S)

**Definition of done:** unauthorized device shows guidance; backup cannot start or silently fail on it.

---

## F5 — Multiple device profiles & user-defined names

Goal: every device has independent identity, name, settings, and history. Plan §5.2–5.3.

1. 💾 SQLite schema + migration runner for `devices`, `device_folders`, `backup_jobs`, `backup_files` (plan §9), with indexes (M)
2. 💾 Device repository: upsert by serial, timestamps, identity fields; in-memory DB tests (M)
3. 💾 Display name: default = model or "Device {serial}"; rename changes the name only, never identity/history linkage (S)
4. 💾 New-serial policy: never auto-merge; explicit "same device" association action for the user (M)
5. 🖥 Rename UI in device panel (S)
6. 🖥 Per-device detail view; independent job slots (parallel backups allowed across devices, one at a time per device) (M)

**Definition of done:** two phones connected → independent names/destinations/histories; rename doesn't orphan history; serial changes require explicit association.

---

## F6 — Global & per-device backup locations

Goal: user controls where backups go; locations are validated. Plan §6.

1. 💾 Global default location in config with platform defaults (`%USERPROFILE%/AndroidBackups` / `~/AndroidBackups`) (S)
2. 🖥 First-launch prompt for backup location (S)
3. 🖥 Per-device "Change Location" via `rfd` native dialog; stored in `devices.destination_path` (S)
4. 🔧 Destination subfolder derived from device display name; created on demand (S)
5. 🔧 Pre-backup validation: exists/creatable, writable probe, free-space check, missing-external-drive detection → typed errors (M)
6. 🖥 Validation messages surfaced before Start Backup; warn when destination unavailable (S)
7. 🔧 Changing a destination affects future backups only; never delete/relocate prior backups (S)

**Definition of done:** user sets global + per-device locations; invalid/missing destination blocks start with an actionable message.

---

## F7 — Folder selection

Goal: user chooses which phone directories to back up. Plan §7.

1. 💾 `device_folders` rows seeded with default set (DCIM, Pictures, Movies, Music, Documents, Download, Android/media, Recordings, Screenshots) (S)
2. 🖥 Folder checkboxes per device; enabled flags persist (S)
3. 🔧 Custom directory input with a dry-run `adb shell ls` validation before saving (M)
4. 🔧 Remote enumeration: safe `adb shell` walker/parser (spaces, quotes, unicode) recording relative path, size, mtime (M)
5. 🔧 Inaccessible/permission-denied directories reported as errors — never silently "empty" (S)
6. 🔧 De-duplication for overlapping selections (Screenshots ⊂ Pictures): each source file enumerated once (S)
7. 💾 Path sanitizer: reject `..`, absolute paths, control chars; deterministic phone→destination path mapping (M)

**Definition of done:** per-device folder set persists; overlaps never copy a file twice; inaccessible folder surfaces an error.

---

## F8 — Incremental backup engine

Goal: copy new/changed files only; never delete; never record failure as success. Plan §8. This is the largest feature — steps build the job pipeline.

1. 🔧 Comparison engine (pure function): manifest vs scan → classify new / changed / unchanged / missing / failed — size + mtime + last backup status, no hashing on scan (M)
2. 🔧 "Missing on phone never deletes from PC" enforced in comparison output (covered by table-driven tests) (S)
3. 🔧 Temporary transfer: pull into `<name>.partial`; rename to final only after verify (M)
4. 🔧 Post-transfer verification: dest exists, size matches, local SHA-256 computed and stored; size-only never claims full integrity (S–M)
5. 🔧 Collision strategy: changed source → previous copy preserved (e.g. `Backups/.backup/history/<relpath>/<timestamp>-<hash8>`); silent overwrite impossible (M)
6. 🔧 Job state machine: Plan → Scanning → Copying → Verifying → Finalizing → Completed/Failed/Cancelled (+ Paused); per-file transactional manifest updates; live progress events (files/bytes/current file/ETA) (L)
7. 🔧 Interrupted-transfer recovery: find stale `.partial` files on scan, clean them, mark rows failed so they re-copy; source re-availability check — file-level restart only, no byte-offset resume in MVP (M)
8. 🔧 Job control API: `start/pause/resume/cancel(device)`; cancel kills current pull and removes partial (M)
9. 💾 Job summary row in `backup_jobs` (started/completed, status, counts, bytes, error summary); failed job never recorded as completed (S)

**Definition of done:** end-to-end mock run: new/changed files copied & verified & recorded; unchanged files untouched; killed run recovers cleanly next time.

---

## F9 — SQLite manifest (backup history)

Goal: reliable, recoverable record of every backup. Plan §9. (Overlaps F5/F8 steps — this feature tracks the DB-specific quality bar.)

1. 💾 WAL mode + transactional writes for transfers (kill-safety) (S)
2. 💾 Embedded, versioned migrations testable from an empty and from an old DB (S)
3. 💾 Stable IDs everywhere; phone filenames never used directly as filesystem paths without sanitization (S)
4. 💾 Prevent duplicate backup records (unique constraint device+source path per job) (S)
5. 💾 Manifest treated as an index, not the only copy — document + never delete rows for files missing from phone (S)
6. 💾 Recovery behavior under app-kill during write verified by test; guidance in troubleshooting docs (M)

**Definition of done:** kill-during-write leaves a consistent DB; migrations apply forwards; no duplicate/dangling records possible.

---

## F10 — Progress & error reporting UI

Goal: the main screen from plan §10, driven by real engine events.

1. 🖥 Progress panel: files done/total, GB copied, current file, speed + ETA (M)
2. 🖥 Start/Pause/Resume/Cancel buttons wired to job API; states disable appropriately (S–M)
3. 🖥 Error & completion summaries per job (copied/skipped/failed counts, error list) (S–M)
4. 🖥 Last-backup line ("Completed — 430 copied, 20 unchanged") (S)
5. 🖥 Backup history list per device from `backup_jobs` with expandable detail (S–M)

**Definition of done:** full §10 mock replicated with live data on a real backup run.

---

## F11 — Release binaries via GitHub Actions

Goal: users get ready-to-run artifacts, no toolchain needed. Plan §15.

1. ⚙️ Windows job: portable .exe artifact on tag; decide/document code-signing stance; no redistributable surprise (M)
2. ⚙️ Linux job: AppImage primary (minimize egui backend system deps); `.deb` optional; pinned build image (M)
3. ⚙️ Tag-triggered publish attaching artifacts to versioned GitHub Release with notes (S)
4. ⚙️ Clean-VM verification: artifacts launch on Windows 10/11, Ubuntu, Arch with no dev tools installed (S)
5. 📓 Setup + troubleshooting docs (install, USB debugging, authorization, Linux udev rules, first backup, interrupted-backup recovery) (M)
6. ⚙️ MVP acceptance run on both platforms: connect → select → back up → reconnect → re-run → nothing re-copied or lost (M)

**Definition of done:** the defining MVP behavior (§12/§16) passes on both OSes from published artifacts.

---

## Execution order & parallelization

Dependencies between features:

```
F1 (workspace/CI)          → everything
F2 (adb)  ──┐
F3 (discovery) needs F2
F5 (profiles) needs F3 + F9-schema
F6 (locations) needs F5
F7 (folders) needs F3
F8 (engine) needs F7 + F9
F10 (UI) needs F8 API + F5/F6/F7 UI models
F11 (release) last, docs alongside
```

Three tracks that can proceed in parallel (single dev: interleave):

- **Track A (engine):** F2.1–2.3 → F3.1–3.5 → F7.4–7.7 → F8.1–8.8
- **Track B (data):** F9.1–9.4 → F5.1–5.4 → F6.1–6.5, 6.7
- **Track C (UI/release):** F10.1–10.5 → F11.1–11.4 (UI shells early with mock data)

## Deferred (not in this breakdown — plan §13)
Scheduled/auto backups, background daemon & notifications, encryption, restore/browse, file filters, cross-path duplicate detection, retention policies, profiles per device, network ADB, config export, DB repair tools, CLI, byte-offset resume.