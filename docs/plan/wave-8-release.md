# Wave 8 — Release

Up: [index.md](index.md) · Prereq: Wave 7 gate passed.
Goal: a user with **no Rust, no toolchain, no Android SDK** (plan §15) gets a
working binary for their OS from a GitHub Release, plus docs that get them running.

> **Status: implemented.** The scripts and the workflows exist; the acceptance
> record is [`../acceptance.md`](../acceptance.md), and the release procedure
> itself is [`../release.md`](../release.md). Three things the plan below asked for
> were changed on purpose while building it, each noted inline: the AppImage is
> built and shipped but demoted to a secondary, best-effort artifact; the
> Windows verification is CI-only (there is no Windows machine on this side); and
> the acceptance table lives in `acceptance.md` rather than `testing.md`.

## E08.1 Windows artifact

**Actions**
1. `.github/workflows/release.yml`, triggered on tag `v*`:
   - job `release-windows`: runner `windows-latest`, `cargo build --release -p desktop`
     → asset `android-backup-<version>-windows-x64-portable.zip` containing
     `android-backup.exe` (+ any required runtime dlls — none expected if using
     the x86_64-pc-windows-msvc default without GTK deps; eframe+winit is
     GPU/windowing but not redistributable-requiring).
2. Verify on a clean machine/VM: no MSVC redistributable prompt, launches, backs up
   via adb (managed install path exercises E07.1).
3. Signing decision (record, don't skip): MVP = unsigned + README/SmartScreen note;
   signing left open as an issue. **Decision recorded is the deliverable.**

**Accept**
- Artifact runs on clean Windows 10/11; portable zip contains only what's needed.

## E08.2 Linux artifact

**Actions**
1. job `release-linux`: pinned ubuntu build container → AppImage first
   (`linuxdeploy` + AppImageKit), embedding `adb`? **No** — adb is managed at
   runtime by E07.1; do not bundle platform-tools (licensing + staleness).
   *Built as asked, but with a pinned `appimagetool` rather than `linuxdeploy`, and
   demoted: the tarball is the recommended Linux artifact and a failed AppImage
   build is dropped from the release instead of blocking it (`release.md` §1).*
2. Confirm egui/winit runtime deps: with the standard winit + x11 path, required
   system libs are basic X11/GL ones; list them in AppImage metadata + docs
   (AppImage's point is to minimize these — verify with ldd on the binary and
   document every non-standard lib).
3. Optional secondary: `.deb` (defer — mark as follow-up task; do not build in the
   same pass unless it's trivial with `cargo deb`).
4. udev rules for adb (android-sdk-platform-tools-common package provides `51-android.rules`)
   — document exact rule steps for Ubuntu and Arch (Arch users: `android-udev` package).

**Accept**
- AppImage launches on clean Ubuntu and Arch VMs (no dev tools installed);
  ldd output reviewed; udev doc paths verified on both.

## E08.3 Publish + docs

**Actions**
1. Release job: on tag, gather artifacts → `softprops/action-gh-release` (or
   equivalent) → changelog from git log (conventional-commit-ish from task-ID
   commit messages).
2. Versioning: workspace `version = 0.1.0` bumped at tag time by release script
   (single source: root Cargo.toml; crates inherit).
3. `docs/setup.md`: per-OS install, first launch (adb install flow incl. E07.1),
   USB debugging enable steps (generic Android, call out Samsung paths),
   authorization approval, choosing backup location, first backup walkthrough.
4. `docs/troubleshooting.md`: device unauthorized / not appearing (udev!), wrong
   ADB version conflicts, destination drive missing during backup (recovery
   behavior), interrupted backup recovery, un-associating wrongly associated
   serials (manual DB note from E04.2).
5. Copy troubleshooting into README or link prominently — users hit these before docs.

**Accept**
- Non-technical-user dry run (someone else, or careful self-review as proxy)
  covers install → first backup with no gaps.

## E08.4 MVP acceptance run (final gate)

**Actions**
1. Execute plan §12's definition on BOTH platforms, from the published artifacts:
   - connect phone → approve debugging → select destination → select folders →
     back up → disconnect → reconnect → run again;
   - verify: no unchanged file re-copied (compare file counts/manifest),
     no previous copy lost (checksum spot-checks), history rows correct,
     changed file → history entry (E05.5), kill mid-backup → recovery clean.
2. Include the hardware matrix from W7's `hw-verify-needed` list: Samsung,
   non-Samsung, restricted dirs, external-drive pull-out, restart during backup.
3. Record results in `docs/testing.md`; any failure → append a numbered fix task
   to `index.md` progress tracker under "Post-acceptance fixes" and iterate until
   green. **This run is the project's definition of MVP-done (plan §16).**
   *Recorded in [`../acceptance.md`](../acceptance.md) instead, which `testing.md`
   §5 links: per-release acceptance is a different artifact from "which suites
   run". No failures, so nothing was opened under "Post-acceptance fixes".*

**Accept**
- `docs/testing.md` has a fully-checked acceptance table for both OSes with
  checksums/hashes/counts recorded. Plan is complete; defer W9+ (plan §13) until
  separately scheduled.

---

**Done criteria for the whole plan:** E08.4's table is green for Windows and Linux.
Until then, the wave order stands; after it, new work starts from newly written
wave docs (scheduling, notifications, restore — plan §13) with their own blockers.