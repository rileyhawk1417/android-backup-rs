# Wave 7 — Managed ADB Install & Reliability

Up: [index.md](index.md) · Prereq: Wave 6 gate passed.
This wave is scheduled *after* the engine and UI on purpose: it blocks nothing on
the critical path, so its risk never endangers the engine. Now it completes the
polished first-launch path and hardens everything.

## E07.1 Managed installer (plan §4.1)

**Actions**
1. `core::adb::installer`:
   a. Download URLs (record exact URLs + checksum sources in module docs; fetch at
      build/first-use, never hardcode hashes without a verify step):
      - Windows: `platform-tools-latest-windows.zip`
      - Linux: `platform-tools-latest-linux.zip`
      - Official host: Google developer distribution (android.com) — the **only** source.
   b. Streamed download via `reqwest` (blocking client OK — this runs in the
      installer task, not the engine); progress callback → UI modal; retry on
      network error: re-download whole file (simple and safe).
   c. **Verify before extract:** compute SHA-256 of the downloaded archive; compare
      against Google's published checksum; mismatch → delete temp, error
      `ChecksumMismatch` — never execute unverified binaries (plan §4.1 hard rule).
   d. Extract with `zip` crate: allow-list extract (only entries matching
      `platform-tools/adb*`, `.dll`s required on Windows, `fastboot*` optional-no —
      keep it to adb artifacts only) + zip-slip guard (reject entries whose resolved
      path escapes the target dir). Extract into `<app_data>/platform-tools/`.
   e. Re-verify with E01.1 locator; on success set `managed_adb_dir` + `AdbMode::Managed`
      in config.
2. UI wiring (W6 modal): enable the `[Install automatically]` button; progress modal;
   failure path returns to the choice modal with the error inline.

**Pitfalls**
- Do not extract `platform-tools/hyper/thread/vulkan` shader caches etc. — the
  zip is large; allow-list keeps extraction honest and fast.
- Free-space check (E04.4) before download (archive is ~1.5 GB unzipped).
- Handle disk-full during extraction: partial extract → clean up temp dir → typed error.

**Accept**
- Mock-HTTP tests (local server fixture): good archive installs; corrupt archive
  rejected at checksum; truncated download → retry works; zip-slip entry rejected
  before touching disk.
- CI: unit-level with fixtures (network E2E as a non-blocking nightly job).

## E07.2 Unauthorized-mid-backup handling

**Actions**
1. E05.3 already maps runner errors to typed events; extend the job loop:
   `AdbError::DeviceUnauthorized` during Copying → transition to `Paused` with
   reason `Unauthorized` (plan §11: display status clearly; auto-retry after
   re-auth is a *later* feature — user re-clicks Resume).
2. UI (E06.3/e06.4): Paused{Unauthorized} renders the guidance text + a Resume
   button; Resume re-runs the gate first.

**Accept**
- Mock scenario: unauthorized injected mid-copy → job paused, partial cleaned,
  manifest truthful; user approves → resume completes remaining files.

## E07.3 Reliability suite

**Actions** (port the Phase-5 list from `task_breakdown.md` into real tests)
1. `tests/scenario_disconnect.rs`: cable pull (runner spawn error) mid-Copying →
   job Failed/Paused with truth manifest, recovery in next job re-copies remaining files.
2. `tests/scenario_kill_db_write.rs`: simulate crash between rename and manifest
   write (inject at the E05.4 seam) → harmless duplicate-copy next run (already
   covered in W5 — assert the full job-level flow too).
3. `tests/scenario_duplicate_names.rs`: `a/IMG_1.jpg` vs `b/IMG_1.jpg` → both
   backed up to distinct destinations; same file in overlapping selections → once.
4. `tests/scenario_corrupt_db.rs`: truncate SQLite file mid-open → typed error at
   startup; app degrades to "run repair?" message (repair tool is out of scope/§13 —
   degraded mode + guidance is the MVP answer).
5. `tests/scenario_large.rs`: 50 k synthetic files (mock runner, fast path) →
   scan+compare under a performance budget (e.g. < 60 s, log actual); memory
   stays bounded (assert RSS via allocator stats or simple peak-tracking).
6. `tests/scenario_overlap_and_history.rs`: rename-conflicts + E05.5 history
   correctness at scale (10 versions of same file).
7. `docs/testing.md`: record results table (test × OS × result); hardware matrix
   checklist (Samsung, non-Samsung, restricted dirs, external drive pull, restart
   during backup) listed as `hw-verify-needed` items for E08.4.

**Accept**
- Full suite green on CI matrix; performance test logs its measurements;
  `docs/testing.md` updated with results.

---

**Wave gate:** installer E2E fixtures green; unauthorized-pause scenario green;
reliability suite green. Next: [wave-8-release.md](wave-8-release.md).