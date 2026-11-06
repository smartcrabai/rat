use std::fs::{self, OpenOptions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration as StdDuration;

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration, SecondsFormat, Utc};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use crate::model::{Job, JobState, NewJob, Run, RunCompletion, RunStatus};
use crate::schedule::Schedule;

const DATABASE_FILE: &str = "rat.sqlite3";
const OUTPUT_DIRECTORY: &str = "output";
const OUTPUT_LIMIT: u64 = 1024 * 1024 * 1024;
const RETENTION_DAYS: i64 = 30;
const SQLITE_BUSY_TIMEOUT: StdDuration = StdDuration::from_secs(5);

pub struct Store {
    root: PathBuf,
    connection: Connection,
}

impl Store {
    /// Opens the private database and output directory under `root`.
    ///
    /// # Errors
    /// Returns an error if storage cannot be created, secured, or opened.
    pub fn open(root: &Path) -> Result<Self> {
        fs::create_dir_all(root)
            .with_context(|| format!("creating data root {}", root.display()))?;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing data root {}", root.display()))?;
        let output_dir = root.join(OUTPUT_DIRECTORY);
        fs::create_dir_all(&output_dir)
            .with_context(|| format!("creating output directory {}", output_dir.display()))?;
        fs::set_permissions(&output_dir, fs::Permissions::from_mode(0o700))
            .with_context(|| format!("securing output directory {}", output_dir.display()))?;

        let database_path = root.join(DATABASE_FILE);
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .mode(0o600)
            .open(&database_path)
            .with_context(|| format!("creating database {}", database_path.display()))?;
        let connection = Connection::open(&database_path)
            .with_context(|| format!("opening database {}", database_path.display()))?;
        connection.busy_timeout(SQLITE_BUSY_TIMEOUT)?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS jobs (
                id INTEGER PRIMARY KEY,
                definition TEXT NOT NULL,
                state TEXT NOT NULL,
                next_at TEXT,
                created_at TEXT NOT NULL
            );
            CREATE TABLE IF NOT EXISTS runs (
                id INTEGER PRIMARY KEY,
                job_id INTEGER NOT NULL REFERENCES jobs(id),
                definition TEXT NOT NULL,
                scheduled_at TEXT NOT NULL,
                started_at TEXT,
                finished_at TEXT,
                status TEXT NOT NULL,
                exit_code INTEGER,
                stdout_bytes INTEGER NOT NULL DEFAULT 0,
                stderr_bytes INTEGER NOT NULL DEFAULT 0,
                truncated INTEGER NOT NULL DEFAULT 0,
                output_missing INTEGER NOT NULL DEFAULT 0,
                message TEXT,
                UNIQUE(job_id, scheduled_at)
            );
            CREATE INDEX IF NOT EXISTS runs_job_id_id ON runs(job_id, id);
            CREATE INDEX IF NOT EXISTS runs_status ON runs(status);",
        )?;
        set_private_file(&database_path)?;
        for suffix in ["-wal", "-shm"] {
            let path = PathBuf::from(format!("{}{}", database_path.display(), suffix));
            if path.exists() {
                set_private_file(&path)?;
            }
        }

        Ok(Self {
            root: root.to_path_buf(),
            connection,
        })
    }

    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    #[must_use]
    pub fn output_paths(&self, run_id: i64) -> (PathBuf, PathBuf) {
        let directory = self.root.join(OUTPUT_DIRECTORY);
        (
            directory.join(format!("{run_id}.stdout")),
            directory.join(format!("{run_id}.stderr")),
        )
    }

    /// Adds a job with its first scheduled occurrence.
    ///
    /// # Errors
    /// Returns an error if the schedule cannot be evaluated or the job cannot be stored.
    pub fn add(&mut self, definition: NewJob, now: DateTime<Utc>) -> Result<Job> {
        let next_at = Some(definition.schedule.first_at(now)?);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let definition_json = encode_definition(&definition)?;
        transaction.execute(
            "INSERT INTO jobs (definition, state, next_at, created_at)
             VALUES (?1, 'Active', ?2, ?3)",
            params![definition_json, next_at.map(timestamp), timestamp(now)],
        )?;
        let id = transaction.last_insert_rowid();
        let job = Job {
            id,
            definition,
            state: JobState::Active,
            next_at,
            created_at: now,
        };
        transaction.commit()?;
        Ok(job)
    }

    /// Returns the job with the given ID.
    ///
    /// # Errors
    /// Returns an error if the job does not exist or cannot be read.
    pub fn get(&self, id: i64) -> Result<Job> {
        read_job(&self.connection, id)
    }

    /// Returns jobs, optionally including completed and deleted jobs.
    ///
    /// # Errors
    /// Returns an error if jobs cannot be read or decoded.
    pub fn jobs(&self, include_finished: bool) -> Result<Vec<Job>> {
        let mut statement = self.connection.prepare(
            "SELECT id, definition, state, next_at, created_at FROM jobs
             WHERE ?1 OR state NOT IN ('Completed', 'Deleted')
             ORDER BY id",
        )?;
        let rows = statement.query_map([include_finished], decode_job)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading jobs")
    }

    /// Returns active jobs due by `now`, ordered by ID.
    ///
    /// # Errors
    /// Returns an error if querying or decoding the due jobs fails.
    pub fn due(&self, now: DateTime<Utc>) -> Result<Vec<Job>> {
        let mut statement = self.connection.prepare(
            "SELECT id, definition, state, next_at, created_at FROM jobs
             WHERE state = 'Active' AND next_at IS NOT NULL AND next_at <= ?1
             ORDER BY id",
        )?;
        let rows = statement.query_map([timestamp(now)], decode_job)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading due jobs")
    }

    /// Updates a job definition and its next scheduled occurrence.
    ///
    /// # Errors
    /// Returns an error if the job is missing, cannot be edited, or cannot be stored.
    pub fn edit(&mut self, id: i64, definition: NewJob, now: DateTime<Utc>) -> Result<Job> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job = read_job(&transaction, id)?;
        match job.state {
            JobState::Completed | JobState::Deleted => {
                bail!("cannot edit a completed or deleted job")
            }
            JobState::Active | JobState::Paused | JobState::NeedsReview => {}
        }
        let next_at = match (&job.definition.schedule, &definition.schedule, job.next_at) {
            (Schedule::OneOff { at: previous, .. }, Schedule::OneOff { at, .. }, None)
                if previous == at =>
            {
                None
            }
            _ => Some(definition.schedule.first_at(now)?),
        };
        transaction.execute(
            "UPDATE jobs SET definition = ?1, next_at = ?2 WHERE id = ?3",
            params![encode_definition(&definition)?, next_at.map(timestamp), id],
        )?;
        job.definition = definition;
        job.next_at = next_at;
        transaction.commit()?;
        Ok(job)
    }

    /// Pauses an active job.
    ///
    /// # Errors
    /// Returns an error if the job is missing, not active, or cannot be stored.
    pub fn pause(&mut self, id: i64) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE jobs SET state = 'Paused' WHERE id = ?1 AND state = 'Active'",
            [id],
        )?;
        if changed != 1 {
            if job_exists(&transaction, id)? {
                bail!("only an active job can be paused")
            }
            bail!("job {id} does not exist")
        }
        transaction.commit()?;
        Ok(())
    }

    /// Resumes a paused job.
    ///
    /// # Errors
    /// Returns an error if the job is missing, not paused, overdue without confirmation,
    /// or cannot be stored.
    pub fn resume(&mut self, id: i64, now: DateTime<Utc>, confirm_overdue: bool) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job = read_job(&transaction, id)?;
        if job.state != JobState::Paused {
            bail!("only a paused job can be resumed")
        }
        match &job.definition.schedule {
            Schedule::OneOff { .. } => {
                if job.next_at.is_some_and(|at| at <= now) && !confirm_overdue {
                    bail!("resuming an overdue one-off job requires confirmation")
                }
            }
            Schedule::Recurring { .. } => {
                job.next_at = job.definition.schedule.next_after(now)?;
            }
        }
        job.state = JobState::Active;
        transaction.execute(
            "UPDATE jobs SET state = 'Active', next_at = ?1 WHERE id = ?2",
            params![job.next_at.map(timestamp), id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Resolves a job that needs review.
    ///
    /// # Errors
    /// Returns an error if the job is missing, does not need review, or cannot be stored.
    pub fn resolve(&mut self, id: i64, now: DateTime<Utc>) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut job = read_job(&transaction, id)?;
        if job.state != JobState::NeedsReview {
            bail!("only a job needing review can be resolved")
        }
        match &job.definition.schedule {
            Schedule::OneOff { .. } => match job.next_at {
                None => job.state = JobState::Completed,
                Some(at) if at <= now => job.state = JobState::Paused,
                Some(_) => job.state = JobState::Active,
            },
            Schedule::Recurring { .. } => {
                job.state = JobState::Active;
                job.next_at = job.definition.schedule.next_after(now)?;
            }
        }
        transaction.execute(
            "UPDATE jobs SET state = ?1, next_at = ?2 WHERE id = ?3",
            params![job_state_name(job.state), job.next_at.map(timestamp), id],
        )?;
        transaction.commit()?;
        Ok(())
    }

    /// Marks a job as deleted.
    ///
    /// # Errors
    /// Returns an error if the job does not exist or cannot be stored.
    pub fn delete(&mut self, id: i64) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed =
            transaction.execute("UPDATE jobs SET state = 'Deleted' WHERE id = ?1", [id])?;
        if changed != 1 {
            bail!("job {id} does not exist")
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns run history, optionally limited to one job.
    ///
    /// # Errors
    /// Returns an error if history cannot be read or decoded.
    pub fn history(&self, job_id: Option<i64>) -> Result<Vec<Run>> {
        let mut statement = self.connection.prepare(
            "SELECT id, job_id, definition, scheduled_at, started_at, finished_at, status,
             exit_code, stdout_bytes, stderr_bytes, truncated, output_missing, message
             FROM runs WHERE (?1 IS NULL OR job_id = ?1) ORDER BY id",
        )?;
        let rows = statement.query_map([job_id], decode_run)?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("reading run history")
    }

    /// Returns the run with the given ID.
    ///
    /// # Errors
    /// Returns an error if the run does not exist or cannot be read.
    pub fn run(&self, id: i64) -> Result<Run> {
        read_run(&self.connection, id)
    }

    /// Reserves the specified due occurrence, recording a skipped run if another run is active.
    ///
    /// # Errors
    /// Returns an error if the schedule, database, or stored job data cannot be read or updated.
    pub fn reserve(
        &mut self,
        job_id: i64,
        scheduled_at: DateTime<Utc>,
        now: DateTime<Utc>,
    ) -> Result<Option<Run>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let Some(job) = read_job_optional(&transaction, job_id)? else {
            transaction.commit()?;
            return Ok(None);
        };
        if job.state != JobState::Active || job.next_at != Some(scheduled_at) || scheduled_at > now
        {
            transaction.commit()?;
            return Ok(None);
        }

        let next_at = match &job.definition.schedule {
            Schedule::OneOff { .. } => None,
            Schedule::Recurring { .. } => job.definition.schedule.next_after(now)?,
        };
        let has_in_flight = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM runs
             WHERE job_id = ?1 AND status IN ('Starting', 'Running'))",
            [job_id],
            |row| row.get::<_, bool>(0),
        )?;
        let status = if has_in_flight {
            RunStatus::Skipped
        } else {
            RunStatus::Starting
        };
        transaction.execute(
            "UPDATE jobs SET next_at = ?1 WHERE id = ?2",
            params![next_at.map(timestamp), job_id],
        )?;
        transaction.execute(
            "INSERT INTO runs
             (job_id, definition, scheduled_at, started_at, finished_at, status,
              stdout_bytes, stderr_bytes, truncated, output_missing)
             VALUES (?1, ?2, ?3, NULL, ?4, ?5, 0, 0, 0, 0)",
            params![
                job_id,
                encode_definition(&job.definition)?,
                timestamp(scheduled_at),
                if status == RunStatus::Skipped {
                    Some(timestamp(now))
                } else {
                    None
                },
                run_status_name(status),
            ],
        )?;
        let run = Run {
            id: transaction.last_insert_rowid(),
            job_id,
            definition: job.definition,
            scheduled_at,
            started_at: None,
            finished_at: (status == RunStatus::Skipped).then_some(now),
            status,
            exit_code: None,
            stdout_bytes: 0,
            stderr_bytes: 0,
            truncated: false,
            output_missing: false,
            message: None,
        };
        transaction.commit()?;
        Ok(Some(run))
    }

    /// Marks a reserved run as running.
    ///
    /// # Errors
    /// Returns an error if the run is not starting or the update cannot be stored.
    pub fn mark_running(&mut self, run_id: i64, now: DateTime<Utc>) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = transaction.execute(
            "UPDATE runs SET status = 'Running', started_at = ?1
             WHERE id = ?2 AND status = 'Starting'",
            params![timestamp(now), run_id],
        )?;
        if changed != 1 {
            bail!("run {run_id} is not starting")
        }
        transaction.commit()?;
        Ok(())
    }

    /// Launches a reserved run and persists its running state.
    ///
    /// # Errors
    /// Returns an error if the reservation cannot be read, launching fails, or running state
    /// cannot be persisted.
    pub fn launch_reserved<T>(
        &mut self,
        run_id: i64,
        now: DateTime<Utc>,
        launch: impl FnOnce() -> Result<T>,
    ) -> Result<Option<T>> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&transaction, run_id)?;
        if run.status != RunStatus::Starting {
            bail!("run {run_id} is not starting")
        }
        let job = read_job(&transaction, run.job_id)?;
        if job.state == JobState::Deleted {
            transaction.execute(
                "UPDATE runs SET status = 'Skipped', finished_at = ?1,
                 message = 'job deleted before launch'
                 WHERE id = ?2 AND status = 'Starting'",
                params![timestamp(now), run_id],
            )?;
            transaction.commit()?;
            return Ok(None);
        }

        let launched = launch()?;
        if let Err(error) = transaction.execute(
            "UPDATE runs SET status = 'Running', started_at = ?1
             WHERE id = ?2 AND status = 'Starting'",
            params![timestamp(now), run_id],
        ) {
            let rollback_result = transaction.rollback();
            let recovery_result = self.mark_uncertain_after_launch(run_id, now);
            return Err(launch_persistence_error(
                &error,
                rollback_result,
                recovery_result,
            ));
        }
        if let Err(error) = transaction.commit() {
            let recovery_result = self.mark_uncertain_after_launch(run_id, now);
            return Err(launch_commit_error(&error, recovery_result));
        }
        Ok(Some(launched))
    }

    /// Finishes a run and updates the job state when the run consumes its final occurrence.
    ///
    /// # Errors
    /// Returns an error if the completion is nonterminal, the run is not active, or the
    /// completion cannot be stored.
    pub fn finish(
        &mut self,
        run_id: i64,
        completion: RunCompletion,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let RunCompletion {
            status,
            exit_code,
            stdout_bytes,
            stderr_bytes,
            truncated,
            output_missing,
            message,
        } = completion;
        if matches!(status, RunStatus::Starting | RunStatus::Running) {
            bail!("run completion must have a terminal status")
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run(&transaction, run_id)?;
        if !matches!(run.status, RunStatus::Starting | RunStatus::Running) {
            bail!("run {run_id} is already terminal")
        }
        transaction.execute(
            "UPDATE runs SET status = ?1, finished_at = ?2, exit_code = ?3,
             stdout_bytes = ?4, stderr_bytes = ?5, truncated = ?6,
             output_missing = output_missing OR ?7, message = ?8 WHERE id = ?9",
            params![
                run_status_name(status),
                timestamp(now),
                exit_code,
                sql_u64(stdout_bytes)?,
                sql_u64(stderr_bytes)?,
                truncated,
                output_missing,
                message,
                run_id,
            ],
        )?;

        let mut job = read_job(&transaction, run.job_id)?;
        if job.state != JobState::Deleted {
            if status == RunStatus::Uncertain {
                job.state = JobState::NeedsReview;
            } else if matches!(&job.definition.schedule, Schedule::OneOff { .. })
                && job.next_at.is_none()
            {
                job.state = JobState::Completed;
            }
            transaction.execute(
                "UPDATE jobs SET state = ?1 WHERE id = ?2",
                params![job_state_name(job.state), job.id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Updates output metadata for a run without changing its execution state.
    ///
    /// This also supports terminal runs whose descendant processes are still writing.
    ///
    /// # Errors
    /// Returns an error if output counts cannot be represented or the metadata update fails.
    pub fn update_capture(&mut self, run_id: i64, completion: &RunCompletion) -> Result<()> {
        self.connection.execute(
            "UPDATE runs SET stdout_bytes = ?1, stderr_bytes = ?2, truncated = ?3,
             output_missing = output_missing OR ?4, message = ?5 WHERE id = ?6",
            params![
                sql_u64(completion.stdout_bytes)?,
                sql_u64(completion.stderr_bytes)?,
                completion.truncated,
                completion.output_missing,
                completion.message,
                run_id,
            ],
        )?;
        Ok(())
    }

    /// Marks in-flight runs uncertain and places their jobs into review.
    ///
    /// # Errors
    /// Returns an error if active runs cannot be read or recovery cannot be stored.
    pub fn recover(&mut self, now: DateTime<Utc>) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id, job_id FROM runs WHERE status IN ('Starting', 'Running') ORDER BY id",
        )?;
        let inflight = statement
            .query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut affected_jobs = Vec::new();
        for (run_id, job_id) in inflight {
            transaction.execute(
                "UPDATE runs SET status = 'Uncertain', finished_at = ?1
                 WHERE id = ?2 AND status IN ('Starting', 'Running')",
                params![timestamp(now), run_id],
            )?;
            if !affected_jobs.contains(&job_id) {
                affected_jobs.push(job_id);
            }
        }
        for job_id in affected_jobs {
            let mut job = read_job(&transaction, job_id)?;
            if job.state == JobState::Deleted {
                continue;
            }
            job.state = JobState::NeedsReview;
            if matches!(&job.definition.schedule, Schedule::Recurring { .. })
                && job.next_at.is_some_and(|at| at <= now)
            {
                job.next_at = job.definition.schedule.next_after(now)?;
            }
            transaction.execute(
                "UPDATE jobs SET state = 'NeedsReview', next_at = ?1 WHERE id = ?2",
                params![job.next_at.map(timestamp), job_id],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    /// Records missed recurring occurrences without launching them.
    ///
    /// # Errors
    /// Returns an error if due jobs cannot be read or updated.
    pub fn skip_missed(&mut self, now: DateTime<Utc>) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id, definition, state, next_at, created_at FROM jobs
             WHERE state = 'Active' AND next_at IS NOT NULL ORDER BY id",
        )?;
        let jobs = statement
            .query_map([], decode_job)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for mut job in jobs {
            if matches!(&job.definition.schedule, Schedule::Recurring { .. })
                && job.next_at.is_some_and(|at| at <= now)
            {
                job.next_at = job.definition.schedule.next_after(now)?;
                transaction.execute(
                    "UPDATE jobs SET next_at = ?1 WHERE id = ?2",
                    params![job.next_at.map(timestamp), job.id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Removes expired history and evicts oldest output files until storage is within quota.
    ///
    /// Returns run IDs whose history or output files were evicted so callers can close any
    /// remaining capture writers.
    ///
    /// # Errors
    /// Returns an error if retention, output sizing, file removal, or database updates fail.
    pub fn prune(&mut self, now: DateTime<Utc>) -> Result<Vec<i64>> {
        let cutoff = now
            .checked_sub_signed(Duration::days(RETENTION_DAYS))
            .ok_or_else(|| anyhow!("history retention cutoff is out of range"))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut statement = transaction.prepare(
            "SELECT id FROM runs WHERE finished_at IS NOT NULL AND finished_at < ?1
             AND status NOT IN ('Starting', 'Running') ORDER BY finished_at, id",
        )?;
        let expired = statement
            .query_map([timestamp(cutoff)], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut evicted_runs = Vec::new();
        for run_id in expired {
            remove_run_outputs(&self.root, run_id)?;
            transaction.execute("DELETE FROM runs WHERE id = ?1", [run_id])?;
            evicted_runs.push(run_id);
        }

        let mut statement = transaction.prepare(
            "SELECT id, job_id, definition, scheduled_at, started_at, finished_at, status,
             exit_code, stdout_bytes, stderr_bytes, truncated, output_missing, message
             FROM runs ORDER BY COALESCE(started_at, scheduled_at), id",
        )?;
        let runs = statement
            .query_map([], decode_run)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        let mut output_bytes = 0_u64;
        let mut candidates = Vec::new();
        for run in runs {
            let size = run_output_size(&self.root, run.id)?;
            output_bytes = output_bytes.saturating_add(size);
            candidates.push((run.id, size));
        }
        for (run_id, size) in candidates {
            if output_bytes <= OUTPUT_LIMIT {
                break;
            }
            if size == 0 {
                continue;
            }
            remove_run_outputs(&self.root, run_id)?;
            transaction.execute("UPDATE runs SET output_missing = 1 WHERE id = ?1", [run_id])?;
            evicted_runs.push(run_id);
            output_bytes = output_bytes.saturating_sub(size);
        }
        transaction.commit()?;
        Ok(evicted_runs)
    }

    fn mark_uncertain_after_launch(&mut self, run_id: i64, now: DateTime<Utc>) -> Result<()> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let run = read_run_optional(&transaction, run_id)?;
        let Some(run) = run else {
            transaction.commit()?;
            return Ok(());
        };
        if matches!(run.status, RunStatus::Starting | RunStatus::Running) {
            transaction.execute(
                "UPDATE runs SET status = 'Uncertain', finished_at = ?1
                 WHERE id = ?2 AND status IN ('Starting', 'Running')",
                params![timestamp(now), run_id],
            )?;
            let job = read_job(&transaction, run.job_id)?;
            if job.state != JobState::Deleted {
                let next_at = match &job.definition.schedule {
                    Schedule::Recurring { .. } if job.next_at.is_some_and(|at| at <= now) => {
                        job.definition.schedule.next_after(now)?
                    }
                    _ => job.next_at,
                };
                transaction.execute(
                    "UPDATE jobs SET state = 'NeedsReview', next_at = ?1 WHERE id = ?2",
                    params![next_at.map(timestamp), job.id],
                )?;
            }
        }
        transaction.commit()?;
        Ok(())
    }
}

fn read_job(connection: &Connection, id: i64) -> Result<Job> {
    read_job_optional(connection, id)?.ok_or_else(|| anyhow!("job {id} does not exist"))
}

fn read_job_optional(connection: &Connection, id: i64) -> Result<Option<Job>> {
    connection
        .query_row(
            "SELECT id, definition, state, next_at, created_at FROM jobs WHERE id = ?1",
            [id],
            decode_job,
        )
        .optional()
        .context("reading job")
}

fn read_run(connection: &Connection, id: i64) -> Result<Run> {
    read_run_optional(connection, id)?.ok_or_else(|| anyhow!("run {id} does not exist"))
}

fn read_run_optional(connection: &Connection, id: i64) -> Result<Option<Run>> {
    connection
        .query_row(
            "SELECT id, job_id, definition, scheduled_at, started_at, finished_at, status,
             exit_code, stdout_bytes, stderr_bytes, truncated, output_missing, message
             FROM runs WHERE id = ?1",
            [id],
            decode_run,
        )
        .optional()
        .context("reading run")
}

fn decode_job(row: &Row<'_>) -> rusqlite::Result<Job> {
    let definition_text = row.get::<_, String>(1)?;
    let definition = serde_json::from_str(&definition_text)
        .map_err(|error| conversion_error(1, Type::Text, error))?;
    let state_text = row.get::<_, String>(2)?;
    let state = parse_job_state(&state_text)
        .ok_or_else(|| conversion_error(2, Type::Text, invalid_enum("job state", &state_text)))?;
    let next_at = row
        .get::<_, Option<String>>(3)?
        .map(|value| {
            parse_timestamp(&value).map_err(|error| conversion_error(3, Type::Text, error))
        })
        .transpose()?;
    let created_at_text = row.get::<_, String>(4)?;
    let created_at = parse_timestamp(&created_at_text)
        .map_err(|error| conversion_error(4, Type::Text, error))?;
    Ok(Job {
        id: row.get(0)?,
        definition,
        state,
        next_at,
        created_at,
    })
}

fn decode_run(row: &Row<'_>) -> rusqlite::Result<Run> {
    let definition_text = row.get::<_, String>(2)?;
    let definition = serde_json::from_str(&definition_text)
        .map_err(|error| conversion_error(2, Type::Text, error))?;
    let scheduled_at_text = row.get::<_, String>(3)?;
    let scheduled_at = parse_timestamp(&scheduled_at_text)
        .map_err(|error| conversion_error(3, Type::Text, error))?;
    let started_at = row
        .get::<_, Option<String>>(4)?
        .map(|value| {
            parse_timestamp(&value).map_err(|error| conversion_error(4, Type::Text, error))
        })
        .transpose()?;
    let finished_at = row
        .get::<_, Option<String>>(5)?
        .map(|value| {
            parse_timestamp(&value).map_err(|error| conversion_error(5, Type::Text, error))
        })
        .transpose()?;
    let status_text = row.get::<_, String>(6)?;
    let status = parse_run_status(&status_text)
        .ok_or_else(|| conversion_error(6, Type::Text, invalid_enum("run status", &status_text)))?;
    let exit_code = row
        .get::<_, Option<i64>>(7)?
        .map(|code| i32::try_from(code).map_err(|error| conversion_error(7, Type::Integer, error)))
        .transpose()?;
    let stdout_bytes = decode_u64(row.get(8)?, 8)?;
    let stderr_bytes = decode_u64(row.get(9)?, 9)?;
    Ok(Run {
        id: row.get(0)?,
        job_id: row.get(1)?,
        definition,
        scheduled_at,
        started_at,
        finished_at,
        status,
        exit_code,
        stdout_bytes,
        stderr_bytes,
        truncated: row.get(10)?,
        output_missing: row.get(11)?,
        message: row.get(12)?,
    })
}

fn conversion_error(
    index: usize,
    column_type: Type,
    error: impl std::error::Error + Send + Sync + 'static,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(index, column_type, Box::new(error))
}

fn invalid_enum(name: &str, value: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("invalid {name} {value:?} in database"),
    )
}

fn decode_u64(value: i64, index: usize) -> rusqlite::Result<u64> {
    u64::try_from(value).map_err(|error| conversion_error(index, Type::Integer, error))
}

fn parse_timestamp(value: &str) -> chrono::ParseResult<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value).map(|timestamp| timestamp.with_timezone(&Utc))
}

fn timestamp(value: DateTime<Utc>) -> String {
    value.to_rfc3339_opts(SecondsFormat::Nanos, true)
}

fn encode_definition(definition: &NewJob) -> Result<String> {
    Ok(serde_json::to_string(definition)?)
}

fn sql_u64(value: u64) -> Result<i64> {
    i64::try_from(value).context("output byte count exceeds SQLite integer range")
}

fn job_exists(connection: &Connection, id: i64) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM jobs WHERE id = ?1)",
        [id],
        |row| row.get(0),
    )?)
}

fn job_state_name(state: JobState) -> &'static str {
    match state {
        JobState::Active => "Active",
        JobState::Paused => "Paused",
        JobState::NeedsReview => "NeedsReview",
        JobState::Completed => "Completed",
        JobState::Deleted => "Deleted",
    }
}

fn parse_job_state(value: &str) -> Option<JobState> {
    match value {
        "Active" => Some(JobState::Active),
        "Paused" => Some(JobState::Paused),
        "NeedsReview" => Some(JobState::NeedsReview),
        "Completed" => Some(JobState::Completed),
        "Deleted" => Some(JobState::Deleted),
        _ => None,
    }
}

fn run_status_name(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "Starting",
        RunStatus::Running => "Running",
        RunStatus::Succeeded => "Succeeded",
        RunStatus::Failed => "Failed",
        RunStatus::Skipped => "Skipped",
        RunStatus::Uncertain => "Uncertain",
    }
}

fn parse_run_status(value: &str) -> Option<RunStatus> {
    match value {
        "Starting" => Some(RunStatus::Starting),
        "Running" => Some(RunStatus::Running),
        "Succeeded" => Some(RunStatus::Succeeded),
        "Failed" => Some(RunStatus::Failed),
        "Skipped" => Some(RunStatus::Skipped),
        "Uncertain" => Some(RunStatus::Uncertain),
        _ => None,
    }
}

fn set_private_file(path: &Path) -> Result<()> {
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("securing file {}", path.display()))
}

fn file_size(path: &Path) -> Result<u64> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(metadata.len()),
        Ok(_) => Ok(0),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error).with_context(|| format!("checking output {}", path.display())),
    }
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).with_context(|| format!("removing output {}", path.display())),
    }
}

fn launch_persistence_error(
    error: &rusqlite::Error,
    rollback_result: rusqlite::Result<()>,
    recovery_result: Result<()>,
) -> anyhow::Error {
    use std::fmt::Write as _;

    let mut message = format!("command spawned but recording its running state failed: {error}");
    if let Err(rollback_error) = rollback_result {
        let _ = write!(message, "; transaction rollback failed: {rollback_error}");
    }
    if let Err(recovery_error) = recovery_result {
        let _ = write!(
            message,
            "; marking execution uncertain failed: {recovery_error:#}"
        );
    }
    anyhow!(message)
}

fn launch_commit_error(error: &rusqlite::Error, recovery_result: Result<()>) -> anyhow::Error {
    match recovery_result {
        Ok(()) => anyhow!(
            "command spawned but transaction commit failed; execution marked uncertain: {error}"
        ),
        Err(recovery_error) => anyhow!(
            "command spawned but transaction commit and uncertainty recovery failed: {error}; {recovery_error:#}"
        ),
    }
}

fn run_output_size(root: &Path, run_id: i64) -> Result<u64> {
    let (stdout, stderr) = output_paths(root, run_id);
    Ok(file_size(&stdout)?.saturating_add(file_size(&stderr)?))
}

fn remove_run_outputs(root: &Path, run_id: i64) -> Result<()> {
    let (stdout, stderr) = output_paths(root, run_id);
    remove_if_present(&stdout)?;
    remove_if_present(&stderr)?;
    Ok(())
}

fn output_paths(root: &Path, run_id: i64) -> (PathBuf, PathBuf) {
    let directory = root.join(OUTPUT_DIRECTORY);
    (
        directory.join(format!("{run_id}.stdout")),
        directory.join(format!("{run_id}.stderr")),
    )
}
