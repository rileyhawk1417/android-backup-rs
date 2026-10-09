//! Automation: when backups run without anyone pressing a button (Wave 9).
//!
//! This module is pure policy and clock arithmetic — it owns no threads and no
//! I/O of its own. [`schedule`] answers "is a backup due?"; the connection
//! policy (E09.2) and the ticking driver (E09.3) sit on top of it.

pub mod schedule;

pub use schedule::{Schedule, MAX_INTERVAL_HOURS, MIN_INTERVAL_HOURS};
