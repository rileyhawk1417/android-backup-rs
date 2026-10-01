# Wave 3 — Device Discovery & Identity

Up: [index](index.md) · Prereq: Wave 2 gate passed.
Clears blocks for: device profiles UI (W4), folder enumeration (W4), UI device list (W6).

Design stance: parsers are pure functions over strings (no adb calls) —
the runner (W1) is the only I/O seam. The watcher is the only stateful component.

## E03.1 `adb devices -l` parser

**Actions**
1. `core::adb::parse_devices(output: &str) -> Result<Vec<DeviceSnapshot>, ParseError>` where
   ```rust
   pub enum RawState { Device, Unauthorized, Offline, Recovery, Unrecognized(String) }
   pub struct DeviceSnapshot {
       pub serial: String,
       pub state: RawState,
       pub model: Option<String>,   // from `device product:`/`model:` fields
   }
   ```
2. Grammar notes for robustness:
   - Header line(s): skip lines matching `List of devices attached` and blanks —
     **do not** assume it's the first line.
   - Entry line: `<serial>\t<state>` optionally followed by space-separated
     `key:value` props (`usb:`, `product:`, `model:`, `device:`, `transport_id:`).
   - Serials contain `:` (wifi adb: `192.168.1.20:5555`) — split on the **first tab**, never on whitespace.
3. Unknown state tokens → `Unrecognized(token)`, not an error: parse-all-we-can.

**Accept**
- Fixtures `tests/fixtures/devices/*.txt` with ≥ 6 real samples: plain,
  `devices -l` verbose, unauthorized-only, offline-only, mixed emulator + physical,
  wifi-adb serial, CRLF line endings, trailing whitespace.
- Property-based test (or generated matrix): random spacing around entries still parses.

## E03.2 State mapping (plan §5.1 five states)

**Actions**
1. `core::devices::state`:
   ```rust
   pub enum DeviceState { Connected, Unauthorized, Offline, Disconnected, Error }
   ```
2. Mapping: `Device→Connected`, `Unauthorized→Unauthorized`, `Offline→Offline`,
   `Unrecognized→Error`, `Recovery→Error` (log a warning that recovery is unsupported — plan §13 out of scope).
3. `Disconnected` and `Error` are **derived by the watcher** (E03.4) — not by this
   mapping. Document that here so W4/W6 consume states from the watcher, not the parser.

**Accept**
- Pure-function tests covering every variant + the "serial was Connected last tick,
  absent this tick" input handled by the caller, not the mapper (asserted by API shape:
  mapper takes one snapshot, watcher takes two).

## E03.3 Device property fetcher

**Actions**
1. `core::adb::props::fetch_device_props(runner, serial) -> DeviceProps`:
   - One batched call: `shell getprop ro.product.manufacturer; echo ---; getprop …` —
     **one round-trip, delimiter-split** (per-call overhead adds up on real devices).
     Parse with the same care as E03.1 (CRLF, stray whitespace).
   - All fields `Option<String>`; a failed/empty getprop section logs and yields None,
     never an error — device presence is established by E03.1, props are enrichment.
2. Also fetch `ro.product.device` (useful disambiguation for model-less devices).

**Accept**
- Mock-runner tests: all props; one missing prop; entire getprop fails → All-None + no error;
  delimiter collision (device printing `---` in a prop — choose a delimiter unlikely to collide, e.g. `\n\u{1F}\n` unit separator).

## E03.4 Polling watcher

**Actions**
1. `core::devices::watcher::DeviceWatcher`:
   ```rust
   pub enum DeviceEvent {
       FirstSeen(Arc<DeviceInfo>), Connected(Arc<DeviceInfo>),
       Unauthorized(Arc<DeviceInfo>), Offline(Arc<DeviceInfo>),
       StateChanged { serial, from, to }, Disconnected { serial },
   }
   ```
2. Own thread: loop { pull snapshots via runner → diff against previous tick →
   derive Disconnected / Error / StateChanged/FirstSeen/Connected → emit to
   `mpsc::Sender<Arc<[DeviceEvent]>>` (batched, to make UI repaint cheap) → sleep poll interval }.
   `stop()` via a shared `AtomicBool` + join handle; the sender is dropped when the
   watcher is dropped so subscribers see channel closure as "watcher gone".
3. Diff rules:
   - previous Connected, now absent → `Disconnected`
   - runner error on the poll call itself → single `Error` event (do not spam per tick;
     only on transition into/out of the error condition)
   - props enrichment (E03.3) happens on FirstSeen and on model/manufacturer change only,
     never every tick.
4. First-seen/props → call E02.2 upsert here (**this is the exact moment devices
   become persistent records**) — guard: watcher emits, but DB write happens via a
   callback trait `DeviceSink` the caller provides, keeping `core::devices` DB-optional.

**Pitfalls**
- Do not run the loop on a UI thread; do not share `Sender` mutably —
  all interior state in one thread owned by the watcher.
- Poll interval comes from config (E02.5); expose a setter for tests (0 delay).

**Accept**
- Mock-runner test scripting a timeline (connect → unauthorized → connect → gone → error-poll → gone):
  subscriber receives the exact event sequence in order.
- `stop()` test: thread joins within one poll interval; no events after stop.
- Sink test: FirstSeen triggers exactly one repo upsert (call-counting mock).

## E03.5 Authorization gate (folds in feature F4)

**Actions**
1. `core::devices::backup_gate::check(snapshot: &DeviceInfo) -> Result<(), GateError>`
   with `GateError::{NotAuthorized, Offline, QueryFailed, UnknownState(String)}` —
   distinct, user-actionable messages ("check the phone screen and approve the USB debugging prompt").
2. Enforced everywhere later: W4 validation + W5 job start + W6 button enable
   all call this one function. Nothing re-implements authorization checks.

**Accept**
- Table tests over all five states; message text asserted to be non-empty and
  actionable (review checkpoint).

---

**Wave gate:** parser fixtures + watcher event-sequence tests green.
Next: [wave-4-profiles-locations.md](wave-4-profiles-locations.md).