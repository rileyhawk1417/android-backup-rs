# Wave 5 — Backup Engine

Up: [index.md](index.md) · Prereq: **all** Wave-4 tasks passed.
This is the wave everything else exists to unblock. Build tasks strictly in order —
each subsumes the previous one. Do not parallelize inside this wave.

Global rule for this wave: **core backup logic never touches the UI crate, and
every failure path leaves the manifest truthful** — a file recorded `verified`
was verified; a job recorded `completed` completed. When in doubt, record less.

## E05.1 Remote enumeration

**Actions**
1. Trait (phone I/O seam — the last big mock point):
   ```rust
   pub trait RemoteFiles: Send + Sync {
       fn list(&self, serial: &str, dir: &str) -> Result<Vec<RemoteFile>, EnumerateError>;
       fn pull(&self, serial: &str, src: &str, dest_partial: &Path) -> Result<(), AdbError>;
   }
   pub struct RemoteFile { pub abs_source: String, pub rel_path: RelPath,
                           pub size: u64, pub mtime: Option<u64> }
   pub enum EnumerateError { AccessDenied { dir: String }, Adb(AdbError), Parse(String) }
   ```
2. Listing strategy v1 (document trade-off in module docs):
   - `shell find <dir> -type f` → null-delimited? Not portable via adb (shells differ).
     Use `shell find <dir> -type f -maxdepth 1` per level? **No — do a breadth-first
     walk:** `find <dir> -maxdepth 1` for dirs (recurse) and files (batch-stat).
   - Batch size/mtimes: chunked `shell stat -c '%s %Y %n'` (toybox stat supports
     `-c`; fall back to `ls -l` parser if stat unsupported — feature-detect once
     per device, cache the result in-memory for the job).
   - mtime absent → `None` — comparison (E05.2) then keys on size only, and the
     manifest records that (honesty about weaker evidence, plan §8.5).
3. Permission handling: `find` errors / `Permission denied` in stderr →
   `AccessDenied { dir }` — **never** an empty result. Surface to UI as
   "this folder could not be read on the phone".
4. Chunking: buffer listing output; `adb` can buffer-deadlock on huge stdout —
   read stdout continuously (dedicated reader thread) inside the runner's exec
   for long outputs (extend E01.2 with `run_streaming` if tests demand it).

**Accept**
- Mock-runner tests: 1k-file listing; filename with spaces/unicode; nested dirs;
  denied dir → `AccessDenied`; parser failure of one line logs + skips line, doesn't
  abort listing (log the skip; count skipped in result meta).

## E05.2 Comparison engine

**Actions**
1. Pure module `core::backup::compare` — **no I/O, no runner, no DB**: inputs are
   plain data so tests are instant and total.
   ```rust
   pub enum FileAction { CopyNew, CopyChanged, SkipUnchanged, MissingOnPhone, RetryAfterFailure }
   pub struct FilePlan { pub rel: RelPath, pub src: RemoteFile, pub action: FileAction }
   pub fn compare(manifest: &[BackupFileRow], scan: &[RemoteFile], now) -> Vec<FilePlan>
   ```
2. Rules (document as a table in module docs, then encode):
   - rel path not in manifest → `CopyNew`
   - in manifest, `status=verified`, size equal AND (mtime equal OR either None) → `SkipUnchanged`
   - in manifest, verified, size equal, **mtime differs** → `CopyChanged` (mtime is
     the only change signal available; a size-equal content change is undetectable —
     accepted, documented)
   - size differs → `CopyChanged`
   - in manifest `status=failed_transfer` and still present in scan → `RetryAfterFailure`
   - in manifest but absent from scan → `MissingOnPhone` (no manifest mutation, no delete)
3. Ordering: output sorted by rel path for deterministic runs and tests.

**Accept**
- Exhaustive table tests: each rule ± mtime-None ± previously-failed;
  **the delete-class absence test**: assert `FileAction` has no destructive variant
  (compile-time; add a `matches!` test enumerating all actions).

## E05.3 Transfer engine

**Actions**
1. `core::backup::transfer::Transfer::pull(plan: &FilePlan, dest_root: &Path)`:
   - dest dir = E02.4 `map_to_destination` (created with `create_dir_all` first).
   - dest temp: `<filename>.partial` **in the same directory** (same-volume rename
     is atomic; cross-dir rename is not).
   - call `RemoteFiles::pull`; on Ok and verification pass (E05.4) → `fs::rename`
     partial → final (overwrites only in the collision-handled case, E05.5).
   - on any error/cancel: `fs::remove_file(partial)` best-effort + recheck removed.
   - retry: whole file (record attempt count in logs; no byte-offset resume in MVP).
2. Cancellation: shared `CancelToken` (AtomicBool + Condvar for prompt abort);
   checked before each file and after pull returns; a pull in flight is killed via
   runner timeout/kill (E01.2's process-group kill).
3. Destination-loss mid-transfer: pull/write errors matching the E04.4 patterns →
   map to `DestError::MissingDrive`, emit `JobEvent::Paused { reason }`, stop cleanly.

**Accept**
- Mock tests: success path leaves exactly one final file and zero partials;
  failure leaves zero files of any kind; cancel mid-run leaves zero partials;
  double-pull cannot overwrite an existing verified file without E05.5 running.

## E05.4 Verification + hash recording

**Actions**
1. Post-pull, pre-rename pipeline (all in the same function, ordered):
   a. partial exists; `metadata.len() == src.size` else fail.
   b. stream SHA-256 (8 KB reads) → hex string.
   c. **rename is the last step**.
2. Record via E02.3 `record_verified` only **after** rename succeeded, in one
   transaction (row + job counters). Timestamp `last_successful_backup = now (UTC)`.
3. Size-mismatch or hash-pipeline failure → `mark_failed`, partial removed.
   The hash is computed on the partial content — that's the same bytes that got
   renamed; document this (hash-then-rename is valid because rename is atomic same-dir).
4. Honesty rule (plan §8.5): the manifest stores `content_hash` of the local copy
   **plus** the evidence used for comparison (source size/mtime as reported by phone).
   Schema already has these columns; a `verified` status means "local copy verified
   and source evidence matched" — never "cryptographically equivalent to source".

**Accept**
- Tests: corrupted pull (mock writes right size, wrong bytes) → **fails on hash**,
  status `failed_transfer`, no final file, counter updated.
  Renamed-then-crashed simulation: rename happened, DB write lost → file present,
  manifest silent → comparison classifies as CopyNew next run → harmless re-copy
  (idempotent, never data loss) — assert exactly that.

## E05.5 Collision versioning

**Actions**
1. Trigger: final dest path already exists AND (E05.2 said CopyChanged). If
   `SkipUnchanged` said otherwise, trust it and never touch the file.
2. Preserve-old: `fs::rename(final, history_dir)` where
   `history_dir = <dest_root>/.backup/history/<rel_path>/<unix_ts>-<hash8>/`.
   `hash8` = first 8 hex of the **old file's** recorded content_hash if known,
   else of a freshly computed local hash; `unix_ts` = file's mtime when recorded,
   else job start time. Create dirs as needed. `.backup/` is app-owned, never
   enumerated as user content, excluded from recovery scans.
3. If rename-to-history itself fails: **abort the copy of this file** (status
   failed, keep both old file and delete partial) — never overwrite as fallback.

**Accept**
- Tests: changed file → history entry exists + new latest present + extension plain;
  unchanged file → mtime/inode untouched (compare before/after handles);
  history-rename-failure → copy aborted, partial cleaned.

## E05.6 Job state machine + progress events

**Actions**
1. `core::backup::job::Job`:
   ```rust
   pub enum JobState { Planning, Scanning, Copying, Verifying, Finalizing,
                       Paused, Completed, Failed, Cancelled }
   pub enum JobEvent { StateChange(JobState), Progress { done_files, total_files,
       copied_bytes, total_bytes, current_file: String, eta_secs: Option<u64> },
       Error(String), Finished { final_state, summary } }
   pub enum Control { Pause, Resume, Cancel }
   ```
2. Own thread per job (spawned by E05.8); control via mpsc receiver checked at
   loop boundaries; progress via an `mpsc::Sender` given at start.
3. Phases wired to E02.3 transitions (legal-transition SQL guards assert them too):
   `Planning` (validation E04.4 + recovery E05.7) → `Scanning` (enumerate + compare)
   → `Copying` (plan items in order; skip items are counted in bulk) →
   `Verifying` folded into per-file flow → `Finalizing` (summary write) → terminal.
4. ETA: rolling window (last 20 files) bytes/sec; `None` until ≥ 5 files sampled.
   Progress events at most every ~100 ms or per-file, whichever is later (UI repaint cost).
5. Counters written at Finalizing via the finalize transition — per-file counters
   already streamed by E02.3's atomic updates; Finalizing reconciles (defensive
   recompute from manifest rows; log a warning if they disagree).

**Accept**
- Mock E2E: scripted 5-file job → jobs row matches counts; events sequence valid
  (state monotonic, progress monotonic within a state); `Paused` → `Resume` → `Completed`.
- Cancel test: cancelled mid-Copying → `Cancelled` row, no partial files, verified
  files retained in manifest, counters consistent.

## E05.7 Interrupted-transfer recovery

**Actions**
1. Called during `Planning` of **every** job (also a standalone `recover(device)`
   for startup-time invocation from W6):
   - walk `dest_root` (not `.backup/`);
   - `*.partial` → `fs::remove_file` + matching manifest row (`status=verified?` —
     it can't be, since verified implies renamed; if a partial exists, set/update the
     manifest row for that rel path to `failed_transfer`) so E05.2 classifies
     `RetryAfterFailure` next;
   - log every removal (path + size).
2. Source availability: retry logic re-checks in Scanning naturally (compare
   against fresh scan) — no separate check needed; document this.
3. Partial-name safety: a `.partial` file whose rel path fails `sanitize_rel` →
   remove + warn (it's app-created junk; never enumerate as user content).

**Accept**
- Integration test: mock job killed (drop thread mid-pull, simulating process kill)
  → restart job → partial gone, row `failed_transfer`, file re-copied and verified,
  previous verified files untouched. Kill-during-rename special case: final file
  exists, row missing/failed → next run: compare says CopyChanged → history-preserved
  re-copy (E05.5), no loss (assert).

## E05.8 Control API (UI/CLI consume this and nothing else)

**Actions**
1. `core::backup::controller::BackupController`:
   ```rust
   pub fn start(&self, device: DeviceRow, cfg: JobConfig, events: Sender<JobEvent>)
       -> Result<JobHandle, StartError>;
   pub fn pause(&self, id: JobId) -> Result<(), ControlError>; // after current file
   pub fn resume(&self, id: JobId) -> Result<(), ControlError>;
   pub fn cancel(&self, id: JobId) -> Result<(), ControlError>;
   ```
2. `JobConfig` carries everything resolved already: destination, folders, gate
   result; the controller **re-validates** destination + gate at start
   (defense; UI can't be trusted).
3. Registry: `Mutex<HashMap<JobId, JobHandle>>`; one job per device
   (`StartError::AlreadyRunning { device_id }`); parallel devices allowed.
4. `JobHandle` = { id, join handle, control sender, events receiver cloned } —
   the UI subscribes to events; it never polls job internals.
5. `resume` semantics: spawns a **new job thread** that Planning-reuses the same
   unfinished job row (status Paused → back to Scanning, recovery included).
   A Paused job's partial rows make compare re-classify. No mid-file offset resume.

**Accept**
- Mock tests: start/double-start/pause/resume/cancel matrix incl. pausing a
  non-running job (error), cancel of finished job (error), device busy while
  second device starts (succeeds).

---

## Wave gate (hard)

End-to-end mock suite in `crates/core/tests/e2e_backup.rs` covering, at minimum:
happy path, changed file + history, cancel, kill-recovery, denied dir, weird
filenames, overlap dedup, failed-then-retry. All green on CI matrix **before**
any UI work begins. If this gate cannot pass, Wave 6 must not start —
every UI feature is downstream of it.

Next: [wave-6-desktop-ui.md](wave-6-desktop-ui.md).