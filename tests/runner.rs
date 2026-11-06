use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use rat::{
    model::{JobState, NewJob, Run, RunStatus},
    runner::Scheduler,
    schedule::Schedule,
    store::Store,
};
use std::os::fd::AsRawFd;
use tempfile::tempdir;

const BASE_SECONDS: i64 = 1_735_689_600;
const OUTPUT_LIMIT: u64 = 10 * 1024 * 1024;

fn instant(seconds: i64) -> Result<DateTime<Utc>> {
    DateTime::<Utc>::from_timestamp(seconds, 0)
        .ok_or_else(|| anyhow!("timestamp {seconds} is out of range"))
}

fn one_off(at: DateTime<Utc>) -> Schedule {
    Schedule::OneOff {
        at,
        timezone: "UTC".to_owned(),
    }
}

fn recurring() -> Schedule {
    Schedule::Recurring {
        cron: "* * * * *".to_owned(),
        timezone: "UTC".to_owned(),
    }
}

fn definition(command: impl Into<String>, cwd: &Path, schedule: Schedule) -> NewJob {
    NewJob {
        command: command.into(),
        cwd: cwd.to_path_buf(),
        path: "/bin:/usr/bin".to_owned(),
        env: BTreeMap::new(),
        schedule,
    }
}

struct StdinBackup(libc::c_int);

impl StdinBackup {
    fn replace(path: &Path) -> Result<Self> {
        let input = fs::File::open(path)?;
        let saved = unsafe { libc::dup(libc::STDIN_FILENO) };
        if saved == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
        if unsafe { libc::dup2(input.as_raw_fd(), libc::STDIN_FILENO) } == -1 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(saved);
            }
            return Err(error.into());
        }
        Ok(Self(saved))
    }
}

impl Drop for StdinBackup {
    fn drop(&mut self) {
        unsafe {
            libc::dup2(self.0, libc::STDIN_FILENO);
            libc::close(self.0);
        }
    }
}

fn wait_for_path(path: &Path, timeout: Duration) -> Result<()> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if path.exists() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("timed out waiting for {}", path.display());
}

fn run_id(scheduler: &Scheduler, job_id: i64) -> Result<i64> {
    let runs = scheduler.store().history(Some(job_id))?;
    if runs.len() != 1 {
        bail!("expected one run for job {job_id}, found {}", runs.len());
    }
    Ok(runs[0].id)
}

fn wait_for_run(
    scheduler: &mut Scheduler,
    job_id: i64,
    run_id: i64,
    tick_at: DateTime<Utc>,
) -> Result<Run> {
    let deadline = Instant::now() + Duration::from_secs(8);
    while Instant::now() < deadline {
        scheduler.tick(tick_at)?;
        if let Some(run) = scheduler
            .store()
            .history(Some(job_id))?
            .into_iter()
            .find(|run| run.id == run_id)
            && matches!(
                run.status,
                RunStatus::Succeeded
                    | RunStatus::Failed
                    | RunStatus::Skipped
                    | RunStatus::Uncertain
            )
        {
            return Ok(run);
        }
        thread::sleep(Duration::from_millis(10));
    }
    bail!("run {run_id} for job {job_id} did not finish before timeout");
}

fn shell_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphabetic() || first == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

fn held_command(started: &str, release: &str, finished: &str) -> String {
    format!(
        ": > {started}; i=0; while [ ! -e {release} ] && [ \"$i\" -lt 1000 ]; do i=$((i + 1)); /bin/sleep 0.01; done; : > {finished}"
    )
}

fn file_len_or_zero(path: &Path) -> Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error.into()),
    }
}

#[test]
fn executes_literal_pipeline_and_redirect_in_saved_cwd() -> Result<()> {
    let root = tempdir()?;
    let cwd = root.path().join("saved-cwd");
    fs::create_dir_all(&cwd)?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let command = "printf 'alpha\\nbeta\\n' | tr '[:lower:]' '[:upper:]' > result.txt; printf 'pipeline complete\\n'";

    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(command, &cwd, one_off(scheduled_at)),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    assert_eq!(scheduler.store().get(job.id)?.definition.command, command);

    let run_id = run_id(&scheduler, job.id)?;
    let run = wait_for_run(&mut scheduler, job.id, run_id, scheduled_at)?;
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(fs::read(cwd.join("result.txt"))?, b"ALPHA\nBETA\n");
    let (stdout, _) = scheduler.store().output_paths(run_id);
    assert_eq!(fs::read(stdout)?, b"pipeline complete\n");
    Ok(())
}

#[test]
fn uses_saved_cwd_path_and_explicit_environment_with_null_stdin() -> Result<()> {
    let root = tempdir()?;
    let cwd = root.path().join("saved-cwd");
    fs::create_dir_all(&cwd)?;
    let inherited_name = std::env::vars_os()
        .filter_map(|(name, _)| name.into_string().ok())
        .find(|name| {
            name != "PATH"
                && !matches!(
                    name.as_str(),
                    "PWD" | "OLDPWD" | "SHLVL" | "_" | "IFS" | "OPTIND" | "PPID" | "RAT_EXPLICIT"
                )
                && shell_identifier(name)
        })
        .ok_or_else(|| anyhow!("test process has no shell-compatible inherited environment key"))?;
    let command = format!(
        "printf 'path=%s\\nexplicit=%s\\ninherited=%s\\n' \"$PATH\" \"$RAT_EXPLICIT\" \"${{{inherited_name}-unset}}\"; if IFS= read -r line; then printf 'stdin=read:%s\\n' \"$line\"; else printf 'stdin=null\\n'; fi"
    );
    let scheduled_at = instant(BASE_SECONDS)?;
    let mut definition = definition(command, &cwd, one_off(scheduled_at));
    definition.path = "/saved/rat-test-path".to_owned();
    definition
        .env
        .insert("RAT_EXPLICIT".to_owned(), "explicit-value".to_owned());

    let mut store = Store::open(root.path())?;
    let job = store.add(definition, scheduled_at - ChronoDuration::seconds(1))?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    let stdin_fixture = root.path().join("inherited-stdin");
    fs::write(&stdin_fixture, b"unexpected inherited input\n")?;
    let stdin_backup = StdinBackup::replace(&stdin_fixture)?;
    scheduler.tick(scheduled_at)?;
    drop(stdin_backup);
    let run_id = run_id(&scheduler, job.id)?;
    let run = wait_for_run(&mut scheduler, job.id, run_id, scheduled_at)?;
    assert_eq!(run.status, RunStatus::Succeeded);

    let (stdout, _) = scheduler.store().output_paths(run_id);
    let output = fs::read_to_string(stdout)?;
    assert_eq!(
        output,
        "path=/saved/rat-test-path\nexplicit=explicit-value\ninherited=unset\nstdin=null\n"
    );
    Ok(())
}

#[test]
fn captures_both_streams_and_records_nonzero_exit() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let command = "printf 'out-line\\n'; printf 'err-line\\n' >&2; exit 23";
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(command, root.path(), one_off(scheduled_at)),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    let run_id = run_id(&scheduler, job.id)?;
    let run = wait_for_run(&mut scheduler, job.id, run_id, scheduled_at)?;

    assert_eq!(run.status, RunStatus::Failed);
    assert_eq!(run.exit_code, Some(23));
    assert_eq!(run.stdout_bytes, 9);
    assert_eq!(run.stderr_bytes, 9);
    let (stdout, stderr) = scheduler.store().output_paths(run_id);
    assert_eq!(fs::read(stdout)?, b"out-line\n");
    assert_eq!(fs::read(stderr)?, b"err-line\n");
    Ok(())
}

#[test]
fn launches_distinct_jobs_concurrently_using_a_file_barrier() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let command_a = held_command("a-started", "b-started", "a-finished");
    let command_b = held_command("b-started", "a-started", "b-finished");
    let mut store = Store::open(root.path())?;
    let job_a = store.add(
        definition(command_a, root.path(), one_off(scheduled_at)),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let job_b = store.add(
        definition(command_b, root.path(), one_off(scheduled_at)),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    let run_a = run_id(&scheduler, job_a.id)?;
    let run_b = scheduler
        .store()
        .history(Some(job_b.id))?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("second job was not dispatched"))?
        .id;

    wait_for_path(&root.path().join("a-started"), Duration::from_secs(6))?;
    wait_for_path(&root.path().join("b-started"), Duration::from_secs(6))?;
    let result_a = wait_for_run(&mut scheduler, job_a.id, run_a, scheduled_at)?;
    let result_b = wait_for_run(&mut scheduler, job_b.id, run_b, scheduled_at)?;
    assert_eq!(result_a.status, RunStatus::Succeeded);
    assert_eq!(result_b.status, RunStatus::Succeeded);
    assert!(root.path().join("a-finished").exists());
    assert!(root.path().join("b-finished").exists());
    Ok(())
}

#[test]
fn deleting_a_running_job_preserves_its_completion_and_history() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let command = held_command("started", "release", "finished");
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(command, root.path(), one_off(scheduled_at)),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    let run_id = run_id(&scheduler, job.id)?;
    wait_for_path(&root.path().join("started"), Duration::from_secs(3))?;
    assert!(scheduler.has_running());

    let mut deletion_store = Store::open(root.path())?;
    deletion_store.delete(job.id)?;
    assert_eq!(deletion_store.get(job.id)?.state, JobState::Deleted);
    fs::write(root.path().join("release"), b"")?;

    let run = wait_for_run(&mut scheduler, job.id, run_id, scheduled_at)?;
    assert_eq!(run.status, RunStatus::Succeeded);
    assert_eq!(scheduler.store().history(Some(job.id))?.len(), 1);
    assert_eq!(scheduler.store().get(job.id)?.state, JobState::Deleted);
    assert!(root.path().join("finished").exists());
    Ok(())
}

#[test]
fn recurring_overlap_is_skipped_without_queueing_another_execution() -> Result<()> {
    let root = tempdir()?;
    let created_at = instant(BASE_SECONDS)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(
            held_command("started", "release", "finished"),
            root.path(),
            recurring(),
        ),
        created_at,
    )?;
    let first_at = job
        .next_at
        .ok_or_else(|| anyhow!("recurring job has no first occurrence"))?;
    let before_first = first_at - ChronoDuration::milliseconds(100);
    let mut scheduler = Scheduler::new(store, before_first)?;
    scheduler.tick(before_first)?;
    scheduler.tick(first_at)?;
    let first_run_id = run_id(&scheduler, job.id)?;
    wait_for_path(&root.path().join("started"), Duration::from_secs(3))?;
    assert!(scheduler.has_running());

    let skipped_at = scheduler
        .store()
        .get(job.id)?
        .next_at
        .ok_or_else(|| anyhow!("recurring job has no next occurrence"))?;
    let before_skipped = skipped_at - ChronoDuration::milliseconds(100);
    scheduler.tick(before_skipped)?;
    scheduler.tick(skipped_at)?;
    let skipped = scheduler
        .store()
        .history(Some(job.id))?
        .into_iter()
        .find(|run| run.scheduled_at == skipped_at)
        .ok_or_else(|| anyhow!("overlapping occurrence was not recorded"))?;
    assert_eq!(skipped.status, RunStatus::Skipped);
    assert_eq!(skipped.started_at, None);

    fs::write(root.path().join("release"), b"")?;
    let first = wait_for_run(&mut scheduler, job.id, first_run_id, skipped_at)?;
    assert_eq!(first.status, RunStatus::Succeeded);
    scheduler.tick(skipped_at)?;
    let history = scheduler.store().history(Some(job.id))?;
    assert_eq!(history.len(), 2);
    assert_eq!(
        history
            .iter()
            .filter(|run| run.status == RunStatus::Skipped)
            .count(),
        1
    );
    assert_eq!(
        history
            .iter()
            .filter(|run| run.status == RunStatus::Succeeded)
            .count(),
        1
    );
    assert!(root.path().join("finished").exists());
    Ok(())
}

#[test]
fn truncates_output_after_ten_mib_without_stopping_command() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let command = "/bin/dd if=/dev/zero bs=1048576 count=11 2>/dev/null || exit 99; printf 'completed' > output-after-cap";
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(command, root.path(), one_off(scheduled_at)),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    let run_id = run_id(&scheduler, job.id)?;
    let run = wait_for_run(&mut scheduler, job.id, run_id, scheduled_at)?;

    assert_eq!(run.status, RunStatus::Succeeded);
    assert!(run.truncated);
    assert_eq!(
        fs::read(root.path().join("output-after-cap"))?,
        b"completed"
    );
    let (stdout, stderr) = scheduler.store().output_paths(run_id);
    let stored = file_len_or_zero(&stdout)? + file_len_or_zero(&stderr)?;
    assert!(stored <= OUTPUT_LIMIT, "captured {stored} bytes");
    assert!(stored > 0);
    Ok(())
}

#[test]
fn recovering_a_lost_scheduler_does_not_replay_an_uncertain_side_effect() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(
            "printf 'effect\\n' >> side-effect.txt; i=0; while [ ! -e release ] && [ \"$i\" -lt 1000 ]; do i=$((i + 1)); /bin/sleep 0.01; done; : > original-finished",
            root.path(),
            one_off(scheduled_at),
        ),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    let run_id = run_id(&scheduler, job.id)?;
    wait_for_path(&root.path().join("side-effect.txt"), Duration::from_secs(3))?;

    drop(scheduler);
    let recovered_at = scheduled_at + ChronoDuration::seconds(1);
    let store = Store::open(root.path())?;
    let mut recovered = Scheduler::new(store, recovered_at)?;
    assert_eq!(recovered.store().get(job.id)?.state, JobState::NeedsReview);
    let history = recovered.store().history(Some(job.id))?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].id, run_id);
    assert_eq!(history[0].status, RunStatus::Uncertain);

    recovered.tick(recovered_at)?;
    assert_eq!(recovered.store().history(Some(job.id))?.len(), 1);
    assert_eq!(fs::read(root.path().join("side-effect.txt"))?, b"effect\n");
    fs::write(root.path().join("release"), b"")?;
    wait_for_path(
        &root.path().join("original-finished"),
        Duration::from_secs(3),
    )?;
    recovered.tick(recovered_at)?;
    assert_eq!(recovered.store().history(Some(job.id))?.len(), 1);
    assert_eq!(recovered.store().get(job.id)?.state, JobState::NeedsReview);
    assert_eq!(fs::read(root.path().join("side-effect.txt"))?, b"effect\n");
    Ok(())
}

#[test]
fn continues_draining_output_from_a_bounded_background_descendant() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = instant(BASE_SECONDS)?;
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(
            "(/bin/sleep 0.2; /bin/echo late && : > descendant-done) &",
            root.path(),
            one_off(scheduled_at),
        ),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    let mut scheduler = Scheduler::new(store, scheduled_at)?;
    scheduler.tick(scheduled_at)?;
    let run_id = run_id(&scheduler, job.id)?;
    let finished = root.path().join("descendant-done");
    let (stdout, _) = scheduler.store().output_paths(run_id);
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut descendant_finished = false;
    let mut output_drained = false;

    while Instant::now() < deadline && !(descendant_finished && output_drained) {
        let tick_started = Instant::now();
        scheduler.tick(scheduled_at)?;
        assert!(
            tick_started.elapsed() < Duration::from_millis(180),
            "tick blocked while the descendant retained the shell's output pipe"
        );
        descendant_finished = finished.exists();
        output_drained = fs::read_to_string(&stdout)?.contains("late\n");
        if !(descendant_finished && output_drained) {
            thread::sleep(Duration::from_millis(10));
        }
    }

    assert!(descendant_finished, "background descendant did not finish");
    assert!(
        output_drained,
        "background descendant's output was not captured"
    );
    Ok(())
}

#[test]
fn once_waits_for_a_spawned_shell_after_running_status_persistence_fails() -> Result<()> {
    let root = tempdir()?;
    let scheduled_at = Utc::now() - ChronoDuration::seconds(1);
    let finished = root.path().join("finished");
    let mut store = Store::open(root.path())?;
    let job = store.add(
        definition(
            "/bin/sleep 0.2; /bin/echo late && : > finished",
            root.path(),
            one_off(scheduled_at),
        ),
        scheduled_at - ChronoDuration::seconds(1),
    )?;
    drop(store);

    let connection = rusqlite::Connection::open(root.path().join("rat.sqlite3"))?;
    connection.execute_batch(
        "CREATE TRIGGER fail_running_status
         BEFORE UPDATE OF status ON runs
         WHEN NEW.status = 'Running'
         BEGIN
             SELECT RAISE(FAIL, 'injected Running status failure');
         END;",
    )?;
    drop(connection);

    let result = rat::runner::run(root.path(), true);
    let finished_before_return = finished.exists();
    wait_for_path(&finished, Duration::from_secs(3))?;
    let error = result
        .err()
        .ok_or_else(|| anyhow!("run-once should report the injected persistence failure"))?;
    assert!(
        format!("{error:#}").contains("injected Running status failure"),
        "run-once returned an unexpected error: {error:#}"
    );
    assert!(
        finished_before_return,
        "run-once returned before its spawned shell completed"
    );

    let store = Store::open(root.path())?;
    assert_eq!(store.get(job.id)?.state, JobState::NeedsReview);
    let history = store.history(Some(job.id))?;
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].status, RunStatus::Uncertain);
    assert_eq!(history[0].stdout_bytes, 5);
    assert_eq!(history[0].stderr_bytes, 0);
    assert!(!history[0].truncated);
    assert!(!history[0].output_missing);
    let (stdout, stderr) = store.output_paths(history[0].id);
    assert_eq!(fs::read(stdout)?, b"late\n");
    assert_eq!(fs::read(stderr)?, b"");
    drop(store);

    let mut scheduler = Scheduler::new(Store::open(root.path())?, Utc::now())?;
    scheduler.tick(Utc::now())?;
    assert_eq!(scheduler.store().get(job.id)?.state, JobState::NeedsReview);
    assert_eq!(scheduler.store().history(Some(job.id))?.len(), 1);
    assert!(finished.exists(), "uncertain job was retried");
    Ok(())
}
