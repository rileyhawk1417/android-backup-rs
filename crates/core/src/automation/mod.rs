//! Automation: when backups run without anyone pressing a button (Wave 9).
//!
//! This module is pure policy and clock arithmetic — it owns no threads and no
//! I/O of its own. [`schedule`] answers "is a backup due?"; [`policy`] decides
//! whether an automatic attempt may happen at all (E09.2); the ticking driver
//! (E09.3) sits on top of both.

pub mod policy;
pub mod schedule;

pub use policy::{AttemptError, Host, Note, PausedJob, Policy};
pub use schedule::{Schedule, MAX_INTERVAL_HOURS, MIN_INTERVAL_HOURS};
