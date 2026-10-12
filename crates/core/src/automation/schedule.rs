//! When automatic backups run — the whole model (Wave 9, E09.1).
//!
//! Deliberately small. A schedule answers one question — "is a backup due
//! now?" — and nothing else. Wall-clock *calendar* scheduling ("every day at
//! 03:00") is not in the model on purpose (wave doc, E09.1 pitfalls): it drags
//! in timezone and missed-run semantics that a phone-backup app does not need.
//! Elapsed time since the last attempt is the entire rule.
//!
//! Time is passed in as [`Instant`], so the model is pure and the tests are
//! deterministic: no clock, no sleeping, no threads. Callers that hold a
//! wall-clock timestamp (the manifest stores ISO-8601 strings) convert to an
//! `Instant` by subtracting the elapsed duration from `Instant::now()` — which
//! also keeps a suspend/resume from confusing the arithmetic.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Bounds for the interval mode, mirrored by the settings UI (E09.6).
pub const MIN_INTERVAL_HOURS: u32 = 1;
pub const MAX_INTERVAL_HOURS: u32 = 72;

/// The interval the settings panel starts from when the user turns "every N
/// hours" on for the first time.
///
/// A day, because that is the unit people think in for this ("back up daily"),
/// and it leaves room in both directions inside the bounds.
pub const DEFAULT_INTERVAL_HOURS: u32 = 24;

const SECS_PER_HOUR: u64 = 3600;

/// When backups run without the user pressing a button.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "mode", content = "hours", rename_all = "snake_case")]
pub enum Schedule {
    /// Never automatic. The default, and the 0.1.0 behaviour: nothing runs
    /// until the user asks.
    #[default]
    Off,
    /// As soon as a device is connected and passes the authorization gate.
    OnConnect,
    /// At most once every `hours`, counted from the last *attempt*.
    EveryHours(u32),
}

impl Schedule {
    /// Is a backup due at `now`, given the last attempt (not the last
    /// completion — a failing backup must not retry itself into a loop)?
    ///
    /// A first-ever run (`None`) is due: "every 6 hours, starting now" is what
    /// a user means by turning this on, and it matches [`Schedule::OnConnect`].
    pub fn due(&self, last_attempt: Option<Instant>, now: Instant) -> bool {
        match self {
            Self::Off => false,
            Self::OnConnect => true,
            Self::EveryHours(hours) => match last_attempt {
                None => true,
                Some(last) => now.saturating_duration_since(last) >= interval(*hours),
            },
        }
    }

    /// When this schedule next comes due, or `None` when it has no clock at
    /// all: `Off` never runs, and `OnConnect` waits for an event rather than a
    /// time.
    ///
    /// An already-due schedule reports `now`, so the caller can render "now"
    /// rather than inventing a time in the past.
    pub fn next_due(&self, last_attempt: Option<Instant>, now: Instant) -> Option<Instant> {
        match self {
            Self::Off | Self::OnConnect => None,
            Self::EveryHours(hours) => {
                let interval = interval(*hours);
                Some(match last_attempt {
                    None => now,
                    Some(last) => last.checked_add(interval).unwrap_or(now).max(now),
                })
            }
        }
    }

    /// Clamp an out-of-range interval into the supported window.
    ///
    /// Applied on config load and by the UI, so a hand-edited `config.toml`
    /// cannot produce an interval the settings panel cannot represent.
    pub fn clamped(self) -> Self {
        match self {
            Self::EveryHours(hours) => {
                Self::EveryHours(hours.clamp(MIN_INTERVAL_HOURS, MAX_INTERVAL_HOURS))
            }
            other => other,
        }
    }

    /// The stable token stored in the manifest (`devices.auto_schedule_kind`),
    /// spelled exactly as in `config.toml` so one word means one thing.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::OnConnect => "on_connect",
            Self::EveryHours(_) => "every_hours",
        }
    }

    /// The interval, when there is one — the second half of the stored pair.
    pub fn hours(&self) -> Option<u32> {
        match self {
            Self::EveryHours(hours) => Some(*hours),
            _ => None,
        }
    }

    /// Rebuild from the manifest's `(kind, hours)` columns.
    ///
    /// Anything unusable — a token from a newer version, NULL, a hand-edited
    /// row — reads as [`Schedule::Off`]. A stored value we cannot understand
    /// must never *enable* automation, only disable it.
    pub fn parse(kind: &str, hours: Option<u32>) -> Self {
        match (kind, hours) {
            ("off", _) => Self::Off,
            ("on_connect", _) => Self::OnConnect,
            ("every_hours", Some(hours)) => Self::EveryHours(hours).clamped(),
            _ => Self::Off,
        }
    }
}

fn interval(hours: u32) -> Duration {
    Duration::from_secs(u64::from(hours) * SECS_PER_HOUR)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A fixed origin; only differences between instants matter.
    fn origin() -> Instant {
        Instant::now()
    }

    fn hours(n: u64) -> Duration {
        Duration::from_secs(n * SECS_PER_HOUR)
    }

    #[test]
    fn off_is_never_due_and_has_no_clock() {
        let t = origin();
        assert!(!Schedule::Off.due(None, t));
        assert!(!Schedule::Off.due(Some(t), t + hours(1_000)));
        assert_eq!(Schedule::Off.next_due(None, t), None);
        assert_eq!(Schedule::Off.next_due(Some(t), t), None);
    }

    #[test]
    fn on_connect_is_due_on_every_check_but_waits_for_an_event() {
        let t = origin();
        // Due with or without a prior attempt: the connection *is* the trigger.
        assert!(Schedule::OnConnect.due(None, t));
        assert!(Schedule::OnConnect.due(Some(t), t));
        assert!(Schedule::OnConnect.due(Some(t), t + hours(500)));
        // But it has no clock, so the UI must say "when connected", not a time.
        assert_eq!(Schedule::OnConnect.next_due(None, t), None);
        assert_eq!(Schedule::OnConnect.next_due(Some(t), t), None);
    }

    #[test]
    fn every_hours_truth_table() {
        let t = origin();
        let s = Schedule::EveryHours(6);

        // First ever: due immediately.
        assert!(s.due(None, t));
        assert_eq!(s.next_due(None, t), Some(t));

        // One second short of the interval: not due.
        assert!(!s.due(Some(t), t + hours(6) - Duration::from_secs(1)));
        // Exactly on the boundary: due. (`>=`, so a tick at the boundary fires.)
        assert!(s.due(Some(t), t + hours(6)));
        // Past it: still due, and it stays due however far past.
        assert!(s.due(Some(t), t + hours(6) + Duration::from_secs(1)));
        assert!(s.due(Some(t), t + hours(6) * 100));
    }

    #[test]
    fn next_due_counts_down_and_never_invents_a_past_time() {
        let t = origin();
        let s = Schedule::EveryHours(6);

        // 4 h in: 2 h to go.
        assert_eq!(s.next_due(Some(t), t + hours(4)), Some(t + hours(6)));
        // Already due: report now, not a time in the past.
        assert_eq!(s.next_due(Some(t), t + hours(7)), Some(t + hours(7)));
        assert_eq!(s.next_due(Some(t), t + hours(100)), Some(t + hours(100)));
    }

    /// Monotonicity, the property the UI's "next run in …" line depends on:
    /// as `now` advances, the answer never moves backwards.
    #[test]
    fn next_due_is_monotonic_in_now() {
        let t = origin();
        let last = Some(t);
        for schedule in [
            Schedule::Off,
            Schedule::OnConnect,
            Schedule::EveryHours(1),
            Schedule::EveryHours(6),
            Schedule::EveryHours(72),
        ] {
            let mut previous: Option<Instant> = None;
            for step in 0..200_u64 {
                let now = t + Duration::from_secs(step * 600);
                let answer = schedule.next_due(last, now);
                if let (Some(prev), Some(current)) = (previous, answer) {
                    assert!(
                        current >= prev,
                        "{schedule:?} went backwards at step {step}: {prev:?} -> {current:?}"
                    );
                }
                previous = answer;
            }
        }
    }

    #[test]
    fn the_default_interval_is_one_the_ui_can_show() {
        // A default outside the bounds would make the settings panel's first
        // click write a value its own control cannot display.
        assert!(
            (MIN_INTERVAL_HOURS..=MAX_INTERVAL_HOURS).contains(&DEFAULT_INTERVAL_HOURS),
            "{DEFAULT_INTERVAL_HOURS} is outside {MIN_INTERVAL_HOURS}..={MAX_INTERVAL_HOURS}"
        );
    }

    #[test]
    fn clamping_brings_a_hand_edited_interval_into_range() {
        assert_eq!(Schedule::EveryHours(0).clamped(), Schedule::EveryHours(1));
        assert_eq!(Schedule::EveryHours(1).clamped(), Schedule::EveryHours(1));
        assert_eq!(
            Schedule::EveryHours(10_000).clamped(),
            Schedule::EveryHours(MAX_INTERVAL_HOURS)
        );
        assert_eq!(Schedule::Off.clamped(), Schedule::Off);
        assert_eq!(Schedule::OnConnect.clamped(), Schedule::OnConnect);
    }

    #[test]
    fn stored_parts_round_trip_and_junk_reads_as_off() {
        for schedule in [
            Schedule::Off,
            Schedule::OnConnect,
            Schedule::EveryHours(1),
            Schedule::EveryHours(72),
        ] {
            assert_eq!(
                Schedule::parse(schedule.as_str(), schedule.hours()),
                schedule
            );
        }

        // Off is the only safe reading of anything we do not understand: a
        // future version's token, a NULL column, a negative interval.
        assert_eq!(Schedule::parse("something_new", None), Schedule::Off);
        assert_eq!(Schedule::parse("every_hours", None), Schedule::Off);
        assert_eq!(Schedule::parse("", Some(6)), Schedule::Off);
        // An interval in the row is clamped, not trusted.
        assert_eq!(
            Schedule::parse("every_hours", Some(100_000)),
            Schedule::EveryHours(MAX_INTERVAL_HOURS)
        );
    }

    #[test]
    fn due_is_robust_even_before_clamping() {
        // A zero interval means "always", which is a coherent reading of a
        // nonsense value; `clamped()` is what keeps it out of the config.
        let t = origin();
        assert!(Schedule::EveryHours(0).due(Some(t), t));
    }
}
