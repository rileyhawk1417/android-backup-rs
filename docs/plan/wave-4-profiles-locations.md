# Wave 4 — Profiles, Locations, Folders

Up: [index](index.md) · Prereq: Wave 3 gate passed.
Clears blocks for: **the backup engine (W5)** — this is the last full wave of
prerequisites before the engine. After this wave passes its gate, Wave 5 has
zero external dependencies left.

## E04.1 Display names & rename

**Actions**
1. Service layer on top of E02.2 (keep repos dumb: services batch + validate):
   `core::devices::profile::ProfileService { rename(device_id, new) -> Result<()> }`.
2. Validation: trims, non-empty, reasonable max length (256 bytes), rejects
   path-hostile names — the name becomes part of filesystem layout (see E04.3):
   reject `/ \ : * ? " < > | ..` leading/trailing dot-space. Reuse E02.4 helpers.
3. Name changes: allowed any time; history is keyed by `device_id`, so nothing
   else re-points (assert below).

**Accept**
- Repo + service tests: rename round-trip; hostile name rejected; a completed
  job rows' `device_id` unaffected before/after rename.

## E04.2 New-serial policy (plan §5.2)

**Actions**
1. Default: unknown serial → **new profile** (E03.4's upsert already does this —
   good, the safe default is automatic).
2. Explicit association only:
   `associate_device(alias_serial, existing_device_id, now) -> Result<()>`:
   - insert into `device_aliases`; **never rewrites old rows**;
   - optional: display-name inheritance if alias device unnamed.
3. Service call surfaces to UI in W6 as a confirmation prompt ("New serial seen.
   Associate with existing profile?") — build the service, defer the dialog.

**Pitfall**
- Association is one-way and permanent in MVP (no un-associate; plan §13 deferred).
  If two devices were wrongly associated, the resolution is manual editing of the DB —
  document that in troubleshooting docs at E08.3, don't build the un-associator now.

**Accept**
- Tests: new serial → separate history untouched; after association, subsequent
  `resolve_active_device(alias)` returns existing profile; original history rows byte-identical.

## E04.3 Destination selection model

**Actions**
1. `core::backup::destination::resolve(device_row, config) -> Result<EffectiveDestination>`:
   - precedence: per-device `destination_path` (explicitly set) → `global_backup_dir/<display_name-santized>` → error if neither global dir nor per-device set.
   - returned struct: `{ root: PathBuf, subfolder_from_name: bool }` — so history views can show what layout applies.
2. **Future-only rule:** there is **no** function that deletes or moves anything
   under an old destination. Enforce with a review checkpoint + test: start a job
   with destination A, change to B mid-scan (job holds its own resolved copy),
   complete → files landed under A; new job lands under B.
3. Name-in-path uses E04.1's sanitized name; if a user renames later, the folder
   on disk is NOT renamed (documented; renaming backups dirs is out of scope).

**Accept**
- Precedence matrix tests (4 cases); rename-then-run keeps old folder name;
  mid-job destination change does not affect the running job (test with the mock runner).

## E04.4 Destination validation

**Actions**
1. Abstract fs probing so it's testable:
   ```rust
   pub trait FsProbe: Send + Sync {
       fn exists_create(&self, path) -> Result<(), DestError>;   // create-if-missing
       fn writable(&self, path) -> Result<(), DestError>;        // probe file create+delete
       fn free_bytes(&self, path) -> Result<Option<u64>, DestError>; // None = unknowable
   }
   ```
2. `DestError` variants (UI-consumable, actionable):
   `MissingDrive`, `NotWritable`, `CannotCreate`, `InsufficientSpace { needed, available: Option<u64> }`.
3. `validate_destination(probe, path, estimated_needed: Option<u64>) -> Result<SpaceInfo, DestError>`
   — runs all checks in order, short-circuits on hard failures; free space < needed
   (if estimable) → `InsufficientSpace`.
4. Platform impls: Windows `GetDiskFreeSpaceExW` (winapi/windows-sys crate), unix `statvfs`
   (nix crate) — implemented in `crates/platform`, trait defined in `core`
   (dependency arrow: core ← platform, never the reverse).
5. Mid-backup destination loss detection: the transfer engine (E05.3) maps
   write/spawn errors with these patterns to `DestError::MissingDrive` —
   the pattern table gains two entries; note them here so W5 implements them.

**Accept**
- Mock-probe tests: each `DestError` variant; create-if-missing does create;
  probe file removed after check (no litter).
- Platform `FsProbe` compiled + `#[cfg]`-gated smoke (real `/tmp` / `%TEMP%` write) —
  hardware-independent by nature.

## E04.5 Folder-selection model

**Actions**
1. Seed defaults on device profile creation (E04.1 service extension):
   ```rust
   const DEFAULT_FOLDERS: &[&str] = &[
       "DCIM", "Pictures", "Movies", "Music", "Documents",
       "Download", "Android/media", "Recordings", "Screenshots",
   ];
   ```
   All enabled by default (user unchecks). `Screenshots` stays distinct even
   though it often overlaps Pictures — dedup (E04.6) makes overlap harmless.
2. Repo methods: `toggle(device_id, source_path, enabled)`, `enabled_folders(device_id)`,
   plus `set_folders(device_id, Vec<(String, bool)>)` for bulk UI save.
3. `source_path` stored as the phone-side absolute path (e.g. `/storage/emulated/0/DCIM`).  Hmm — plan says shared storage; the storage root is device-dependent. Resolve at enumeration time: folders are stored **as given**; the engine resolves against the detected storage root (E05.1 stores the root it used in the job row? no — keep simple: store phone-absolute paths and re-resolve each job).

**Accept**
- Tests: seeding exactly once (second creation call is a no-op, not duplicates —
  ON CONFLICT DO NOTHING); toggle persists; enabled-set query correct.

## E04.6 Enumeration dedup + custom directories

**Actions**
1. `core::backup::selection::merge_configs(enabled_folders, custom: Vec<String>)
   -> Result<Vec<String>, SelectionError>` —
   - normalize: strip trailing `/`, dedupe exact matches,
   - flag hierarchical overlaps for the *engine's* dedup (e.g. `DCIM` and `DCIM/Camera`
     → engine enumerates parent, child is redundant: implement `is_covered_by(a, b)`
     pure function, test it thoroughly — this is where `Screenshots ⊂ Pictures` gets
     handled even if a user adds `Pictures/Screenshots` manually).
2. Custom path validation: `validate_custom(runner, serial, path) -> Result<(), SelectionError>`
   — dry-run `shell ls <path>` via E01.2; `ls: … Permission denied`/`No such file` →
   typed error with the adb stderr attached. Never save unvalidated.

**Accept**
- `is_covered_by` matrix tests incl. equal-path, parent/child, sibling,
  trailing-slash variants; custom-path failure via mock runner surfaces stderr.

---

**Wave gate:** all tests green; enumerate that W5 needs only `FsProbe`,
`resolve_active_device`, `enabled_folders`, comparisons, runner — i.e. **no
unfinished items from any wave**. Verify by grepping W5's planned imports.
Next: [wave-5-backup-engine.md](wave-5-backup-engine.md) — the critical wave.