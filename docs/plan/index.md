# Android Backup Utility — Build Plan Index

> **You are the executing agent. Start at Wave 0. Work waves top-to-bottom, tasks
> left in the order listed. Do not skip a wave whose acceptance gate has not passed.**

This index is the single source of truth for *what to do next*. Each wave has its
own doc with the full per-task instructions. Mark checkboxes here (in this file)
as tasks complete — this is the only progress record.

## Execution rules

1. A task is done only when its per-wave doc's **Acceptance** section passes and the
   repo is green: `cargo fmt --check && cargo clippy -- -D warnings && cargo test`.
2. One commit per task, prefix the message with the task ID: `[E05.2] comparison engine`.
3. Never ship stub logic that pretends to work. If hardware is unavailable, run tests
   via the mock harness and tag the commit `hw-verify-needed` (batch-verified at E08.4).
4. If a task fails acceptance twice, stop; fix the failing `deps` task first; resume.
5. Deferred features (plan §13) are out of scope. If you discover a missing
   dependency, update the wave docs' dep lines and re-order **before** continuing.

## Why this order (blocker resolution)

The most-blocked feature is the backup engine (Wave 5). It waits on the ADB runner,
the manifest DB, device profiles, validated destinations, and remote enumeration.
Everything before Wave 5 exists to clear those blocks in topological order.
Managed ADB download/install blocks nothing on this path — hence Wave 7.

| Wave | Doc | Clears blocks for | Gate |
|------|-----|-------------------|------|
| 0 | [wave-0-workspace.md](wave-0-workspace.md) | everything | builds + CI green both OSes |
| 1 | [wave-1-adb-core.md](wave-1-adb-core.md) | discovery, engine | mock ADB runner tests green |
| 2 | [wave-2-data-foundation.md](wave-2-data-foundation.md) | profiles, engine, folders | migration + repo tests green |
| 3 | [wave-3-device-discovery.md](wave-3-device-discovery.md) | profiles, folders, UI | watcher event tests green |
| 4 | [wave-4-profiles-locations.md](wave-4-profiles-locations.md) | engine | destination + folder model tests green |
| 5 | [wave-5-backup-engine.md](wave-5-backup-engine.md) | UI, release | **full mock E2E green — hard gate** |
| 6 | [wave-6-desktop-ui.md](wave-6-desktop-ui.md) | release | manual app run via mock + device |
| 7 | [wave-7-install-reliability.md](wave-7-install-reliability.md) | release | installer E2E + reliability suite |
| 8 | [wave-8-release.md](wave-8-release.md) | — | MVP acceptance run |
| 9 | [wave-9-automation.md](wave-9-automation.md) | — | 0.2.0 release |

## Progress tracker

Wave 0 — Workspace & conventions
- [x] E00.1 Workspace skeleton
- [x] E00.2 Error/log conventions
- [x] E00.3 CI plumbing

**E00.3 note (added during the Wave 7 audit):** the checkbox was ticked but
`.github/workflows/` did not exist — the gate was recorded without its artifact.
It now does: `ci.yml` (fmt + clippy `-D warnings` + the whole suite on
`ubuntu-latest` **and** `windows-latest`) and `nightly.yml` (the live installer
test, `continue-on-error`). Neither has run on GitHub yet, because this checkout
has no remote; the first green run is the outstanding half of the gate and the
`apt-get` list for the Linux GUI build is the most likely first edit.

Wave 1 — ADB execution core
- [x] E01.1 ADB locator
- [x] E01.2 Command runner + error mapping
- [x] E01.3 Server lifecycle (reuse, never kill)

Wave 2 — Data foundation
- [x] E02.1 SQLite + migrations
- [x] E02.2 Device repository
- [x] E02.3 Job & file repositories
- [x] E02.4 Path sanitization
- [x] E02.5 App config

Wave 3 — Device discovery & identity
- [x] E03.1 `adb devices -l` parser
- [x] E03.2 State mapping
- [x] E03.3 Property fetcher
- [x] E03.4 Polling watcher
- [x] E03.5 Authorization gate

**Wave 3 gate: PASSED.** 11 fixture files (`crates/core/tests/fixtures/devices/*.txt`)
plus `crates/core/tests/device_listing_fixtures.rs` (8 tests: exhaustive expectation
table, CRLF bytes, spacing matrix, corpus coverage) and the watcher suite in
`crates/core/src/devices/watcher.rs` — including one test that drives the real
thread through a scripted timeline (unauthorized → authorized → unplugged →
adb dead → adb back) and asserts the exact event sequence, and one that counts
sink upserts to prove enrichment is not written once per poll tick.

Decisions taken while building this wave:
- **One event per device per tick**, best-variant-wins: a transition to
  `Connected` emits `DeviceEvent::Connected`, not `Connected` *and*
  `StateChanged`. `StateChanged` covers transitions with no named variant
  (i.e. into `Error`). `DeviceEvent` gained `PollFailed`/`PollRecovered`, which
  are E03.4's "single Error event on transition into/out of the error condition"
  in concrete form.
- **`stop()` uses a condvar, not a bare `AtomicBool`**, so a UI with a 30s poll
  interval still quits instantly (the wave doc's accept asks for "joins within one
  poll interval" — this is stronger).
- **Everything above the `List of devices attached` header is discarded**, and a
  space-separated entry must carry a known state token. Real adb prints
  `adb server version (41) doesn't match this client (39)` before the header;
  before this rule that line parsed as a phantom device called `adb`, and a
  phantom device is one the user cannot explain or remove.
- **A failed poll keeps the last good snapshot** and flags it (`last_poll_ok()`),
  so the UI can say "the list may be stale" instead of silently showing a phone
  that has been unplugged.
- **`DeviceInfo::effective_props()`** falls back to the listing's `model:`/`product:`
  when `getprop` was silent, so an unauthorized phone still gets a sensible name
  hint (and the same name the manifest default would choose).
- **`Db` implements `DeviceSink`** in `database::devices`; a failed write is
  logged, not propagated — a full disk must not take down device detection.

Wave 4 — Profiles, locations, folders
- [x] E04.1 Display names & rename
- [x] E04.2 New-serial policy
- [x] E04.3 Destination selection model
- [x] E04.4 Destination validation
- [x] E04.5 Folder-selection model
- [x] E04.6 Enumeration dedup + custom dirs

Wave 5 — Backup engine
- [x] E05.1 Remote enumeration
- [x] E05.2 Comparison engine
- [x] E05.3 Transfer engine
- [x] E05.4 Verification + hashing
- [x] E05.5 Collision versioning
- [x] E05.6 Job state machine + progress
- [x] E05.7 Interrupted-transfer recovery
- [x] E05.8 Control API

**Wave 5 gate: PASSED.** `cargo test -p android-backup-core --test e2e_backup`
(25 tests) runs the real engine, manifest, staging/verify/rename/history and
filesystem against a scripted phone ([`MockRemote`], [`LocalFsProbe`]): happy
path · skip-on-second-run · deleted-on-phone retention · changed file + history ·
locally deleted/truncated re-copy · pause/resume · cancel (before and mid-copy) ·
kill recovery · verify failure + retry · denied folder · hostile filenames ·
overlap dedup · whole storage · failed-then-retry · transient reconnect · no
storage root · destination refused at start · control-API error matrix ·
SHA-256 verification. No UI work may start unless this stays green.

Decisions taken while building this wave (they refine, not contradict, the plan):
- **Hash verification needs a remote digest.** `RemoteFiles::hash` runs
  `sha256sum` on the phone; the staged copy must reproduce it. A device without
  `sha256sum` answers `Ok(None)`, which hash mode records as *evidence only* and
  logs — it never pretends the bytes were compared (plan §8.5).
- **A fatal infrastructure error always emits `JobEvent::Fatal`**, even when it
  propagates out of the engine loop, so the UI never has to parse a summary.
- **The destination is validated in `BackupController::start`** (fail fast, no
  job row, no phone traffic) *and* again inside the engine (defence in depth).

Wave 6 — Desktop UI
- [x] E06.1 App shell
- [x] E06.2 Live device list
- [x] E06.3 Device detail panel
- [x] E06.4 Progress panel
- [x] E06.5 History + summaries
- [x] E06.6 First-launch flows

**Wave 6 gate: PASSED for everything a machine can check; manual run tagged
`hw-verify-needed`.** `crates/desktop` renders through the real `egui` pass in
`tests/ui_headless.rs` (13 tests: chrome + empty state, connected device panel,
Start-button gating, unauthorized help text, running job progress and controls,
paused reason + Resume, completed summary, welcome wizard, ADB-not-found modal
with all three routes, install progress modal + cancel, error banner, expanded
history row). The architecture gate
is *enforced*, not documented: `tests/architecture.rs` (4 tests) fails the build if
the UI acquires `sha2`/`rusqlite`/`fs::rename` (backup logic), if `core` acquires
`egui`/`eframe`/`rfd`, if a second `eframe::App` appears, or if the view stops
folding core's own `JobEvent`/`JobStatus`. The release binary was started with no
display: it logs, writes the manifest and the log file, reports the reason, and
exits 0. The human matrix (real phone on Linux/Windows) is in
[docs/testing.md](../testing.md) §2 and is PENDING.

Decisions taken while building this wave:
- **`egui::Context::run` is the test harness.** Asserting on the view model alone
  would pass even if a panel never drew the value, so the tests read the text out
  of the frame's shapes. No window, no GPU, no event loop, so it runs in CI.
- **Rendering and logic are separated by an `Intent` queue.** Widgets push
  `Intent` values; `App::handle` runs them after the frame, which avoids mutating
  state while `egui` holds a borrow. Every `Intent` maps to a method call, never
  to work.
- **A device with a manifest row but no live snapshot is gated as
  `Disconnected`.** Otherwise a phone that was unplugged since the last run would
  show an enabled Start button it cannot honour.
- **History is re-read on every selection change**, not only on job completion, so
  the list always matches the manifest (and survives a restart, which is E06.5's
  acceptance).
- **The device subfolder may not exist yet; its parent must.** The engine creates
  `<destination>/<Device Name>/` on the first backup, but if the drive holding the
  parent is gone, creating the path would silently write to a plain directory on
  another disk — so validation targets the parent, and the wizard's suggested
  `~/AndroidBackups` is created explicitly on accept.
- **One module knows `eframe`** (`window.rs`), so the window size/title/exit
  behaviour are testable values and `App` stays headless-testable.
- **Logging is a file, always**: `logs/android-backup.log` under the data
  directory, `ANDROID_BACKUP_LOG` to override the level, stderr as a fallback when
  the directory is unusable. A failed log must never stop the app from starting.

Wave 7 — ADB install & reliability
- [x] E07.1 Managed installer
- [x] E07.2 Unauthorized-mid-backup handling
- [x] E07.3 Reliability suite

**Wave 7 gate: PASSED for everything a machine can check; the real download is
tagged `hw-verify-needed`.** E07.1: `core::adb::installer` fetches Google's
pinned `repository2-3.xml`, streams the archive, verifies the published digest
*before* extracting, allow-list extracts only what adb needs, and swaps the
result into `<data>/platform-tools` only after the new binary answers
`adb version` (staging sibling + rename, so a crash or a cancel leaves the old
installation untouched). The UI wiring is live: the `[Install automatically]`
button starts a worker-thread install, a progress modal shows the stage, the byte
count and a Cancel, and every failure returns to the choice modal with the reason
inline plus the two routes that still work. E07.2 was already satisfied by the
engine's pause path (`PauseReason::AuthorizationNeeded` on a transient error that
`needs_authorization()`); `scenario_disconnect.rs` pins it end to end. E07.3: six
scenario suites (23 tests) — disconnect/unauthorized, kill between rename and
manifest write, duplicate names and overlapping selections, corrupt manifest,
50,000 files under budget, and 10-version history.

Decisions taken while building this wave:
- **The installer is a trait in the UI layer** (`startup::AdbInstaller`), with
  `ManagedInstaller` as the production implementation. The UI needs to render a
  checksum mismatch, a full disk and a cancellation, and no test can provoke those
  against `dl.google.com`; `testutil::ScriptedInstaller` scripts them instead.
- **A failed or cancelled install writes nothing.** The config is only repointed
  *after* the report comes back, so the worst case is the state the user was in
  before they clicked.
- **The progress modal stays up past the last byte** (“Checking that adb runs”):
  the install is finished when `adb version` answers, not when the bytes land.
- **`set_modal` exists beside `show_modal`.** `show_modal` suppresses a kind the
  user dismissed (that is what stops the adb nag); a progress dialog is the
  opposite case — the user just asked for it — so it is set explicitly.
- **`VersionProbe` is `Send + Sync`.** The managed installer carries one and runs
  on a worker thread; a probe is a pure function of a path, so this was free.
- **A cancel is noticed on the next progress tick**, not pre-emptively: the
  installer checks the callback's answer and returns `InstallError::Cancelled`
  with its staging directory removed on the way out.

Wave 8 — Release
- [x] E08.1 Windows artifact
- [x] E08.2 Linux artifact
- [x] E08.3 Publish + docs
- [x] E08.4 MVP acceptance run

**Wave 8 gate: PASSED for everything this machine can check; the MVP acceptance
record is [`docs/acceptance.md`](../acceptance.md), and it names what is still
unobserved.** E08.1: `scripts/package-windows.ps1` stages the `.exe`, README,
licences and docs into `android-backup-<v>-windows-x86_64.zip`, and
`smoke-windows.ps1` checks `--version`, the handling of an unknown argument, and
that a first run creates its data directory inside an isolated `%APPDATA%`.
Nothing has been built or run on Windows yet — the release workflow's
`windows-latest` job is the first real run — so the script logic was exercised
here under PowerShell 7 with a stand-in binary, and the Windows rows in
`acceptance.md` stay PENDING until CI says otherwise. The signing decision
(E08.1 item 3, whose deliverable is the decision itself) is recorded in [`docs/release.md`](../release.md)
§3: unsigned for the MVP, with what signing would buy, why it is not in yet, and
the follow-up. E08.2: `package-linux.sh` builds the tarball (binary, `install.sh`,
launcher, icon, docs, `.deps.txt`), `build-appimage.sh` builds the AppImage from a
pinned `appimagetool` with its checksum verified, `check-glibc.sh` reports the
glibc floor, and `smoke-linux.sh` proves the *artifact* starts: private `Xvfb`,
`env -i`, throwaway `HOME`, window title asserted. `ldd` shows only
`libc`/`libm`/`libgcc_s`/`ld-linux`; X11, Wayland, GL and GTK are `dlopen`ed from
the host desktop, and both udev rule sets (Ubuntu package, Arch package) are in
`setup.md` §5. `adb` is not bundled — E07.1 installs it at runtime. The `.deb`
stays deferred. E08.3: `release.yml` runs verify → linux → windows → publish on a
`v*` tag, with `bump-version.sh`, `verify-tag.sh` and `release-notes.sh` beside it,
plus `CHANGELOG.md`, `setup.md`, `troubleshooting.md` and `release.md`. E08.4:
`acceptance.md` records the MVP run item by item — 421 automated checks green, the
first backup on the maintainer's own phone, and an explicit PENDING list with a
procedure per row. No failures, so the "Post-acceptance fixes" tracker is empty.

Decisions taken while building this wave:
- **The AppImage is secondary, and CI treats it that way.** The tarball is the
  artifact we stand behind; the AppImage wraps the same binary but depends on
  FUSE and on AppImageLauncher behaving. A failed AppImage build or smoke test is
  therefore dropped from the release (`continue-on-error` plus a delete step)
  rather than blocking it — and never shipped as a known-broken download.
- **The release is created as a draft.** Notes and checksums get one human read
  before they are visible; publishing is one click, correcting a published
  release is not.
- **The pipeline installs its own toolchain and uses the runner's own `gh`.**
  Inside `debian:12-slim` that means rustup via `curl`, and for publishing
  `gh release create` instead of a third-party release action: one dependency
  fewer in the path that can publish artifacts.
- **Packaging scripts build the binary themselves** (`cargo build --release
  --locked`), so "package a release" is one command per OS and CI cannot package a
  stale or differently-flagged build.
- **The glibc floor we advertise is the container's, not this desktop's.** A local
  build needs 2.43 (this machine's distribution); the release builds in
  `debian:12-slim` for a 2.36 floor and records the result in the artifact's
  `.deps.txt`, so the promise is per release and auditable.
- **The signing decision is written down rather than skipped** (`release.md` §3),
  including the limit of the mitigation: a checksum served beside the artifact
  protects against a bad download, not against a compromised account.
- **One plan deviation, on purpose:** E08.4 asked for the acceptance table to live
  in `docs/testing.md`. It lives in [`docs/acceptance.md`](../acceptance.md)
  instead — a per-release record with its own evidence, which `testing.md` §5 and
  `release.md` both link — because the answer to "is the product done" is not the
  same table as "which suites run".

## Wave 9 — Automation

> Scope confirmed with the maintainer: scheduling + auto-backup on connection
> are the must-haves; background = tray-resident app (no daemon); **Windows is
> the primary target** (Linux tray is best-effort); restore is **explicitly
> out** — the app stays one-way (phone → PC), restore is only noted in the wave
> doc's deferred section. Interrupted backups **resume on reconnect**, but only
> jobs the automation itself started (`trigger = auto`); a user-started paused
> job keeps waiting for its Resume click.

- [x] E09.1 Schedule model
- [ ] E09.2 Auto-backup on connection
- [ ] E09.3 Interval scheduler driver
- [ ] E09.4 OS notifications
- [ ] E09.5 System tray & close-to-tray
- [ ] E09.6 Settings & status UI
- [ ] E09.7 Docs, packaging, release (wave gate)

**Standing rules for this wave (from wave-9-automation.md):** one attempt per
trigger and one notification per connection episode, the gate decides exactly
as it does for the Start button, `DeviceBusy` is silence (the manual user
wins), automation is off by default and opt-in per device, and jobs record
`trigger = manual | auto` so the resume-on-reconnect rule can never touch work
the user started by hand.

## References

- `docs/overview_plan.md` — the authoritative product/architecture plan (§ numbers cited throughout)
- `docs/feature_tasks.md` — feature-by-feature view (F-numbers)
- `docs/task_breakdown.md` — original task IDs, dependency overview