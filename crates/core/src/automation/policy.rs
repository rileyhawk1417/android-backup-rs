//! Who is allowed to press the user's buttons, and when (Wave 9, E09.2).
//!
//! The policy turns "a phone became ready" into at most one *attempt*: it asks
//! the device's own schedule, the manifest's history, and the host (which owns
//! the authorization gate and the job pipeline), and it decides. It performs no
//! I/O of its own — everything it can touch is behind [`Host`] — so the whole
//! rulebook is testable against a scripted timeline without a database, a phone
//! or a running job.
//!
//! Three rules shape every decision:
//!
//! 1. **A manual job always wins.** If a job is running, the attempt is dropped
//!    silently. If a job is *paused* and a person started it, automation stays
//!    out of it: they pressed Start, so Resume is theirs to press too.
//! 2. **A trigger fires an attempt, not a run.** The gate decides whether the
//!    attempt is possible; a device that fails it produces at most one
//!    explanation per appearance, never a notification loop.
//! 3. **Automatic means bounded.** A flapping cable gets a cooldown, an
//!    appearance gets a budget of attempts, and a job that keeps re-pausing
//!    stops being resumed automatically and asks for a person instead.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::automation::Schedule;
use crate::database::jobs::JobTrigger;
use crate::devices::{DeviceEvent, DeviceInfo, DeviceState};

/// How long a device is left alone after an attempt that *began* and failed for
/// an infrastructure reason. This is what makes a cable that reconnects every
/// few seconds cheap: the flapping produces no retries.
///
/// Deliberately not applied to [`AttemptError::NotReady`]: the user allowing USB
/// debugging, or plugging in the destination drive, must take effect at once,
/// not two minutes later.
pub const FAILURE_COOLDOWN: Duration = Duration::from_secs(120);

/// The shortest gap between two things automation says about the same device.
///
/// The per-appearance rule already limits a plug-in to one explanation; this
/// bounds the *appearance* count itself, because a phone that flaps between
/// unauthorized and connected is a device that re-appears every few seconds.
pub const EXPLANATION_INTERVAL: Duration = Duration::from_secs(120);

/// How many attempts one appearance of a device may be refused before
/// automation stops trying until it is unplugged and plugged back in.
pub const MAX_ATTEMPTS_PER_APPEARANCE: u32 = 3;

/// How many automatic resumes one paused job gets before automation gives up
/// and asks for a person.
pub const MAX_RESUMES_PER_JOB: u32 = 2;

/// What automation says when it stops resuming a job that keeps stopping
/// (user-facing text; the desktop renders it, the policy owns the wording).
pub const GIVE_UP_REASON: &str =
    "the connection kept dropping, so resuming it automatically was stopped";

/// A paused job, as the policy needs to see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PausedJob {
    pub id: i64,
    pub trigger: JobTrigger,
}

/// Why an attempt did not happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptError {
    /// A job is already live for this device, or the app is busy with something
    /// that must not be interrupted. Silence: the user is already looking at it.
    Busy,
    /// The device cannot be used *yet*, and the user can do something about it —
    /// the authorization gate's own words, normally. Nothing was attempted, so
    /// no cooldown: fixing it works immediately.
    NotReady(String),
    /// The attempt began and failed for an infrastructure reason (the cable, the
    /// phone sleeping, a destination that went away). Worth a cooldown.
    Failed(String),
}

/// Everything the policy is allowed to touch.
///
/// Implemented by the app, which owns the manifest, the gate and the job
/// pipeline. Two notes for implementors:
///
/// * `schedule` must answer `Off` for a device it does not know. Automation is
///   opt-in, and an unknown phone has opted into nothing.
/// * `start` and `resume` must not block the caller's thread for long and must
///   never be called in a way that can deadlock with the poll loop: hand the
///   work to a worker or a queue, exactly as the Start button does. The policy
///   only decides; the host decides how to do it safely.
pub trait Host {
    /// The device's own schedule. `Off` for an unknown device.
    fn schedule(&self, serial: &str) -> Schedule;

    /// When this device's most recent job started, whoever started it (the
    /// manifest's history is the clock). `None` when it has never run.
    ///
    /// A stored timestamp that lands in the future — a laptop whose clock was
    /// wrong and has been corrected — must be reported as it is, *not* clamped
    /// here: the schedule reads anything after `now` as "just now", and
    /// clamping at both ends would hide the difference between "this ran a
    /// moment ago" and "this ran an hour from now".
    fn last_attempt(&self, serial: &str) -> Option<Instant>;

    /// The job for this device that is sitting paused, if there is one.
    fn paused_job(&self, serial: &str) -> Option<PausedJob>;

    /// Start a new job exactly as the Start button would, gate included.
    fn start(&self, device: &DeviceInfo) -> Result<(), AttemptError>;

    /// Resume a paused job, gate included.
    fn resume(&self, device: &DeviceInfo, job_id: i64) -> Result<(), AttemptError>;
}

/// Something automation did that a person may need to be told about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Note {
    /// An automatic backup began, so that a phone starting to chatter on
    /// plug-in is explained rather than mysterious.
    Started { serial: String },
    /// An interrupted automatic backup is continuing where it stopped.
    Resumed { serial: String, job_id: i64 },
    /// Nothing could start because the device is not usable yet, and the reason
    /// is something the user can fix. `reason` is the gate's own wording.
    Blocked { serial: String, reason: String },
    /// An automatic attempt began and failed.
    Failed { serial: String, reason: String },
    /// Automation has stopped trying and needs a person.
    NeedsAttention { serial: String, reason: String },
}

impl Note {
    pub fn serial(&self) -> &str {
        match self {
            Self::Started { serial }
            | Self::Resumed { serial, .. }
            | Self::Blocked { serial, .. }
            | Self::Failed { serial, .. }
            | Self::NeedsAttention { serial, .. } => serial,
        }
    }
}

/// The counting for one *appearance* of a device — the stretch between two
/// plug-ins. All of it is reset when the device is seen again, which is what
/// makes "back up when I plug it in" mean once per plug-in, and what re-arms
/// the one-shot explanation.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Appearance {
    /// Automatic runs begun in this appearance, started or resumed — or already
    /// running without us. `OnConnect` allows one.
    backed_up: u32,
    /// Attempts refused in this appearance; the budget that stops automation
    /// from hammering a device that cannot be backed up.
    attempts: u32,
    /// Whether this appearance has had its one explanation.
    told: bool,
}

/// The resumes spent on one paused job, and whether automation has already
/// complained about it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Resuming {
    job_id: i64,
    times: u32,
    warned: bool,
}

/// Everything remembered about one device between events. Deliberately keyed by
/// serial and free of device *rows*: the policy holds no view of the device that
/// could go stale, only counters and two timestamps.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Device {
    appearance: Appearance,
    /// When an automatic attempt last *failed*. Survives the unplug: a flapping
    /// cable is precisely a device that keeps re-appearing, so a cooldown that
    /// reset with the appearance would never apply.
    failed_at: Option<Instant>,
    /// When automation last explained itself about this device, across
    /// appearances, so a device that flaps cannot produce a notification loop.
    told_at: Option<Instant>,
    /// The paused job automation is willing to resume, and its budget.
    ///
    /// Scoped to the *job*, not to the appearance, and therefore not reset by a
    /// reconnect — the wave doc says "twice within one connection episode", but
    /// the reconnects that break a job are exactly what ends an episode, so an
    /// appearance-scoped count would reset itself and loop forever.
    resuming: Option<Resuming>,
}

impl Device {
    fn cooling_down(&self, now: Instant) -> bool {
        self.failed_at
            .is_some_and(|at| now.saturating_duration_since(at) < FAILURE_COOLDOWN)
    }

    /// May this appearance be told why nothing is happening? Sets the flag and
    /// the cross-appearance clock when the answer is yes.
    fn may_explain(&mut self, now: Instant) -> bool {
        if self.appearance.told {
            return false;
        }
        if self
            .told_at
            .is_some_and(|at| now.saturating_duration_since(at) < EXPLANATION_INTERVAL)
        {
            return false;
        }
        self.appearance.told = true;
        self.told_at = Some(now);
        true
    }
}

/// The automation rulebook. Holds counters only; ask [`crate::devices::watcher`]
/// for what is plugged in and [`Host`] for everything else.
#[derive(Debug, Default, Clone)]
pub struct Policy {
    devices: HashMap<String, Device>,
}

impl Policy {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed a watcher event. The notes returned are what happened — the actions
    /// have already been performed through `host`.
    pub fn on_event(&mut self, host: &dyn Host, event: &DeviceEvent, now: Instant) -> Vec<Note> {
        match event {
            // A device we have not seen in this appearance: a fresh plug-in
            // re-arms everything, including the one-shot explanation.
            DeviceEvent::FirstSeen(info) => {
                self.devices
                    .entry(info.serial.clone())
                    .or_default()
                    .appearance = Appearance::default();
                self.evaluate(host, info, now).into_iter().collect()
            }
            // Plugging in, or being granted access, are both "it is usable now".
            // Whether it really is usable is the gate's question, not ours.
            DeviceEvent::Connected(info) | DeviceEvent::Unauthorized(info) => {
                self.evaluate(host, info, now).into_iter().collect()
            }
            DeviceEvent::Disconnected { serial } => {
                self.appearance_ended(serial, now);
                Vec::new()
            }
            _ => Vec::new(),
        }
    }

    /// Re-evaluate every device that is currently ready.
    ///
    /// The event path covers arrivals; this covers the clock (E09.3). It does
    /// nothing for a device that is not [`DeviceState::Connected`]: an
    /// unauthorized device has nothing to attempt, and the one explanation for
    /// that comes from the event path.
    pub fn tick(&mut self, host: &dyn Host, ready: &[Arc<DeviceInfo>], now: Instant) -> Vec<Note> {
        ready
            .iter()
            .filter(|info| info.state == DeviceState::Connected)
            .filter_map(|info| self.evaluate(host, info, now))
            .collect()
    }

    /// The device is gone. Its appearance is over; what it learned about a
    /// paused job, a failing attempt and a recent explanation stays, because all
    /// three outlive an unplug by design.
    fn appearance_ended(&mut self, serial: &str, now: Instant) {
        let finished = self.devices.get(serial).is_some_and(|device| {
            device.resuming.is_none()
                && device.failed_at.is_none()
                && device
                    .told_at
                    .is_none_or(|at| now.saturating_duration_since(at) >= EXPLANATION_INTERVAL)
        });
        if finished {
            // Nothing left that has to survive: forget the device entirely, so a
            // machine that has seen a thousand phone serials does not keep a
            // thousand counters.
            self.devices.remove(serial);
        } else if let Some(device) = self.devices.get_mut(serial) {
            device.appearance = Appearance::default();
        }
    }

    /// The whole rulebook, for one device, right now.
    fn evaluate(&mut self, host: &dyn Host, info: &DeviceInfo, now: Instant) -> Option<Note> {
        let serial = info.serial.clone();
        let device = self.devices.entry(serial.clone()).or_default();

        // Off — or a device nobody has enrolled — means automation does not
        // exist for it. Not even the resume rule: one switch, one meaning.
        let schedule = host.schedule(&serial);
        if schedule == Schedule::Off {
            return None;
        }

        // A paused job owns this device. A reconnect is a *resume*, not a new
        // job, and nobody may start a second one while it waits.
        if let Some(paused) = host.paused_job(&serial) {
            if paused.trigger == JobTrigger::Manual {
                // The user pressed Start. Their job, their Resume button.
                return None;
            }

            let mut spent = false;
            {
                let entry = device.resuming.get_or_insert(Resuming {
                    job_id: paused.id,
                    times: 0,
                    warned: false,
                });
                // A different job: this is a fresh budget.
                if entry.job_id != paused.id {
                    *entry = Resuming {
                        job_id: paused.id,
                        times: 0,
                        warned: false,
                    };
                }
                if entry.times >= MAX_RESUMES_PER_JOB {
                    if entry.warned {
                        return None;
                    }
                    entry.warned = true;
                    spent = true;
                }
            }
            if spent {
                return Some(Note::NeedsAttention {
                    serial,
                    reason: GIVE_UP_REASON.to_string(),
                });
            }
            if device.cooling_down(now) {
                return None;
            }

            return match host.resume(info, paused.id) {
                Ok(()) => {
                    if let Some(entry) = device.resuming.as_mut() {
                        if entry.job_id == paused.id {
                            entry.times += 1;
                        }
                    }
                    device.appearance.backed_up += 1;
                    device.failed_at = None;
                    Some(Note::Resumed {
                        serial,
                        job_id: paused.id,
                    })
                }
                Err(AttemptError::Busy) => None,
                Err(AttemptError::NotReady(reason)) => {
                    device.appearance.attempts += 1;
                    if device.may_explain(now) {
                        Some(Note::Blocked { serial, reason })
                    } else {
                        None
                    }
                }
                Err(AttemptError::Failed(reason)) => {
                    device.appearance.attempts += 1;
                    device.failed_at = Some(now);
                    if device.may_explain(now) {
                        Some(Note::Failed { serial, reason })
                    } else {
                        None
                    }
                }
            };
        }

        // Nothing is paused: is a new run due?

        // "Back up when I plug it in" is once per plug-in, not once per tick.
        // A *refused* attempt does not count: the user granting USB debugging
        // afterwards should still get their backup.
        if schedule == Schedule::OnConnect && device.appearance.backed_up > 0 {
            return None;
        }
        if device.appearance.attempts >= MAX_ATTEMPTS_PER_APPEARANCE {
            return None;
        }

        // The manifest is the clock: a person's manual backup five minutes ago
        // also satisfies "every six hours".
        if !schedule.due(host.last_attempt(&serial), now) {
            return None;
        }
        if device.cooling_down(now) {
            return None;
        }

        match host.start(info) {
            Ok(()) => {
                device.appearance.backed_up += 1;
                // The old job, if any, is history now.
                device.resuming = None;
                device.failed_at = None;
                Some(Note::Started { serial })
            }
            // A job is already live. That *is* this appearance's backup — the
            // user's, started by hand — so "when connected" is satisfied and
            // automation does not come back later with a second one.
            Err(AttemptError::Busy) => {
                device.appearance.backed_up += 1;
                None
            }
            Err(AttemptError::NotReady(reason)) => {
                device.appearance.attempts += 1;
                if device.may_explain(now) {
                    Some(Note::Blocked { serial, reason })
                } else {
                    None
                }
            }
            Err(AttemptError::Failed(reason)) => {
                device.appearance.attempts += 1;
                device.failed_at = Some(now);
                if device.may_explain(now) {
                    Some(Note::Failed { serial, reason })
                } else {
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adb::parse::RawState;
    use crate::adb::props::DeviceProps;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// The "now" the tests work from: a year ahead of the machine's clock, so
    /// that a timestamp can be placed in the past without depending on how long
    /// this machine happens to have been up. Nothing here is ever compared with
    /// the real clock — the policy is pure — so a shifted origin is harmless.
    fn origin() -> Instant {
        Instant::now() + Duration::from_secs(400 * 86400)
    }

    fn hours(n: u64) -> Duration {
        Duration::from_secs(n * 3600)
    }

    fn device(serial: &str, state: DeviceState) -> Arc<DeviceInfo> {
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

    #[derive(Default)]
    struct Calls {
        /// Every attempt handed to the fake, including the ones it refused: this
        /// is what "no retries" is measured in.
        attempts: Vec<String>,
        resumes: Vec<(String, i64)>,
    }

    /// Stands in for the app: a manifest and a job pipeline, scripted by the
    /// test. It is deliberately *honest* — a successful start moves the clock
    /// the way inserting a job row would, and a successful resume un-pauses the
    /// job the way the control API would.
    struct FakeHost {
        /// The instant the test treats as "now", so a stored timestamp can be
        /// placed before or after it without depending on how long this machine
        /// happens to have been up.
        origin: Instant,
        schedules: HashMap<String, Schedule>,        last_attempt: RefCell<HashMap<String, Instant>>,
        paused: RefCell<HashMap<String, PausedJob>>,
        /// Answers for the next `start`/`resume` calls; exhausted → `Ok`.
        answers: RefCell<VecDeque<Result<(), AttemptError>>>,
        calls: RefCell<Calls>,
    }

    impl Default for FakeHost {
        fn default() -> Self {
            Self {
                origin: Instant::now(),
                schedules: HashMap::new(),
                last_attempt: RefCell::new(HashMap::new()),
                paused: RefCell::new(HashMap::new()),
                answers: RefCell::new(VecDeque::new()),
                calls: RefCell::new(Calls::default()),
            }
        }
    }

    impl FakeHost {
        fn new(now: Instant, schedule: Schedule) -> Self {
            let mut host = Self {
                origin: now,
                ..Self::default()
            };
            host.schedules
                .insert("SER1".to_string(), schedule.clamped());
            host
        }

        fn answers(&self, answers: impl IntoIterator<Item = Result<(), AttemptError>>) {
            self.answers.borrow_mut().extend(answers);
        }

        /// Pretend the manifest says the last job started this long ago.
        fn ran_ago(&self, age: Duration) {
            self.ran_at(self.origin - age);
        }

        /// A stored timestamp ahead of the clock: a laptop whose clock was wrong
        /// and has since been corrected.
        fn ran_ahead(&self, ahead: Duration) {
            self.ran_at(self.origin + ahead);
        }

        fn ran_at(&self, at: Instant) {
            self.last_attempt
                .borrow_mut()
                .insert("SER1".to_string(), at);
        }

        fn paused(&self, id: i64, trigger: JobTrigger) {
            self.paused
                .borrow_mut()
                .insert("SER1".to_string(), PausedJob { id, trigger });
        }

        fn calls(&self) -> (Vec<String>, Vec<(String, i64)>) {
            let calls = self.calls.borrow();
            (calls.attempts.clone(), calls.resumes.clone())
        }

        fn attempt_count(&self) -> usize {
            self.calls.borrow().attempts.len()
        }

        fn resume_count(&self) -> usize {
            self.calls.borrow().resumes.len()
        }
    }

    impl Host for FakeHost {
        fn schedule(&self, literal: &str) -> Schedule {
            self.schedules
                .get(literal)
                .copied()
                .unwrap_or(Schedule::Off)
        }

        fn last_attempt(&self, serial: &str) -> Option<Instant> {
            self.last_attempt.borrow().get(serial).copied()
        }

        fn paused_job(&self, serial: &str) -> Option<PausedJob> {
            self.paused.borrow().get(serial).copied()
        }

        fn start(&self, device: &DeviceInfo) -> Result<(), AttemptError> {
            self.calls.borrow_mut().attempts.push(device.serial.clone());
            let answer = self.answers.borrow_mut().pop_front().unwrap_or(Ok(()));
            if answer.is_ok() {
                // As if a job row had just been inserted.
                self.ran_at(self.origin);
            }
            answer
        }

        fn resume(&self, device: &DeviceInfo, job_id: i64) -> Result<(), AttemptError> {
            self.calls
                .borrow_mut()
                .resumes
                .push((device.serial.clone(), job_id));
            let answer = self.answers.borrow_mut().pop_front().unwrap_or(Ok(()));
            if answer.is_ok() {
                // As if the control API had put the job back to work.
                self.paused.borrow_mut().remove(&device.serial);
                self.ran_at(self.origin);
            }
            answer
        }
    }

    /// The plug-in we are testing against: the watcher's first sight of a phone
    /// that is already connected.
    fn plugged_in(state: DeviceState) -> DeviceEvent {
        DeviceEvent::FirstSeen(device("SER1", state))
    }

    /// "Allow USB debugging on the phone", in the gate's voice.
    fn not_ready() -> Result<(), AttemptError> {
        Err(AttemptError::NotReady(
            "Allow USB debugging on the phone".to_string(),
        ))
    }

    fn flaky() -> Result<(), AttemptError> {
        Err(AttemptError::Failed("the phone went away".to_string()))
    }

    fn unplugged() -> DeviceEvent {
        DeviceEvent::Disconnected {
            serial: "SER1".to_string(),
        }
    }

    fn started() -> Note {
        Note::Started {
            serial: "SER1".to_string(),
        }
    }

    fn resumed(job_id: i64) -> Note {
        Note::Resumed {
            serial: "SER1".to_string(),
            job_id,
        }
    }

    fn blocked() -> Note {
        Note::Blocked {
            serial: "SER1".to_string(),
            reason: "Allow USB debugging on the phone".to_string(),
        }
    }

    /// Everything that is plugged in and connected.
    fn ready() -> Vec<Arc<DeviceInfo>> {
        vec![device("SER1", DeviceState::Connected)]
    }

    #[test]
    fn plugging_in_starts_exactly_one_automatic_run() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t),
            vec![started()]
        );

        // The tick that follows, and the hundreds after it, must be silent:
        // otherwise "back up when connected" would mean "back up forever".
        for step in 1..50_u64 {
            assert!(
                policy.tick(&host, &ready(), t + hours(step)).is_empty(),
                "step {step}"
            );
        }
        assert_eq!(host.attempt_count(), 1);
    }

    #[test]
    fn a_second_plug_in_earns_a_second_run() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        policy.on_event(&host, &plugged_in(DeviceState::Connected), t);
        policy.on_event(&host, &unplugged(), t + hours(1));
        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t + hours(2)),
            vec![started()]
        );
        assert_eq!(host.attempt_count(), 2);
    }

    #[test]
    fn an_unauthorized_plug_in_explains_itself_once_and_still_backs_up_afterwards() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        host.answers([not_ready()]);
        let mut policy = Policy::new();

        // First sight: unauthorized. Zero jobs, one explanation.
        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Unauthorized), t),
            vec![blocked()]
        );
        assert!(
            host.last_attempt("SER1").is_none(),
            "a phone we cannot reach must not acquire a job"
        );

        // The user reads the prompt: still exactly one explanation.
        for step in 1..20_u64 {
            assert!(policy
                .tick(
                    &host,
                    &[device("SER1", DeviceState::Unauthorized)],
                    t + hours(step)
                )
                .is_empty());
        }

        // The user allows it, ten seconds later. The gate is not a failure of
        // ours, so nothing delays the backup: it happens at once.
        let allowed = t + Duration::from_secs(10);
        assert_eq!(
            policy.on_event(
                &host,
                &DeviceEvent::Connected(device("SER1", DeviceState::Connected)),
                allowed
            ),
            vec![started()]
        );
        assert_eq!(
            host.attempt_count(),
            2,
            "the refused attempt, then the one that ran"
        );
        assert!(host.last_attempt("SER1").is_some());
    }

    #[test]
    fn an_unauthorized_phone_that_flaps_is_explained_no_more_than_once_per_interval() {
        // The cable only carries power, or the user keeps tapping "Deny": the
        // phone re-appears every few seconds and is unauthorized every time.
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        host.answers([
            not_ready(),
            not_ready(),
            not_ready(),
            not_ready(),
            not_ready(),
        ]);
        let mut policy = Policy::new();

        let mut notes = 0;
        for step in 0..5_u64 {
            let at = t + Duration::from_secs(step * 3);
            policy.on_event(&host, &unplugged(), at);
            notes += policy
                .on_event(&host, &plugged_in(DeviceState::Unauthorized), at)
                .len();
        }

        assert_eq!(notes, 1, "one explanation, not one per flap");
        assert_eq!(
            host.attempt_count(),
            5,
            "the attempts are cheap — the gate refuses before anything is read"
        );
    }

    #[test]
    fn a_flapping_cable_gets_one_attempt_per_cooldown_not_eleven() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::EveryHours(1));
        host.answers([flaky(), flaky()]);
        let mut policy = Policy::new();

        assert_eq!(
            policy
                .on_event(&host, &plugged_in(DeviceState::Connected), t)
                .len(),
            1,
            "the first attempt reports why it could not run"
        );

        // Ten reconnects in the next two minutes: each one is a fresh
        // appearance, and none of them may retry yet.
        for step in 0..10_u64 {
            let at = t + Duration::from_secs(step * 10);
            policy.on_event(&host, &unplugged(), at);
            assert_eq!(
                policy.on_event(&host, &plugged_in(DeviceState::Connected), at),
                Vec::<Note>::new(),
                "step {step} must be inside the cooldown and silent"
            );
        }
        assert_eq!(
            host.attempt_count(),
            1,
            "one attempt in the window, not eleven"
        );

        // Once the cooldown has passed, a reconnect earns one more try.
        let later = t + FAILURE_COOLDOWN + Duration::from_secs(1);
        policy.on_event(&host, &plugged_in(DeviceState::Connected), later);
        assert_eq!(host.attempt_count(), 2);
    }

    #[test]
    fn a_broken_device_stops_being_tried_until_it_is_replugged() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::EveryHours(1));
        host.answers([flaky(), flaky(), flaky()]);
        let mut policy = Policy::new();

        // Only the first failure is reported; the rest of the appearance is
        // silent, and the budget is spent after three tries.
        assert_eq!(
            policy
                .on_event(&host, &plugged_in(DeviceState::Connected), t)
                .len(),
            1
        );
        for step in 1..10_u64 {
            let at = t + FAILURE_COOLDOWN * (step as u32) + Duration::from_secs(1);
            assert!(policy.tick(&host, &ready(), at).is_empty(), "step {step}");
        }
        assert_eq!(
            host.attempt_count(),
            MAX_ATTEMPTS_PER_APPEARANCE as usize,
            "automation gives up after a bounded number of attempts"
        );

        // A new plug-in is a new budget — and the attempt succeeds this time.
        policy.on_event(&host, &unplugged(), t + hours(1));
        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t + hours(1)),
            vec![started()]
        );
    }

    #[test]
    fn a_live_job_makes_automation_wait_without_a_word() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        host.answers([Err(AttemptError::Busy)]);
        let mut policy = Policy::new();

        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t),
            Vec::<Note>::new(),
            "a job is already on screen; a toast about it would be noise"
        );
        assert_eq!(host.attempt_count(), 1, "one attempt, then silence");

        // The user's backup satisfies "back up when connected": automation does
        // not queue a second one behind it.
        policy.tick(&host, &ready(), t + hours(1));
        assert_eq!(host.attempt_count(), 1);
    }

    #[test]
    fn a_paused_manual_job_is_never_resumed() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        host.paused(7, JobTrigger::Manual);
        let mut policy = Policy::new();

        for step in 0..3_u64 {
            let at = t + Duration::from_secs(step * 30);
            policy.on_event(&host, &unplugged(), at);
            assert_eq!(
                policy.on_event(&host, &plugged_in(DeviceState::Connected), at),
                Vec::<Note>::new(),
                "the user pressed Start; Resume is theirs"
            );
        }
        assert_eq!(host.attempt_count() + host.resume_count(), 0);
    }

    #[test]
    fn a_paused_automatic_job_is_resumed_instead_of_restarted() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        host.paused(7, JobTrigger::Auto);
        let mut policy = Policy::new();

        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t),
            vec![resumed(7)]
        );

        let (starts, resumes) = host.calls();
        assert!(
            starts.is_empty(),
            "a reconnect continues the job, it does not add one"
        );
        assert_eq!(resumes, vec![("SER1".to_string(), 7)]);
    }

    #[test]
    fn a_job_that_keeps_stopping_stops_being_resumed_and_asks_for_a_person() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        // The same job pauses again after every reconnect, and the cable keeps
        // flapping. The budget is per job, so the appearances resetting do not
        // reset it.
        for attempt in 0..3_u64 {
            let at = t + Duration::from_secs(attempt * 60);
            host.paused(7, JobTrigger::Auto);
            let notes = policy.on_event(&host, &plugged_in(DeviceState::Connected), at);
            match attempt {
                0 | 1 => assert_eq!(notes, vec![resumed(7)], "attempt {attempt} resumes"),
                _ => assert_eq!(
                    notes,
                    vec![Note::NeedsAttention {
                        serial: "SER1".to_string(),
                        reason: GIVE_UP_REASON.to_string(),
                    }],
                    "the third time it stops, automation gives up"
                ),
            }
            policy.on_event(&host, &unplugged(), at + Duration::from_secs(30));
        }

        assert_eq!(host.resume_count(), MAX_RESUMES_PER_JOB as usize);
        // And it says so exactly once, however many times the phone reappears.
        for step in 0..5_u64 {
            let at = t + hours(1 + step);
            host.paused(7, JobTrigger::Auto);
            assert_eq!(
                policy.on_event(&host, &plugged_in(DeviceState::Connected), at),
                Vec::<Note>::new()
            );
        }
    }

    #[test]
    fn a_different_paused_job_gets_a_fresh_resume_budget() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        host.paused(7, JobTrigger::Auto);
        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t),
            vec![resumed(7)]
        );
        policy.on_event(&host, &unplugged(), t + Duration::from_secs(10));

        // The user finished job 7 by hand and started job 8, which then paused.
        // A new job gets its own budget.
        host.paused(8, JobTrigger::Auto);
        assert_eq!(
            policy.on_event(
                &host,
                &plugged_in(DeviceState::Connected),
                t + Duration::from_secs(20)
            ),
            vec![resumed(8)]
        );
    }

    #[test]
    fn automation_off_is_inert() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::Off);
        host.paused(7, JobTrigger::Auto);
        let mut policy = Policy::new();

        assert!(policy
            .on_event(&host, &plugged_in(DeviceState::Connected), t)
            .is_empty());
        assert!(policy.tick(&host, &ready(), t + hours(100)).is_empty());
        assert_eq!(
            host.attempt_count() + host.resume_count(),
            0,
            "an unenrolled device is untouched, even with its own job paused"
        );
    }

    #[test]
    fn an_unknown_device_is_untouched() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        assert!(policy
            .on_event(
                &host,
                &DeviceEvent::FirstSeen(device("OTHER", DeviceState::Connected)),
                t
            )
            .is_empty());
        assert_eq!(host.schedule("OTHER"), Schedule::Off);
        assert_eq!(host.attempt_count(), 0);
    }

    #[test]
    fn a_long_interval_waits_for_the_clock() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::EveryHours(6));
        let mut policy = Policy::new();

        // Never backed up: due now.
        assert_eq!(
            policy.on_event(&host, &plugged_in(DeviceState::Connected), t),
            vec![started()]
        );
        assert_eq!(host.attempt_count(), 1);

        // Plugged in again an hour later: not due, and not a word about it.
        host.ran_ago(hours(1));
        policy.on_event(&host, &unplugged(), t + hours(1));
        assert!(policy
            .on_event(&host, &plugged_in(DeviceState::Connected), t + hours(1))
            .is_empty());
        assert_eq!(host.attempt_count(), 1);

        // Seven hours after the last attempt, a tick is enough.
        host.ran_ago(hours(7));
        assert_eq!(policy.tick(&host, &ready(), t + hours(7)), vec![started()]);
        assert_eq!(host.attempt_count(), 2);
    }

    #[test]
    fn ticks_ignore_devices_that_are_not_ready() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        for state in [
            DeviceState::Unauthorized,
            DeviceState::Offline,
            DeviceState::Error,
            DeviceState::Disconnected,
        ] {
            assert!(policy.tick(&host, &[device("SER1", state)], t).is_empty());
        }
        assert_eq!(host.attempt_count(), 0);
    }

    #[test]
    fn a_clock_that_disagrees_with_the_manifest_does_not_cause_a_storm() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::EveryHours(1));
        let mut policy = Policy::new();

        policy.on_event(&host, &plugged_in(DeviceState::Connected), t);

        // A job row stamped ten hours in the future (a laptop whose clock was
        // wrong, then corrected). Until real time catches up it must read as
        // "just now", never as "never ran": a clock skew must not back up a
        // phone on every tick.
        host.ran_ahead(hours(10));
        for step in 0..10_u64 {
            assert!(
                policy.tick(&host, &ready(), t + hours(step)).is_empty(),
                "step {step} is before the timestamp and must do nothing"
            );
        }

        // And it is a guard, not a permanent lock: once the clock catches up,
        // the schedule is honoured again.
        assert_eq!(policy.tick(&host, &ready(), t + hours(11)), vec![started()]);
    }

    #[test]
    fn a_repeated_connected_event_is_still_one_run() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        policy.on_event(&host, &plugged_in(DeviceState::Connected), t);
        // A watcher that repeats a transition (a duplicated event, a re-poll)
        // must not multiply the attempts.
        policy.on_event(
            &host,
            &DeviceEvent::Connected(device("SER1", DeviceState::Connected)),
            t,
        );
        assert_eq!(host.attempt_count(), 1);
    }

    #[test]
    fn events_about_other_devices_do_not_disturb_the_one_we_care_about() {
        let t = origin();
        let host = FakeHost::new(t, Schedule::OnConnect);
        let mut policy = Policy::new();

        // Poll failures and other devices' noise are not this device's business.
        assert!(policy
            .on_event(
                &host,
                &DeviceEvent::PollFailed {
                    message: "adb is not running".to_string()
                },
                t
            )
            .is_empty());
        assert!(policy
            .on_event(&host, &DeviceEvent::PollRecovered, t)
            .is_empty());
        assert!(policy
            .on_event(
                &host,
                &DeviceEvent::StateChanged {
                    serial: "SER1".to_string(),
                    from: DeviceState::Connected,
                    to: DeviceState::Error,
                },
                t
            )
            .is_empty());
        assert_eq!(host.attempt_count(), 0);
    }
}
