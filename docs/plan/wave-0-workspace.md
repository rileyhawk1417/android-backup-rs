# Wave 0 — Workspace & Conventions

Up: [index](index.md). Nothing is blocked by anything; everything waits on this wave.
Do it well — every later task assumes these conventions are real and enforced.

## E00.1 Create workspace skeleton

**Actions**
1. Root `Cargo.toml`:
   ```toml
   [workspace]
   resolver = "2"
   members = ["crates/core", "crates/platform", "crates/desktop", "crates/cli"]
   ```
   Workspace-level `[workspace.dependencies]` for shared crates (rusqlite, serde, tracing, thiserror, anyhow, rfd, reqwest, eframe, sha2) so versions are pinned once.
2. Crates:
   - `core`: library, no `eframe`/`rfd` dependencies. Modules as **empty files with doc comments**: `adb`, `devices`, `backup`, `manifest`, `database`, `config`, `platform_traits`.
   - `platform`: library, cfg-partitioned code (see E00.2 note below).
   - `desktop`: binary crate depending on `core` + `platform`.
   - `cli`: placeholder binary `fn main() {}` with a comment that scope is deferred (plan §13).
3. `.gitignore`: `/target`, `*.partial`, `/*.local.toml`.
4. `rust-toolchain.toml`: stable channel, pinned.
5. LICENSE (choose MIT + Apache-2.0 dual, note decision in README stub).

**Platform partition note:** `crates/platform/windows/` and `crates/platform/linux/`
are module dirs guarded by `#[cfg(windows)]` / `#[cfg(unix)]` at the parent module
level — keep every OS-specific import inside its submodule so the other OS still compiles.

**Accept**
- `cargo build && cargo test` green.
- `crates/core` has **zero UI dependencies** in its `Cargo.toml` (this is the architectural invariant; verify by reading the manifest, not by trusting a comment).

## E00.2 Error handling & logging conventions

**Actions**
1. `crates/core/src/lib.rs`: `pub type Result<T, E = Error>;` and a top-level
   `core::Error` enum with `#[from]` variants delegating to module error types
   added later. Keep the top-level enum **thin** — module errors carry the detail.
2. Rule: `thiserror` in all core modules; `anyhow` only in `desktop`/`cli`.
3. `tracing` calls in core modules; subscriber configured in `desktop::main`
   (stderr + rolling file under the platform app-data dir; see platform crate).
4. `crates/core/Cargo.toml` lints (Rust 2024-era `[lints]` table):
   `clippy::unwrap_used = "deny"`, `clippy::expect_used = "deny"`, `clippy::panic = "deny"`.
   Test code is exempt via `#[allow]` inside `#[cfg(test)]` modules only.
5. Add `core::app_dirs()` (in `platform`) resolving log dir + config dir per OS.
   This is the first real use of the platform crate.

**Accept**
- A demo function in `backup` (temporary, or kept as doc example) logs through `tracing`,
  returns `Result`, and the failure path compiles without `unwrap`.
- `cargo clippy -p core -- -D warnings` fails if someone adds `unwrap()` in core (test by trying it locally, then removing).

## E00.3 CI plumbing

**Actions**
1. `.github/workflows/ci.yml`:
   - Trigger: push to main + all PRs.
   - Matrix: `ubuntu-latest`, `windows-latest`.
   - Steps: checkout → rust-toolchain action → `cargo fmt --check` → `cargo clippy --all-targets -- -D warnings` → `cargo test --workspace`.
2. Add a branch-protection note in README (main requires CI green) if repo settings allow.

**Accept**
- First green run on both OSes with at least one placeholder test per crate
  (write one trivial test per crate now — CI without tests passes trivially and
  proves nothing).

---

**Wave gate:** CI green on ubuntu + windows, conventions demonstrably enforced.
Proceed to [wave-1-adb-core.md](wave-1-adb-core.md).