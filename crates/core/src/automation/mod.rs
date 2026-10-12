//! Automation: when backups run without anyone pressing a button (Wave 9).
//!
//! [`schedule`] answers "is a backup due?"; [`policy`] decides whether an
//! automatic attempt may happen at all (E09.2); [`driver`] is the clock that
//! keeps asking both of them (E09.3). The policy and the scheduling arithmetic are
//! pure — no threads, no I/O — and the driver owns the only thread in the module.

pub mod driver;
pub mod policy;
pub mod schedule;

pub use driver::{AutomationDriver, DriverConfig, NoteSink, SharedNoteSink, TickHost};
pub use policy::{AttemptError, Host, Note, PausedJob, Policy};
pub use schedule::{Schedule, MAX_INTERVAL_HOURS, MIN_INTERVAL_HOURS};
