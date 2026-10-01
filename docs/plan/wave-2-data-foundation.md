# Wave 2 — Data Foundation

Up: [index](index.md) · Prereq: Wave 1 gate passed.
Clears blocks for: device profiles (W4), folder model (W4), the backup engine (W5).

Everything here is I/O-adjacent but test-first: use an **in-memory SQLite**
(`rusqlite::Connection::open_in_memory`) in unit tests; the file-backed DB differs
only in connection setup. This wave is deliberately DB-only — no backup logic mixes in.

## E02.1 SQLite core + migrations

**Actions**
1. `core::database` owns the connection wrapper:
   ```rust
   pub struct Db { conn: rusqlite::Connection }
   impl Db {
       pub fn open_in_memory() -> Result<Db>;
       pub fn open_file(path: &Path) -> Result<Db>;
       pub fn migrate(&mut self) -> Result<u32, MigrationError>; // returns new version
   }
   ```
2. Pragmas on every open: `journal_mode=WAL` (file DB only), `synchronous=NORMAL`,
   `foreign_keys=ON`, `busy_timeout=5000`.
3. Migrations: `core/database/migrations/v1.sql` (and later v2.sql, …) embedded with
   `include_str!`. Runner = apply every file with version > `PRAGMA user_version`,
   each in its own transaction with `user_version` set inside the same transaction.
4. Schema v1 — exactly plan §9, plus the additions marked:
   ```sql
   CREATE TABLE devices (
     id INTEGER PRIMARY KEY,
     serial TEXT NOT NULL UNIQUE,
     display_name TEXT NOT NULL,
     manufacturer TEXT, model TEXT, android_version TEXT,
     destination_path TEXT,
     created_at TEXT NOT NULL, last_seen_at TEXT NOT NULL
   );
   CREATE TABLE device_aliases ( -- E04.2 needs this; add now to avoid a v2 migration
     alias_serial TEXT PRIMARY KEY, device_id INTEGER NOT NULL REFERENCES devices(id)
   );
   CREATE TABLE device_folders (
     id INTEGER PRIMARY KEY, device_id INTEGER NOT NULL REFERENCES devices(id),
     source_path TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 1,
     UNIQUE(device_id, source_path)
   );
   CREATE TABLE backup_jobs (
     id INTEGER PRIMARY KEY, device_id INTEGER NOT NULL REFERENCES devices(id),
     started_at TEXT NOT NULL, completed_at TEXT,
     status TEXT NOT NULL, -- planning|scanning|copying|verifying|paused|cancelled|completed|failed
     files_scanned INTEGER NOT NULL DEFAULT 0, files_copied INTEGER DEFAULT 0,
     files_skipped INTEGER DEFAULT 0, files_failed INTEGER DEFAULT 0,
     bytes_copied INTEGER DEFAULT 0, error_summary TEXT
   );
   CREATE TABLE backup_files (
     id INTEGER PRIMARY KEY, device_id INTEGER NOT NULL REFERENCES devices(id),
     job_id INTEGER NOT NULL REFERENCES backup_jobs(id),
     source_path TEXT NOT NULL, destination_path TEXT NOT NULL,
     source_size INTEGER, source_modified_at INTEGER,
     content_hash TEXT, last_successful_backup TEXT, status TEXT NOT NULL
   );
   CREATE UNIQUE INDEX idx_bf_dev_src ON backup_files(device_id, source_path);
   CREATE INDEX idx_jobs_dev ON backup_jobs(device_id, started_at);
   ```
   (Adjust exactly to plan §9 if the plan lists more columns; the **unique index**
   and **device_aliases** are the non-negotiable additions.)
5. Error type `MigrationError { Io, Sql(usize, rusqlite::Error), VersionSkew }` —
   `VersionSkew` covers a `user_version` **higher** than known migrations (future app
   opened old DB) → must fail loudly, not silently migrate backward.

**Pitfalls**
- WAL doesn't work on `:memory:` — that's fine, tests just don't test WAL; a separate
  file-backed test exercises WAL creation and cleanup (`-wal`/`-shm` files).
- Timestamps: store ISO-8601 UTC strings (`chrono` or `time` crate — pick one in
  workspace deps and use it everywhere). Never store phone-reported times as DB times.

**Accept**
- Empty DB migrates to v1; re-run is no-op (idempotent); version asserted = 1.
- Foreign-key violation test (insert folder for missing device) fails as expected.
- A DB with `user_version=999` → `VersionSkew` error, schema untouched.

## E02.2 Device repository

**Actions**
1. `core::database::devices`:
   - `upsert_seen(&Db, serial, manufacturer: Option<&str>, model: Option<&str>, android_version: Option<&str>, now) -> Result<DeviceRow, DbError>`
     — INSERT on first sight, UPDATE `last_seen_at` + non-null props otherwise. One statement pair, wrapped in a transaction.
   - `rename(&Db, device_id, new_name) -> Result<(), DbError>` — display name only.
   - `get_by_serial`, `get_by_id`, `list`.
   - `resolve_active_device(&Db, serial) -> Result<DeviceRow>` — looks up `device_aliases` first, then `devices`. (E04.2 consumes this; put the lookup here now.)
2. Display-name defaulting lives here: if no name given at insert → model, else
   `"Device {last 4 of serial}"`.

**Accept**
- In-memory tests: upsert twice → 1 row; `last_seen_at` advanced; rename changes
  name only; `resolve_active_device` hits alias table correctly.

## E02.3 Job & file repositories

**Actions**
1. `core::database::jobs`:
   - `insert_job(&Db, device_id, now) -> Result<i64>` (status `planning`).
   - One method per transition — **do not** expose raw "set status": 
     `begin_scanning`, `begin_copying`, `begin_verifying`, `finalize(summary)` where
     `Summary = { status: Completed|Failed|Cancelled, counts, bytes, error_summary }`.
     Each enforces the legal transition set (a match on current status inside SQL `WHERE status = ?` and check rows-affected) — makes an illegal transition an error, which is how "a failed backup must not be recorded as completed" is enforced at the data layer.
   - `latest_job(device_id)`, `history(device_id, limit)`.
2. `core::database::files`:
   - `record_verified(&Db, job_id, device_id, file: &FileRecord) -> Result<()>` —
     single transaction: upsert row (ON CONFLICT(device_id, source_path) DO UPDATE)
     **and** increment the job's counters atomically. `FileRecord` carries destination,
     size, mtime, sha256, timestamp; `status = verified`.
   - `mark_failed(&Db, job_id, device_id, rel_path, error) -> Result<()>` — same atomic
     counter behavior, `status = failed_transfer`.
   - `manifest_for_device(device_id) -> Vec<BackupFileRow>` — the engine's comparison input.
3. Invariant tests: a `finalize(Completed)` call when status is `failed` → error.

**Pitfalls**
- Counter increments must be `UPDATE ... SET files_copied = files_copied + 1` —
  not read-modify-write.
- `files_skipped` counts unchanged files; update it at job finalize time in bulk, not per file.

**Accept**
- Tests: duplicate (device, source) insert via ON CONFLICT updates, doesn't duplicate;
  forced mid-transaction error rolls back both row + counters; transition legality per
  the state table; "failed job cannot be marked completed".

## E02.4 Path sanitization

**Actions**
1. `core::paths` — pure module, zero I/O dependencies:
   - `sanitize_rel(source: &str) -> Result<RelPath, PathError>` — reject: absolute,
     any `.`/`..` component beyond leading normalization inside a single segment,
     control/NUL chars, Windows-reserved basenames (`CON`, `PRN`, `AUX`, `NUL`,
     `COM1`-`9`, `LPT1`-`9`, case-insensitive), trailing separators, empty.
   - `RelPath` newtype implementing `Display`; **converts to a `PathBuf` only via
     `RelPath::join_to(base) -> PathBuf`** which is itself re-checked (defense in depth).
   - `map_to_destination(base: &Path, rel: &RelPath) -> PathBuf` — deterministic,
     documented mapping phone-path → backup-path (device dest + rel segments).
2. `#[non_exhaustive] PathError` variants: `Traversal`, `Absolute`, `ControlChar`,
     `ReservedName(String)`, `Empty`, `InvalidChar(char)`.

**Pitfalls**
- Android paths use `/`; Windows comparison must be case-insensitive **for uniqueness
  decisions only, never for display** — record mtime/size from the phone instead
  and keep the source-side case as-is (don't lowercase user files).
- Filenames can end in spaces/dots on Android and are legal on Linux but not
  Windows: sanitize to a documented replacement convention (e.g. trailing `.` → `_`)
  and **record the mapping in the manifest** so re-direction stays deterministic.

**Accept**
- Table-driven tests: `../../../etc/passwd`, `C:\x`, `/sdcard/DCIM//x`, embedded
  NUL, `CON`, unicode names, names with spaces, trailing-dot name — each classified
  correctly; round-trip: sanitize → join → equals expected layout tree.

## E02.5 App config

**Actions**
1. `core::config` + platform app-dir from E00.2.
2. Serde TOML file `<app_dir>/config.toml`:
   ```rust
   pub struct AppConfig {
       pub adb_mode: AdbMode,           // Managed | System | Manual(PathBuf)
       pub global_backup_dir: Option<PathBuf>,
       pub poll_interval_secs: u64,     // default 4
       pub managed_adb_dir: Option<PathBuf>,
       pub config_version: u32,         // default 1 — for forward migrations
   }
   ```
3. `load()` → corrupt/missing → defaults + `tracing::warn!` + **do not rewrite** (only
   `save()` writes, and only explicit saves — never silently overwrite a manual ADB path, plan §4.1).
4. Backward-compat: unknown fields ignored (serde default), `config_version` bump
   procedure documented in module docs.

**Accept**
- Round-trip test; corrupt-JSON file → defaults + warning logged (capture via
  `tracing` test subscriber); explicit-save-only invariant (load never writes).

---

**Wave gate:** all migration + repo + sanitization + config tests green.
Next: [wave-3-device-discovery.md](wave-3-device-discovery.md).