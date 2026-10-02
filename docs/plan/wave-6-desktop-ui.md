# Wave 6 — Desktop UI

Up: [index.md](index.md) · Prereq: **Wave 5 hard gate passed.**
The UI is a *subscriber*: it renders state and forwards intents. It holds no
backup logic (the W0 architectural invariant is checked again at this wave's end).

egui/eframe specifics to respect throughout:
- The app owns a shared state struct; background threads send data via channels;
  the UI **polls channels and calls `ctx.request_repaint()`** when events arrived.
- Never block in UI callbacks (no `.recv()` without timeout) — use `try_recv`
  loops drained each frame.
- Wrap the whole app in a `Result`-returning closure with a top-level error
  banner; a panic in a callback kills the app — avoid, via typed results.

## E06.1 App shell

**Actions**
1. `desktop::main`: initialize tracing (E00.2), load config (E02.5), locate adb
   (E01.1), construct the app state:
   ```rust
   struct AppState {
       cfg: RwLock<AppConfig>,
       adb: Option<Arc<ProcessAdbRunner>>,
       watcher: Option<DeviceWatcher>,
       events_rx: Receiver<DeviceEvent>, // from watcher
       devices: BTreeMap<String, DeviceView>, // serial → UI state
       selected: Option<String>,
       jobs: HashMap<DeviceId, JobView>,
       controller: Option<BackupController>,
   }
   struct DeviceView { info: DeviceInfo, state: DeviceState, name: String }
   struct JobView { state: JobState, progress, last_summary }
   ```
2. Window: title "Android Backup", min size ~800×620; **TopBottomPanel** status
   bar (adb + server version, app version) → **SidePanel** devices (left) →
   **CentralPanel** device detail + progress + history sections as inert layout.
   Use the plan §10 mock to place panels; rough is fine, structure is the deliverable.
3. Panic-safety: `std::panic::set_hook` → log via tracing + show a crash message
   before eframe exits (MVP-nice, keep small).

**Accept**
- Opens/closes clean on both OSes; layout renders sections with placeholder text;
  closing the window stops the watcher (join) and does **not** kill the adb server.

## E06.2 Live device list

**Actions**
1. Drain `events_rx` each frame; fold `DeviceEvent`s into `devices` map;
   `ctx.request_repaint()` whenever anything changed (else idle at default fps).
2. Render per device: sanitized display name + state label ("Connected",
   "Unauthorized — approve on phone", "Offline", "Disconnected", "Error").
   Click = selection.
3. Empty state: "No devices connected — plug in a phone with USB debugging enabled."
4. Poll-interval from config; no manual refresh button (plan §5.1: automatic).
5. On `FirstSeen` events also trigger props enrichment callback (E03.4 sink wired
   to profile service) so names appear as real model names.

**Accept**
- Run with mock runner: timeline test in an `#[cfg(test)]` headless harness or
  manual run with a real device — both acceptable; state changes appear within
  one poll interval.

## E06.3 Device detail panel

**Actions**
1. Shown when a device is selected. Sections in order:
   a. Name: `egui` editable single-line + Apply button (E04.1 validation; show
      inline error text on hostile names).
   b. Backup Location: current effective path (E04.3), `[Change Location]` → `rfd::pick_folder`;
      invalid pick → inline message, keep old path.
   c. Folders: checkbox list from `device_folders` (E04.5), custom-path text input
      + Add button (E04.6 validation, inline error).
   d. `[Start Backup]` (+ Pause/Resume/Cancel contextually). Button enabled only
      when: gate E03.5 passes AND no job running for this device AND
      `controller.is_some()`.
2. Start flow: resolve destination → validate (E04.4) → if `DestError`, show
   actionable inline error and **don't start**. On success spawn controller
   E05.8 with event channel into `JobView`.
3. Unauthorized device: show the gate's actionable help text instead of controls.

**Accept**
- Manual run with mock + real device: full flow start/pause/resume/cancel works;
  destination change affects next backup only.

## E06.4 Progress panel

**Actions**
1. Drain job event channels each frame into `JobView`.
2. Render: `files: {done}/{total} | {copied_GB} GB | ETA {mm:ss}` + progress bar
   (`egui::ProgressBar`), current file name (truncate middle if long), speed
   when available.
3. Terminal states: Completed → summary strip; Failed/Cancelled → error strip
   with expandable error list (keep last N errors in JobView).

**Accept**
- Matches a mock-driven run event-for-event (headless test asserting the rendered
  values derive from events).

## E06.5 History + summaries

**Actions**
1. Status bar row: last completed job for selected device
   ("Last backup: Completed — 430 copied, 20 unchanged, 2 failed") from E02.3
   `latest_job` (read from DB, cached in JobView).
2. Collapsible "Backup history" table: started, duration, status, counts, bytes;
   row expand → per-file listing of that job from `backup_files` (page/load lazily,
   cap at 500 visible with "load more").
3. Refresh triggers: on job Finished events and on app start.

**Accept**
- Restart persistence: history identical across app restarts (DB-sourced, not memory).

## E06.6 First-launch flows

**Actions**
1. Startup detection order in `main`:
   a. No config yet → location + preferences wizard (single modal: adb choice
      deferred to (c), backup location, done).
   b. ADB locator fails → "ADB not found" modal with three options:
      `[Use system adb]` (sets `AdbMode::System`, locator retries),
      `[Choose adb binary…]` (rfd file pick), `[Install automatically]` → until
      Wave 7 exists, show "coming in a later build" disabled button (graceful
      degradation is acceptable and expected here — note it in the wave gate).
      *Landed in E07.1.2*: the button is live, opens a progress modal and
      switches the config to `Managed` once the new binary answers `adb version`.
   c. Server conflict (`AdbError::ServerError`) → modal with the raw message +
      guidance, app continues (watcher retries).
2. All modals re-derivable: they show again next launch if still unresolved.

**Accept**
- Clean-profile launch shows both prompts exactly once; choosing manual path
  sticks (config saved); app opens with no devices without crashing.

---

## Wave gate

- Architectural check: `crates/desktop/Cargo.toml` contains **no backup engine
  code** (grep for `compare`, `transfer`, `sha2` in desktop source → only calls
  into `core` allowed).
- Manual run matrix on both OSes: mock device + (if available) real device;
  results filed in `docs/testing.md`.
Next: [wave-7-install-reliability.md](wave-7-install-reliability.md).