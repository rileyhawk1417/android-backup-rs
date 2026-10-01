//! Minimal time helpers.
//!
//! All timestamps stored in the database are ISO-8601 UTC strings so they sort
//! lexicographically and are unambiguous. Phone-reported times (file mtimes) are
//! stored as unix seconds in the manifest and are *never* conflated with these.

use chrono::{DateTime, SecondsFormat, TimeZone, Utc};

/// Current UTC time as an ISO-8601 string (`2026-01-01T12:00:00Z`).
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true)
}

/// Current unix time in seconds.
pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}

/// Convert unix seconds to an ISO-8601 UTC string.
pub fn unix_to_iso(secs: i64) -> String {
    match Utc.timestamp_opt(secs, 0) {
        chrono::LocalResult::Single(t) => t.to_rfc3339_opts(SecondsFormat::Secs, true),
        _ => unix_fallback(secs),
    }
}

fn unix_fallback(secs: i64) -> String {
    // Out-of-range timestamps are recorded verbatim rather than dropped.
    format!("@{secs}")
}

/// Parse an ISO-8601 string back to unix seconds, if it round-trips.
pub fn iso_to_unix(iso: &str) -> Option<i64> {
    DateTime::parse_from_rfc3339(iso)
        .ok()
        .map(|dt| dt.timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn now_iso_is_rfc3339_utc() {
        let s = now_iso();
        assert!(s.ends_with('Z'), "{s}");
        assert_eq!(s.len(), 20, "{s}");
        assert!(iso_to_unix(&s).is_some());
    }

    #[test]
    fn round_trip() {
        let secs = 1_700_000_000;
        let iso = unix_to_iso(secs);
        assert_eq!(iso_to_unix(&iso), Some(secs));
    }

    #[test]
    fn out_of_range_is_recorded_verbatim() {
        assert_eq!(unix_to_iso(i64::MAX), format!("@{}", i64::MAX));
    }
}
