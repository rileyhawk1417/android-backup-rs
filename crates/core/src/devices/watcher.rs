//! The polling watcher: the only stateful component of the device layer.
//!
//! One thread polls `adb devices -l` on an interval, enriches newly-arrived
//! devices with `getprop`, and turns *two consecutive ticks* into a stream of
//! [`DeviceEvent`]s. Everything else in this module exists to make that thread
//! cheap (batched events, one shell round-trip per enrichment) and stoppable
//! (a condvar, so `stop()` never waits a full poll interval).
//!
//! ## What `adb devices -l` sees vs. what the user sees
//!
//! adb only ever reports devices it can enumerate; a device that is unplugged
//! simply disappears from the list. "Disconnected" and "the poll itself failed"
//! are therefore *derived* here, from the difference between ticks, and are the
//! only two states the mapper ([`crate::devices::state`]) cannot produce.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::adb::parse::{parse_devices, ParseError, RawState};
use crate::adb::props::{self, DeviceProps};
use crate::adb::runner::{Adb, DEFAULT_TIMEOUT};
use crate::adb::AdbError;
use crate::config::{AppConfig, DEFAULT_POLL_INTERVAL_SECS};
use crate::devices::{DeviceInfo, DeviceSink, DeviceState, SharedSink};

/// A busy loop would starve the very mock (and phone) we are polling, so even a
/// scripted "no delay" is clamped to this.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// How the watcher is configured. Comes from [`AppConfig`] in the application,
/// from a literal in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WatcherConfig {
    pub poll_interval: Duration,
    /// Per-command timeout (`devices`, `getprop`). A phone that stops answering
    /// mid-poll must not hang the watcher thread forever.
    pub command_timeout: Duration,
}

impl Default for WatcherConfig {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(DEFAULT_POLL_INTERVAL_SECS),
            command_timeout: DEFAULT_TIMEOUT,
        }
    }
}

impl WatcherConfig {
    /// Take the poll interval from the user's configuration file. A zero value
    /// (which a hand-edited file can contain) means "as fast as is sane", not
    /// "spin".
    pub fn from_app_config(config: &AppConfig) -> Self {
        Self {
            poll_interval: Duration::from_secs(config.poll_interval_secs.max(1)),
            ..Self::default()
        }
    }

    pub fn with_poll_interval(mut self, interval: Duration) -> Self {
        self.poll_interval = interval;
        self
    }

    pub fn with_command_timeout(mut self, timeout: Duration) -> Self {
        self.command_timeout = timeout;
        self
    }
}

/// Something the device list did between two ticks.
///
/// **Exactly one event per device per tick** is emitted, chosen from the most
/// informative variant — a transition to `Connected` produces [`DeviceEvent::Connected`],
/// not `Connected` *and* [`DeviceEvent::StateChanged`] — so a subscriber can
/// toast, log or auto-select without de-duplicating. [`DeviceEvent::StateChanged`]
/// exists for the transitions that have no named variant (a state token we do not
/// understand, i.e. [`DeviceState::Error`]).
///
/// [`DeviceEvent::FirstSeen`] means "first seen in this appearance": unplug and
/// replug the same phone and it is first-seen again, which is what the UI wants
/// in order to re-select it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceEvent {
    FirstSeen(Arc<DeviceInfo>),
    Connected(Arc<DeviceInfo>),
    Unauthorized(Arc<DeviceInfo>),
    Offline(Arc<DeviceInfo>),
    StateChanged {
        serial: String,
        from: DeviceState,
        to: DeviceState,
    },
    Disconnected {
        serial: String,
    },
    /// The listing itself could not be read. Emitted **once** on entering the
    /// failing state — a phone that stays unplugged and an adb server that stays
    /// dead must not produce an event every few seconds.
    PollFailed {
        message: String,
    },
    /// The listing works again. Emitted once, so a UI can clear its banner.
    PollRecovered,
}

impl DeviceEvent {
    /// The serial this event is about, for the variants that have one.
    pub fn serial(&self) -> Option<&str> {
        match self {
            Self::FirstSeen(info)
            | Self::Connected(info)
            | Self::Unauthorized(info)
            | Self::Offline(info) => Some(&info.serial),
            Self::StateChanged { serial, .. } | Self::Disconnected { serial } => Some(serial),
            Self::PollFailed { .. } | Self::PollRecovered => None,
        }
    }

    /// The device snapshot carried by the "this device did something" variants.
    pub fn info(&self) -> Option<&Arc<DeviceInfo>> {
        match self {
            Self::FirstSeen(info)
            | Self::Connected(info)
            | Self::Unauthorized(info)
            | Self::Offline(info) => Some(info),
            _ => None,
        }
    }

    /// The state this event reports, when it reports one.
    pub fn state(&self) -> Option<DeviceState> {
        match self {
            Self::FirstSeen(info)
            | Self::Connected(info)
            | Self::Unauthorized(info)
            | Self::Offline(info) => Some(info.state),
            Self::StateChanged { to, .. } => Some(*to),
            Self::Disconnected { .. } => Some(DeviceState::Disconnected),
            Self::PollFailed { .. } => Some(DeviceState::Error),
            Self::PollRecovered => None,
        }
    }
}

/// A batch of events, as delivered to subscribers.
pub type EventBatch = Arc<[DeviceEvent]>;

struct InnerState {
    stop: bool,
    interval: Duration,
    latest: Arc<[Arc<DeviceInfo>]>,
    last_poll_ok: bool,
}

impl InnerState {
    fn new(interval: Duration) -> Self {
        Self {
            stop: false,
            interval,
            latest: Arc::from(Vec::new()),
            last_poll_ok: false,
        }
    }
}

struct Inner {
    state: Mutex<InnerState>,
    wake: Condvar,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Polls `adb` on its own thread and publishes device events.
///
/// Dropping the watcher stops the thread, so a UI can simply drop it (or the
/// whole app) without leaking a thread that keeps spawning `adb`.
pub struct DeviceWatcher {
    inner: Arc<Inner>,
    handle: Option<JoinHandle<()>>,
    rx: Receiver<EventBatch>,
}

impl DeviceWatcher {
    /// Start watching. The first poll happens immediately, so a caller that
    /// spawns the watcher at application start has a device list "right away"
    /// rather than one poll interval later.
    pub fn spawn(adb: Arc<dyn Adb>, sink: Option<SharedSink>, config: WatcherConfig) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(InnerState::new(config.poll_interval)),
            wake: Condvar::new(),
        });
        let (tx, rx) = mpsc::channel();
        let handle = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("device-watcher".to_string())
                .spawn(move || watch_loop(adb, sink, inner, tx, config))
        };
        match handle {
            Ok(handle) => Self {
                inner,
                handle: Some(handle),
                rx,
            },
            Err(err) => {
                // A machine that cannot spawn a thread cannot run the app at all;
                // the caller sees an empty, non-running watcher rather than a panic.
                tracing::error!(error = %err, "could not start the device watcher thread");
                Self {
                    inner,
                    handle: None,
                    rx,
                }
            }
        }
    }

    /// Receive batches as they are produced (blocking). The UI should prefer
    /// [`DeviceWatcher::drain`] on its own repaint schedule.
    pub fn receiver(&self) -> &Receiver<EventBatch> {
        &self.rx
    }

    /// Every event produced since the last call, without blocking.
    pub fn drain(&self) -> Vec<DeviceEvent> {
        let mut events = Vec::new();
        while let Ok(batch) = self.rx.try_recv() {
            events.extend(batch.iter().cloned());
        }
        events
    }

    /// Block for up to `timeout` for the next batch, then drain whatever else is
    /// already queued. Handy for tests and for a headless subscriber.
    pub fn wait_for_events(&self, timeout: Duration) -> Vec<DeviceEvent> {
        let mut events = match self.rx.recv_timeout(timeout) {
            Ok(batch) => batch.to_vec(),
            Err(_) => Vec::new(),
        };
        events.extend(self.drain());
        events
    }

    /// The device list as of the last successful poll.
    pub fn snapshot(&self) -> Arc<[Arc<DeviceInfo>]> {
        lock(&self.inner.state).latest.clone()
    }

    /// A clonable handle to the same device list, for a thread that does not own
    /// the watcher — see [`DeviceSnapshotHandle`].
    pub fn snapshot_handle(&self) -> DeviceSnapshotHandle {
        DeviceSnapshotHandle {
            inner: Arc::clone(&self.inner),
        }
    }

    /// False when the most recent poll failed: the snapshot is then the last one
    /// that worked, and the UI should say so instead of showing stale devices as
    /// if they were live.
    pub fn last_poll_ok(&self) -> bool {
        lock(&self.inner.state).last_poll_ok
    }

    pub fn poll_interval(&self) -> Duration {
        lock(&self.inner.state).interval
    }

    /// Change the poll interval, waking the loop so the new value takes effect
    /// immediately instead of after the old interval.
    pub fn set_poll_interval(&self, interval: Duration) {
        lock(&self.inner.state).interval = interval;
        self.inner.wake.notify_all();
    }

    pub fn is_running(&self) -> bool {
        self.handle.is_some() && !lock(&self.inner.state).stop
    }

    /// Stop the thread and wait for it to finish. Idempotent.
    pub fn stop(&mut self) {
        {
            let mut state = lock(&self.inner.state);
            state.stop = true;
        }
        self.inner.wake.notify_all();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::error!("the device watcher thread panicked");
            }
        }
    }
}

impl Drop for DeviceWatcher {
    fn drop(&mut self) {
        self.stop();
    }
}

/// What the watcher last saw, readable from any thread (Wave 9, E09.3).
///
/// The watcher owns a thread and an event channel and therefore cannot be
/// shared, but this part of it can: it is the same state, not a copy of it. The
/// automation clock asks it what is plugged in, and the tray (E09.5) will ask it
/// the same question. Because the watcher is the only writer, nothing here can
/// fall out of date behind its back — which is exactly what a second, separately
/// maintained device list would do.
///
/// Like [`DeviceWatcher::snapshot`], the answer is always the *last successful*
/// poll: check [`DeviceSnapshotHandle::last_poll_ok`] before treating an empty list as
/// "nothing is plugged in".
#[derive(Clone)]
pub struct DeviceSnapshotHandle {
    inner: Arc<Inner>,
}

impl DeviceSnapshotHandle {
    /// A handle with no watcher behind it: an empty list that never changes.
    ///
    /// For a caller that has no device thread — a window with no `adb`, or a test
    /// that feeds [`DeviceEvent`](super::DeviceEvent)s by hand. Such a handle reports
    /// an empty list and [`Self::last_poll_ok`] false forever, so the automation clock
    /// finds nothing to start (which is true) while the events the caller *does* feed
    /// still work, because those arrive with their device attached rather than being
    /// looked up here.
    pub fn empty() -> Self {
        Self {
            inner: Arc::new(Inner {
                state: Mutex::new(InnerState::new(Duration::from_secs(1))),
                wake: Condvar::new(),
            }),
        }
    }

    /// Every device the last successful poll listed.
    pub fn devices(&self) -> Arc<[Arc<DeviceInfo>]> {
        lock(&self.inner.state).latest.clone()
    }

    /// False when the most recent poll failed, in which case [`Self::devices`] is
    /// the last list that worked rather than the current one.
    pub fn last_poll_ok(&self) -> bool {
        lock(&self.inner.state).last_poll_ok
    }
}

/// Properties cached between ticks, plus what they were derived from.
struct CachedProps {
    /// The listing fields the fetch was based on: if these change, the phone has
    /// probably changed with them (`model:` is stable, but a different device on
    /// the same serial after a factory reset is not).
    basis: (Option<String>, Option<String>),
    props: DeviceProps,
    /// Whether the fetch happened while the device was actually `device`
    /// (connected). A phone that was unauthorized on arrival answers nothing; the
    /// one that made it worth retrying is the first tick where it is connected.
    fetched_while_connected: bool,
}

/// One tick: read the device list, then enrich the entries that need it.
///
/// Pure with respect to the watcher (no shared state): everything it changes, it
/// is handed explicitly, which is what makes the rules testable without threads.
fn poll_once(
    adb: &dyn Adb,
    timeout: Duration,
    previous: &BTreeMap<String, DeviceState>,
    cache: &mut HashMap<String, CachedProps>,
    sink: Option<&dyn DeviceSink>,
) -> Result<Vec<Arc<DeviceInfo>>, PollError> {
    let output = adb.run(&["devices", "-l"], timeout)?;
    let snapshots = parse_devices(&output.stdout)?;

    // A phone that is no longer plugged in must not keep its cached props alive.
    cache.retain(|serial, _| snapshots.iter().any(|s| &s.serial == serial));

    let mut devices = Vec::with_capacity(snapshots.len());
    for snapshot in &snapshots {
        let basis = (snapshot.model.clone(), snapshot.product.clone());
        let cached = cache.get(&snapshot.serial);
        let connected = snapshot.state == RawState::Device;
        let needs_fetch = match cached {
            None => true,
            Some(entry) => entry.basis != basis || (connected && !entry.fetched_while_connected),
        };
        let (props, refreshed) = match cached {
            Some(entry) if !needs_fetch => (entry.props.clone(), false),
            _ => (
                props::fetch_device_props(adb, &snapshot.serial, timeout),
                true,
            ),
        };
        let changed = refreshed && cached.is_some_and(|entry| entry.props != props);
        cache.insert(
            snapshot.serial.clone(),
            CachedProps {
                basis,
                props: props.clone(),
                fetched_while_connected: connected
                    || cached.is_some_and(|entry| entry.fetched_while_connected),
            },
        );

        let info = DeviceInfo::from_snapshot(snapshot).with_props(props);
        // The manifest is written on arrival and on enrichment, never per tick.
        if let Some(sink) = sink {
            if !previous.contains_key(&snapshot.serial) || changed {
                sink.upsert(&info);
            }
        }
        devices.push(Arc::new(info));
    }
    Ok(devices)
}

/// Turn two ticks into events. See [`DeviceEvent`] for the one-event-per-device rule.
fn plan_events(
    previous: &BTreeMap<String, DeviceState>,
    devices: &[Arc<DeviceInfo>],
) -> Vec<DeviceEvent> {
    let mut events = Vec::new();
    for device in devices {
        match previous.get(&device.serial) {
            None => events.push(DeviceEvent::FirstSeen(Arc::clone(device))),
            Some(from) if *from == device.state => {}
            Some(from) => {
                let serial = device.serial.clone();
                let from = *from;
                events.push(match device.state {
                    DeviceState::Connected => DeviceEvent::Connected(Arc::clone(device)),
                    DeviceState::Unauthorized => DeviceEvent::Unauthorized(Arc::clone(device)),
                    DeviceState::Offline => DeviceEvent::Offline(Arc::clone(device)),
                    to => DeviceEvent::StateChanged { serial, from, to },
                });
            }
        }
    }
    for serial in previous.keys() {
        if !devices.iter().any(|device| &device.serial == serial) {
            events.push(DeviceEvent::Disconnected {
                serial: serial.clone(),
            });
        }
    }
    events
}

#[derive(Debug, thiserror::Error)]
enum PollError {
    #[error(transparent)]
    Adb(#[from] AdbError),
    #[error(transparent)]
    Parse(#[from] ParseError),
}

fn watch_loop(
    adb: Arc<dyn Adb>,
    sink: Option<SharedSink>,
    inner: Arc<Inner>,
    tx: Sender<EventBatch>,
    config: WatcherConfig,
) {
    let mut previous: BTreeMap<String, DeviceState> = BTreeMap::new();
    let mut cache: HashMap<String, CachedProps> = HashMap::new();
    let mut error_active = false;

    loop {
        let mut events: Vec<DeviceEvent> = Vec::new();
        let poll = poll_once(
            adb.as_ref(),
            config.command_timeout,
            &previous,
            &mut cache,
            sink.as_deref(),
        );
        match poll {
            Ok(devices) => {
                if error_active {
                    error_active = false;
                    events.push(DeviceEvent::PollRecovered);
                }
                events.extend(plan_events(&previous, &devices));
                previous = devices
                    .iter()
                    .map(|device| (device.serial.clone(), device.state))
                    .collect();
                let mut state = lock(&inner.state);
                state.latest = Arc::from(devices);
                state.last_poll_ok = true;
            }
            Err(err) => {
                // Deliberately keep `previous` and `latest`: the phone is still
                // plugged in as far as we know, we just cannot ask adb right now.
                if !error_active {
                    error_active = true;
                    tracing::warn!(error = %err, "could not read the device list");
                    events.push(DeviceEvent::PollFailed {
                        message: err.to_string(),
                    });
                }
                lock(&inner.state).last_poll_ok = false;
            }
        }

        if !events.is_empty() && tx.send(Arc::from(events)).is_err() {
            // The subscriber is gone; there is nobody left to notify.
            tracing::debug!("device event subscriber went away, stopping the watcher");
            break;
        }

        let guard = lock(&inner.state);
        if guard.stop {
            break;
        }
        let interval = guard.interval.max(MIN_INTERVAL);
        let (guard, _) = inner
            .wake
            .wait_timeout(guard, interval)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if guard.stop {
            break;
        }
        drop(guard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::props::SEPARATOR;
    use crate::mock::MockAdb;

    fn info(serial: &str, state: DeviceState) -> Arc<DeviceInfo> {
        Arc::new(DeviceInfo {
            serial: serial.to_string(),
            state,
            raw_state: RawState::Device,
            listing_model: None,
            listing_product: None,
            transport_id: None,
            props: DeviceProps::default(),
        })
    }

    fn listing(entries: &[&str]) -> String {
        let mut out = String::from("List of devices attached\n");
        for entry in entries {
            out.push_str(entry);
            out.push('\n');
        }
        out
    }

    fn states(pairs: &[(&str, DeviceState)]) -> BTreeMap<String, DeviceState> {
        pairs
            .iter()
            .map(|(serial, state)| (serial.to_string(), *state))
            .collect()
    }

    /// A sink that counts what it was told, so "exactly one upsert" is a test.
    #[derive(Default)]
    struct CountingSink {
        upserts: Mutex<Vec<String>>,
    }

    impl CountingSink {
        fn upserts(&self) -> Vec<String> {
            lock(&self.upserts).clone()
        }
    }

    impl DeviceSink for CountingSink {
        fn upsert(&self, device: &DeviceInfo) {
            lock(&self.upserts).push(device.serial.clone());
        }
    }

    #[test]
    fn a_new_device_is_first_seen_and_nothing_else() {
        let devices = vec![info("A", DeviceState::Connected)];
        let events = plan_events(&BTreeMap::new(), &devices);
        assert_eq!(events.len(), 1, "one event per device per tick: {events:?}");
        assert!(matches!(&events[0], DeviceEvent::FirstSeen(i) if i.serial == "A"));
    }

    #[test]
    fn an_unchanged_device_is_silent() {
        let previous = states(&[("A", DeviceState::Connected)]);
        let events = plan_events(&previous, &[info("A", DeviceState::Connected)]);
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn transitions_pick_the_most_specific_variant() {
        let previous = states(&[("A", DeviceState::Connected)]);
        let cases = [
            (DeviceState::Unauthorized, "unauthorized"),
            (DeviceState::Offline, "offline"),
            (DeviceState::Error, "error"),
        ];
        for (state, expected) in cases {
            let events = plan_events(&previous, &[info("A", state)]);
            assert_eq!(events.len(), 1, "state {state}: {events:?}");
            match (&events[0], expected) {
                (DeviceEvent::Unauthorized(i), "unauthorized") => assert_eq!(i.serial, "A"),
                (DeviceEvent::Offline(i), "offline") => assert_eq!(i.serial, "A"),
                (DeviceEvent::StateChanged { from, to, .. }, "error") => {
                    assert_eq!((*from, *to), (DeviceState::Connected, DeviceState::Error));
                }
                (other, _) => panic!("unexpected event for {state}: {other:?}"),
            }
        }
    }

    #[test]
    fn becoming_connected_again_is_reported_again() {
        let previous = states(&[("A", DeviceState::Unauthorized)]);
        let events = plan_events(&previous, &[info("A", DeviceState::Connected)]);
        assert!(matches!(&events[0], DeviceEvent::Connected(i) if i.serial == "A"));
    }

    #[test]
    fn a_vanished_device_is_disconnected() {
        let previous = states(&[("A", DeviceState::Connected)]);
        let events = plan_events(&previous, &[]);
        assert_eq!(
            events,
            vec![DeviceEvent::Disconnected { serial: "A".into() }]
        );
    }

    #[test]
    fn an_unplugged_and_replugged_device_is_first_seen_again() {
        // The watcher drops a serial from `previous` when it disappears, so the
        // tick it comes back is a tick it was not in `previous`: FirstSeen, which
        // is what the UI needs in order to re-select the phone.
        let events = plan_events(&BTreeMap::new(), &[info("A", DeviceState::Connected)]);
        assert!(matches!(&events[0], DeviceEvent::FirstSeen(_)));

        // A serial that is still in `previous` (it never left the list) and has
        // changed state is a transition, not a fresh arrival.
        let previous = states(&[("A", DeviceState::Disconnected)]);
        let events = plan_events(&previous, &[info("A", DeviceState::Connected)]);
        assert!(matches!(&events[0], DeviceEvent::Connected(_)));
    }

    #[test]
    fn events_are_batched_in_a_deterministic_order() {
        let previous = states(&[("A", DeviceState::Connected), ("B", DeviceState::Connected)]);
        let devices = vec![
            info("A", DeviceState::Unauthorized),
            info("C", DeviceState::Connected),
        ];
        let events = plan_events(&previous, &devices);
        let described: Vec<(Option<&str>, Option<DeviceState>)> =
            events.iter().map(|e| (e.serial(), e.state())).collect();
        assert_eq!(
            described,
            vec![
                (Some("A"), Some(DeviceState::Unauthorized)),
                (Some("C"), Some(DeviceState::Connected)),
                (Some("B"), Some(DeviceState::Disconnected)),
            ]
        );
    }

    #[test]
    fn event_accessors_look_through_the_stream() {
        let device = info("A", DeviceState::Connected);
        let event = DeviceEvent::FirstSeen(Arc::clone(&device));
        assert_eq!(event.serial(), Some("A"));
        assert_eq!(event.state(), Some(DeviceState::Connected));
        assert_eq!(event.info().map(|i| i.serial.as_str()), Some("A"));
        let failed = DeviceEvent::PollFailed {
            message: "adb died".into(),
        };
        assert_eq!(failed.serial(), None);
        assert_eq!(failed.state(), Some(DeviceState::Error));
        assert_eq!(DeviceEvent::PollRecovered.state(), None);
    }

    #[test]
    fn the_config_interval_never_becomes_a_hot_loop() {
        let mut config = AppConfig {
            poll_interval_secs: 0,
            ..AppConfig::default()
        };
        assert_eq!(
            WatcherConfig::from_app_config(&config).poll_interval,
            Duration::from_secs(1)
        );
        config.poll_interval_secs = 30;
        assert_eq!(
            WatcherConfig::from_app_config(&config).poll_interval,
            Duration::from_secs(30)
        );
    }

    #[test]
    fn the_literal_timeline_from_the_wave_doc() {
        // connect → unauthorized → connect → gone → error-poll, as the wave doc
        // writes it, so the accepted sequence is not a paraphrase of it.
        let previous = |pairs: &[(&str, DeviceState)]| states(pairs);
        let mut timeline: Vec<String> = Vec::new();
        timeline.extend(describe(&plan_events(
            &BTreeMap::new(),
            &[info("A", DeviceState::Connected)],
        )));
        timeline.extend(describe(&plan_events(
            &previous(&[("A", DeviceState::Connected)]),
            &[info("A", DeviceState::Unauthorized)],
        )));
        timeline.extend(describe(&plan_events(
            &previous(&[("A", DeviceState::Unauthorized)]),
            &[info("A", DeviceState::Connected)],
        )));
        timeline.extend(describe(&plan_events(
            &previous(&[("A", DeviceState::Connected)]),
            &[],
        )));
        timeline.push("poll-failed".to_string());
        assert_eq!(
            timeline,
            vec![
                "first-seen:connected",
                "unauthorized",
                "connected",
                "gone:A",
                "poll-failed",
            ]
        );
    }

    fn describe(events: &[DeviceEvent]) -> Vec<String> {
        events
            .iter()
            .map(|event| match event {
                DeviceEvent::FirstSeen(info) => format!("first-seen:{}", info.state),
                DeviceEvent::Connected(_) => "connected".to_string(),
                DeviceEvent::Unauthorized(_) => "unauthorized".to_string(),
                DeviceEvent::Offline(_) => "offline".to_string(),
                DeviceEvent::StateChanged { from, to, .. } => format!("{from}->{to}"),
                DeviceEvent::Disconnected { serial } => format!("gone:{serial}"),
                DeviceEvent::PollFailed { .. } => "poll-failed".to_string(),
                DeviceEvent::PollRecovered => "poll-recovered".to_string(),
            })
            .collect()
    }

    /// The full timeline the wave doc asks for, driven through the real thread.
    #[test]
    fn the_watcher_reports_a_scripted_timeline_in_order() {
        let adb = Arc::new(
            MockAdb::new()
                .script(vec![
                    // tick 1: plugged in but not yet authorized
                    MockAdb::ok(&listing(&["ABC123\tunauthorized"])),
                    MockAdb::ok(&format!("{SEPARATOR}{SEPARATOR}{SEPARATOR}")),
                    // tick 2: the user tapped "Allow"
                    MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
                    MockAdb::ok(&format!(
                        "Samsung{SEPARATOR}SM_A536B{SEPARATOR}a53x{SEPARATOR}14"
                    )),
                    // tick 3: unplugged
                    MockAdb::ok(&listing(&[])),
                    // tick 4: adb itself stops answering
                    MockAdb::err(AdbError::ServerError {
                        msg: "could not connect to tcp:5037".into(),
                    }),
                ])
                .always(MockAdb::ok(&listing(&[]))),
        );
        let sink = Arc::new(CountingSink::default());
        let mut watcher = DeviceWatcher::spawn(
            adb.clone(),
            Some(sink.clone()),
            WatcherConfig::default().with_poll_interval(Duration::from_millis(1)),
        );

        let mut seen: Vec<DeviceEvent> = Vec::new();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            seen.extend(watcher.wait_for_events(Duration::from_millis(50)));
            if seen
                .iter()
                .any(|e| matches!(e, DeviceEvent::PollFailed { .. }))
                && seen.iter().any(|e| matches!(e, DeviceEvent::PollRecovered))
            {
                break;
            }
        }
        watcher.stop();

        let described = describe(&seen);
        assert_eq!(
            described,
            vec![
                "first-seen:unauthorized",
                "connected",
                "gone:ABC123",
                "poll-failed",
                "poll-recovered",
            ]
        );

        // The event carries the enrichment: the props fetched on authorization
        // are visible to whoever receives the event, without a second lookup.
        let connected = seen
            .iter()
            .find_map(|event| match event {
                DeviceEvent::Connected(info) => Some(info),
                _ => None,
            })
            .expect("a Connected event");
        assert_eq!(connected.name_hint(), "Samsung SM-A536B");

        // The sink is called on arrival and on enrichment — not once per tick.
        assert_eq!(
            sink.upserts(),
            vec!["ABC123".to_string(), "ABC123".to_string()]
        );
    }

    #[test]
    fn stopping_is_immediate_and_silences_the_stream() {
        let adb = Arc::new(MockAdb::new().always(MockAdb::ok(&listing(&[]))));
        let mut watcher = DeviceWatcher::spawn(
            adb,
            None,
            WatcherConfig::default().with_poll_interval(Duration::from_secs(30)),
        );
        assert!(watcher.is_running());
        let started = std::time::Instant::now();
        watcher.stop();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "stop() must not wait for a 30s poll interval"
        );
        assert!(!watcher.is_running());
        watcher.stop(); // idempotent
        assert!(watcher.drain().is_empty());
    }

    #[test]
    fn a_failed_poll_is_reported_once_and_the_snapshot_is_marked_stale() {
        let adb = Arc::new(
            MockAdb::new()
                .script(vec![MockAdb::ok(&listing(&["ABC123\tdevice"]))])
                .always(MockAdb::err(AdbError::ServerError {
                    msg: "server is dead".into(),
                })),
        );
        let mut watcher = DeviceWatcher::spawn(
            adb,
            None,
            WatcherConfig::default().with_poll_interval(Duration::from_millis(1)),
        );
        let mut failures = 0usize;
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            for event in watcher.wait_for_events(Duration::from_millis(20)) {
                if matches!(event, DeviceEvent::PollFailed { .. }) {
                    failures += 1;
                }
            }
            if failures > 0 {
                // Keep polling a little longer to prove it does not repeat.
                std::thread::sleep(Duration::from_millis(50));
                failures += watcher
                    .drain()
                    .iter()
                    .filter(|e| matches!(e, DeviceEvent::PollFailed { .. }))
                    .count();
                break;
            }
        }
        let last_ok = watcher.last_poll_ok();
        let snapshot = watcher.snapshot();
        watcher.stop();
        assert_eq!(failures, 1, "a persistent failure must not spam the UI");
        assert!(!last_ok, "the UI must be able to say the list is stale");
        assert_eq!(snapshot.len(), 1, "the last good list is kept for display");
    }

    #[test]
    fn a_phone_that_was_unauthorized_gets_its_props_on_authorization() {
        let adb = MockAdb::new().script(vec![
            // tick 1: present, not authorized, `getprop` answers nothing
            MockAdb::ok(&listing(&["ABC123\tunauthorized"])),
            MockAdb::ok(&format!("{SEPARATOR}{SEPARATOR}{SEPARATOR}")),
            // tick 2: authorized — the listing gains model/product *and* the
            // device is now worth asking
            MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
            MockAdb::ok(&format!(
                "Samsung{SEPARATOR}SM_A536B{SEPARATOR}a53x{SEPARATOR}14"
            )),
            // tick 3: nothing changed, so nothing is asked
            MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
        ]);
        let mut cache = HashMap::new();
        let mut previous = BTreeMap::new();

        let first = poll_once(&adb, Duration::from_secs(1), &previous, &mut cache, None).unwrap();
        assert_eq!(first[0].state, DeviceState::Unauthorized);
        assert_eq!(first[0].props, DeviceProps::default());
        previous = first.iter().map(|d| (d.serial.clone(), d.state)).collect();

        let second = poll_once(&adb, Duration::from_secs(1), &previous, &mut cache, None).unwrap();
        assert_eq!(second[0].state, DeviceState::Connected);
        assert_eq!(second[0].props.model.as_deref(), Some("SM_A536B"));
        assert_eq!(second[0].name_hint(), "Samsung SM-A536B");
        assert_eq!(
            adb.count_matching("getprop"),
            2,
            "one ask per attempt, not per tick"
        );

        previous = second.iter().map(|d| (d.serial.clone(), d.state)).collect();
        let third = poll_once(&adb, Duration::from_secs(1), &previous, &mut cache, None).unwrap();
        assert_eq!(third[0].props.model.as_deref(), Some("SM_A536B"));
        assert_eq!(adb.count_matching("getprop"), 2, "cached props are reused");
    }

    #[test]
    fn a_cache_entry_dies_with_the_device() {
        let adb = MockAdb::new()
            .script(vec![
                MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
                MockAdb::ok(&format!(
                    "Samsung{SEPARATOR}SM_A536B{SEPARATOR}a53x{SEPARATOR}14"
                )),
                MockAdb::ok(&listing(&[])),
                MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
                MockAdb::ok(&format!(
                    "Samsung{SEPARATOR}SM_A536B{SEPARATOR}a53x{SEPARATOR}14"
                )),
            ])
            .always(MockAdb::ok(&listing(&[])));
        let mut cache = HashMap::new();
        let previous = BTreeMap::new();

        let _ = poll_once(&adb, Duration::from_secs(1), &previous, &mut cache, None).unwrap();
        assert_eq!(cache.len(), 1);
        let _ = poll_once(&adb, Duration::from_secs(1), &previous, &mut cache, None).unwrap();
        assert!(
            cache.is_empty(),
            "an unplugged phone must not leave props behind"
        );
        let again = poll_once(&adb, Duration::from_secs(1), &previous, &mut cache, None).unwrap();
        assert_eq!(again[0].props.model.as_deref(), Some("SM_A536B"));
        assert_eq!(adb.count_matching("getprop"), 2, "replugging asks again");
    }

    #[test]
    fn the_snapshot_holds_the_enriched_devices() {
        let adb = MockAdb::new()
            .on(
                "devices",
                MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
            )
            .on(
                "getprop",
                MockAdb::ok(&format!(
                    "Samsung{SEPARATOR}SM_A536B{SEPARATOR}a53x{SEPARATOR}14"
                )),
            );
        let mut watcher = DeviceWatcher::spawn(
            Arc::new(adb),
            None,
            WatcherConfig::default().with_poll_interval(Duration::from_millis(1)),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while watcher.snapshot().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let snapshot = watcher.snapshot();
        watcher.stop();
        assert_eq!(snapshot.len(), 1);
        assert_eq!(snapshot[0].props.manufacturer.as_deref(), Some("Samsung"));
        assert_eq!(snapshot[0].name_hint(), "Samsung SM-A536B");
    }

    /// The handle a background thread holds (E09.3's clock) is the watcher's own
    /// state, not a copy that can fall behind it.
    #[test]
    fn a_snapshot_handle_sees_what_the_watcher_sees() {
        let adb = MockAdb::new()
            .on(
                "devices",
                MockAdb::ok(&listing(&["ABC123\tdevice product:a53x model:SM_A536B"])),
            )
            .on(
                "getprop",
                MockAdb::ok(&format!(
                    "Samsung{SEPARATOR}SM_A536B{SEPARATOR}a53x{SEPARATOR}14"
                )),
            );
        let mut watcher = DeviceWatcher::spawn(
            Arc::new(adb),
            None,
            WatcherConfig::default().with_poll_interval(Duration::from_millis(1)),
        );
        // Taken *before* the first poll lands, so this is a view and not a copy.
        let handle = watcher.snapshot_handle();
        assert!(handle.devices().is_empty());
        assert!(
            !handle.last_poll_ok(),
            "nothing has been polled yet, so the list is not to be trusted"
        );

        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while handle.devices().is_empty() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        let seen = handle.devices();
        assert_eq!(seen.len(), 1, "the handle stayed empty");
        assert_eq!(seen[0].serial, "ABC123");
        assert!(handle.last_poll_ok());
        assert_eq!(watcher.snapshot().len(), seen.len());
        assert_eq!(watcher.last_poll_ok(), handle.last_poll_ok());
        watcher.stop();
    }
}
