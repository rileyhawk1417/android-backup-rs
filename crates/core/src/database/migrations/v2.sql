-- Schema v2 — Wave 9 (automation). See docs/plan/wave-9-automation.md (E09.1).
--
-- Notes:
--  * `devices.auto_schedule_kind` / `_hours` are the per-device answer to "when
--    do automatic backups run for this phone?" — the same three values the
--    settings panel offers (off / on_connect / every_hours + N). Default 'off',
--    so an upgrade enrols nobody: 0.1.0 behaviour is kept until the user asks.
--  * The schedule is stored as a schedule rather than as a boolean "enabled"
--    flag because a flag plus a separate interval would be two sources of truth
--    for one decision, and a row could then say "on" with no interval to run on.
--  * `backup_jobs.trigger` records **who started a job**. Existing rows are
--    backfilled as 'manual': history must not retroactively credit the machine
--    with work a person did. The resume-on-reconnect rule (E09.2) turns on
--    exactly this distinction, so it has to be durable, not in-memory.

ALTER TABLE devices ADD COLUMN auto_schedule_kind TEXT NOT NULL DEFAULT 'off';

ALTER TABLE devices ADD COLUMN auto_schedule_hours INTEGER;

ALTER TABLE backup_jobs ADD COLUMN trigger TEXT NOT NULL DEFAULT 'manual';
