# Wave 9 — Automation: schedules, auto-backup on connect, tray background

Up: [index.md](index.md) · Prereq: Wave 8 gate passed (0.1.0 released).
Goal: backups happen without anyone pressing a button — the user plugs the phone
in (or the clock hits the interval) and the computer does the rest, quietly
notified. The app keeps running in the background as a **system-tray
application**; there is no separate daemon.

> **Scope decisions (recorded 2026-10, with the maintainer):**
>
> - **Scheduling and auto-backup on connection are the must-haves.** Scheduling
>   means *interval-based* backup ("every N hours while the phone is connected");
>   trigger-on-connect is the second mode. A cron-like calendar scheduler is
>   over-engineering for a phone-backup app and is not planned.
> - **Background = one process, resident in the system tray.** Close hides to
>   the tray, notifications report what happened without the window. No IPC, no
>   daemon, no second copy of the manifest/adb logic to keep honest.
> - **Windows is the primary target.** The tray app is the normal Windows
>   experience (tray always present, toasts always available). Linux tray is
>   best-effort: it works on KDE/Xfce/Cinnamon/most WMs, and vanilla GNOME shows
>   no icon without an AppIndicator extension — a known limitation we document
>   in one line, not a blocker.
> - **Interrupted backups resume themselves.** A cable pulled mid-copy pauses
>   the job; when the phone is ready again the paused job continues from the
>   remainder (the engine already re-verifies what landed). See the ownership
>   rule in E09.2, which is what makes this safe rather than surprising.
> - **Restore is explicitly NOT in this wave.** This app is a one-way street:
>   phone → PC. Restore is noted in §13-deferred at the bottom of this doc; the
>   engine never pushes to the device, and nothing here changes that.
> - The remaining §13 items (encryption, filters, retention, network ADB, CLI…)
>   stay deferred until separately scheduled.

## The problem this wave must not create

Automation is where backups get *dangerous* in the other direction: jobs start
at awkward moments (battery, roaming, a flaky cable that reconnects every few
seconds), fail in ways nobody sees (the toast was missed), and fight with
manual jobs. Every task below answers one question: **who is allowed to press
the user's buttons, and when.**

Rules that hold across the whole wave:

1. **One job per device, ever.** The existing `StartError::DeviceBusy` rule is
   load-bearing here: an automatic trigger that finds a live job *does nothing*
   and does not queue a retry. The manual user always wins.
2. **A trigger fires an *attempt*, not a run.** The authorization gate
   (`devices::backup_gate`) decides, as it does for the Start button; a device
   that fails the gate produces at most one notification about why, not a
   notification loop.
3. **Automation is opt-in per device.** The 0.1.0 behaviour — nothing runs
   until the user says so — remains the default. The wizard may *offer* the
   toggle, not assume it.
4. **Everything the automation does is visible in the window and the manifest.**
   An automatic job is a normal job row; the history panel is the audit log.
   Jobs also record **who started them** (`trigger`: `manual` | `auto`), because
   the resume rule in E09.2 turns on that distinction — and an audit log that
   cannot tell a machine's work from a person's is not one.

## E09.1 Schedule model (core) — **implemented**

**Actions**
1. New module `core::automation::schedule`: a small enum that carries the
   user's intent and a pure function `due(&self, last_run: Option<Instant>,
   now: Instant) -> bool`:
   - `Off` — never automatic (the default; 0.1.0 behaviour).
   - `OnConnect` — due as soon as a device enters the ready state.
   - `EveryHours(hours: u32)` — due when ≥ `hours` have passed since the
     device's last *successful or attempted* job start (attempted, not
     completed: a failing backup must not retry itself into an infinite loop).
2. Config: `AppConfig` gains `automation: AutomationConfig { default_schedule }`
   — the machine-level default that *seeds* a device's own setting (the wizard
   checkbox, the settings panel's starting value). It is a default, not a
   second switch: once a device has a schedule of its own, that is the truth.
   Config version stays 1 with `#[serde(default)]` on the new field; old
   config.toml files must load unchanged (add one round-trip test proving it).
3. Per-device setting lives with the **profile** (it is per-device user intent,
   and the manifest already owns profile rows): migration to version 2 adds
   `auto_schedule_kind` (`off` | `on_connect` | `every_hours`) and
   `auto_schedule_hours` to the device row, defaulting to `off`, **and**
   `trigger` to the jobs table (`manual` | `auto`, default `manual` for existing
   rows — history must not retroactively claim the machine did work a person
   did). Migration tests prove a v1 database upgrades in place with all rows
   kept. The stored form is the schedule itself, not an "enabled" boolean: a
   flag plus a separate interval is two sources of truth for one decision, and
   such a row can say "on" with nothing to run on. A kind or interval that
   cannot be read means `off` — bad data may only ever *disable* automation.
4. `next_due(last_run, schedule, now) -> Option<Instant>` for the UI ("next
   automatic backup in 2 h 05 m") — pure, fuzz-free but property-tested for
   monotonicity (`now` advancing never moves the answer backwards).

**Pitfalls**
- Do not let the schedule tick on wall-clock *calendar* time ("at midnight") —
  that reintroduces the cron mistake and a whole timezone/battery discussion.
  Elapsed time since last attempt is the whole model.
- `Instant` vs `SystemTime`: across-suspend, `Instant` may stop on Linux; if
  tests show that matters, store last-attempt wall time in the manifest jobs
  table (it already has the timestamp) rather than in config.

**Accept**
- Unit tests: every schedule variant's `due` truth table; config round-trip
  with and without the new field; migration v1→v2 keeps rows and adds the
  default; monotonicity property.

## E09.2 Auto-backup on connection (core) — **implemented**

**Where it lives:** `core::automation::policy`. The decision function is pure:
everything it may touch is behind the `Host` trait (`schedule`, `last_attempt`,
`paused_job`, `start`, `resume`), which the app implements over the manifest,
the gate and the job pipeline. One consequence, and the reason the trait
exists: **the whole rulebook is tested against a scripted timeline with no
database, no phone, no running job and no thread** — the tests in that module
are the *Accept* list below, spelled out one scenario each.

**Actions**
1. `Policy::on_event(&dyn Host, &DeviceEvent, now) -> Vec<Note>` turns
   **`FirstSeen` / `Connected` / `Unauthorized` "became ready" events** into at
   most one *attempt*. `Policy::tick` does the same for the clock (E09.3), and
   only for devices that really are `Connected`:
   - The device's own schedule decides, first thing (E09.1's per-device
     `Schedule`): `Off` → nothing, ever — not even the resume rule. One switch,
     one meaning. A serial the host does not know answers `Off`, so an
     unenrolled phone is untouched by construction.
   - The gate is asked through `Host::start`/`Host::resume`, i.e. *the attempt
     itself is gate-checked*, exactly as the Start button is. The policy never
     inspects readiness on its own; a refusal comes back as
     `AttemptError::NotReady(<the gate's words>)` and is *reported once per
     connection episode*, then silent until the state changes again — the
     episode boundary (a `Disconnected` / new `FirstSeen` pair) re-arms the
     one-shot explanation.
   - The schedule decides whether this connection is *due*: `OnConnect` is due
     immediately; `EveryHours` is not, unless the time has really passed. The
     manifest's last job row — whoever started it — is the clock, so a manual
     backup five minutes ago also satisfies "every six hours".
2. The attempt goes through the same entry point as the button. The three
   answers are deliberately distinct, because they want three different
   reactions:
   - `Busy` → silence: a manual job is running, and automation must not cancel,
     not queue, not notify — the user is already looking at it. For
     `OnConnect` this also *consumes* the plug-in's run: the user's own backup
     **is** "backed up when I plug in", so automation does not come back later
     with a second one.
   - `NotReady(reason)` → one `Note::Blocked` per episode and **no cooldown**:
     nothing was attempted, and the user allowing USB debugging or plugging in
     the destination drive has to take effect immediately, not two minutes
     later.
   - `Failed(reason)` → one `Note::Failed` per episode, plus the cooldown below.
3. **Cooldown on flaky connectivity:** an attempt that begins within
   `FAILURE_COOLDOWN` (120 s) of the previous **failed** attempt for the same
   serial is skipped. That is the cable flap loop (connect → start → fail →
   disconnect → reconnect …) made cheap: the flapping produces no retries and,
   because explanations are rate-limited too, no notification loop. The clock is
   per device and *survives the unplug* — a cooldown that reset with the
   appearance would never apply to the one case it exists for. It clears on a
   successful start, and a separate `MAX_ATTEMPTS_PER_APPEARANCE` (3) budget
   stops a device that stays plugged in and broken from being retried forever.
   (Notifications are limited independently by `EXPLANATION_INTERVAL`, because a
   phone flapping between unauthorized and connected re-appears every few
   seconds and each appearance would otherwise be entitled to its own
   explanation.)
4. **Reconnect handling — the resume rule.** When a device becomes ready and a
   job for it is sitting **paused**, a reconnect is a *resume*, not a new job:
   - **job `trigger = auto`** → the automation calls `resume()`, which is itself
     gate-checked. This counts as the attempt for this connection episode: no
     second job row, no `Busy` no-op. A notification says so ("Resuming
     <Device> — continuing where it stopped"), so it is not silent.
   - **job `trigger = manual`** → stay out, silently. The user pressed Start and
     then the cable came out; the panel's Resume button is still theirs to
     press. A user-owned job that the automation silently drove to completion
     would be exactly the surprise rule 3 exists to prevent.
   - Either way the engine's pause semantics are untouched — the control API is
     still the only thing that resumes anything; the automation is just a
     caller that has been granted standing for its own jobs.
   - **Bounded, so a bad cable cannot loop:** resumes obey the same cooldown as
     starts, and a job that re-pauses `MAX_RESUMES_PER_JOB` (2) times stops being
     auto-resumed: it raises one `Note::NeedsAttention` and waits for the user.
     **The budget is per *job*, not per connection episode** — the reconnects
     that break a job are exactly what ends an episode, so an episode-scoped
     count would reset itself and loop forever. A different paused job starts
     with a fresh budget.
   - No extra setting for this: resume-on-reconnect is on exactly when the
     device's auto-backup is on. Auto-backup off means nothing automatic at all,
     including this.

**Pitfalls**
- The watcher emits events inside its own thread; never call `start` (which
  takes locks) inline in a way that could deadlock with the poll loop — hand
  the attempt to a short-lived worker or a queue, the way the UI does. That is a
  constraint on the `Host` implementation, so it is written into the trait's doc
  comment, where the implementor will read it.
- Do not treat `FirstSeen` as consent: a phone that is *seen* while
  unauthorized must never become a job that sits paused with no user watching.
  Gate-check first, attempt second.
- Keep the policy object free of device *rows* (serials only), so it can be
  tested against a watcher timeline without a database. It holds counters, a
  per-job resume budget and two timestamps; an entry is dropped when a device
  unplugs with nothing left that has to outlive the unplug.
- A stored timestamp that lands in the *future* — a laptop whose clock was wrong
  and has since been corrected — must read as "just now", never as "never ran",
  or a clock skew becomes a backup on every tick. `Schedule`'s saturating
  arithmetic is where that is handled, which is why `Host::last_attempt` hands
  back an `Instant` and not an age: it keeps "a moment ago" and "an hour from
  now" distinguishable.

**Accept** — all green in `policy`'s own test module, each line one scenario,
driven by a `FakeHost` that is honest about what a real host does (a successful
start moves the manifest clock, a successful resume un-pauses its job):
- Connect → one automatic job; the next fifty ticks are silent; re-plug → a
  second run. `OnConnect` means once per plug-in, not once per tick.
- Unauthorized connect → one explanation, zero jobs, nothing re-fires until the
  user allows it — and then the backup the refused attempt never consumed
  happens at once.
- Flaky-cable timeline → one attempt in the window, not eleven; a device that
  stays broken gets three attempts per plug-in and then stops; a phone that
  flaps while unauthorized explains itself once, not once per flap.
- Manual job live → busy, silent, not queued behind. A paused *manual* job →
  never touched. A paused *auto* job → the same job resumes (assert: the control
  API was called and no start was attempted).
- An auto job that re-pauses twice → the third reconnect only raises "needs your
  attention", exactly once. A different job id gets a fresh budget.
- Auto off / unknown serial → the whole module is inert. A repeated `Connected`
  event, a `PollFailed` or a `StateChanged` one changes nothing. A timestamp in
  the future schedules nothing until the clock catches up.

**Wiring (delivered with E09.5)**
- `desktop::automation::AppHost` implements `Host` over the manifest, the gate
  and the `BackupController`; it starts and resumes on the driver's thread and
  never blocks the watcher. `desktop::jobconfig` is the one place that turns a
  `DeviceRow` into a `JobConfig`, so the button and the policy cannot disagree
  about destination or folders.
- `crates/core/tests/automation_resume.rs` closes the second gap: the real
  `Policy` drives the real controller through an interrupted copy and a
  reconnect. The resumed job is the *same row* (`trigger = auto`, one job in the
  history), the file already verified before the slip is not rewritten, and the
  bytes copied after reconnecting are exactly `b` + `c`. Its companion test pins
  the other half of the rule: a *manual* paused job is left for the person who
  started it.
  - Worth knowing about this job's ledger: `files_copied` counts the job's whole
    life (1 before the slip + 2 after), and `skipped` counts what *earlier* jobs
    left behind — so "only the remainder" is proven by the byte delta and the
    untouched file, not by `skipped`.

## E09.3 Interval scheduler driver (core) — **implemented**


**What landed.** `core::automation::driver`: an `AutomationDriver` on one
stoppable thread (a `Condvar` wait, joinable within a tick), a `TickHost`
extending `Host` with `connected()`, and a `tick_once` that takes the policy lock
once and then *waits*, so ticks cannot pile up behind a slow gate. The interval
comes from the config (`automation.tick_interval_secs`, default 60 s, floor 1 ms
so a test cannot spin). The device list is the watcher's own
`DeviceSnapshotHandle`, read fresh every tick — nothing about enrollment or
connection state is cached in the driver.
**Actions**
1. New module `core::automation::driver`: one thread, owned by the app,
   ticking once a minute (cheap; configurable in tests):
   - For each enrolled device (auto-backup on): if the schedule is due, run the
     same gate → attempt pipeline as E09.2 (shared code — the driver is
     *only* the clock; the policy is E09.2's).
   - Ticks do nothing when no device is connected — no busy wake, no log spam.
2. The driver must survive the device list changing under it: enrollment is
   re-read from the manifest each tick (a new device profiled mid-run becomes
   eligible without a restart).
3. Start/stop lifecycle for tests and for the tray "Pause automation" menu
   item is a condvar-stop like the watcher's, joinable within a tick.

**Pitfalls**
- Do not cache "due" inside the driver: the manifest's job history is the
  clock's partner. Recompute from last-job rows each tick; a clock that
  *remembers* becomes a second, divergent source of truth.
- A tick must be bounded: if `gate()` on a dead adb blocks for 30 s, ticks
  must not pile up (guard with a "tick in progress" flag; skip, don't queue).

**Accept**
- A mock-clock scenario test: device connected, `EveryHours(1)`; two ticks at
  t+59 m do nothing; t+60 m starts exactly one job; next tick finds
  `DeviceBusy` (job still live) and stays silent; job completes; 60 m later
  the next fires. Same test with the device unplugged: every tick is a no-op.

## E09.4 OS notifications (desktop) — **implemented**


**What landed.** `desktop::notifications`: a `Notifier` trait with `Notice` /
`NoticeKind` (started, finished, paused, fatal, blocked, attention), a `Delivery`
policy (progress toasts are suppressed while the window is focused and not
minimized; paused/fatal are never suppressed), per-device-per-kind coalescing on
a 60 s window, and a `SystemNotifier` whose `show` is non-blocking (worker thread,
bounded channel, 300 ms minimum gap) so a toast can never slow or fail a job.
`RecordingNotifier` in `testutil` asserts payloads head-lessly. The mapping from
a `JobEvent` to a `Notice` is the pure `job_notice` function, and
`AutomationToasts` bridges policy notes to toasts, resolving display names from
the manifest. Windows carries the `AndroidBackup.Desktop` AppUserModelID;
Focus Assist behaviour is documented in `troubleshooting.md` §17.
**Actions**
1. `desktop` (this is a presentation concern — the view model gains the
   decision, the OS crate does the showing):
   - `completed (n files, x skipped, y kept)` per finished job;
   - paused with the user-facing reason (the `PauseReason` text, unmodified —
     same wording as the panel, from the same source);
   - fatal error (one, distinct, quiet-able);
   - automation-specific: an auto-started job announces "Backing up <Device>"
     so a phone that starts chattering on plug-in is explained.
2. **Windows toast specifics** (this is the platform being built for; the Linux
   tray caveat above is the exception, not the rule): register an
   AppUserModelID so toasts carry the app's real name and icon instead of a
   generic host process, and note Focus Assist behaviour in the docs — a
   suppressed toast is not a lost backup, the panel and the tray badge still
   tell the truth.
3. Crate: one notifications module, one implementation per OS (e.g.
   `notify-rust` on Linux via DBus, the Windows toast bridge on winrt), behind
   a small trait so tests assert on the *payload* with a fake sink — and so a
   machine without a notification daemon still passes CI.
4. Delivery policy (kept deliberately simple, written in the module doc):
   - notify always when the window is hidden (that is the point of the tray);
   - suppress success notifications while the window is visible (the panel is
     already on screen; a toast over your own app is noise);
   - never suppress paused/fatal: those need a witness even on screen.
5. All notification kinds share one "why is backup stuck" wording rule: the
   text comes from the same string source the panel renders, so the toast and
   the panel never disagree.

**Pitfalls**
- Notifications arrive from worker threads; rate-limit per kind (an auto job
  that finishes and immediately pauses must not race two toasts in one second
  — queue or coalesce).
- A notification that *fails* (no daemon, Windows Focus Assist) must log and
  return; it must never propagate into the job path. It is decoration on top
  of an already-visible state, not a dependency of it.

**Accept**
- Payload tests with the fake sink: every event type produces the exact text
  (assert the pause reason survives intact, including the named file);
  visible-window suppression; coalescing of back-to-back events.
- Headless UI tests already render every pause/fatal state — the toast is the
  same event, second channel; no new view-model states are allowed to appear
  that tests do not cover.

## E09.5 System tray & close-to-tray (desktop) — **implemented**


**What landed.** `desktop::tray` draws its three icon states in code (idle /
running / attention — no PNG assets to ship or regenerate) and puts `tray-icon`
+ `muda` behind a `TrayHandle` trait, so menu behaviour is tested against a
recording sink. Menu clicks become `TrayCommand`s and then the same `Intent`s the
window widgets emit; the wiring also carries an `egui::Context` so a click on a
hidden window still produces a repaint. Close-to-tray is the default
(`background.close_to_tray`), the tray is best-effort (a machine that cannot make
one still runs with its window), and `--hidden` serves the run-at-login entry.
Autostart lives in `desktop::autostart` (Windows Run key via `reg.exe`, Linux
`.desktop` file) and is reconciled from the config at startup — both through
`core::fsx::write_atomic`, so a crash cannot leave a truncated entry.

**Two decisions worth recording.**
- **Quit cancels live jobs.** Stopping the driver and joining the job threads is
  not enough: a live job left behind would be recorded as *interrupted* and need
  recovery on the next launch, which is exactly the surprise a Quit should not
  create. `stop_background_work` now cancels cooperatively (the existing control
  API) before joining, and a regression test asserts the row ends `Cancelled`
  with `unfinished_jobs()` empty.
- **Single instance is a lock file, not a platform crate.** `instance.lock` beside
  the manifest, taken with `File::try_lock` (stable since 1.89), is one
  implementation that covers Windows and Linux and keeps the manifest's writer
  unique — the property that actually matters. A second launch says the app is
  already running and exits; it does not try to wake the first one over IPC (see
  the tradeoff note in the module).

**Actions**
1. `tray-icon` (+ `muda` for the menu; `winit` event loop integration through
   eframe's raw events) — one module `tray.rs`, which owns the icon, the menu
   and the event channel; the menu emits `Intent`s through the same queue as
   the window widgets, so the app's command path is unchanged.
   - Menu: **Open** (show/focus window) · **Back up all connected now** ·
     **Pause/Resume automation** · **Quit**.
   - Icon states: idle / *job running* (accent) / *paused-attention* (badge) —
     three small PNG assets, generated (scripts), stored in `assets/`.
2. **Close = hide to tray** (default on, a setting): the window hides, the
   event loop keeps running, the watcher/automation keep working. **Quit** is
   the only exit; it stops the driver, cancels live jobs (cleanly, via the
   existing control API), joins threads, and only then exits.
3. Single instance: a second launch must not silently run two apps sharing one
   manifest and one adb server. Use the established platform mechanism
   (`platform` crate: a named mutex on Windows, a lock file with PID + a named
   check on Linux); the second process shows a message (and can open the first,
   where trivially possible) then exits. The same trait also carries
   **autostart** ("run at login": Run key or Startup folder on Windows,
   `~/.config/autostart` entry on Linux), which is what makes the tray app
   actually present when a phone is plugged in.
4. No display available (the 0.1.0 headless case) is unchanged: tray icon is
   best-effort; failure to create it logs and the app *still runs* with the
   window — the tray must not be load-bearing.

**Pitfalls**
- **Linux:** the tray needs `libayatana-appindicator` at runtime (KDE, Xfce,
  Cinnamon, most WMs OK); **vanilla GNOME shows no tray icon at all** without
  an AppIndicator extension. One line in `setup.md`, no more: Windows is the
  target platform, and on Linux the app must still behave sanely without the
  icon (notifications reach the user either way).
- CI's Linux job needs `libayatana-appindicator3-dev` (or the build must
   feature-gate the tray) — update `.github/workflows/ci.yml`'s apt list,
   which is exactly the edit the index warned was coming for the GUI build.
- Tray menus fire events from threads the eframe loop doesn't own: route
  through a channel drained each frame (the `Intent` queue already exists for
  exactly this shape).

**Accept**
- Headless tests: menu intents map to the right commands (fake tray sink);
  close-to-tray hides the window and *does not* drop the watcher (assert the
  poll still drives events); Quit cancels a live job and the manifest rows are
  consistent afterwards (the E05.6 kill-recovery suite re-run through the Quit
  path).
- Architecture test extended: tray/events code touches no `rusqlite`, no
  hashing — same wall that keeps the UI logic-free.

## E09.6 Settings & status UI (desktop) — **implemented**

**What landed.** The panel's schedule row is the single place a device's automation
is decided (Off / On connection / Every N hours, 1–72, `DragValue` + radio rows), and it
persists through `ProfileService::set_auto_schedule` on the click — no Save button, no
unsaved state. The status line is a pure function of the manifest row, the clock and
the pause flag (`view::auto_status_line`), so "Next automatic backup in 2 h 05 m",
"starts when … is connected", "an automatic backup is due now" and "automatic backups
are paused" are testable strings rather than a screenshot. The wizard's checkbox writes
`default_schedule` *and* pushes it into the running watcher's sink, so a phone
discovered after the box is ticked — by a watcher spawned before it — is enrolled;
only rows that do not exist yet ever hear that answer, so a device the user later
turns off stays off.

A **Background** dialog (the status bar's button) is the second half of the settings,
and the only part of the window that is about the application rather than a device:
whether closing hides the window or quits, whether the program starts at login, and a
**Quit now** button. The quit button is not decoration — with a tray icon the window's
own close button hides instead of quitting, so without it the tray menu would be the
only way out of the program, which is not something to ask of a user who has just
turned automation off. The dialog reports what the *desktop* did about the login entry
(`run at login: enabled` / `could not be changed (…)` / `not available (…)`) rather than
what was asked for, keeps the user's wish in the configuration when the machine refuses
it, and distinguishes "no tray on this desktop" from "turned off in the configuration
file" instead of blaming the desktop for a setting.

**One decision worth recording.**
- **"Away" is not "blocked".** An unplugged device with a schedule is a *waiting*
  state, so the line says when it will next run rather than inventing a fault to
  fix; only a gate that actually refuses (unauthorized, no destination, paused)
  speaks up with the gate's own words.

**Actions**
1. Device detail panel: an **"Automatically back up when…"** control — Off /
   On connection / Every N hours (`N` editable, sane bounds 1–72) — persisted on
   change with the profile (`devices.auto_schedule_kind` / `_hours`, the pair
   E09.1 added; the config's `default_schedule` only pre-selects it).
2. The schedule line says the truth about what will happen ("Next automatic
   start in 2 h 05 m" / "starts when <Device Name> is connected" / "automatic
   backup is off"), including *why nothing is scheduled* when the gate would
   refuse (unauthorized → say so; that is E02-style plain language, and it
   comes from the same gate strings).
3. Tray "Pause automation" reflects and drives the same paused flag the driver
   uses — the settings panel and the tray cannot disagree.
4. Wizard: offer auto-backup on connection as an optional checkbox at the end
   (default off) — new devices get a sensible opt-in, old behaviour is zero-click.

**Accept**
- ui_headless tests: control renders, toggles persist through a restart of the
  view model, status line text for each state, wizard checkbox appears and
  defaults off.

## E09.7 Docs, packaging, release (wave gate) — **implemented**

**Actions**
1. Docs: `setup.md` gains "Background & automatic backups" (tray, why closing
   is not quitting, how to actually quit, run-at-login; the one-line GNOME tray
   caveat); `troubleshooting.md` gains "my phone keeps backing up / won't back up
   automatically / it stopped half-way and did not continue" (gate checklist in
   user language, and what the resume rule does and does not do); README's status
   line and feature list updated ("Wave 9 of 9" → this wave is the last).
2. Packaging: Linux `.deps.txt` gains the tray runtime dep; the tarball
   install.sh prints the optional package; Windows needs nothing new.
3. Release: version to 0.2.0, CHANGELOG filled from this wave's commits,
   release notes highlight auto-backup + tray. Tag `v0.2.0`.
4. Acceptance record: `acceptance.md` gains a Wave 9 section mirroring §2's
   honesty rules — automation logic is PASS (simulated) via the scripted
   harness, tray/UI PASS where a machine can check, and the real-phone +
   real-Windows observation rows stay PENDING with §7 procedures:
   plug the phone in at the scheduled interval; flap the cable; pull the cable
   mid-backup on Windows and confirm the tray app resumes it on reconnect;
   close-to-tray and quit mid-job; a notification arrives while the window is
   hidden.
5. Restore, as agreed, is *noted down only* — see the deferred section below.

**Accept (wave gate)**
- `cargo fmt --check && cargo clippy --workspace --all-targets -D warnings &&
   cargo test --workspace` green on both CI OSes.
- No new PDB/dbg assertions, no stub that pretends to schedule: every behaviour
  above is covered by at least one automated check on this machine.
- The release pipeline still builds and the Windows ZIP / Linux tarball still
  smoke-test (the same scripts, updated for the new runtime dep).

---

## Deferred (noted down, out of scope, on purpose)

**Restore / backup browsing** — plan §13 keeps it; the maintainer's direction
for this product is one-way (phone → PC). Nothing in waves 0–9 added a `push`
to the device, and nothing here shall. If a restore is ever built it is its
own wave: verify-before-overwrite is a *new* correctness problem (the
manifest's history directory is the obvious source, and the existing
size-mtime-hash comparison engine is reusable in reverse), and the "never
delete anything on the PC" rule must be restated as "never delete anything on
the *phone*" first.

Everything else in plan §13 (encryption, filters, retention, network ADB,
multi-profile, CLI, config export, integrity tools) stays deferred until
separately scheduled.