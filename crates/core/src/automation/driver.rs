//! The clock behind interval schedules (Wave 9, E09.3).
//!
//! [`schedule`](crate::automation::schedule) answers "is a backup due?" and
//! [`policy`](crate::automation::policy) decides whether an automatic attempt
//! may happen at all. Both are pure. This module is the only part of automation
//! that owns a thread, and it does exactly one thing: wake up once in a while and
//! ask the policy to look at what is plugged in.
//!
//! It exists because a phone left plugged in overnight has no *event* left to
//! fire on. Everything that arrives does get an event, and the event path handles
//! it — including a device that is already due when the app starts, which the
//! watcher's first poll delivers as an arrival. What the event path cannot see is
//! the clock crossing an interval boundary while the cable stays in, and a paused
//! job that will never get a reconnect to resume it. Those two are the tick's
//! whole job.
//!
//! Three deliberate non-features:
//!
//! * **No queue.** The loop waits *after* it works, so a tick that takes 30 s
//!   against a dead phone delays the next tick instead of stacking one behind it.
//!   A "tick in progress" flag would be a second source of truth for a property
//!   the loop already has by construction.
//! * **No memory of due-ness.** Every tick re-reads the device list and the
//!   manifest through [`TickHost`]; nothing is cached here. A clock that
//!   remembers becomes a second, divergent source of truth for a decision the
//!   manifest already records.
//! * **No ticking while nothing is plugged in.** An empty device list is not
//!   worth taking the policy lock for.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::config::{AppConfig, DEFAULT_AUTOMATION_TICK_SECS};
use crate::devices::DeviceInfo;

use super::policy::{Host, Note, Policy};

/// A busy loop would starve the database it is ticking against, so even a
/// scripted "no delay" is clamped to this.
const MIN_INTERVAL: Duration = Duration::from_millis(1);

/// The clock's view of the world: [`Host`], plus what is plugged in.
///
/// One trait rather than two, because every implementation is the same object:
/// the app, holding the manifest, the gate and the job pipeline, plus a
/// [`DeviceSnapshot`](crate::devices::DeviceSnapshot) to ask what is connected.
///
/// `Send + Sync` because the clock runs on its own thread, beside the UI thread's
/// calls to [`Policy::on_event`]. Implementations must be safe to call from
/// either, and — the ordering rule that keeps the two from deadlocking — **the
/// policy lock is always taken before whatever `start`/`resume` take, never
/// after**.
pub trait TickHost: Host + Send + Sync {
    /// Every device the watcher last listed, whether or not it is usable. The
    /// policy filters for itself; this is the raw list.
    ///
    /// Re-read on every tick, which is what makes a device enrolled while the app
    /// is running eligible without a restart.
    fn connected(&self) -> Arc<[Arc<DeviceInfo>]>;
}

/// Who hears about what the clock decided.
///
/// Called on the clock's thread, so an implementation must return quickly: the
/// app forwards to a channel and wakes the UI, and E09.4 turns the same notes into
/// OS notifications. It is called *without* the policy lock held.
pub trait NoteSink: Send + Sync {
    fn note(&self, note: Note);
}

/// The usual way to hand out a [`NoteSink`].
pub type SharedNoteSink = Arc<dyn NoteSink>;

/// How the clock is configured. From [`AppConfig`] in the application, from a
/// literal in tests.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DriverConfig {
    /// How long to wait between ticks. The schedules it serves are measured in
    /// hours, so this only has to be short enough to look prompt.
    pub interval: Duration,
}

impl DriverConfig {
    /// The default: [`DEFAULT_AUTOMATION_TICK_SECS`] between ticks.
    pub fn new() -> Self {
        Self {
            interval: Duration::from_secs(DEFAULT_AUTOMATION_TICK_SECS),
        }
    }

    /// Take the tick interval from the user's configuration file. A zero value
    /// (which a hand-edited file can contain) means "as fast as is sane", not
    /// "spin".
    pub fn from_app_config(config: &AppConfig) -> Self {
        Self {
            interval: Duration::from_secs(config.automation.tick_interval_secs.max(1)),
        }
    }

    pub fn with_interval(mut self, interval: Duration) -> Self {
        self.interval = interval.max(MIN_INTERVAL);
        self
    }
}

struct InnerState {
    stop: bool,
    paused: bool,
    interval: Duration,
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

/// The automation clock.
///
/// Dropping it stops the thread, so the app can simply drop it (or the whole app)
/// without leaking one that keeps waking up to query a closed database.
pub struct AutomationDriver {
    inner: Arc<Inner>,
    handle: Option<JoinHandle<()>>,
}

impl AutomationDriver {
    /// Start ticking. The first tick happens one interval from now: anything
    /// already due at startup is covered by the watcher's first poll, which
    /// arrives as an event, so an immediate tick would only duplicate it.
    ///
    /// `policy` is shared with the caller, which feeds it events as they arrive on
    /// the UI thread; the clock feeds the same rulebook its ticks.
    pub fn spawn(
        host: Arc<dyn TickHost>,
        policy: Arc<Mutex<Policy>>,
        sink: SharedNoteSink,
        config: DriverConfig,
    ) -> Self {
        let inner = Arc::new(Inner {
            state: Mutex::new(InnerState {
                stop: false,
                paused: false,
                interval: config.interval.max(MIN_INTERVAL),
            }),
            wake: Condvar::new(),
        });
        let handle = {
            let inner = Arc::clone(&inner);
            std::thread::Builder::new()
                .name("automation-clock".to_string())
                .spawn(move || run_loop(host, policy, sink, inner))
        };
        match handle {
            Ok(handle) => Self {
                inner,
                handle: Some(handle),
            },
            Err(err) => {
                // Like the watcher: a machine that cannot spawn a thread cannot run
                // the app. The caller gets a clock that never ticks rather than a
                // panic, and everything else keeps working by hand.
                tracing::error!(error = %err, "could not start the automation clock thread");
                Self {
                    inner,
                    handle: None,
                }
            }
        }
    }

    pub fn interval(&self) -> Duration {
        lock(&self.inner.state).interval
    }

    /// Change the tick interval, waking the loop so the new value takes effect now
    /// instead of after the old one.
    pub fn set_interval(&self, interval: Duration) {
        lock(&self.inner.state).interval = interval.max(MIN_INTERVAL);
        self.inner.wake.notify_all();
    }

    /// Suspend automatic attempts (the tray's "Pause automation", E09.5).
    ///
    /// A pause is read by the *tick*, not by a wait in progress: a tick that has
    /// already begun finishes. It does not wake the loop either — there is nothing
    /// to abort, and the next tick is at most one interval away, exactly as it
    /// would be if the app had been started then. Nothing is undone: a device that
    /// was due while automation was paused is still due when it resumes.
    pub fn set_paused(&self, paused: bool) {
        lock(&self.inner.state).paused = paused;
    }

    pub fn is_paused(&self) -> bool {
        lock(&self.inner.state).paused
    }

    pub fn is_running(&self) -> bool {
        self.handle.is_some() && !lock(&self.inner.state).stop
    }

    /// Stop the thread and wait for it to finish. Idempotent.
    pub fn stop(&mut self) {
        lock(&self.inner.state).stop = true;
        self.inner.wake.notify_all();
        if let Some(handle) = self.handle.take() {
            if handle.join().is_err() {
                tracing::error!("the automation clock thread panicked");
            }
        }
    }
}

impl Drop for AutomationDriver {
    fn drop(&mut self) {
        self.stop();
    }
}

/// One pass: ask what is plugged in, then let the policy look.
///
/// Pure with respect to the clock — it holds no state of its own, so the tests
/// call it with a chosen `now` and never wait an interval to find out what
/// happens. Reading the device list here rather than inside the policy is also
/// what keeps a tick with nothing to do from serialising behind the UI thread.
fn tick_once(host: &dyn TickHost, policy: &Mutex<Policy>, now: Instant) -> Vec<Note> {
    let devices = host.connected();
    if devices.is_empty() {
        return Vec::new();
    }
    let mut guard = lock(policy);
    guard.tick(host, &devices, now)
}

fn run_loop(
    host: Arc<dyn TickHost>,
    policy: Arc<Mutex<Policy>>,
    sink: SharedNoteSink,
    inner: Arc<Inner>,
) {
    loop {
        // Wait is checked-then-entered under one lock: a `stop()` cannot land
        // between the check and the wait, because the setter has to take the same
        // mutex the wait is about to release — which is also why the wake-up below
        // is never lost.
        let state = lock(&inner.state);
        if state.stop {
            return;
        }
        let interval = state.interval;
        let state = match inner.wake.wait_timeout(state, interval) {
            Ok((state, _timed_out)) => state,
            Err(poisoned) => poisoned.into_inner().0,
        };
        if state.stop {
            return;
        }
        if state.paused {
            // The tick is the only thing the clock does, so this is the whole pause.
            continue;
        }
        drop(state);

        for note in tick_once(&*host, &policy, Instant::now()) {
            sink.note(note);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse::RawState;
    use crate::adb::props::DeviceProps;
    use crate::automation::policy::{AttemptError, PausedJob};
    use crate::automation::Schedule;
    use crate::database::jobs::JobTrigger;
    use crate::devices::DeviceState;
    use std::collections::{HashMap, VecDeque};

    /// The "now" the tests work from: a year ahead of the machine's clock, so a
    /// timestamp can be placed in the past without depending on how long this
    /// machine happens to have been up. Nothing here is compared with the real
    /// clock — a tick is handed its `now` — so a shifted origin is harmless.
    fn origin() -> Instant {
        Instant::now() + Duration::from_secs(400 * 86400)
    }

    fn hours(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    fn device(serial: &str) -> Arc<DeviceInfo> {
        Arc::new(DeviceInfo {
            serial: serial.to_string(),
            state: DeviceState::Connected,
            raw_state: RawState::Device,
            listing_model: None,
            listing_product: None,
            transport_id: None,
            props: DeviceProps::default(),
        })
    }

    fn policy() -> Mutex<Policy> {
        Mutex::new(Policy::new())
    }

    /// The policy's fake, made `Sync` for the clock's thread and given a clock of
    /// its own. It is deliberately *honest*: a successful start inserts a job row,
    /// which is what the manifest's timestamp — the only clock a schedule has —
    /// actually is.
    struct FakeHost {
        /// The test's "now", moved by [`FakeHost::tick`]. A start records it, so the
        /// cadence the tests assert is the cadence the policy really computes.
        now: Mutex<Instant>,
        plug_in: Mutex<Vec<Arc<DeviceInfo>>>,
        schedules: Mutex<HashMap<String, Schedule>>,
        last_attempt: Mutex<HashMap<String, Instant>>,
        paused: Mutex<HashMap<String, PausedJob>>,
        /// Answers for the next attempts; exhausted → `Ok`. Lets a test say "the job
        /// is still running" without inventing a second job row.
        answers: Mutex<VecDeque<Result<(), AttemptError>>>,
        /// Every attempt handed to the host, including refused ones: this is what
        /// "no retries" is measured in.
        attempts: Mutex<Vec<String>>,
        started: Mutex<Vec<String>>,
        resumed: Mutex<Vec<(String, i64)>>,
    }

    impl FakeHost {
        /// One device, `SER1`, plugged in and connected. Nothing is enrolled until
        /// [`FakeHost::enrol`] says so.
        fn new(now: Instant) -> Self {
            Self {
                now: Mutex::new(now),
                plug_in: Mutex::new(vec![device("SER1")]),
                schedules: Mutex::new(HashMap::new()),
                last_attempt: Mutex::new(HashMap::new()),
                paused: Mutex::new(HashMap::new()),
                answers: Mutex::new(VecDeque::new()),
                attempts: Mutex::new(Vec::new()),
                started: Mutex::new(Vec::new()),
                resumed: Mutex::new(Vec::new()),
            }
        }

        fn enrol(&self, schedule: Schedule) {
            self.schedules
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert("SER1".to_string(), schedule.clamped());
        }

        /// One tick by hand, at `now`, with the fake's clock moved to match.
        fn tick(&self, policy: &Mutex<Policy>, now: Instant) -> Vec<Note> {
            *lock(&self.now) = now;
            tick_once(self, policy, now)
        }

        fn now(&self) -> Instant {
            *lock(&self.now)
        }

        /// The device is gone: unplugged, or the poll is failing.
        fn unplug(&self) {
            lock(&self.plug_in).clear();
        }

        /// Pretend the manifest's last job row for `SER1` is this old.
        fn ran_ago(&self, age: Duration) {
            let at = self.now() - age;
            lock(&self.last_attempt).insert("SER1".to_string(), at);
        }

        fn paused(&self, id: i64, trigger: JobTrigger) {
            lock(&self.paused).insert("SER1".to_string(), PausedJob { id, trigger });
        }

        fn answer(&self, answer: Result<(), AttemptError>) {
            lock(&self.answers).push_back(answer);
        }

        fn attempts(&self) -> usize {
            lock(&self.attempts).len()
        }

        fn starts(&self) -> usize {
            lock(&self.started).len()
        }

        fn resumes(&self) -> usize {
            lock(&self.resumed).len()
        }
    }

    impl Host for FakeHost {
        fn schedule(&self, serial: &str) -> Schedule {
            lock(&self.schedules)
                .get(serial)
                .copied()
                .unwrap_or(Schedule::Off)
        }

        fn last_attempt(&self, serial: &str) -> Option<Instant> {
            lock(&self.last_attempt).get(serial).copied()
        }

        fn paused_job(&self, serial: &str) -> Option<PausedJob> {
            lock(&self.paused).get(serial).copied()
        }

        fn start(&self, device: &DeviceInfo) -> Result<(), AttemptError> {
            lock(&self.attempts).push(device.serial.clone());
            let answer = lock(&self.answers).pop_front().unwrap_or(Ok(()));
            if answer.is_ok() {
                lock(&self.started).push(device.serial.clone());
                // A job row appeared, and the job row is the clock.
                self.ran_ago(Duration::ZERO);
            }
            answer
        }

        fn resume(&self, device: &DeviceInfo, job_id: i64) -> Result<(), AttemptError> {
            lock(&self.resumed).push((device.serial.clone(), job_id));
            // The control API the real host calls clears the paused row, so the next
            // tick must see a job that is simply running.
            lock(&self.paused).remove(&device.serial);
            Ok(())
        }
    }

    impl TickHost for FakeHost {
        fn connected(&self) -> Arc<[Arc<DeviceInfo>]> {
            lock(&self.plug_in).clone().into()
        }
    }

    #[derive(Default)]
    struct RecordingSink {
        notes: Mutex<Vec<Note>>,
    }

    impl RecordingSink {
        fn count(&self) -> usize {
            lock(&self.notes).len()
        }
    }

    impl NoteSink for RecordingSink {
        fn note(&self, note: Note) {
            lock(&self.notes).push(note);
        }
    }

    /// Wait for the clock's own thread to do something. Bounded, so a broken clock
    /// fails the test rather than hanging it.
    fn wait_for(mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        condition()
    }

    /// The heart of E09.3: a device left plugged in is backed up again on the
    /// cadence, and a tick that finds a job already running stays silent.
    #[test]
    fn an_interval_fires_on_the_boundary_and_not_before_it() {
        let t0 = origin();
        let host = FakeHost::new(t0);
        host.enrol(Schedule::EveryHours(1));
        let policy = policy();

        // The first-ever run is due on the spot: turning "every hour" on means
        // "starting now", and that first job is what the cadence counts from.
        let first = host.tick(&policy, t0);
        assert_eq!(host.starts(), 1, "{first:?}");
        assert!(matches!(
            first.first(),
            Some(Note::Started { serial }) if serial == "SER1"
        ));

        // Two ticks a minute short of the hour: nothing, both times.
        assert!(host
            .tick(&policy, t0 + hours(1) - Duration::from_secs(59))
            .is_empty());
        assert!(host
            .tick(&policy, t0 + hours(1) - Duration::from_secs(58))
            .is_empty());
        assert_eq!(host.starts(), 1, "nothing was due yet");

        // On the boundary: exactly one job.
        let second = host.tick(&policy, t0 + hours(1));
        assert_eq!(host.starts(), 2, "{second:?}");

        // The job is still running: the next tick asks, is told Busy, and says
        // nothing — no queue, no second job, no notification.
        host.ran_ago(hours(2));
        host.answer(Err(AttemptError::Busy));
        assert!(host
            .tick(&policy, t0 + hours(1) + Duration::from_secs(1))
            .is_empty());
        assert_eq!(host.attempts(), 3, "the attempt was made and refused");
        assert_eq!(host.starts(), 2, "a live job must not be re-run");

        // Once that job is gone, the next due moment fires again.
        host.tick(&policy, t0 + hours(2));
        assert_eq!(host.starts(), 3);
    }

    /// The other half of "ticks do nothing when no device is connected": with
    /// nothing plugged in, the host is not even asked what is enrolled.
    #[test]
    fn an_unplugged_device_makes_every_tick_a_no_op() {
        let t0 = origin();
        let host = FakeHost::new(t0);
        host.enrol(Schedule::EveryHours(1));
        let policy = policy();
        host.unplug();

        for step in 0..24 {
            let notes = host.tick(&policy, t0 + hours(step));
            assert!(notes.is_empty(), "tick {step}: {notes:?}");
        }
        assert_eq!(host.attempts(), 0);
        assert_eq!(host.starts(), 0);
        assert_eq!(host.resumes(), 0);
    }

    /// Automation off is off, ticks included.
    #[test]
    fn an_unenrolled_device_is_never_ticked() {
        let t0 = origin();
        let host = FakeHost::new(t0);
        let policy = policy();

        for step in 0..24 {
            assert!(host.tick(&policy, t0 + hours(step)).is_empty());
        }
        assert_eq!(host.attempts(), 0);
    }

    /// A paused job whose device never left: the reconnect that would normally
    /// resume it never comes, so the tick is the only thing that can. It resumes at
    /// once — the interval governs *new* runs, and unfinished work does not wait for
    /// a schedule to come round again.
    #[test]
    fn a_paused_automatic_job_is_resumed_by_the_clock() {
        let t0 = origin();
        let host = FakeHost::new(t0);
        host.enrol(Schedule::EveryHours(1));
        let policy = policy();
        // Just ran, so the interval is *not* due: the resume has to stand on its own.
        host.ran_ago(Duration::ZERO);
        host.paused(7, JobTrigger::Auto);

        let notes = host.tick(&policy, t0);
        assert_eq!(host.resumes(), 1, "{notes:?}");
        assert_eq!(host.starts(), 0, "a resume is not a start");
        assert!(matches!(
            notes.first(),
            Some(Note::Resumed { job_id: 7, .. })
        ));

        // And once it is running again there is nothing to say until something is
        // due: the clock is not a source of chatter.
        assert!(host.tick(&policy, t0 + Duration::from_secs(60)).is_empty());
        assert_eq!(host.resumes(), 1);
    }

    /// The resume ceiling is the policy's, and the clock is not a way around it:
    /// a job that keeps stopping is not resumed forever just because the ticks
    /// keep coming.
    #[test]
    fn the_clock_respects_the_resume_budget() {
        let t0 = origin();
        let host = FakeHost::new(t0);
        host.enrol(Schedule::EveryHours(1));
        let policy = policy();
        host.paused(7, JobTrigger::Auto);

        let mut notes = Vec::new();
        for step in 0..12 {
            host.paused(7, JobTrigger::Auto);
            notes.extend(host.tick(&policy, t0 + hours(step)));
        }
        assert!(
            host.resumes() <= 2,
            "the clock resumed a job past its budget: {}",
            host.resumes()
        );
        assert!(
            notes
                .iter()
                .any(|note| matches!(note, Note::NeedsAttention { .. })),
            "giving up has to be said out loud: {notes:?}"
        );
    }

    /// A paused job the *user* started is not the clock's to resume, however long
    /// the device stays plugged in.
    #[test]
    fn a_paused_manual_job_is_left_alone_by_the_clock() {
        let t0 = origin();
        let host = FakeHost::new(t0);
        host.enrol(Schedule::EveryHours(1));
        let policy = policy();
        host.paused(7, JobTrigger::Manual);

        for step in 0..24 {
            host.tick(&policy, t0 + hours(step));
        }
        assert_eq!(host.resumes(), 0);
        assert_eq!(host.attempts(), 0, "a paused job owns the device");
    }

    /// The thread, not the rule: it starts, it can be paused out of, and it stops
    /// when asked. The fake's clock is real time here, so the cadence under test is
    /// "does it tick at all", one interval at a time.
    #[test]
    fn the_clock_runs_pauses_and_stops() {
        let host = Arc::new(FakeHost::new(Instant::now()));
        host.enrol(Schedule::EveryHours(1));
        let sink = Arc::new(RecordingSink::default());
        let policy = Arc::new(policy());
        let mut clock = AutomationDriver::spawn(
            Arc::clone(&host) as Arc<dyn TickHost>,
            Arc::clone(&policy),
            Arc::clone(&sink) as SharedNoteSink,
            DriverConfig::new().with_interval(Duration::from_millis(5)),
        );
        assert!(clock.is_running());
        assert!(!clock.is_paused());

        // It ticks on its own.
        assert!(wait_for(|| host.starts() == 1), "the clock never fired");

        // Paused: no further attempts, however many ticks go by. The manifest is
        // made due *while* paused, so "nothing happened" is about the pause and not
        // about the clock not being due.
        clock.set_paused(true);
        assert!(clock.is_paused());
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(host.starts(), 1);
        host.ran_ago(hours(2));
        std::thread::sleep(Duration::from_millis(30));
        assert_eq!(host.starts(), 1, "a paused clock still ticked");
        assert_eq!(host.attempts(), 1);

        // Unpaused and still due: it gets back to work.
        clock.set_paused(false);
        assert!(wait_for(|| host.starts() == 2), "the clock did not resume");

        clock.stop();
        assert!(!clock.is_running());
        // Idempotent, and the loop is gone before we touch its notes.
        clock.stop();
        assert!(
            sink.count() >= host.starts(),
            "every start should have been announced"
        );
    }

    /// A tightened interval takes effect at once. The wait has to be woken for
    /// that, which is exactly the kind of thing that silently rots.
    #[test]
    fn a_new_interval_takes_effect_without_waiting_out_the_old_one() {
        let host = Arc::new(FakeHost::new(Instant::now()));
        host.enrol(Schedule::EveryHours(1));
        let clock = AutomationDriver::spawn(
            Arc::clone(&host) as Arc<dyn TickHost>,
            Arc::new(policy()),
            Arc::new(RecordingSink::default()) as SharedNoteSink,
            // An hour between ticks: the only way a start can happen soon is by
            // waking that wait.
            DriverConfig::new().with_interval(Duration::from_secs(3600)),
        );
        assert_eq!(host.starts(), 0);
        clock.set_interval(Duration::from_millis(5));
        assert_eq!(clock.interval(), Duration::from_millis(5));
        assert!(
            wait_for(|| host.starts() == 1),
            "the tightened interval did not wake the wait"
        );
    }

    /// Dropping the clock stops the thread; and a paused clock is stopped by the
    /// wake, not by waiting out an interval nobody wanted to wait.
    #[test]
    fn dropping_the_clock_stops_the_thread_even_when_paused() {
        let host = Arc::new(FakeHost::new(Instant::now()));
        let clock = AutomationDriver::spawn(
            Arc::clone(&host) as Arc<dyn TickHost>,
            Arc::new(policy()),
            Arc::new(RecordingSink::default()) as SharedNoteSink,
            // Long enough that the join could only succeed by waking the wait.
            DriverConfig::new().with_interval(Duration::from_secs(3600)),
        );
        clock.set_paused(true);
        drop(clock);
    }
}
