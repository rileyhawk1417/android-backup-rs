//! OS notifications: what the app says when nobody is looking at the window
//! (Wave 9, E09.4).
//!
//! The tray (E09.5) is the reason this exists. Once closing the window means the
//! app keeps running, an automatic backup can start, pause or fail with no window
//! on screen. A toast is the *second channel for the same events the panel already
//! renders* — never a second source of truth, and never somewhere a new state can
//! appear that the view model does not already have.
//!
//! # Delivery policy
//!
//! Deliberately small, and all of it in [`Delivery`] and [`Notifications::announce`]:
//!
//! * **Success is suppressed while the user is looking at the window.** The panel is
//!   already showing that backup, and a toast over our own window is noise.
//!   "Looking at" means focused and not minimized, which the frame loop reports
//!   through [`Notifications::set_window_visible`]. See [`NoticeKind::is_progress`].
//! * **Trouble is never suppressed.** A pause, a fatal stop, or a "here is why
//!   nothing started" explanation needs a witness even with the window in front of
//!   the user.
//! * **Repeats are coalesced per device and per kind** for [`COOLDOWN`]. An automatic
//!   job that fails, retries and fails again must not turn the action centre into a
//!   log; a *different* phone's news is never delayed by another's.
//!
//! # Failure is decoration, not a dependency
//!
//! Nothing here may hold up a backup. [`Notifier::show`] is called from the UI
//! thread and from the automation driver, so it must not block: [`SystemNotifier`]
//! hands the notice to a worker thread and never waits for it, even if the desktop
//! bus is wedged. A toast that fails — no notification daemon, Focus Assist, a
//! Windows toast dropped for want of a registered AppUserModelID — only logs. The
//! manifest, the status bar and (E09.5) the tray icon keep telling the truth.
//!
//! # Windows
//!
//! See [`APP_ID`]: toasts are shown under an AppUserModelID, and until a shortcut
//! carrying ours exists the toast wears a generic identity and is the first thing
//! Focus Assist throws away. A suppressed toast is not a lost backup.

use android_backup_core::automation::driver::NoteSink;
use android_backup_core::automation::Note;
use android_backup_core::backup::{JobOutcome, PauseReason};
use android_backup_core::database::devices;
use android_backup_core::database::jobs::JobStatus;
use android_backup_core::database::Db;

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crate::view;

/// How long the same news about the same device is not news any more.
///
/// Long enough to swallow the retry storm of an automatic job that fails, is
/// retried and fails again; short enough that a phone whose cable really did
/// flap twice this hour is reported twice.
pub const COOLDOWN: Duration = Duration::from_secs(60);

/// The Windows AppUserModelID, and (on Linux) nothing.
///
/// Windows shows a toast as an application, not as a process: the toast is
/// attributed to whatever owns this id, and the icon and name come from the Start
/// Menu shortcut that registered it. Our releases are a plain zip
/// (`scripts/package-windows.ps1`), so no shortcut carries this id yet: the toast
/// is then shown under a generic identity instead, and is the first candidate for
/// suppression when Focus Assist is on. Creating that shortcut at install or first
/// run is a packaging decision for E09.7, and it must use *this* string.
pub const APP_ID: &str = "AndroidBackup.Desktop";

/// The shortest gap between two toasts, whatever the platform does with them.
///
/// Two different kinds of news about one device — "completed" and, a moment later,
/// "paused" — are both worth showing, but showing them in the same second looks
/// like a glitch. The OS queues toasts anyway; this only keeps the queue tidy.
const MIN_GAP: Duration = Duration::from_millis(300);

/// Notices the worker will hold before it starts dropping them.
///
/// Only a runaway producer could fill this: [`Delivery`] already collapses repeats,
/// and the events behind these notices are at most a handful per job.
const QUEUE_CAPACITY: usize = 8;

/// How many `(device, kind)` pairs [`Delivery`] remembers. A machine that has seen
/// a hundred phones should not keep all of them for the life of the process.
const MAX_TRACKED: usize = 64;

/// How loud a notice is allowed to be, where the platform cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

/// What a notice is *about*. Determines its urgency, whether a visible window
/// silences it, and which repetitions count as the same news.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum NoticeKind {
    /// An automatic backup began or continued (E09.2/E09.3). Informational: the
    /// phone starting to chatter on plug-in is explained rather than mysterious.
    Started,
    /// A job finished and did what it was asked.
    Completed,
    /// A job paused and cannot continue on its own.
    Paused,
    /// A job, or automation, stopped with an error.
    Stopped,
    /// An automatic attempt was refused for a reason the user can fix.
    Blocked,
    /// Automation has given up and wants a person.
    Attention,
}

impl NoticeKind {
    /// Is this news the window already delivers? Only progress can be suppressed
    /// while the user is looking at the panel — trouble always gets through.
    pub fn is_progress(&self) -> bool {
        matches!(self, Self::Started | Self::Completed)
    }

    /// How loud to be. Where the platform ignores urgency, this is only a hint.
    pub fn urgency(&self) -> Urgency {
        match self {
            Self::Started => Urgency::Low,
            Self::Completed | Self::Paused | Self::Blocked => Urgency::Normal,
            Self::Stopped | Self::Attention => Urgency::Critical,
        }
    }
}

/// One thing to tell the user, already worded.
///
/// Plain data — no platform types and no handles — so the payload can be asserted
/// in a test without a notification daemon anywhere near it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notice {
    pub kind: NoticeKind,
    /// The device this is about, named the way the panel names it (the user's name
    /// for it if they set one, otherwise what the phone calls itself). Kept apart
    /// from the text so one phone's news cannot silence another's.
    pub device: String,
    /// First line of the toast.
    pub title: String,
    /// Second line of the toast: the detail that makes it worth reading.
    pub body: String,
}

impl Notice {
    /// A job reached a terminal state.
    ///
    /// `None` when there is nothing to say: a job the user cancelled, and a job
    /// whose terminal state is the pause that was already announced, are
    /// bookkeeping rather than news. `kept` is how many earlier versions were
    /// archived — the panel counts them, and a toast that omitted them would be
    /// describing a different backup.
    pub fn finished(device: String, outcome: &JobOutcome, kept: u64) -> Option<Self> {
        let kept = match kept {
            0 => String::new(),
            1 => ", 1 kept as an earlier version".to_string(),
            n => format!(", {n} kept as earlier versions"),
        };
        let (kind, title, body) = match outcome.status {
            JobStatus::Cancelled | JobStatus::Paused => return None,
            JobStatus::Completed => (
                NoticeKind::Completed,
                "Backup complete".to_string(),
                format!(
                    "{device}: {}{kept} in {}",
                    view::outcome_detail(outcome),
                    view::format_duration(outcome.duration)
                ),
            ),
            _ => (
                NoticeKind::Stopped,
                "Backup failed".to_string(),
                format!(
                    "{device}: {} ({})",
                    view::outcome_detail(outcome),
                    view::outcome_reason(outcome)
                ),
            ),
        };
        Some(Self {
            kind,
            device,
            title,
            body,
        })
    }

    /// A job paused on its own and is waiting for the user.
    ///
    /// `None` when the user pressed Pause: they know. The reason text is the
    /// engine's, unmodified, so the toast says exactly what the panel's strip says;
    /// the file in flight is named underneath when the engine reported one, since
    /// "the drive is nearly full" without saying *where* it stopped is only half an
    /// explanation.
    pub fn paused(device: String, reason: PauseReason, file: Option<&str>) -> Option<Self> {
        if !reason.is_automatic() {
            return None;
        }
        let body = match file {
            Some(file) => format!(
                "{device}: {}\n{}",
                reason.message(),
                middle_shortened(file, 96)
            ),
            None => format!("{device}: {}", reason.message()),
        };
        Some(Self {
            kind: NoticeKind::Paused,
            device,
            title: "Backup paused".to_string(),
            body,
        })
    }

    /// A job stopped for a reason it cannot come back from.
    pub fn fatal(device: String, message: &str) -> Self {
        Self {
            kind: NoticeKind::Stopped,
            device: device.clone(),
            title: "Backup stopped".to_string(),
            body: format!("{device}: {message}"),
        }
    }

    /// What automation did, in the policy's own words (E09.2).
    ///
    /// The reason strings come from the gate and the engine, so they read the same
    /// here as they do in the panel — the one wording rule for "why is the backup
    /// stuck".
    pub fn automation(note: &Note, device: String) -> Self {
        match note {
            Note::Started { .. } => Self {
                kind: NoticeKind::Started,
                device: device.clone(),
                title: format!("Backing up {device}"),
                body: "An automatic backup started.".to_string(),
            },
            Note::Resumed { .. } => Self {
                kind: NoticeKind::Started,
                device: device.clone(),
                title: format!("Backing up {device}"),
                body: "An automatic backup resumed where it stopped.".to_string(),
            },
            Note::Blocked { reason, .. } => Self {
                kind: NoticeKind::Blocked,
                device: device.clone(),
                body: format!("{device}: {reason}"),
                title: "Backup didn't start".to_string(),
            },
            Note::Failed { reason, .. } => Self {
                kind: NoticeKind::Stopped,
                device: device.clone(),
                title: "Automatic backup failed".to_string(),
                body: format!("{device}: {reason}"),
            },
            Note::NeedsAttention { reason, .. } => Self {
                kind: NoticeKind::Attention,
                device: device.clone(),
                title: "Backup needs your attention".to_string(),
                body: format!("{device}: {reason}"),
            },
        }
    }
}

/// Shorten a path from the middle, keeping the file name.
///
/// Toasts are narrow and a phone's paths are long: the tail is the part that
/// identifies the file, and the head is the part that identifies the folder.
fn middle_shortened(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let head = limit / 2;
    let tail = limit - head - 3;
    format!(
        "{}...{}",
        chars[..head].iter().collect::<String>(),
        chars[chars.len() - tail..].iter().collect::<String>()
    )
}

/// Somewhere a notice can be shown. One implementation per OS, plus a recorder for
/// tests.
///
/// Implementations must return promptly: the callers are the UI thread and the
/// automation driver, and neither may wait on a notification daemon.
pub trait Notifier: Send + Sync {
    fn show(&self, notice: &Notice);
}

/// The rules that decide whether a notice is worth showing, and when not.
///
/// Pure and clock-injected: the same notice, the same moment, the same answer.
#[derive(Debug, Default)]
pub struct Delivery {
    /// When each `(device, kind)` was last shown.
    recent: HashMap<(String, NoticeKind), Instant>,
}

impl Delivery {
    /// Should this notice be shown? Records it when the answer is yes.
    pub fn admit(&mut self, notice: &Notice, window_visible: bool, now: Instant) -> bool {
        // The panel is already saying it, and it is saying it about a job that is
        // going the way the user wanted.
        if window_visible && notice.kind.is_progress() {
            return false;
        }
        let key = (notice.device.clone(), notice.kind);
        if self
            .recent
            .get(&key)
            .is_some_and(|at| now.saturating_duration_since(*at) < COOLDOWN)
        {
            return false;
        }
        if self.recent.len() >= MAX_TRACKED {
            self.recent
                .retain(|_, at| now.saturating_duration_since(*at) < COOLDOWN);
            // Everything in the table is recent — a hundred devices in the same
            // minute — so make room by forgetting the least recent news.
            while self.recent.len() >= MAX_TRACKED {
                let oldest = self
                    .recent
                    .iter()
                    .min_by_key(|(_, at)| **at)
                    .map(|(key, _)| key.clone());
                match oldest {
                    Some(key) => self.recent.remove(&key),
                    None => break,
                };
            }
        }
        self.recent.insert(key, now);
        true
    }
}

/// The application's outbound notification channel: what the frame loop, the job
/// pump and the automation driver all talk to.
///
/// Cheap to share ([`Arc`]) because the driver thread holds one too.
pub struct Notifications {
    /// `None` when no notifier was installed — a headless run, or a test that does
    /// not care. Notices are then dropped with a debug log rather than inventing a
    /// fallback channel.
    notifier: Option<Arc<dyn Notifier>>,
    /// Is the user looking at the window? See [`Self::set_window_visible`].
    window_visible: AtomicBool,
    delivery: Mutex<Delivery>,
}

impl Notifications {
    /// A channel with no notifier: everything [`Self::announce`]s is dropped.
    pub fn disabled() -> Self {
        Self::new(None)
    }

    pub fn new(notifier: Option<Arc<dyn Notifier>>) -> Self {
        Self {
            notifier,
            // The window is about to exist; assume the user is looking at it until a
            // frame says otherwise. Erring this way means erring towards silence.
            window_visible: AtomicBool::new(true),
            delivery: Mutex::new(Delivery::default()),
        }
    }

    /// Is there anything to announce *to*?
    pub fn is_enabled(&self) -> bool {
        self.notifier.is_some()
    }

    /// Tell the channel whether the user is looking at the window right now.
    ///
    /// The frame loop reports what `egui` knows (focused and not minimized). A
    /// hidden window stops drawing, so whatever hides it — the tray (E09.5) — has to
    /// say so explicitly, otherwise the last frame before hiding would leave this
    /// stuck at "yes, they are watching".
    /// Is the window currently visible and focused? What [`Self::set_window_visible`]
    /// was last told, and what decides whether progress notices are worth a toast.
    pub fn window_visible(&self) -> bool {
        self.window_visible.load(Ordering::Relaxed)
    }

    pub fn set_window_visible(&self, visible: bool) {
        self.window_visible.store(visible, Ordering::Relaxed);
    }

    /// Show a notice, if the delivery policy allows it. Never blocks, never fails.
    pub fn announce(&self, notice: Notice) {
        let Some(notifier) = self.notifier.as_ref() else {
            tracing::debug!(kind = ?notice.kind, device = %notice.device,
                "notification dropped: none installed");
            return;
        };
        let admitted = {
            let mut delivery = lock(&self.delivery);
            delivery.admit(
                &notice,
                self.window_visible.load(Ordering::Relaxed),
                Instant::now(),
            )
        };
        if !admitted {
            tracing::debug!(kind = ?notice.kind, device = %notice.device,
                "notification coalesced");
            return;
        }
        tracing::debug!(kind = ?notice.kind, device = %notice.device, title = %notice.title,
            "showing a notification");
        notifier.show(&notice);
    }
}

/// A mutex guard that survives a poisoned lock: one panicking frame must not turn
/// every later notification into a different failure.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// OS notifications, one worker thread for all of them.
///
/// The thread exists for two reasons: `notify-rust` blocks while the desktop bus
/// answers (and *forever* if the daemon is wedged), and toasts shown back to back
/// look like a malfunction. Both are the worker's problem, and neither can reach
/// the caller: [`Notifier::show`] is a channel send.
pub struct SystemNotifier {
    /// `None` once dropped: closing it is what stops the worker.
    sender: Option<SyncSender<Notice>>,
    worker: Option<JoinHandle<()>>,
}

impl SystemNotifier {
    pub fn new() -> Self {
        Self::with_delivery(Box::new(deliver), MIN_GAP)
    }

    /// The same queue and the same pacing, with something other than the OS at the
    /// other end. The tests use this to pin the rules that matter (nothing waits on
    /// the caller, the overflow is dropped, notices keep their order) without a
    /// notification daemon anywhere near them.
    fn with_delivery(show: Box<dyn Fn(&Notice) + Send>, gap: Duration) -> Self {
        let (sender, receiver) = sync_channel(QUEUE_CAPACITY);
        let worker = std::thread::Builder::new()
            .name("notifications".to_string())
            .spawn(move || show_loop(receiver, show, gap))
            .ok();
        Self {
            sender: Some(sender),
            worker,
        }
    }
}

impl Default for SystemNotifier {
    fn default() -> Self {
        Self::new()
    }
}

impl Notifier for SystemNotifier {
    fn show(&self, notice: &Notice) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        match sender.try_send(notice.clone()) {
            Ok(()) => {}
            // Never wait: the backup in progress matters more than the toast about
            // it. A full queue means the OS is not keeping up, which is its answer.
            Err(TrySendError::Full(_)) => {
                tracing::warn!(kind = ?notice.kind, "notification queue is full; dropping one");
            }
            Err(TrySendError::Disconnected(_)) => {
                tracing::debug!("notification worker is gone; dropping one");
            }
        }
    }
}

impl Drop for SystemNotifier {
    fn drop(&mut self) {
        // Closing the channel ends the loop; the join is bounded by MIN_GAP, since
        // the worst the worker can be doing is pacing a toast.
        self.sender = None;
        if let Some(worker) = self.worker.take() {
            if worker.join().is_err() {
                tracing::warn!("the notification worker panicked");
            }
        }
    }
}

/// The worker: one toast at a time, spaced out, and a failure never ends the loop.
fn show_loop(receiver: Receiver<Notice>, show: Box<dyn Fn(&Notice) + Send>, gap: Duration) {
    let mut last: Option<Instant> = None;
    while let Ok(notice) = receiver.recv() {
        if let Some(previous) = last {
            let wait = gap.saturating_sub(previous.elapsed());
            if !wait.is_zero() {
                std::thread::sleep(wait);
            }
        }
        show(&notice);
        last = Some(Instant::now());
    }
}

/// Hand one notice to the OS. Errors are logged: a toast is decoration on top of
/// state the window already holds, not a step that can be retried into working.
fn deliver(notice: &Notice) {
    let urgency = match notice.kind.urgency() {
        Urgency::Low => notify_rust::Urgency::Low,
        Urgency::Normal => notify_rust::Urgency::Normal,
        Urgency::Critical => notify_rust::Urgency::Critical,
    };
    let mut toast = notify_rust::Notification::new();
    toast
        .appname("android-backup")
        .summary(&notice.title)
        .body(&notice.body)
        .urgency(urgency);
    // Only Windows has an AppUserModelID to set; `app_id` does not exist elsewhere.
    #[cfg(target_os = "windows")]
    toast.app_id(APP_ID);
    match toast.show() {
        Ok(_) => tracing::debug!(title = %notice.title, "notification shown"),
        Err(err) => tracing::warn!(error = %err, title = %notice.title,
            "could not show a notification"),
    }
}

/// What automation says when the window may not be listening (E09.5).
///
/// The clock runs on its own thread and knows a device only by serial, so this is the
/// one thing it is handed back: a way to say what it did. It announces *directly*
/// rather than posting to the frame loop, because close-to-tray is the whole point —
/// a hidden window stops drawing, and a note queued for a window that is not drawing
/// would be delivered late or never.
///
/// The manifest is read here to name the phone the way the panel names it, including
/// the user's own name for it. A read on the driver's thread is the trade the policy
/// already makes when it asks about `last_attempt`.
pub struct AutomationToasts {
    notifications: Arc<Notifications>,
    /// `None` in a run without a manifest: the serial then stands in for the name,
    /// which at least says *which* phone.
    db: Option<Arc<Db>>,
}

impl AutomationToasts {
    pub fn new(notifications: Arc<Notifications>, db: Option<Arc<Db>>) -> Self {
        Self { notifications, db }
    }

    /// The name the panel is showing for `serial`, or the serial itself.
    fn device_name(&self, serial: &str) -> String {
        let name = self
            .db
            .as_ref()
            .and_then(|db| devices::resolve_active_device(db, serial).ok().flatten())
            .map(|row| row.display_name);
        match name {
            Some(name) if !name.trim().is_empty() => name,
            _ => serial.to_string(),
        }
    }
}

impl NoteSink for AutomationToasts {
    fn note(&self, note: Note) {
        let device = self.device_name(note.serial());
        self.notifications
            .announce(Notice::automation(&note, device));
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

    use super::*;
    use android_backup_core::database::jobs::JobCounters;
    use std::time::Duration;

    use crate::testutil::RecordingNotifier;

    fn outcome(status: JobStatus, error: Option<&str>) -> JobOutcome {
        JobOutcome {
            job_id: 7,
            device_id: 1,
            status,
            counters: JobCounters {
                scanned: 1615,
                copied: 412,
                skipped: 1203,
                failed: if status == JobStatus::Failed { 3 } else { 0 },
                bytes_copied: 1024,
            },
            error_summary: error.map(str::to_string),
            duration: Duration::from_secs(192),
            failures: Vec::new(),
        }
    }

    fn notices(sink: &RecordingNotifier) -> Vec<Notice> {
        sink.notices()
    }

    #[test]
    fn a_finished_job_says_what_it_did_in_the_panels_own_words() {
        let notice = Notice::finished(
            "Pixel 8".to_string(),
            &outcome(JobStatus::Completed, None),
            0,
        )
        .expect("news");
        assert_eq!(notice.kind, NoticeKind::Completed);
        assert_eq!(notice.title, "Backup complete");
        assert_eq!(
            notice.body, "Pixel 8: 412 copied, 1203 unchanged, 0 failed in 3m 12s",
            "the toast and the panel's strip must not be able to disagree"
        );
        assert_eq!(notice.device, "Pixel 8");
    }

    #[test]
    fn archived_versions_are_counted_because_the_panel_counts_them() {
        let notice = Notice::finished(
            "Pixel 8".to_string(),
            &outcome(JobStatus::Completed, None),
            2,
        )
        .expect("news");
        assert_eq!(
            notice.body,
            "Pixel 8: 412 copied, 1203 unchanged, 0 failed, 2 kept as earlier versions in 3m 12s"
        );
        let one = Notice::finished(
            "Pixel 8".to_string(),
            &outcome(JobStatus::Completed, None),
            1,
        )
        .expect("news");
        assert!(
            one.body.contains(", 1 kept as an earlier version in"),
            "{}",
            one.body
        );
    }

    #[test]
    fn a_failed_job_carries_the_engines_reason() {
        let notice = Notice::finished(
            "Pixel 8".to_string(),
            &outcome(JobStatus::Failed, Some("the destination disappeared")),
            0,
        )
        .expect("news");
        assert_eq!(notice.kind, NoticeKind::Stopped);
        assert_eq!(notice.title, "Backup failed");
        assert_eq!(
            notice.body,
            "Pixel 8: 412 copied, 1203 unchanged, 3 failed (the destination disappeared)"
        );
        assert_eq!(notice.kind.urgency(), Urgency::Critical);
    }

    #[test]
    fn a_job_the_user_ended_is_not_news() {
        for status in [JobStatus::Cancelled, JobStatus::Paused] {
            assert!(
                Notice::finished("Pixel 8".to_string(), &outcome(status, None), 0).is_none(),
                "{status:?} is bookkeeping, not news"
            );
        }
        assert!(
            Notice::paused("Pixel 8".to_string(), PauseReason::UserRequested, None).is_none(),
            "the user knows: they pressed Pause"
        );
    }

    #[test]
    fn a_pause_keeps_the_reason_and_names_the_file() {
        let notice = Notice::paused(
            "Pixel 8".to_string(),
            PauseReason::DeviceDisconnected,
            Some("/storage/emulated/0/DCIM/Camera/VID_20240811_181500.mp4"),
        )
        .expect("news");
        assert_eq!(notice.kind, NoticeKind::Paused);
        assert_eq!(notice.title, "Backup paused");
        assert_eq!(
            notice.body,
            "Pixel 8: The phone disconnected. Reconnect it and press Resume to continue \
             where the backup stopped.\n/storage/emulated/0/DCIM/Camera/VID_20240811_181500.mp4",
            "the reason is the engine's own wording, and the file in flight is named"
        );
        assert_eq!(
            notice.kind.urgency(),
            Urgency::Normal,
            "trouble is loud enough"
        );

        let without_file =
            Notice::paused("Pixel 8".to_string(), PauseReason::LowSpace, None).expect("news");
        assert_eq!(
            without_file.body,
            "Pixel 8: The backup drive is nearly full. Free some space, then press Resume."
        );
    }

    #[test]
    fn a_long_path_is_shortened_from_the_middle() {
        let long = format!("/storage/emulated/0/{}z", "d".repeat(300));
        let shortened = middle_shortened(&long, 40);
        assert_eq!(shortened.chars().count(), 40);
        assert!(shortened.ends_with('z'), "{shortened}");
        assert!(shortened.starts_with("/storage/emulated/0/"), "{shortened}");
        assert!(shortened.contains("..."));
        assert_eq!(middle_shortened("/a/b.mp4", 40), "/a/b.mp4");
    }

    #[test]
    fn a_fatal_stop_names_the_job_and_the_device() {
        let notice = Notice::fatal("Pixel 8".to_string(), "there is no storage root");
        assert_eq!(notice.kind, NoticeKind::Stopped);
        assert_eq!(notice.title, "Backup stopped");
        assert_eq!(notice.body, "Pixel 8: there is no storage root");
    }

    #[test]
    fn automation_notes_read_like_the_panel() {
        let started = Notice::automation(
            &Note::Started {
                serial: "SER1".to_string(),
            },
            "Pixel 8".to_string(),
        );
        assert_eq!(started.kind, NoticeKind::Started);
        assert_eq!(started.title, "Backing up Pixel 8");
        assert_eq!(started.body, "An automatic backup started.");

        let resumed = Notice::automation(
            &Note::Resumed {
                serial: "SER1".to_string(),
                job_id: 7,
            },
            "Pixel 8".to_string(),
        );
        assert_eq!(resumed.kind, NoticeKind::Started);
        assert!(
            resumed.body.contains("resumed where it stopped"),
            "{}",
            resumed.body
        );

        let blocked = Notice::automation(
            &Note::Blocked {
                serial: "SER1".to_string(),
                reason: "USB debugging is off".to_string(),
            },
            "Pixel 8".to_string(),
        );
        assert_eq!(blocked.kind, NoticeKind::Blocked);
        assert_eq!(blocked.title, "Backup didn't start");
        assert_eq!(blocked.body, "Pixel 8: USB debugging is off");

        let failed = Notice::automation(
            &Note::Failed {
                serial: "SER1".to_string(),
                reason: "the destination is not writable".to_string(),
            },
            "Pixel 8".to_string(),
        );
        assert_eq!(failed.kind, NoticeKind::Stopped);
        assert_eq!(failed.title, "Automatic backup failed");

        let attention = Notice::automation(
            &Note::NeedsAttention {
                serial: "SER1".to_string(),
                reason: "three attempts failed".to_string(),
            },
            "Pixel 8".to_string(),
        );
        assert_eq!(attention.kind, NoticeKind::Attention);
        assert_eq!(attention.title, "Backup needs your attention");
        assert_eq!(attention.kind.urgency(), Urgency::Critical);
    }

    fn completed() -> Notice {
        Notice::finished(
            "Pixel 8".to_string(),
            &outcome(JobStatus::Completed, None),
            0,
        )
        .expect("news")
    }

    #[test]
    fn success_is_suppressed_while_the_user_is_looking_at_the_window() {
        let mut delivery = Delivery::default();
        let now = Instant::now();
        assert!(
            !delivery.admit(&completed(), true, now),
            "the panel is already saying this"
        );
        assert!(
            delivery.admit(&completed(), false, now),
            "hidden: the toast is the only thing that can say it"
        );
        let started = Notice::automation(
            &Note::Started {
                serial: "SER1".to_string(),
            },
            "Pixel 8".to_string(),
        );
        let mut delivery = Delivery::default();
        assert!(!delivery.admit(&started, true, now));
        assert!(delivery.admit(&started, false, now));
    }

    #[test]
    fn trouble_is_never_suppressed_by_a_visible_window() {
        let now = Instant::now();
        let notices = [
            Notice::paused("Pixel 8".to_string(), PauseReason::LowSpace, None).expect("news"),
            Notice::fatal("Pixel 8".to_string(), "no storage root"),
            Notice::finished(
                "Pixel 8".to_string(),
                &outcome(JobStatus::Failed, Some("gone")),
                0,
            )
            .expect("news"),
            Notice::automation(
                &Note::Blocked {
                    serial: "SER1".to_string(),
                    reason: "USB debugging is off".to_string(),
                },
                "Pixel 8".to_string(),
            ),
            Notice::automation(
                &Note::NeedsAttention {
                    serial: "SER1".to_string(),
                    reason: "three attempts failed".to_string(),
                },
                "Pixel 8".to_string(),
            ),
        ];
        for notice in notices {
            let mut delivery = Delivery::default();
            assert!(
                delivery.admit(&notice, true, now),
                "{:?} needs a witness even on screen",
                notice.kind
            );
        }
    }

    #[test]
    fn back_to_back_repeats_are_coalesced_per_device_and_kind() {
        let now = Instant::now();
        let mut delivery = Delivery::default();
        assert!(delivery.admit(&completed(), false, now));
        assert!(
            !delivery.admit(&completed(), false, now + Duration::from_secs(1)),
            "the retry storm of one job is one piece of news"
        );
        assert!(
            delivery.admit(&completed(), false, now + COOLDOWN),
            "an hour later it is news again"
        );

        // A different phone's news is never delayed by this one's.
        let other = Notice::finished(
            "Tablet".to_string(),
            &outcome(JobStatus::Completed, None),
            0,
        )
        .expect("news");
        assert!(delivery.admit(&other, false, now));

        // Neither is different news about the same phone: a job that finishes and
        // then pauses has two things to say.
        let paused =
            Notice::paused("Pixel 8".to_string(), PauseReason::LowSpace, None).expect("news");
        assert!(delivery.admit(&paused, false, now));
    }

    #[test]
    fn the_delivery_table_does_not_grow_forever() {
        let now = Instant::now();
        let mut delivery = Delivery::default();
        for serial in 0..200 {
            let notice = Notice::finished(
                format!("phone {serial}"),
                &outcome(JobStatus::Completed, None),
                0,
            )
            .expect("news");
            assert!(delivery.admit(&notice, false, now));
        }
        assert!(
            delivery.recent.len() <= MAX_TRACKED,
            "forgotten devices must not be remembered for the life of the process: {}",
            delivery.recent.len()
        );
    }

    /// A channel with the window hidden, which is the case most of these tests are
    /// about: with the window in front, progress notices are suppressed by design.
    fn hidden(notifier: Arc<dyn Notifier>) -> Notifications {
        let notifications = Notifications::new(Some(notifier));
        notifications.set_window_visible(false);
        notifications
    }

    #[test]
    fn a_failing_notifier_is_logged_and_forgotten() {
        let sink = Arc::new(RecordingNotifier::failing());
        let notifications = hidden(sink.clone());
        notifications.announce(completed());
        assert_eq!(sink.attempts(), 1, "it was offered to the OS");
        assert!(sink.notices().is_empty(), "and it failed");
        // The next one still goes: a broken toast must not poison the channel.
        notifications.announce(Notice::fatal("Pixel 8".to_string(), "boom"));
        assert_eq!(sink.attempts(), 2);
    }

    #[test]
    fn a_channel_with_no_notifier_drops_quietly() {
        let notifications = Notifications::disabled();
        assert!(!notifications.is_enabled());
        notifications.announce(completed());
        let sink = Arc::new(RecordingNotifier::new());
        let notifications = hidden(sink.clone());
        assert!(notifications.is_enabled());
        notifications.announce(completed());
        assert_eq!(notices(&sink), vec![completed()]);
    }

    #[test]
    fn hiding_the_window_starts_telling_the_truth_about_success() {
        let sink = Arc::new(RecordingNotifier::new());
        let notifications = Notifications::new(Some(sink.clone() as Arc<dyn Notifier>));
        notifications.announce(completed());
        assert!(sink.notices().is_empty(), "visible window: suppressed");
        notifications.set_window_visible(false);
        notifications.announce(completed());
        assert_eq!(sink.notices(), vec![completed()], "hidden: announced once");
    }

    /// A recorder that holds the worker until the test lets it go.
    fn gated() -> (
        Arc<Mutex<Vec<Notice>>>,
        std::sync::mpsc::Sender<()>,
        SystemNotifier,
    ) {
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();
        let shown = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&shown);
        let notifier = SystemNotifier::with_delivery(
            Box::new(move |notice: &Notice| {
                let _ = gate_rx.recv();
                lock(&recorded).push(notice.clone());
            }),
            Duration::ZERO,
        );
        (shown, gate_tx, notifier)
    }

    #[test]
    fn showing_never_waits_for_the_worker_and_drops_the_overflow() {
        let (shown, gate, notifier) = gated();
        let started = Instant::now();
        for _ in 0..20 {
            notifier.show(&completed());
        }
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "the UI thread must not wait on a busy or wedged daemon: {:?}",
            started.elapsed()
        );
        drop(gate);
        drop(notifier); // joins
        let shown = shown.lock().unwrap().len();
        assert!(shown >= 1, "what the queue accepted is shown");
        assert!(
            shown <= QUEUE_CAPACITY + 1,
            "the rest must be dropped, not piled up: {shown}"
        );
    }

    #[test]
    fn the_worker_shows_notices_in_order_and_spaces_them_out() {
        let mut seen = Vec::new();
        let gap = Duration::from_millis(60);
        // The recorder's own clock, so the assertion does not depend on the test's
        // timing: each toast records when it was shown.
        let times = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&times);
        let sink = Arc::new(RecordingNotifier::new());
        let echoed = Arc::clone(&sink);
        let notifier = SystemNotifier::with_delivery(
            Box::new(move |notice: &Notice| {
                lock(&recorded).push(Instant::now());
                echoed.show(notice);
            }),
            gap,
        );
        for _ in 0..3 {
            notifier.show(&completed());
        }
        drop(notifier); // joins, so everything sent has been shown
        seen.extend(sink.titles());
        assert_eq!(seen, vec!["Backup complete"; 3], "order is preserved");
        let times = times.lock().unwrap();
        assert_eq!(times.len(), 3);
        assert!(
            times[1].saturating_duration_since(times[0]) >= gap,
            "two toasts in the same instant look like a malfunction"
        );
        // The first one is not made to wait for a predecessor that never existed.
        assert!(
            times[2].saturating_duration_since(times[1]) >= gap,
            "and so do the next two"
        );
    }
}
