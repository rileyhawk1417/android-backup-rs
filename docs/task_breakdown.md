# Android Backup Utility — Actionable Task Breakdown

Derived from `docs/overview_plan.md`. Tasks are grouped by phase (matching §16 of the plan).
Each task has an ID, dependencies (`deps`), and acceptance criteria (`done when`).
Estimates are rough ideal-time for one developer: S = hours, M = 1–2 days, L = 3–5 days.

---

## Phase 0 — Project Bootstrap

- [ ] **0.1 Create Rust workspace skeleton** — S
  Create root `Cargo.toml` with workspace members: `crates/core`, `crates/platform`, `crates/cli` (stub), `crates/desktop` (stub). Add `.gitignore`, `rust-toolchain.toml`, repo README stub, MIT/Apache license decision.
  *done when:* `cargo build` succeeds on the empty workspace.

- [ ] **0.2 Configure CI smoke check** — S
  GitHub Actions workflow running `cargo fmt --check`, `cargo clippy`, `cargo test` on ubuntu-latest and windows-latest for every push/PR.
  *done when:* CI is green on an empty workspace on both OSes.
  *deps:* 0.1

- [ ] **0.3 Set up error handling & logging conventions** — S
  Add `thiserror` (library crates) and `anyhow` (binaries) patterns; wire `tracing` + `tracing-subscriber` (file + stderr) in the desktop binary; add a shared `core::Error` type.
  *done when:* a demo function logs through `tracing` and errors propagate without `unwrap()`.
  *deps:* 0.1

---

## Phase 1 — Core Foundation (ADB + devices visible in UI)

### ADB module (`crates/core/src/adb/`)

- [ ] **1.1 ADB executable discovery** — M
  Locate adb: (a) user-configured path from config, (b) app-managed directory, (c) system `PATH`. Record version via `adb version`. Distinguish "not found" / "found but not executable" / "version parse failed".
  *done when:* discovery returns a typed result on both Windows and Linux; unit tests with fake `adb` scripts.
  *deps:* 0.3

- [ ] **1.2 ADB command runner with timeouts** — M
  Thin wrapper around `std::process::Command` (async via tokio optional — decide now). Per-command timeout, stdout/stderr capture, non-zero exit mapped to error variants. Central place to detect `device unauthorized`, `device offline`, `no devices`.
  *done when:* runner handles timeout kill, stderr surfacing, and exit-code mapping; unit-tested.
  *deps:* 1.1

- [ ] **1.3 Server lifecycle handling** — S–M
  Reuse a running ADB server (`adb start-server` is idempotent); never kill it on app exit; detect server-version conflicts and surface a meaningful error.
  *done when:* app starts alongside an existing server without restarting it; conflict error is surfaced.
  *deps:* 1.2

- [ ] **1.4 Device enumeration & state** — M
  Parse `adb devices -l` output into structs: serial, state (`device`/`unauthorized`/`offline`), model, transport info. Map to the plan's five states (Connected/Unauthorized/Offline/Disconnected/Error).
  *done when:* parser has unit tests against real-world output samples including the unauthorized/offline cases.
  *deps:* 1.2

- [ ] **1.5 Device property queries** — S
  Wrapper for `adb shell getprop ro.product.manufacturer`, `ro.product.model`, `ro.build.version.release`; handle empty/failing props gracefully.
  *done when:* properties returned as `Option<String>`; tested with mock output.
  *deps:* 1.2

- [ ] **1.6 Device watcher (polling)** — S–M
  Background task polling `adb devices` every N seconds (default 3–5s, configurable); emits `DevicesChanged` events; detects newly connected / disappeared / state-changed devices.
  *done when:* a test subscriber receives correct event sequences from a mocked runner.
  *deps:* 1.4

### Desktop shell (`crates/desktop/`)

- [ ] **1.7 Minimal egui window** — S
  `eframe` app shell: window title, close handling, a fixed-size main layout placeholder matching the plan's mock (device panel, device-detail panel, progress panel, status bar as empty sections).
  *done when:* window opens on Windows + Linux; no panics on close.
  *deps:* 0.1

- [ ] **1.8 Device list UI (read-only)** — S–M
  Wire watcher events (1.6) into the device panel: device serial + state badge, live updates without manual refresh, empty-state text when no devices.
  *done when:* plugging/unplugging a phone (or running the mock) updates the list in real time.
  *deps:* 1.6, 1.7

- [ ] **1.9 Platform CI check for both OSes** — S
  Confirm Phase 1 runs on Windows 10/11 and Linux (incl. one Arch-based distro manual pass); document Linux udev/USB permission pitfalls (plugdev group / udev rule) in a SETUP note.
  *done when:* manual checklist documented and passing.
  *deps:* 1.8

---

## Phase 2 — Device Management & Persistence

- [ ] **2.1 SQLite schema + migrations** — M
  `rusqlite` with `migrations` embedded via `include_str!`; tables `devices`, `device_folders`, `backup_jobs`, `backup_files` per plan §9 (indexes on `backup_files(device_id, source_path)`); migration runner testable from zero.
  *done when:* fresh DB → all migrations apply; opening an old-version DB migrates; schema version asserted in tests.
  *deps:* 0.1, 0.3

- [ ] **2.2 Device identity & profile storage** — M
  Repository layer for `devices`: upsert by serial, first-seen/last-seen timestamps, manufacturer/model/android version, display name (default = model or "Device {serial}"). Rename never changes identity.
  *done when:* repo unit tests (in-memory DB) cover upsert, rename, last-seen update, serial-uniqueness.
  *deps:* 2.1, 1.4

- [ ] **2.3 Serial-change handling** — S–M
  When an unknown serial appears, do not silently merge. Detect "new serial, known device characteristics" (optional heuristic) and offer the user an explicit "this is the same device" association action; support one explicit re-association.
  *done when:* test: device re-appears with new serial → history stays separate until explicit association merges the profile forward.
  *deps:* 2.2, 1.8

- [ ] **2.4 Multiple-device state in UI** — M
  Per-device selected profile, independent destination and settings in the detail panel; concurrency-safe per-device job slots (one backup per device at a time, multiple devices in parallel).
  *done when:* two mocked devices can be configured independently; starting backup on one doesn't block the other.
  *deps:* 1.8, 2.2

- [ ] **2.5 Config persistence (app-level)** — M
  `serde` + JSON/TOML config file in platform app-data dir: ADB mode (managed/system/manual + path), global backup location, poll interval. Config is separate from the SQLite manifest. Never overwrite a user-set ADB path silently.
  *done when:* config round-trips; corrupt config falls back to defaults with a logged warning; unit tests.
  *deps:* 0.3

- [ ] **2.6 Backup location selection UI** — S–M
  Global default prompt on first launch (suggest `%USERPROFILE%/AndroidBackups` / `~/AndroidBackups`); "Change Location" button per device via `rfd`; per-device destination stored in `devices.destination_path`.
  *done when:* user can set global + per-device locations; changing a device destination leaves previous backups untouched.
  *deps:* 2.4, 2.5

- [ ] **2.7 Destination validation** — M
  Pre-backup checks: exists/creatable, writable (probe file), free-space check where available, missing-external-drive detection; typed validation errors shown in UI. Mid-backup destination loss → safe pause with report (hook for 3.x).
  *done when:* unit tests inject each failure mode; UI shows a specific, actionable message.
  *deps:* 2.6

---

## Phase 3 — Backup Engine (`crates/core/src/backup/`)

- [ ] **3.1 Remote filesystem enumeration** — M
  Enumerate phone directories via `adb shell` (e.g. `ls -R` / `stat`-based walker or `find <dir> -type f` with null-delimited parsing) handling spaces and unicode in names. Record: relative path, size, mtime (when available). Report inaccessible/permission-denied dirs distinctly (plan §7).
  *done when:* parser handles filenames with spaces/quotes/newline edge cases; inaccessible dirs surfaced as errors, not "empty".
  *deps:* 1.2

- [ ] **3.2 Path sanitization & safety** — M
  Shared sanitizer: reject absolute paths, `..` traversal, NUL/control chars; map phone paths → destination paths deterministically; never trust a phone-provided name as a filesystem path raw (plan §9).
  *done when:* property-based/unit tests cover traversal attacks and weird names.
  *deps:* 0.3

- [ ] **3.3 Folder selection model** — S
  Default folder set (DCIM, Pictures, Movies, Music, Documents, Download, Android/media, Recordings, Screenshots) with per-device enable flags in `device_folders`; UI checkboxes; custom path entry with validation via a dry-run `adb shell ls`.
  *done when:* per-device folder set persists and round-trips; custom path validated before save.
  *deps:* 2.2, 3.1

- [ ] **3.4 Duplicate-source elimination** — S
  Deduplicate overlapping selections (e.g. Screenshots ⊂ Pictures): single canonical enumeration per device, unique by absolute source path, so no file is copied twice.
  *done when:* test with overlapping folder configs yields each file once.
  *deps:* 3.3

- [ ] **3.5 File comparison logic** — M
  Compare enumerated sources vs `backup_files` manifest using relative path + size + mtime (+ last backup status). Classify: new / changed / unchanged / missing-from-phone / failed-transfer. Pure function, unit-testable. Missing on phone ⇒ never deletes anything.
  *done when:* table-driven tests cover all five classifications incl. equal-size-different-mtime cases.
  *deps:* 2.1, 3.1

- [ ] **3.6 Safe file transfer** — M
  Pull each file via `adb pull` into `*.partial` temp name; on success verify (3.8) then rename to final name; never move partials blindly. Per-file retry policy (initially: no byte-offset resume, restart the file). Cancelable mid-file (kill pull, remove partial).
  *done when:* simulated pull failure leaves no final-file; retry restarts cleanly; cancel leaves no artifacts.
  *deps:* 1.2, 3.2

- [ ] **3.7 Collision / versioning strategy** — M
  If destination file exists but differs (changed source): preserve old copy without polluting the user tree — e.g. `Backups/<Device>/<relpath>` holds latest; prior versions moved to `Backups/.backup/history/<relpath>/<timestamp>-<hash8>`. Never silently overwrite. Document chosen strategy in code/docs.
  *done when:* changed file triggers history-preserving move; unchanged file never touches existing copy; unit tests.
  *deps:* 3.6

- [ ] **3.8 Transfer verification** — S–M
  Post-transfer: dest exists, size equals source size (from enumeration metadata), compute local SHA-256, store hash + `verified` status in manifest. Size-only is explicitly NOT claimed as full integrity (plan §8.5).
  *done when:* verification results recorded per file; size-mismatch ⇒ file marked failed and moved back to partial handling.
  *deps:* 3.6, 2.1

- [ ] **3.9 Backup job orchestration** — L
  Job state machine per device: `Plan → ScanPending → Copying → Verifying → Finalizing → Completed/Failed/Cancelled` (+ `Paused` for later). Runs on a worker thread/task; streams progress events (files_done, bytes_done, current file, ETA); records `backup_jobs` row with counts at each transition; transactional manifest updates per file (plan §9).
  *done when:* mock end-to-end run produces a correct `backup_jobs` row and per-file `backup_files` rows; cancel mid-run marks job cancelled, completed files remain recorded.
  *deps:* 3.5, 3.6, 3.7, 3.8

- [ ] **3.10 Interrupted-transfer recovery** — M
  On scan/startup: find `*.partial` files under per-device destinations; verify against manifest; delete stale partials (not counted as backup); mark corresponding `backup_files` rows as failed so they re-copy next run. Re-verify source availability before re-copy.
  *done when:* integration test: kill process mid-copy → relaunch → partial cleaned, file re-copied, nothing falsely recorded complete.
  *deps:* 3.6, 3.9

- [ ] **3.11 Job control API (pause/resume/cancel)** — M
  Public core API used by desktop and future CLI: `start(device)`, `pause`, `resume`, `cancel`. Pause stops after current file completes (MVP: no mid-file offset resume). Resume continues from scan results + manifest.
  *done when:* pause/cancel/resume sequences pass against a mocked transfer harness.
  *deps:* 3.9

---

## Phase 4 — Usable MVP Interface

- [ ] **4.1 Progress UI** — M
  Wire job events into the progress panel: `128 / 450 files`, GB copied, current filename, speed + ETA (computed from event stream).
  *done when:* live view matches a real (or mocked) backup's progress.
  *deps:* 3.9, 1.7

- [ ] **4.2 Start/pause/resume/cancel controls** — S–M
  Buttons per selected device wired to 3.11; disabled states reflect job state machine; unauthorized device blocks start with guidance message.
  *done when:* full control flow works against a real device.
  *deps:* 3.11, 4.1

- [ ] **4.3 Error & completion summaries** — S–M
  Per-job summary: files_scanned/copied/skipped/failed, bytes, error list; last-backup line on main screen ("Completed — 430 copied, 20 unchanged"); failure keeps job row but no "completed" status (plan §2).
  *done when:* forced-failure run shows error summary; successful run shows counts.
  *deps:* 3.9, 4.1

- [ ] **4.4 Backup history view** — S–M
  List of `backup_jobs` for the selected device (date, status, counts, duration); expandable job detail listing copied/failed files.
  *done when:* history renders across app restarts from the DB.
  *deps:* 2.1, 4.1

- [ ] **4.5 Device rename UI** — S
  Editable display name in device panel; reflected in destination folder suggestion and history (history keyed by device id, not name).
  *done when:* rename persists and existing history remains attached.
  *deps:* 2.2, 1.8

- [ ] **4.6 ADB managed installation** — L
  First-launch flow (plan §4.1): detect missing adb → prompt → download official Platform Tools zip for platform (reqwest, streamed, resumable-on-retry) → verify checksum against Google-published hash → extract only expected entries (zip-slip protection) → verify `adb version` → persist path in config. Support "use system adb" and "pick manually" alternatives; never execute unverified binaries.
  *done when:* end-to-end on Windows + Linux: no adb installed → app guides install → adb usable; corrupted download rejected; interrupted download retriable.
  *deps:* 1.1, 2.5
  *note:* check for offline/frozen availability of Google's checksum file; store downloaded artifacts outside the backup DB per plan §4.1.

- [ ] **4.7 Destination validation surfaced in UI** — S
  Wire 2.7 checks into the Start Backup flow with actionable messages + retry suggestion; warnings for low disk space include required vs available.
  *done when:* pre-flight failures block start with clear messages.
  *deps:* 2.7, 4.2

---

## Phase 5 — Reliability Hardening

- [ ] **5.1 Integration test harness (mock ADB)** — L
  Fake adb fixture (script or in-process mock behind the runner trait) able to simulate: normal listing, disconnected mid-backup, unauthorized, permission-denied dirs, slow/streamed transfers, weird filenames, duplicate basenames in different folders.
  *done when:* harness drives the full engine end-to-end in CI without a real phone.
  *deps:* 1.2, 3.9

- [ ] **5.2 Disconnect & interrupt scenario tests** — M
  Scenario suite: cable pull mid-copy, phone reboot, adb-server restart, app kill during copy/DB write; assert manifest consistency and recoverability after each.
  *done when:* all scenarios recover correctly; no partially-verified files recorded complete.
  *deps:* 5.1, 3.10

- [ ] **5.3 Duplicate-name & changed-file behavior tests** — S
  Files with identical names in different folders; same file under overlapping selections; same path with changed content → versioning per 3.7.
  *done when:* golden-result assertions on destination tree + manifest.
  *deps:* 5.1, 3.7

- [ ] **5.4 Database recovery tests** — S
  Corrupt/truncated SQLite recovery guidance, migration re-runs, transaction integrity under kill (WAL mode decision), backup-copy of manifest DB note (plan §9).
  *done when:* kill-during-write test leaves a consistent DB; recovery path documented.
  *deps:* 2.1

- [ ] **5.5 Large-scale performance test** — M
  Synthesize a ≥ 50k-file / multi-GB library on a device-emulating machine; measure scan time, per-file overhead, UI responsiveness.
  *done when:* second-run scan of 50k unchanged files completes in acceptable time (baseline: minutes, not hours); no unbounded memory growth.
  *deps:* 5.1

- [ ] **5.6 Hardware test pass** — L
  Manual matrix per plan §14: one Samsung + one non-Samsung device; restricted/unavailable directories; external destination drive pull-out during backup; Windows + Linux USB; app restart mid-backup.
  *done when:* checklist in `docs/testing.md` fully checked with findings logged as issues.
  *deps:* 4.x complete

- [ ] **5.7 Unit-test coverage completion** — M
  Ensure plan §14 unit-test list is covered: path sanitization, comparison, manifest updates, collisions, state transitions, interrupted recovery, identity matching.
  *done when:* checklist mapped to test names in a tracking doc; CI enforces them.
  *deps:* 5.1

---

## Phase 6 — Release & Distribution

- [ ] **6.1 Windows release build workflow** — M
  GitHub Actions job producing portable x86_64 .exe artifact on tag push; code-signing decision (even "unsigned, document SmartScreen" is a decision to record); static-link check (no MSVC redistributable requirement, or document it).
  *done when:* artifact runs on a clean Windows VM/box with no Rust/toolchain installed.
  *deps:* 0.2

- [ ] **6.2 Linux release build workflow** — M
  AppImage (primary; minimizes system-dependency issues for egui backends — verify GTK/x11 vs winit requirements); `.deb` optional; build in a pinned base image; document required udev rule for adb on clean systems.
  *done when:* artifact launches on clean Arch and Ubuntu VMs; SETUP doc includes udev/permission steps.
  *deps:* 0.2

- [ ] **6.3 GitHub release publishing** — S
  Tag-triggered workflow attaching both artifacts to a versioned GitHub Release with auto-generated notes; version scheme (`0.1.0`) decided; workspace crates versioned in lockstep.
  *done when:* tagged release publishes correct artifacts automatically.
  *deps:* 6.1, 6.2

- [ ] **6.4 Setup & troubleshooting documentation** — S–M
  `docs/setup.md`: download/install per platform, USB debugging enablement, authorization prompt, udev rules (Linux), choosing backup location, first backup walkthrough. `docs/troubleshooting.md`: unauthorized device, drive not found, adb conflicts, recovery after interrupted backup.
  *done when:* a non-technical user can follow setup on both platforms without docs gaps (do a dry-run with someone else).
  *deps:* 6.1, 6.2

- [ ] **6.5 MVP acceptance run** — M
  Execute the plan's MVP definition end-to-end on both platforms: connect phone → select destination → back up → disconnect → reconnect → re-run → verify no redundant re-copy of unchanged files and no loss of previous copies.
  *done when:* recorded test results affirm the defining MVP behavior (plan §12 final line + §16 Phase 5).
  *deps:* 5.6, 6.3, 6.4

---

## Dependency overview (critical path)

```
0.1 → 0.3 → 1.1 → 1.2 → {1.4, 1.5, 1.6} → 1.8 → Phase 2 …
0.1 → 0.2 (CI early, keeps platform parity honest)
2.1 → 2.2 → {2.3, 2.4, 3.3}
3.1 → 3.5 → 3.9 ; 3.6 → {3.7, 3.8} → 3.9 → 3.10/3.11 → Phase 4 UI → Phase 5 → Phase 6
4.6 (ADB install) is independent of 3.x and can run in parallel once 1.1/2.5 are done
```

**Suggested parallel tracks** (assuming 1 dev, pick one per track):
1. ADB/engine track: 1.1–1.6 → 3.1–3.11
2. Persistence track: 2.1–2.2 → 3.5 manifest side → 2.5
3. UI track: 1.7–1.8 → 2.4–2.6 → 4.1–4.5
4. Release/infra track: 0.2 → 4.6 → 6.1–6.4 (start CI early; packaging last once UI stabilizes)

## Explicitly deferred (do NOT schedule now)
Per plan §13: scheduling/auto-backup, background daemon/notifications, encryption, restore/browse, file filters, cross-path duplicate detection, retention policies, multiple profiles per device, network ADB, config export, DB repair tools, CLI automation, byte-offset resume (§8.4 — file-level restart only for MVP).