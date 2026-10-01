# Wave 1 — ADB Execution Core

Up: [index](index.md) · Prereq: Wave 0 gate passed.
Clears blocks for: device discovery (W3), and everything downstream of it.

Design stance: **everything in this wave is behind traits** so Waves 2–5 and the
reliability suite (W7) can drive behavior with mocks instead of a phone.

## E01.1 ADB locator

**Actions**
1. `core::adb::locate::find_adb() -> Result<AdbFound, AdbLocateError>` where
   ```rust
   pub struct AdbFound { pub path: PathBuf, pub version: AdbVersion }
   pub enum AdbLocateError { NotFound, NotExecutable(PathBuf), InvalidOutput(String) }
   ```
2. Resolution order (stop at first success):
   a. Configured manual path (read app config; if wave 0 config module doesn't
      exist yet, accept a `Option<PathBuf>` parameter — config wiring lands in E02.5).
   b. App-managed dir `<app_data>/platform-tools/adb(.exe)`.
   c. `PATH` lookup (Windows: also probe common SDK locations? **No** — do not guess
      beyond PATH; keep resolution predictable, plan §4.1).
3. Validate by executing `adb version`; parse `Android Debug Bridge version X.Y[.Z]`
   into `(major, minor, patch)`.

**Pitfalls**
- On Windows, `adb.exe` lives next to `AdbWinApi.dll` / `AdbWinUsbApi.dll` — the
  locator should record the containing dir so the runner can set working dir if needed.
- `std::process::Command` on Windows resolves `adb.bat` too; prefer exact `.exe` matching.

**Accept**
- Unit tests using fixture scripts (`tests/fixtures/fake_adb.sh`, `fake_adb.bat`)
  simulating: success, exit-1, garbage output, not-found, found-but-schmod-x (unix only).
- Both OSes exercised in CI matrix.

## E01.2 Command runner with timeouts + error mapping

**Actions**
1. Trait (the seam every later wave mocks against):
   ```rust
   pub trait Adb: Send + Sync {
       fn run(&self, args: &[&str], timeout: Duration) -> Result<CmdOutput, AdbError>;
       fn serial_run(&self, serial: &str, args: &[&str], timeout: Duration)
           -> Result<CmdOutput, AdbError>; // prepends `-s <serial>`
   }
   pub struct CmdOutput { pub stdout: String, pub stderr: String, pub code: i32 }
   ```
2. `AdbError` variants — **the single pattern-matching table all later modules reuse:**
   - `Timeout`, `AdbMissing` (spawn error), `ProcessFailed { code, stderr }`
   - `DeviceUnauthorized` (stderr contains `device unauthorized`), `DeviceOffline`,
     `NoDevices` (stderr `no devices`/`device not found`), `DeviceNotFound { serial }`
   - `ServerError { msg }` (stderr contains `adb server version doesn't match`,
     `cannot bind`, `daemon failed to start` — record the raw message)
3. Implementation: `std::process::Command` (async conversion deliberately deferred —
   note this in the module doc; the trait API is conversion-friendly).
   Per-call timeout: wait on a thread or use `try_wait` loop; on expiry kill the
   process **group** (unix: spawn in new `setsid` + `killpg`; windows: `taskkill /T /F` —
   this belongs in the platform crate).
4. Encode the stderr/stdout pattern table as static data
   (`const PATTERNS: &[(&str, AdbError-fn)]`) so it is testable and greppable.

**Pitfalls**
- adb writes a lot of noise to stderr on `-s` commands for offline devices;
  match patterns in order of specificity.
- Never treat empty stdout as an error — many adb commands legitimately print nothing.

**Accept**
- Tests (mock process fixtures): timeout kill observed; each pattern variant
  produced by an injected fixture string → correct typed error.
- No direct `std::process::Command` calls for adb **outside** this module
  (grep-based test or review checkpoint).

## E01.3 ADB server lifecycle (reuse, never kill)

**Actions**
1. `adb_start` lazy-init: on runner creation, if `adb version` fails, run
   `adb start-server`; both are idempotent and reuse an existing server.
2. Explicitly **forbid** `kill-server`: add a unit assertion test that greps the
   `adb` module source for the literal `kill-server` and fails if found outside
   an explanatory comment (cheap, effective tripwire).
3. On `ServerError` from E01.2 patterns: return the actionable error upward; do
   not auto-restart the server (user may have a mismatched server from another
   toolchain — restarting it is a user-visible decision, surface via UI in W6).
4. Record detected server version; expose `server_info()` for the UI status line.

**Accept**
- Two consecutive `run` calls with a mocked server → second call makes **no**
  additional spawn (assert via call-counting mock).
- No `kill-server` occurrences in source (tripwire test).
- Version conflict string produces `ServerError` (pattern-table test).

---

**Wave gate:** `cargo test -p core` green; a smoke binary check
(`#[cfg(test)] mod smoke`) that, if a real adb happens to exist on the machine,
verifies locator+runner against it and is skipped otherwise (env-gated, not
CI-blocking). Next: [wave-2-data-foundation.md](wave-2-data-foundation.md).