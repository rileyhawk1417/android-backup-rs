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

## Progress tracker

Wave 0 — Workspace & conventions
- [x] E00.1 Workspace skeleton
- [x] E00.2 Error/log conventions
- [x] E00.3 CI plumbing

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
- [ ] E06.1 App shell
- [ ] E06.2 Live device list
- [ ] E06.3 Device detail panel
- [ ] E06.4 Progress panel
- [ ] E06.5 History + summaries
- [ ] E06.6 First-launch flows

Wave 7 — ADB install & reliability
- [ ] E07.1 Managed installer
- [ ] E07.2 Unauthorized-mid-backup handling
- [ ] E07.3 Reliability suite

Wave 8 — Release
- [ ] E08.1 Windows artifact
- [ ] E08.2 Linux artifact
- [ ] E08.3 Publish + docs
- [ ] E08.4 MVP acceptance run

## References

- `docs/overview_plan.md` — the authoritative product/architecture plan (§ numbers cited throughout)
- `docs/feature_tasks.md` — feature-by-feature view (F-numbers)
- `docs/task_breakdown.md` — original task IDs, dependency overview