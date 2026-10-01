-- Schema v1 — see docs/overview_plan.md §9.
--
-- Notes:
--  * `device_aliases` lives in v1 on purpose: a device re-appearing with a new
--    serial must be associable with an existing profile, and adding that table
--    later would force an avoidable migration.
--  * `backup_files` is unique on (device_id, source_path): one manifest row per
--    source file per device is what makes incremental comparison sound.
--  * Timestamps are ISO-8601 UTC strings; phone-reported mtimes are unix seconds.

CREATE TABLE devices (
    id               INTEGER PRIMARY KEY,
    serial           TEXT    NOT NULL UNIQUE,
    display_name     TEXT    NOT NULL,
    manufacturer     TEXT,
    model            TEXT,
    android_version  TEXT,
    destination_path TEXT,
    created_at       TEXT    NOT NULL,
    last_seen_at     TEXT    NOT NULL
);

CREATE TABLE device_aliases (
    alias_serial TEXT    PRIMARY KEY,
    device_id    INTEGER NOT NULL REFERENCES devices(id),
    created_at   TEXT    NOT NULL
);

CREATE TABLE device_folders (
    id          INTEGER PRIMARY KEY,
    device_id   INTEGER NOT NULL REFERENCES devices(id),
    source_path TEXT    NOT NULL,
    enabled     INTEGER NOT NULL DEFAULT 1,
    UNIQUE (device_id, source_path)
);

CREATE TABLE backup_jobs (
    id             INTEGER PRIMARY KEY,
    device_id      INTEGER NOT NULL REFERENCES devices(id),
    started_at     TEXT    NOT NULL,
    completed_at   TEXT,
    status         TEXT    NOT NULL,
    files_scanned  INTEGER NOT NULL DEFAULT 0,
    files_copied   INTEGER NOT NULL DEFAULT 0,
    files_skipped  INTEGER NOT NULL DEFAULT 0,
    files_failed   INTEGER NOT NULL DEFAULT 0,
    bytes_copied   INTEGER NOT NULL DEFAULT 0,
    error_summary  TEXT
);

CREATE TABLE backup_files (
    id                     INTEGER PRIMARY KEY,
    device_id              INTEGER NOT NULL REFERENCES devices(id),
    job_id                 INTEGER NOT NULL REFERENCES backup_jobs(id),
    source_path            TEXT    NOT NULL,
    destination_path       TEXT    NOT NULL,
    source_size            INTEGER,
    source_modified_at     INTEGER,
    content_hash           TEXT,
    last_successful_backup TEXT,
    status                 TEXT    NOT NULL
);

CREATE UNIQUE INDEX idx_backup_files_device_source
    ON backup_files (device_id, source_path);

CREATE INDEX idx_backup_files_status
    ON backup_files (device_id, status);

CREATE INDEX idx_backup_jobs_device
    ON backup_jobs (device_id, started_at);
