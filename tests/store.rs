use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rat::model::{JobState, NewJob, RunCompletion, RunStatus};
use rat::schedule::Schedule;
use rat::store::Store;
use tempfile::tempdir;

const OUTPUT_LIMIT: u64 = 1024 * 1024 * 1024;

fn utc(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Result<DateTime<Utc>> {
    Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
        .single()
        .ok_or_else(|| anyhow!("invalid fixed test instant"))
}

fn one_off(at: DateTime<Utc>) -> Schedule {
    Schedule::OneOff {
        at,
        timezone: "UTC".to_owned(),
    }
}

fn recurring(cron: &str) -> Schedule {
    Schedule::Recurring {
        cron: cron.to_owned(),
        timezone: "UTC".to_owned(),
    }
}

fn definition(command: &str, schedule: Schedule) -> NewJob {
    NewJob {
        command: command.to_owned(),
        cwd: PathBuf::from("/tmp"),
        path: "/usr/bin:/bin".to_owned(),
        env: BTreeMap::new(),
        schedule,
    }
}

fn completion(status: RunStatus, exit_code: Option<i32>) -> RunCompletion {
    RunCompletion {
        status,
        exit_code,
        stdout_bytes: 0,
        stderr_bytes: 0,
        truncated: false,
        output_missing: false,
        message: None,
    }
}

fn only<T>(items: &[T]) -> Result<&T> {
    if items.len() != 1 {
        return Err(anyhow!("expected exactly one item, got {}", items.len()));
    }
    items.first().ok_or_else(|| anyhow!("item disappeared"))
}

fn create_sparse_file(path: &Path, length: u64) -> Result<()> {
    let file = OpenOptions::new().create_new(true).write(true).open(path)?;
    file.set_len(length)?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[test]
fn reopen_preserves_jobs_runs_and_private_storage() -> Result<()> {
    let root = tempdir()?;
    let root_path = root.path().to_path_buf();
    let now = utc(2025, 3, 1, 10, 0, 0)?;
    let scheduled = utc(2025, 3, 1, 10, 1, 0)? + Duration::milliseconds(123);
    let mut store = Store::open(&root_path)?;
    let job = store.add(definition("echo durable", one_off(scheduled)), now)?;
    let run = store
        .reserve(job.id, scheduled, scheduled)?
        .ok_or_else(|| anyhow!("due reservation was not created"))?;
    let (stdout, stderr) = store.output_paths(run.id);
    assert_eq!(
        stdout,
        root_path.join("output").join(format!("{}.stdout", run.id))
    );
    assert_eq!(
        stderr,
        root_path.join("output").join(format!("{}.stderr", run.id))
    );
    let reserved_job = store.get(job.id)?;
    drop(store);

    let reopened = Store::open(&root_path)?;
    assert_eq!(reopened.root(), root_path);
    assert_eq!(reopened.get(job.id)?, reserved_job);
    assert_eq!(reopened.run(run.id)?.status, RunStatus::Starting);
    assert_eq!(reopened.run(run.id)?.scheduled_at, scheduled);

    let output_dir = fs::metadata(root_path.join("output"))?;
    assert!(output_dir.is_dir());
    assert_eq!(output_dir.permissions().mode() & 0o077, 0);
    let mut database_files = 0;
    for entry in fs::read_dir(&root_path)? {
        let entry = entry?;
        if entry.file_type()?.is_file() {
            database_files += 1;
            assert_eq!(entry.metadata()?.permissions().mode() & 0o077, 0);
        }
    }
    assert!(database_files > 0);
    Ok(())
}

#[test]
fn overdue_oneoff_resume_requires_confirmation_and_preserves_due_occurrence() -> Result<()> {
    let root = tempdir()?;
    let created = utc(2025, 3, 1, 9, 0, 0)?;
    let overdue = utc(2025, 3, 1, 9, 30, 0)?;
    let now = utc(2025, 3, 1, 10, 0, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo once", one_off(overdue)), created)?;

    store.pause(job.id)?;
    assert!(store.resume(job.id, now, false).is_err());
    assert_eq!(store.get(job.id)?.state, JobState::Paused);

    store.resume(job.id, now, true)?;
    let resumed = store.get(job.id)?;
    assert_eq!(resumed.state, JobState::Active);
    assert!(resumed.next_at.is_some_and(|at| at <= now));
    Ok(())
}

#[test]
fn recurring_resume_skips_missed_occurrences_and_chooses_a_future_time() -> Result<()> {
    let root = tempdir()?;
    let created = utc(2025, 3, 1, 10, 0, 0)?;
    let resume_at = utc(2025, 3, 1, 10, 22, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition("echo recurring", recurring("*/5 * * * *")),
        created,
    )?;
    store.pause(job.id)?;

    store.resume(job.id, resume_at, false)?;
    let resumed = store.get(job.id)?;
    assert_eq!(resumed.state, JobState::Active);
    assert_eq!(resumed.next_at, Some(utc(2025, 3, 1, 10, 25, 0)?));
    assert!(resumed.next_at.is_some_and(|at| at > resume_at));
    Ok(())
}

#[test]
fn reservation_is_single_use_across_same_and_reopened_connections() -> Result<()> {
    let root = tempdir()?;
    let now = utc(2025, 3, 1, 10, 0, 0)?;
    let scheduled = utc(2025, 3, 1, 10, 1, 0)?;
    let mut first = Store::open(root.path())?;
    let job = first.add(definition("echo once", one_off(scheduled)), now)?;
    let mut second = Store::open(root.path())?;

    let reserved = first
        .reserve(job.id, scheduled, scheduled)?
        .ok_or_else(|| anyhow!("first reservation was not created"))?;
    assert!(first.reserve(job.id, scheduled, scheduled)?.is_none());
    assert!(second.reserve(job.id, scheduled, scheduled)?.is_none());

    let history = first.history(Some(job.id))?;
    assert_eq!(history.len(), 1);
    assert_eq!(only(&history)?.id, reserved.id);
    Ok(())
}

#[test]
fn overlapping_recurring_occurrences_are_skipped_without_a_queue() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let first_due = utc(2025, 3, 1, 10, 1, 0)?;
    let second_due = utc(2025, 3, 1, 10, 2, 0)?;
    let third_due = utc(2025, 3, 1, 10, 3, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo overlap", recurring("* * * * *")), initial)?;

    let started = store
        .reserve(job.id, first_due, first_due)?
        .ok_or_else(|| anyhow!("first reservation was not created"))?;
    let second = store
        .reserve(job.id, second_due, second_due)?
        .ok_or_else(|| anyhow!("overlapping occurrence was not recorded"))?;
    let third = store
        .reserve(job.id, third_due, third_due)?
        .ok_or_else(|| anyhow!("later overlapping occurrence was not recorded"))?;

    assert_eq!(started.status, RunStatus::Starting);
    assert_eq!(second.status, RunStatus::Skipped);
    assert_eq!(third.status, RunStatus::Skipped);
    assert!(second.started_at.is_none());
    assert!(third.started_at.is_none());
    assert_eq!(second.scheduled_at, second_due);
    assert_eq!(third.scheduled_at, third_due);
    assert_eq!(store.history(Some(job.id))?.len(), 3);
    assert_eq!(store.get(job.id)?.next_at, Some(utc(2025, 3, 1, 10, 4, 0)?));
    Ok(())
}

#[test]
fn failed_oneoff_completes_while_failed_recurring_job_remains_active() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let oneoff_at = utc(2025, 3, 1, 10, 1, 0)?;
    let recurring_base = utc(2025, 3, 1, 10, 1, 0)?;
    let recurring_at = utc(2025, 3, 1, 10, 2, 0)?;
    let mut store = Store::open(root.path())?;
    let oneoff = store.add(definition("false", one_off(oneoff_at)), initial)?;
    let oneoff_run = store
        .reserve(oneoff.id, oneoff_at, oneoff_at)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    store.mark_running(oneoff_run.id, oneoff_at)?;
    store.finish(
        oneoff_run.id,
        completion(RunStatus::Failed, Some(1)),
        oneoff_at + Duration::seconds(1),
    )?;
    assert_eq!(store.get(oneoff.id)?.state, JobState::Completed);

    let recurring_job = store.add(definition("false", recurring("* * * * *")), recurring_base)?;
    let recurring_run = store
        .reserve(recurring_job.id, recurring_at, recurring_at)?
        .ok_or_else(|| anyhow!("recurring reservation was not created"))?;
    store.mark_running(recurring_run.id, recurring_at)?;
    store.finish(
        recurring_run.id,
        completion(RunStatus::Failed, Some(1)),
        recurring_at + Duration::seconds(1),
    )?;
    let continued = store.get(recurring_job.id)?;
    assert_eq!(continued.state, JobState::Active);
    assert_eq!(continued.next_at, Some(utc(2025, 3, 1, 10, 3, 0)?));
    assert_eq!(store.run(recurring_run.id)?.status, RunStatus::Failed);
    Ok(())
}

#[test]
fn recovery_marks_starting_and_running_runs_uncertain_without_retry() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let oneoff_at = utc(2025, 3, 1, 10, 1, 0)?;
    let recurring_at = utc(2025, 3, 1, 10, 1, 0)?;
    let recovery_at = utc(2025, 3, 1, 10, 10, 0)?;
    let mut store = Store::open(root.path())?;
    let oneoff = store.add(definition("echo starting", one_off(oneoff_at)), initial)?;
    let starting = store
        .reserve(oneoff.id, oneoff_at, oneoff_at)?
        .ok_or_else(|| anyhow!("starting run was not reserved"))?;

    let recurring_job = store.add(definition("echo running", recurring("* * * * *")), initial)?;
    let running = store
        .reserve(recurring_job.id, recurring_at, recurring_at)?
        .ok_or_else(|| anyhow!("running run was not reserved"))?;
    store.mark_running(running.id, recurring_at)?;

    store.recover(recovery_at)?;
    assert_eq!(store.get(oneoff.id)?.state, JobState::NeedsReview);
    assert_eq!(store.get(recurring_job.id)?.state, JobState::NeedsReview);
    assert_eq!(store.run(starting.id)?.status, RunStatus::Uncertain);
    assert_eq!(store.run(running.id)?.status, RunStatus::Uncertain);
    assert_eq!(
        store.get(recurring_job.id)?.next_at,
        Some(utc(2025, 3, 1, 10, 11, 0)?)
    );

    store.recover(utc(2025, 3, 1, 10, 11, 0)?)?;
    assert_eq!(store.history(None)?.len(), 2);
    assert_eq!(store.get(oneoff.id)?.state, JobState::NeedsReview);
    assert_eq!(store.get(recurring_job.id)?.state, JobState::NeedsReview);
    Ok(())
}

#[test]
fn review_is_distinct_from_resume_and_acknowledgement_does_not_replay() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let oneoff_at = utc(2025, 3, 1, 10, 1, 0)?;
    let recurring_at = utc(2025, 3, 1, 10, 1, 0)?;
    let recovery_at = utc(2025, 3, 1, 10, 10, 0)?;
    let mut store = Store::open(root.path())?;
    let oneoff = store.add(definition("echo once", one_off(oneoff_at)), initial)?;
    let oneoff_run = store
        .reserve(oneoff.id, oneoff_at, oneoff_at)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    let recurring_job = store.add(
        definition("echo recurring", recurring("* * * * *")),
        initial,
    )?;
    let recurring_run = store
        .reserve(recurring_job.id, recurring_at, recurring_at)?
        .ok_or_else(|| anyhow!("recurring reservation was not created"))?;
    store.recover(recovery_at)?;

    assert!(store.resume(oneoff.id, recovery_at, true).is_err());
    assert!(store.resume(recurring_job.id, recovery_at, true).is_err());
    assert_eq!(store.get(oneoff.id)?.state, JobState::NeedsReview);
    assert_eq!(store.get(recurring_job.id)?.state, JobState::NeedsReview);

    store.resolve(oneoff.id, recovery_at)?;
    store.resolve(recurring_job.id, recovery_at)?;
    assert_eq!(store.get(oneoff.id)?.state, JobState::Completed);
    assert_eq!(store.get(recurring_job.id)?.state, JobState::Active);
    assert!(
        store
            .get(recurring_job.id)?
            .next_at
            .is_some_and(|at| at > recovery_at)
    );
    assert_eq!(store.run(oneoff_run.id)?.status, RunStatus::Uncertain);
    assert_eq!(store.run(recurring_run.id)?.status, RunStatus::Uncertain);
    assert_eq!(store.history(None)?.len(), 2);
    Ok(())
}

#[test]
fn review_preserves_future_oneoff_replacement() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let original_at = utc(2025, 3, 1, 10, 1, 0)?;
    let recovery_at = utc(2025, 3, 1, 10, 10, 0)?;
    let replacement_at = utc(2025, 3, 1, 10, 20, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo once", one_off(original_at)), initial)?;
    let run = store
        .reserve(job.id, original_at, original_at)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    store.mark_running(run.id, original_at)?;
    store.recover(recovery_at)?;

    store.edit(
        job.id,
        definition("echo replacement", one_off(replacement_at)),
        recovery_at,
    )?;
    store.resolve(job.id, recovery_at)?;

    let reviewed = store.get(job.id)?;
    assert_eq!(reviewed.state, JobState::Active);
    assert_eq!(reviewed.next_at, Some(replacement_at));
    assert_eq!(store.run(run.id)?.status, RunStatus::Uncertain);
    assert_eq!(store.history(Some(job.id))?.len(), 1);
    Ok(())
}

#[test]
fn review_pauses_overdue_oneoff_replacement_until_confirmed_resume() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let original_at = utc(2025, 3, 1, 10, 1, 0)?;
    let replacement_at = utc(2025, 3, 1, 10, 5, 0)?;
    let recovery_at = utc(2025, 3, 1, 10, 10, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo once", one_off(original_at)), initial)?;
    let run = store
        .reserve(job.id, original_at, original_at)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    store.mark_running(run.id, original_at)?;
    store.recover(recovery_at)?;

    store.edit(
        job.id,
        definition("echo replacement", one_off(replacement_at)),
        recovery_at,
    )?;
    store.resolve(job.id, recovery_at)?;

    let reviewed = store.get(job.id)?;
    assert_eq!(reviewed.state, JobState::Paused);
    assert_eq!(reviewed.next_at, Some(replacement_at));
    assert!(store.resume(job.id, recovery_at, false).is_err());
    store.resume(job.id, recovery_at, true)?;
    let resumed = store.get(job.id)?;
    assert_eq!(resumed.state, JobState::Active);
    assert_eq!(resumed.next_at, Some(replacement_at));
    assert_eq!(store.run(run.id)?.status, RunStatus::Uncertain);
    assert_eq!(store.history(Some(job.id))?.len(), 1);
    Ok(())
}

#[test]
fn deleting_a_running_job_keeps_its_history_and_deleted_state() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let scheduled = utc(2025, 3, 1, 10, 1, 0)?;
    let finished = utc(2025, 3, 1, 10, 2, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo retained", one_off(scheduled)), initial)?;
    let run = store
        .reserve(job.id, scheduled, scheduled)?
        .ok_or_else(|| anyhow!("reservation was not created"))?;
    store.mark_running(run.id, scheduled)?;
    store.delete(job.id)?;
    store.finish(run.id, completion(RunStatus::Succeeded, Some(0)), finished)?;

    assert_eq!(store.get(job.id)?.state, JobState::Deleted);
    assert!(store.jobs(false)?.iter().all(|listed| listed.id != job.id));
    assert!(store.jobs(true)?.iter().any(|listed| listed.id == job.id));
    let history = store.history(Some(job.id))?;
    assert_eq!(only(&history)?.id, run.id);
    assert_eq!(only(&history)?.status, RunStatus::Succeeded);
    Ok(())
}

#[test]
fn editing_a_running_job_changes_only_its_future_definition() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let first_due = utc(2025, 3, 1, 10, 5, 0)?;
    let edit_at = utc(2025, 3, 1, 10, 5, 30)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition("echo old definition", recurring("*/5 * * * *")),
        initial,
    )?;
    let run = store
        .reserve(job.id, first_due, first_due)?
        .ok_or_else(|| anyhow!("reservation was not created"))?;
    store.mark_running(run.id, first_due)?;

    let edited = store.edit(
        job.id,
        definition("echo new definition", recurring("*/10 * * * *")),
        edit_at,
    )?;
    assert_eq!(edited.definition.command, "echo new definition");
    assert_eq!(edited.next_at, Some(utc(2025, 3, 1, 10, 10, 0)?));
    let unchanged_run = store.run(run.id)?;
    assert_eq!(unchanged_run.scheduled_at, first_due);
    assert_eq!(unchanged_run.started_at, Some(first_due));
    assert_eq!(unchanged_run.status, RunStatus::Running);
    Ok(())
}

#[test]
fn editing_running_oneoff_does_not_restore_consumed_occurrence() -> Result<()> {
    let root = tempdir()?;
    let created = utc(2025, 3, 1, 10, 0, 0)?;
    let due = utc(2025, 3, 1, 10, 1, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo original", one_off(due)), created)?;
    let run = store
        .reserve(job.id, due, due)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    store.mark_running(run.id, due)?;

    let edited = store.edit(job.id, definition("echo edited", one_off(due)), due)?;
    assert_eq!(edited.definition.command, "echo edited");
    assert_eq!(edited.next_at, None);
    assert!(store.reserve(job.id, due, due)?.is_none());
    assert_eq!(store.run(run.id)?.definition.command, "echo original");
    Ok(())
}

#[test]
fn editing_running_oneoff_timezone_does_not_restore_consumed_occurrence() -> Result<()> {
    let root = tempdir()?;
    let created = utc(2025, 3, 1, 10, 0, 0)?;
    let due = utc(2025, 3, 1, 10, 1, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo original", one_off(due)), created)?;
    let run = store
        .reserve(job.id, due, due)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    store.mark_running(run.id, due)?;

    let edited = store.edit(
        job.id,
        definition(
            "echo original",
            Schedule::OneOff {
                at: due,
                timezone: "America/Los_Angeles".to_owned(),
            },
        ),
        due,
    )?;
    assert_eq!(edited.next_at, None);
    assert_eq!(
        edited.definition.schedule,
        Schedule::OneOff {
            at: due,
            timezone: "America/Los_Angeles".to_owned(),
        }
    );
    assert_eq!(store.run(run.id)?.status, RunStatus::Running);
    assert!(store.reserve(job.id, due, due)?.is_none());
    Ok(())
}

#[test]
fn editing_uncertain_oneoff_timezone_does_not_restore_consumed_occurrence() -> Result<()> {
    let root = tempdir()?;
    let created = utc(2025, 3, 1, 10, 0, 0)?;
    let due = utc(2025, 3, 1, 10, 1, 0)?;
    let recovered_at = utc(2025, 3, 1, 10, 10, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(definition("echo original", one_off(due)), created)?;
    let run = store
        .reserve(job.id, due, due)?
        .ok_or_else(|| anyhow!("one-off reservation was not created"))?;
    store.mark_running(run.id, due)?;
    store.recover(recovered_at)?;

    let edited = store.edit(
        job.id,
        definition(
            "echo original",
            Schedule::OneOff {
                at: due,
                timezone: "America/Los_Angeles".to_owned(),
            },
        ),
        recovered_at,
    )?;
    assert_eq!(edited.next_at, None);
    assert_eq!(store.run(run.id)?.status, RunStatus::Uncertain);
    store.resolve(job.id, recovered_at)?;
    assert_eq!(store.get(job.id)?.state, JobState::Completed);
    assert_eq!(store.get(job.id)?.next_at, None);
    assert_eq!(store.history(Some(job.id))?.len(), 1);
    Ok(())
}

#[test]
fn finishing_old_recurring_run_completes_consumed_edited_oneoff() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let old_due = utc(2025, 3, 1, 10, 1, 0)?;
    let edit_at = utc(2025, 3, 1, 10, 1, 30)?;
    let new_due = utc(2025, 3, 1, 10, 3, 0)?;
    let finished = utc(2025, 3, 1, 10, 4, 0)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition("echo recurring", recurring("* * * * *")),
        initial,
    )?;
    let old_run = store
        .reserve(job.id, old_due, old_due)?
        .ok_or_else(|| anyhow!("recurring reservation was not created"))?;
    store.mark_running(old_run.id, old_due)?;

    let edited = store.edit(
        job.id,
        definition("echo one-off", one_off(new_due)),
        edit_at,
    )?;
    assert_eq!(edited.next_at, Some(new_due));
    let overlap = store
        .reserve(job.id, new_due, new_due)?
        .ok_or_else(|| anyhow!("overlapping one-off was not recorded"))?;
    assert_eq!(overlap.status, RunStatus::Skipped);
    assert_eq!(store.get(job.id)?.next_at, None);

    store.finish(
        old_run.id,
        completion(RunStatus::Succeeded, Some(0)),
        finished,
    )?;
    let completed = store.get(job.id)?;
    assert_eq!(completed.state, JobState::Completed);
    assert_eq!(completed.next_at, None);
    assert_eq!(store.run(old_run.id)?.status, RunStatus::Succeeded);
    assert_eq!(store.run(overlap.id)?.status, RunStatus::Skipped);
    Ok(())
}

#[test]
fn prune_expires_history_older_than_thirty_days() -> Result<()> {
    let root = tempdir()?;
    let old = utc(2025, 1, 1, 10, 0, 0)?;
    let current = utc(2025, 2, 2, 10, 0, 0)?;
    let old_due = utc(2025, 1, 1, 10, 1, 0)?;
    let current_due = utc(2025, 2, 2, 10, 1, 0)?;
    let mut store = Store::open(root.path())?;
    let old_job = store.add(definition("echo old", one_off(old_due)), old)?;
    let old_run = store
        .reserve(old_job.id, old_due, old_due)?
        .ok_or_else(|| anyhow!("old reservation was not created"))?;
    store.finish(
        old_run.id,
        completion(RunStatus::Succeeded, Some(0)),
        old_due,
    )?;
    let current_job = store.add(definition("echo recent", one_off(current_due)), current)?;
    let current_run = store
        .reserve(current_job.id, current_due, current_due)?
        .ok_or_else(|| anyhow!("recent reservation was not created"))?;
    store.finish(
        current_run.id,
        completion(RunStatus::Succeeded, Some(0)),
        current_due,
    )?;

    store.prune(current)?;
    let history = store.history(None)?;
    assert_eq!(only(&history)?.id, current_run.id);
    Ok(())
}

#[test]
fn prune_enforces_output_cap_and_marks_retained_history_missing() -> Result<()> {
    let root = tempdir()?;
    let initial = utc(2025, 3, 1, 10, 0, 0)?;
    let first_due = utc(2025, 3, 1, 10, 1, 0)?;
    let second_due = utc(2025, 3, 1, 10, 2, 0)?;
    let prune_at = utc(2025, 3, 1, 10, 3, 0)?;
    let mut store = Store::open(root.path())?;
    let first_job = store.add(definition("echo large", one_off(first_due)), initial)?;
    let first_run = store
        .reserve(first_job.id, first_due, first_due)?
        .ok_or_else(|| anyhow!("first reservation was not created"))?;
    let second_job = store.add(definition("echo small", one_off(second_due)), initial)?;
    let second_run = store
        .reserve(second_job.id, second_due, second_due)?
        .ok_or_else(|| anyhow!("second reservation was not created"))?;
    store.finish(
        second_run.id,
        RunCompletion {
            stdout_bytes: 1,
            ..completion(RunStatus::Succeeded, Some(0))
        },
        second_due,
    )?;
    store.finish(
        first_run.id,
        RunCompletion {
            stdout_bytes: OUTPUT_LIMIT,
            ..completion(RunStatus::Succeeded, Some(0))
        },
        second_due + Duration::seconds(1),
    )?;

    let (first_stdout, _) = store.output_paths(first_run.id);
    let (second_stdout, _) = store.output_paths(second_run.id);
    create_sparse_file(&first_stdout, OUTPUT_LIMIT)?;
    create_sparse_file(&second_stdout, 1)?;

    store.prune(prune_at)?;
    let first_retained = store.run(first_run.id)?;
    let second_retained = store.run(second_run.id)?;
    assert!(first_retained.output_missing);
    assert!(!second_retained.output_missing);
    assert!(!first_stdout.exists());
    assert!(second_stdout.exists());
    assert_eq!(store.history(None)?.len(), 2);
    Ok(())
}

#[test]
fn prune_enforces_output_cap_across_running_captures() -> Result<()> {
    const CAPTURE_COUNT: usize = 103;
    const CAPTURE_SIZE: u64 = 10 * 1024 * 1024;

    let root = tempdir()?;
    let created = utc(2025, 3, 1, 10, 0, 0)?;
    let due = utc(2025, 3, 1, 10, 1, 0)?;
    let prune_at = utc(2025, 3, 1, 10, 2, 0)?;
    let mut store = Store::open(root.path())?;
    let mut run_ids = Vec::with_capacity(CAPTURE_COUNT);

    for _ in 0..CAPTURE_COUNT {
        let job = store.add(definition("echo active capture", one_off(due)), created)?;
        let run = store
            .reserve(job.id, due, due)?
            .ok_or_else(|| anyhow!("active capture reservation was not created"))?;
        store.mark_running(run.id, due)?;
        let (stdout, _) = store.output_paths(run.id);
        create_sparse_file(&stdout, CAPTURE_SIZE)?;
        run_ids.push(run.id);
    }

    store.prune(prune_at)?;
    let history = store.history(None)?;
    assert_eq!(history.len(), CAPTURE_COUNT);
    let oldest_id = *run_ids
        .first()
        .ok_or_else(|| anyhow!("no active captures were created"))?;
    assert_eq!(history[0].id, oldest_id);
    let oldest = store.run(oldest_id)?;
    assert_eq!(oldest.status, RunStatus::Running);
    assert!(oldest.output_missing);
    let (oldest_stdout, _) = store.output_paths(oldest_id);
    assert!(!oldest_stdout.exists());

    let mut output_bytes = 0_u64;
    for run in &history {
        assert_eq!(run.status, RunStatus::Running);
        let job = store.get(run.job_id)?;
        assert_eq!(job.state, JobState::Active);
        assert_eq!(job.definition.command, "echo active capture");
        let (stdout, stderr) = store.output_paths(run.id);
        for path in [&stdout, &stderr] {
            if path.exists() {
                output_bytes = output_bytes.saturating_add(fs::metadata(path)?.len());
            }
        }
    }
    assert!(output_bytes <= OUTPUT_LIMIT);
    Ok(())
}
