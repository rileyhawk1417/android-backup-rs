//! Backup job records and the status state machine that guards them.

use rusqlite::{params, OptionalExtension, Row};

use super::{Db, DbError};

/// Persisted job status. Transitions are validated before every write so an
/// interrupted run can never silently turn into a "Completed" row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobStatus {
    Planning,
    Scanning,
    Copying,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

impl JobStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Planning => "planning",
            Self::Scanning => "scanning",
            Self::Copying => "copying",
            Self::Paused => "paused",
            Self::Cancelled => "cancelled",
            Self::Completed => "completed",
            Self::Failed => "failed",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "planning" => Self::Planning,
            "scanning" => Self::Scanning,
            "copying" => Self::Copying,
            "paused" => Self::Paused,
            "cancelled" => Self::Cancelled,
            "completed" => Self::Completed,
            "failed" => Self::Failed,
            _ => return None,
        })
    }

    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Cancelled | Self::Completed | Self::Failed)
    }

    /// Whether `self -> next` is a legal transition.
    pub fn can_transition_to(&self, next: JobStatus) -> bool {
        use JobStatus::*;
        if *self == next {
            return true;
        }
        if self.is_terminal() {
            return false;
        }
        match (self, next) {
            (Planning, Scanning | Paused | Failed | Cancelled) => true,
            (Scanning, Copying | Paused | Failed | Cancelled) => true,
            (Scanning, Completed) => true, // nothing to copy
            (Copying, Paused | Failed | Cancelled | Completed) => true,
            (Paused, Scanning | Copying | Cancelled) => true,
            _ => false,
        }
    }
}

/// Who started a job (Wave 9).
///
/// The resume-on-reconnect rule (E09.2) resumes only jobs the automation
/// started: work a person began stays theirs to finish. That rule is only
/// sound if the origin is durable, which is why this is a column and not an
/// in-memory set that a restart would lose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JobTrigger {
    #[default]
    Manual,
    Auto,
}

impl JobTrigger {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "manual" => Self::Manual,
            "auto" => Self::Auto,
            _ => return None,
        })
    }
}

/// A row of `backup_jobs`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRow {
    pub id: i64,
    pub device_id: i64,
    pub started_at: String,
    pub completed_at: Option<String>,
    pub status: JobStatus,
    pub files_scanned: i64,
    pub files_copied: i64,
    pub files_skipped: i64,
    pub files_failed: i64,
    pub bytes_copied: i64,
    pub error_summary: Option<String>,
    /// Whether a person or the automation started this job.
    pub trigger: JobTrigger,
}

impl JobRow {
    fn from_row(row: &Row<'_>) -> rusqlite::Result<Self> {
        let status: String = row.get("status")?;
        Ok(Self {
            id: row.get("id")?,
            device_id: row.get("device_id")?,
            started_at: row.get("started_at")?,
            completed_at: row.get("completed_at")?,
            status: JobStatus::parse(&status).unwrap_or(JobStatus::Failed),
            files_scanned: row.get("files_scanned")?,
            files_copied: row.get("files_copied")?,
            files_skipped: row.get("files_skipped")?,
            files_failed: row.get("files_failed")?,
            bytes_copied: row.get("bytes_copied")?,
            error_summary: row.get("error_summary")?,
            trigger: JobTrigger::parse(&row.get::<_, String>("trigger")?)
                .unwrap_or(JobTrigger::Manual),
        })
    }
}

/// Create a job in `planning`, started by a person.
pub fn insert_job(db: &Db, device_id: i64, now: &str) -> Result<i64, DbError> {
    insert_job_as(db, device_id, now, JobTrigger::Manual)
}

/// Create a job in `planning`, recording who started it.
pub fn insert_job_as(
    db: &Db,
    device_id: i64,
    now: &str,
    trigger: JobTrigger,
) -> Result<i64, DbError> {
    db.conn().execute(
        "INSERT INTO backup_jobs (device_id, started_at, status, trigger)
         VALUES (?1, ?2, 'planning', ?3)",
        params![device_id, now, trigger.as_str()],
    )?;
    Ok(db.conn().last_insert_rowid())
}

pub fn get_job(db: &Db, job_id: i64) -> Result<Option<JobRow>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare("SELECT * FROM backup_jobs WHERE id = ?1")?;
    Ok(stmt
        .query_row(params![job_id], JobRow::from_row)
        .optional()?)
}

/// Jobs for a device, newest first.
pub fn history(db: &Db, device_id: i64, limit: u32) -> Result<Vec<JobRow>, DbError> {
    let conn = db.conn();
    let mut stmt =
        conn.prepare("SELECT * FROM backup_jobs WHERE device_id = ?1 ORDER BY id DESC LIMIT ?2")?;
    let rows = stmt.query_map(params![device_id, limit], JobRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

/// Every job still in a non-terminal status (used to recover after a crash).
pub fn unfinished_jobs(db: &Db) -> Result<Vec<JobRow>, DbError> {
    let conn = db.conn();
    let mut stmt = conn.prepare(
        "SELECT * FROM backup_jobs
         WHERE status IN ('planning', 'scanning', 'copying', 'paused')
         ORDER BY id ASC",
    )?;
    let rows = stmt.query_map([], JobRow::from_row)?;
    Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
}

pub fn latest_job(db: &Db, device_id: i64) -> Result<Option<JobRow>, DbError> {
    history(db, device_id, 1).map(|mut rows| rows.pop())
}

/// Guarded status transition. Fails loudly on an illegal transition.
pub fn transition(db: &Db, job_id: i64, next: JobStatus) -> Result<JobStatus, DbError> {
    let current = get_job(db, job_id)?.ok_or_else(|| DbError::NotFound {
        what: "job",
        key: job_id.to_string(),
    })?;
    if !current.status.can_transition_to(next) {
        return Err(DbError::IllegalTransition {
            from: current.status.as_str().to_string(),
            to: next.as_str().to_string(),
        });
    }
    if current.status == next {
        return Ok(next);
    }
    db.conn().execute(
        "UPDATE backup_jobs SET status = ?2 WHERE id = ?1",
        params![job_id, next.as_str()],
    )?;
    Ok(next)
}

/// Write final counters and close the job. This is the only place `completed_at`
/// is written.
pub fn finalize(
    db: &Db,
    job_id: i64,
    status: JobStatus,
    summary: &JobCounters,
    error_summary: Option<&str>,
    now: &str,
) -> Result<(), DbError> {
    if !status.is_terminal() {
        return Err(DbError::IllegalTransition {
            from: status.as_str().to_string(),
            to: "finalized".to_string(),
        });
    }
    let current = get_job(db, job_id)?.ok_or_else(|| DbError::NotFound {
        what: "job",
        key: job_id.to_string(),
    })?;
    if current.status.is_terminal() {
        return Err(DbError::IllegalTransition {
            from: current.status.as_str().to_string(),
            to: status.as_str().to_string(),
        });
    }
    db.conn().execute(
        "UPDATE backup_jobs SET
             status = ?2, completed_at = ?3, files_scanned = ?4, files_copied = ?5,
             files_skipped = ?6, files_failed = ?7, bytes_copied = ?8, error_summary = ?9
         WHERE id = ?1",
        params![
            job_id,
            status.as_str(),
            now,
            summary.scanned as i64,
            summary.copied as i64,
            summary.skipped as i64,
            summary.failed as i64,
            summary.bytes_copied as i64,
            error_summary,
        ],
    )?;
    Ok(())
}

/// Counters reported at the end of a job.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobCounters {
    pub scanned: u64,
    pub copied: u64,
    pub skipped: u64,
    pub failed: u64,
    pub bytes_copied: u64,
}

/// Incremental counter updates so a crash-observed job still shows real numbers.
pub fn set_scanned(db: &Db, job_id: i64, scanned: u64, skipped: u64) -> Result<(), DbError> {
    db.conn().execute(
        "UPDATE backup_jobs SET files_scanned = ?2, files_skipped = ?3 WHERE id = ?1",
        params![job_id, scanned as i64, skipped as i64],
    )?;
    Ok(())
}

pub fn record_copied(db: &Db, job_id: i64, bytes: u64) -> Result<(), DbError> {
    db.conn().execute(
        "UPDATE backup_jobs SET files_copied = files_copied + 1, bytes_copied = bytes_copied + ?2
         WHERE id = ?1",
        params![job_id, bytes as i64],
    )?;
    Ok(())
}

pub fn record_failed(db: &Db, job_id: i64) -> Result<(), DbError> {
    db.conn().execute(
        "UPDATE backup_jobs SET files_failed = files_failed + 1 WHERE id = ?1",
        params![job_id],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::database::devices;

    fn device(db: &Db) -> i64 {
        devices::upsert_seen(db, "SER1", &Default::default(), "2026-01-01T00:00:00Z")
            .unwrap()
            .id
    }

    #[test]
    fn a_job_records_who_started_it() {
        let db = Db::open_in_memory().unwrap();
        let device_id = device(&db);

        // The existing entry point is the manual one, by definition.
        let manual = insert_job(&db, device_id, "2026-01-01T00:00:00Z").unwrap();
        assert_eq!(
            get_job(&db, manual).unwrap().unwrap().trigger,
            JobTrigger::Manual
        );

        let auto = insert_job_as(&db, device_id, "2026-01-01T01:00:00Z", JobTrigger::Auto).unwrap();
        assert_eq!(
            get_job(&db, auto).unwrap().unwrap().trigger,
            JobTrigger::Auto
        );

        // Both survive the round trip through history, newest first.
        let rows = history(&db, device_id, 10).unwrap();
        assert_eq!(rows[0].id, auto);
        assert_eq!(rows[0].trigger, JobTrigger::Auto);
        assert_eq!(rows[1].trigger, JobTrigger::Manual);
    }

    #[test]
    fn trigger_strings_round_trip_and_unknown_values_do_not_panic() {
        for trigger in [JobTrigger::Manual, JobTrigger::Auto] {
            assert_eq!(JobTrigger::parse(trigger.as_str()), Some(trigger));
        }
        assert_eq!(JobTrigger::parse("something-else"), None);
        assert_eq!(JobTrigger::default(), JobTrigger::Manual);
    }

    #[test]
    fn job_starts_in_planning_and_is_not_terminal() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        let job = get_job(&db, id).unwrap().unwrap();
        assert_eq!(job.status, JobStatus::Planning);
        assert_eq!(job.completed_at, None);
        assert!(!job.status.is_terminal());
        assert!(unfinished_jobs(&db).unwrap().iter().any(|j| j.id == id));
    }

    #[test]
    fn happy_path_transitions() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        transition(&db, id, JobStatus::Scanning).unwrap();
        transition(&db, id, JobStatus::Copying).unwrap();
        transition(&db, id, JobStatus::Completed).unwrap();
        assert_eq!(
            get_job(&db, id).unwrap().unwrap().status,
            JobStatus::Completed
        );
    }

    #[test]
    fn interrupted_job_can_be_finalised_as_cancelled() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        transition(&db, id, JobStatus::Scanning).unwrap();
        transition(&db, id, JobStatus::Copying).unwrap();
        transition(&db, id, JobStatus::Paused).unwrap();
        transition(&db, id, JobStatus::Cancelled).unwrap();
        assert!(get_job(&db, id).unwrap().unwrap().status.is_terminal());
    }

    #[test]
    fn illegal_transitions_are_refused_and_nothing_is_written() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        assert!(matches!(
            transition(&db, id, JobStatus::Completed),
            Err(DbError::IllegalTransition { .. })
        ));
        assert_eq!(
            get_job(&db, id).unwrap().unwrap().status,
            JobStatus::Planning
        );
    }

    #[test]
    fn a_finished_job_cannot_be_reopened() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        finalize(
            &db,
            id,
            JobStatus::Failed,
            &JobCounters::default(),
            Some("boom"),
            "2026-01-01T00:10:00Z",
        )
        .unwrap();
        assert!(matches!(
            transition(&db, id, JobStatus::Copying),
            Err(DbError::IllegalTransition { .. })
        ));
        assert!(matches!(
            finalize(
                &db,
                id,
                JobStatus::Completed,
                &JobCounters::default(),
                None,
                "2026-01-01T00:20:00Z"
            ),
            Err(DbError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn finalize_cannot_be_called_with_a_non_terminal_status() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        assert!(matches!(
            finalize(
                &db,
                id,
                JobStatus::Copying,
                &JobCounters::default(),
                None,
                "2026-01-01T00:01:00Z"
            ),
            Err(DbError::IllegalTransition { .. })
        ));
    }

    #[test]
    fn counters_accumulate_incrementally() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        set_scanned(&db, id, 10, 4).unwrap();
        record_copied(&db, id, 1000).unwrap();
        record_copied(&db, id, 500).unwrap();
        record_failed(&db, id).unwrap();
        let job = get_job(&db, id).unwrap().unwrap();
        assert_eq!(job.files_scanned, 10);
        assert_eq!(job.files_skipped, 4);
        assert_eq!(job.files_copied, 2);
        assert_eq!(job.files_failed, 1);
        assert_eq!(job.bytes_copied, 1500);
    }

    #[test]
    fn finalize_writes_summary_and_timestamp() {
        let db = Db::open_in_memory().unwrap();
        let id = insert_job(&db, device(&db), "2026-01-01T00:00:00Z").unwrap();
        transition(&db, id, JobStatus::Scanning).unwrap();
        let counters = JobCounters {
            scanned: 5,
            copied: 4,
            skipped: 1,
            failed: 0,
            bytes_copied: 4096,
        };
        finalize(
            &db,
            id,
            JobStatus::Completed,
            &counters,
            None,
            "2026-01-01T00:05:00Z",
        )
        .unwrap();
        let job = get_job(&db, id).unwrap().unwrap();
        assert_eq!(job.completed_at.as_deref(), Some("2026-01-01T00:05:00Z"));
        assert_eq!(job.bytes_copied, 4096);
        assert!(unfinished_jobs(&db).unwrap().is_empty());
    }

    #[test]
    fn history_is_newest_first_and_limited() {
        let db = Db::open_in_memory().unwrap();
        let device_id = device(&db);
        for day in 1..=5 {
            insert_job(&db, device_id, &format!("2026-01-0{day}T00:00:00Z")).unwrap();
        }
        let rows = history(&db, device_id, 3).unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows[0].id > rows[1].id);
        assert_eq!(latest_job(&db, device_id).unwrap().unwrap().id, rows[0].id);
    }

    #[test]
    fn status_round_trips_through_strings() {
        for status in [
            JobStatus::Planning,
            JobStatus::Scanning,
            JobStatus::Copying,
            JobStatus::Paused,
            JobStatus::Cancelled,
            JobStatus::Completed,
            JobStatus::Failed,
        ] {
            assert_eq!(JobStatus::parse(status.as_str()), Some(status));
        }
        assert_eq!(JobStatus::parse("garbage"), None);
    }
}
