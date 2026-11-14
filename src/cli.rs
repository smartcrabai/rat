use std::collections::BTreeMap;
use std::fmt::{self, Display, Formatter};
use std::fs::{self, File};
use std::io::{self, IsTerminal, Write};
use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand, error::ErrorKind};
use dialoguer::{Confirm, Select, theme::SimpleTheme};

use crate::model::{Job, JobState, NewJob, Run, RunStatus};
use crate::runner;
use crate::schedule::{self, Schedule};
use crate::service;
use crate::store::Store;

#[derive(Debug, Parser)]
#[command(name = "rat", about = "Schedule local shell commands")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Register one command string.
    Add(AddArgs),
    /// Show scheduled jobs.
    List {
        #[arg(long)]
        all: bool,
        #[arg(long)]
        json: bool,
    },
    /// Show one job.
    Show {
        id: i64,
        #[arg(long)]
        json: bool,
    },
    /// Edit a job's future definition.
    Edit(EditArgs),
    /// Pause a job.
    Pause { id: i64 },
    /// Resume a paused job.
    Resume {
        id: i64,
        #[arg(long)]
        confirm_overdue: bool,
    },
    /// Explicitly acknowledge an uncertain occurrence without replaying it.
    Review {
        id: i64,
        #[arg(long)]
        acknowledge: bool,
    },
    /// Stop future starts while retaining the run history.
    Delete { id: i64 },
    /// Show recorded executions.
    History {
        id: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Print captured output for one run.
    Output { run_id: i64 },
    /// Run the scheduler in the foreground.
    Daemon {
        #[arg(long)]
        once: bool,
    },
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
}

#[derive(Debug, Args)]
struct AddArgs {
    #[arg(long, conflicts_with = "schedule")]
    cron: Option<String>,
    #[arg(long)]
    timezone: Option<String>,
    #[arg(long = "env", num_args = 1..)]
    env: Vec<String>,
    #[arg(value_name = "TIME", num_args = 0.., allow_negative_numbers = true)]
    schedule: Vec<String>,
    #[arg(last = true, required = true, value_name = "COMMAND", num_args = 1..)]
    command: Vec<String>,
}

#[derive(Debug, Args)]
struct EditArgs {
    id: i64,
    #[arg(long, conflicts_with = "at")]
    cron: Option<String>,
    #[arg(long = "at", num_args = 1.., conflicts_with = "cron", allow_negative_numbers = true)]
    at: Vec<String>,
    #[arg(long)]
    timezone: Option<String>,
    #[arg(long = "env", num_args = 1..)]
    env: Vec<String>,
    #[arg(last = true, required = false, value_name = "COMMAND", num_args = 1..)]
    command: Vec<String>,
}

#[derive(Debug, Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Status,
}

/// Run the command-line or interactive interface.
///
/// # Errors
///
/// Returns an error for invalid arguments, unavailable scheduler mutations, failed
/// persistence, or terminal and service-manager I/O failures.
pub fn run() -> Result<()> {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            ) =>
        {
            writeln!(io::stdout().lock(), "{error}")?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let root = service::data_root()?;
    match cli.command {
        None => run_interactive(&root),
        Some(command) => run_command(command, &root),
    }
}

fn run_command(command: Command, root: &Path) -> Result<()> {
    match command {
        Command::Add(args) => add_command(args, root),
        Command::List { all, json } => {
            let store = Store::open(root)?;
            let mut jobs = store.jobs(all)?;
            jobs.sort_by_key(|job| job.id);
            if json {
                print_json(&jobs)
            } else {
                print_jobs(&jobs)
            }
        }
        Command::Show { id, json } => {
            let store = Store::open(root)?;
            let job = store.get(id)?;
            if json {
                print_json(&job)
            } else {
                print_job(&job)
            }
        }
        Command::Edit(args) => edit_command(args, root),
        Command::Pause { id } => {
            Store::open(root)?.pause(id)?;
            print_message(&format!("Paused job {id}."))
        }
        Command::Resume {
            id,
            confirm_overdue,
        } => {
            require_scheduler(root)?;
            Store::open(root)?.resume(id, Utc::now(), confirm_overdue)?;
            print_message(&format!("Resumed job {id}."))
        }
        Command::Review { id, acknowledge } => {
            ensure!(
                acknowledge,
                "review requires --acknowledge to consume the uncertain occurrence"
            );
            Store::open(root)?.resolve(id, Utc::now())?;
            print_message(&format!(
                "Acknowledged the uncertain occurrence for job {id}; it will not be replayed."
            ))
        }
        Command::Delete { id } => {
            Store::open(root)?.delete(id)?;
            print_message(&format!(
                "Deleted job {id}; an already-running command is not stopped."
            ))
        }
        Command::History { id, json } => {
            let store = Store::open(root)?;
            let mut runs = store.history(id)?;
            runs.sort_by_key(|run| run.id);
            if json {
                print_json(&runs)
            } else {
                print_history(&runs)
            }
        }
        Command::Output { run_id } => print_output(root, run_id),
        Command::Daemon { once } => runner::run(root, once),
        Command::Service { action } => match action {
            ServiceAction::Install => {
                service::install(root)?;
                print_message(&format!(
                    "Installed rat's login service for {}.",
                    root.display()
                ))
            }
            ServiceAction::Uninstall => {
                service::uninstall(root)?;
                print_message(&format!(
                    "Uninstalled rat's login service for {}.",
                    root.display()
                ))
            }
            ServiceAction::Status => print_message(&service::status(root)?),
        },
    }
}

fn add_command(args: AddArgs, root: &Path) -> Result<()> {
    require_scheduler(root)?;
    let command = exactly_one_command(args.command)?;
    validate_command(&command)?;
    let timezone = args
        .timezone
        .map_or_else(default_timezone, validate_timezone)?;
    let now = Utc::now();
    let schedule = match args.cron {
        Some(cron) => {
            ensure!(
                args.schedule.is_empty(),
                "--cron cannot be combined with one-off time tokens"
            );
            schedule::parse_recurring(&cron, &timezone)?
        }
        None => schedule::parse_one_off(&args.schedule, now, &timezone)?,
    };
    let env = parse_environment(args.env)?;
    let definition = new_definition(command, schedule, env)?;
    let job = Store::open(root)?.add(definition, now)?;
    print_saved_job(&job)
}

fn edit_command(args: EditArgs, root: &Path) -> Result<()> {
    require_scheduler(root)?;
    let EditArgs {
        id,
        cron,
        at,
        timezone: timezone_option,
        env: environment,
        command: command_args,
    } = args;
    let command = if command_args.is_empty() {
        None
    } else {
        Some(exactly_one_command(command_args)?)
    };
    if let Some(value) = &command {
        validate_command(value)?;
    }
    ensure!(
        cron.is_some()
            || !at.is_empty()
            || timezone_option.is_some()
            || !environment.is_empty()
            || command.is_some(),
        "edit requires at least one of --cron, --at, --timezone, --env, or -- COMMAND"
    );

    let mut store = Store::open(root)?;
    let existing = store.get(id)?;
    let now = Utc::now();
    let timezone = match &timezone_option {
        Some(value) => validate_timezone(value.clone())?,
        None => existing.definition.schedule.timezone().to_owned(),
    };
    let schedule = match (cron, at.is_empty(), timezone_option) {
        (Some(cron), _, _) => schedule::parse_recurring(&cron, &timezone)?,
        (None, false, _) => schedule::parse_one_off(&at, now, &timezone)?,
        (None, true, Some(_)) => update_schedule_timezone(&existing.definition.schedule, timezone),
        (None, true, None) => existing.definition.schedule.clone(),
    };
    let definition = NewJob {
        command: command.unwrap_or(existing.definition.command),
        cwd: existing.definition.cwd,
        path: existing.definition.path,
        env: if environment.is_empty() {
            existing.definition.env
        } else {
            parse_environment(environment)?
        },
        schedule,
    };
    let job = store.edit(id, definition, now)?;
    print_saved_job(&job)
}

fn update_schedule_timezone(schedule: &Schedule, timezone: String) -> Schedule {
    match schedule {
        Schedule::OneOff { at, .. } => Schedule::OneOff { at: *at, timezone },
        Schedule::Recurring { cron, .. } => Schedule::Recurring {
            cron: cron.clone(),
            timezone,
        },
    }
}

fn exactly_one_command(mut command: Vec<String>) -> Result<String> {
    ensure!(
        command.len() == 1,
        "the argument after -- must be exactly one shell command string"
    );
    Ok(command.remove(0))
}

fn validate_command(command: &str) -> Result<()> {
    ensure!(
        !command.trim().is_empty(),
        "command string must not be empty"
    );
    ensure!(
        !command.contains('\0'),
        "command string must not contain NUL bytes"
    );
    Ok(())
}

fn parse_environment(values: Vec<String>) -> Result<BTreeMap<String, String>> {
    let mut variables = BTreeMap::new();
    for value in values {
        let (name, content) = value
            .split_once('=')
            .with_context(|| format!("environment entry {value:?} must use KEY=VALUE"))?;
        ensure!(
            valid_environment_name(name),
            "invalid environment variable name {name:?}"
        );
        ensure!(
            !content.contains('\0'),
            "environment values must not contain NUL bytes"
        );
        ensure!(
            variables
                .insert(name.to_owned(), content.to_owned())
                .is_none(),
            "environment variable {name:?} was specified more than once"
        );
    }
    Ok(variables)
}

fn valid_environment_name(name: &str) -> bool {
    let mut characters = name.bytes();
    matches!(characters.next(), Some(b'A'..=b'Z' | b'a'..=b'z' | b'_'))
        && characters.all(|character| character.is_ascii_alphanumeric() || character == b'_')
}

fn new_definition(
    command: String,
    schedule: Schedule,
    env: BTreeMap<String, String>,
) -> Result<NewJob> {
    let cwd = std::env::current_dir().context("cannot determine the current working directory")?;
    let path = match std::env::var_os("PATH") {
        Some(value) => value
            .into_string()
            .map_err(|_| anyhow::anyhow!("PATH must be valid UTF-8 to save it with a job"))?,
        None => String::new(),
    };
    Ok(NewJob {
        command,
        cwd,
        path,
        env,
        schedule,
    })
}

fn default_timezone() -> Result<String> {
    if let Ok(value) = std::env::var("TZ") {
        let candidate = value.strip_prefix(':').unwrap_or(&value);
        if !candidate.is_empty() && chrono_tz::Tz::from_str(candidate).is_ok() {
            return Ok(candidate.to_owned());
        }
    }
    let timezone = iana_time_zone::get_timezone()
        .context("cannot determine the local IANA timezone; pass --timezone or set TZ")?;
    validate_timezone(timezone)
}

fn validate_timezone(timezone: String) -> Result<String> {
    ensure!(
        chrono_tz::Tz::from_str(&timezone).is_ok(),
        "timezone {timezone:?} is not a recognized IANA timezone"
    );
    Ok(timezone)
}

fn require_scheduler(root: &Path) -> Result<()> {
    if runner::scheduler_available(root) {
        return Ok(());
    }
    bail!(
        "rat's scheduler is not running for {}; start it with `rat daemon` or explicitly install autostart with `rat service install`",
        root.display()
    )
}

fn print_saved_job(job: &Job) -> Result<()> {
    match job.next_at {
        Some(next_at) => print_message(&format!(
            "Saved job {}; next at {} ({})",
            job.id,
            format_instant(next_at),
            job.definition.schedule.timezone()
        )),
        None => print_message(&format!("Saved job {}; no future occurrence", job.id)),
    }
}

fn format_instant(instant: DateTime<Utc>) -> String {
    instant.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer_pretty(&mut output, value)?;
    writeln!(output)?;
    Ok(())
}

fn print_message(message: &str) -> Result<()> {
    let stdout = io::stdout();
    writeln!(stdout.lock(), "{message}")?;
    Ok(())
}

fn print_jobs(jobs: &[Job]) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "ID  STATE        NEXT_AT                     TIMEZONE       COMMAND"
    )?;
    for job in jobs {
        let next_at = job.next_at.map_or_else(|| "-".to_owned(), format_instant);
        writeln!(
            output,
            "{}  {:<12} {:<27} {:<14} {}",
            job.id,
            state_label(job.state),
            next_at,
            job.definition.schedule.timezone(),
            job.definition.command
        )?;
    }
    Ok(())
}

fn print_job(job: &Job) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "{}",
        JobDetails {
            job,
            sanitize: false,
        }
    )?;
    Ok(())
}

struct JobDetails<'a> {
    job: &'a Job,
    sanitize: bool,
}

impl Display for JobDetails<'_> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        let job = self.job;
        writeln!(formatter, "Job {}", job.id)?;
        writeln!(formatter, "State: {}", state_label(job.state))?;
        writeln!(
            formatter,
            "Next at: {}",
            job.next_at.map_or_else(|| "-".to_owned(), format_instant)
        )?;
        writeln!(
            formatter,
            "Timezone: {}",
            JobDetailValue {
                value: job.definition.schedule.timezone(),
                sanitize: self.sanitize,
            }
        )?;
        writeln!(formatter, "Schedule: {:?}", job.definition.schedule)?;
        writeln!(
            formatter,
            "Working directory: {}",
            JobDetailValue {
                value: job.definition.cwd.display(),
                sanitize: self.sanitize,
            }
        )?;
        writeln!(
            formatter,
            "PATH: {}",
            JobDetailValue {
                value: job.definition.path.as_str(),
                sanitize: self.sanitize,
            }
        )?;
        writeln!(
            formatter,
            "Command: {}",
            JobDetailValue {
                value: job.definition.command.as_str(),
                sanitize: self.sanitize,
            }
        )?;
        write!(formatter, "Environment:")?;
        for (name, value) in &job.definition.env {
            write!(
                formatter,
                "\n  {}={}",
                JobDetailValue {
                    value: name.as_str(),
                    sanitize: self.sanitize,
                },
                JobDetailValue {
                    value: value.as_str(),
                    sanitize: self.sanitize,
                }
            )?;
        }
        Ok(())
    }
}

struct JobDetailValue<T> {
    value: T,
    sanitize: bool,
}

impl<T: Display> Display for JobDetailValue<T> {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> fmt::Result {
        if self.sanitize {
            let rendered = self.value.to_string();
            formatter.write_str(&safe_label(&rendered))
        } else {
            Display::fmt(&self.value, formatter)
        }
    }
}

fn print_history(runs: &[Run]) -> Result<()> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    writeln!(
        output,
        "RUN  JOB  STATUS      SCHEDULED_AT                STARTED_AT                  FINISHED_AT                 EXIT"
    )?;
    for run in runs {
        writeln!(
            output,
            "{}  {}  {:<11} {}  {}  {}  {}",
            run.id,
            run.job_id,
            run_status_label(run.status),
            format_instant(run.scheduled_at),
            run.started_at
                .map_or_else(|| "-".to_owned(), format_instant),
            run.finished_at
                .map_or_else(|| "-".to_owned(), format_instant),
            run.exit_code
                .map_or_else(|| "-".to_owned(), |code| code.to_string())
        )?;
    }
    Ok(())
}

fn state_label(state: JobState) -> &'static str {
    match state {
        JobState::Active => "active",
        JobState::Paused => "paused",
        JobState::NeedsReview => "needs-review",
        JobState::Completed => "completed",
        JobState::Deleted => "deleted",
    }
}

fn run_status_label(status: RunStatus) -> &'static str {
    match status {
        RunStatus::Starting => "starting",
        RunStatus::Running => "running",
        RunStatus::Succeeded => "succeeded",
        RunStatus::Failed => "failed",
        RunStatus::Skipped => "skipped",
        RunStatus::Uncertain => "uncertain",
    }
}

fn print_output(root: &Path, run_id: i64) -> Result<()> {
    let store = Store::open(root)?;
    let run = store.run(run_id)?;
    let (stdout_path, stderr_path) = store.output_paths(run_id);
    let stdout = io::stdout();
    let mut stdout = stdout.lock();
    let stderr = io::stderr();
    let mut stderr = stderr.lock();
    writeln!(stdout, "stdout:")?;
    let stdout_missing = write_output_file(&stdout_path, "stdout", &mut stdout)?;
    writeln!(stderr, "stderr:")?;
    let stderr_missing = write_output_file(&stderr_path, "stderr", &mut stderr)?;
    if stdout_missing || stderr_missing || run.output_missing {
        writeln!(
            stderr,
            "warning: some captured output for run {run_id} is missing"
        )?;
    }
    if run.truncated {
        writeln!(
            stderr,
            "warning: output for run {run_id} was truncated at the per-run limit"
        )?;
    }
    Ok(())
}

fn write_output_file(path: &Path, stream: &str, output: &mut impl Write) -> Result<bool> {
    match File::open(path) {
        Ok(mut file) => {
            io::copy(&mut file, output)
                .with_context(|| format!("cannot read {stream} output for run"))?;
            Ok(false)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(error).with_context(|| format!("cannot open {stream} output")),
    }
}

#[derive(Debug)]
enum InteractiveChoice {
    Add,
    Refresh,
    RunHistory,
    Rat(Job),
    At(crate::native::AtJob),
    Cron {
        job: crate::native::CronJob,
        raw_line: String,
    },
    AtUnavailable(String),
    CronUnavailable(String),
    Quit,
}

#[derive(Debug)]
struct InteractiveCancelled;

impl std::fmt::Display for InteractiveCancelled {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("interactive action cancelled")
    }
}

impl std::error::Error for InteractiveCancelled {}

fn run_interactive(root: &Path) -> Result<()> {
    ensure!(
        io::stdin().is_terminal() && io::stdout().is_terminal(),
        "no command was given and stdin/stdout are not terminals; use `rat list` for a non-interactive listing"
    );
    let mut store = Store::open(root)?;
    let stopping = Arc::new(AtomicBool::new(false));
    let _signals = runner::SignalRegistrations::install(Arc::clone(&stopping))?;
    loop {
        if stopping.load(Ordering::Relaxed) {
            return Ok(());
        }
        let (choices, labels) = match interactive_choices(&store) {
            Ok(choices) => choices,
            Err(_) if stopping.load(Ordering::Relaxed) => return Ok(()),
            Err(error) => return Err(error),
        };
        if stopping.load(Ordering::Relaxed) {
            return Ok(());
        }
        let selected = match select_menu("Scheduled jobs", &labels, 0) {
            Ok(selected) => selected,
            Err(error) if error.is::<InteractiveCancelled>() => return Ok(()),
            Err(_) if stopping.load(Ordering::Relaxed) => return Ok(()),
            Err(error) => return Err(error),
        };
        if stopping.load(Ordering::Relaxed) {
            return Ok(());
        }
        let Some(selected) = selected else {
            return Ok(());
        };
        let Some(choice) = choices.get(selected) else {
            continue;
        };
        let result = match choice {
            InteractiveChoice::Add => interactive_add_menu(root, &mut store),
            InteractiveChoice::Refresh => Ok(()),
            InteractiveChoice::RunHistory => interactive_output(root, &store, None),
            InteractiveChoice::Rat(job) => interactive_rat_job(root, &mut store, job.id),
            InteractiveChoice::At(job) => interactive_at_details(job),
            InteractiveChoice::Cron { job, raw_line } => interactive_cron_details(job, raw_line),
            InteractiveChoice::AtUnavailable(message) => {
                print_interactive_message(&format!("at is unavailable: {message}"))
            }
            InteractiveChoice::CronUnavailable(message) => {
                print_interactive_message(&format!("cron is unavailable: {message}"))
            }
            InteractiveChoice::Quit => return Ok(()),
        };
        if stopping.load(Ordering::Relaxed) {
            return Ok(());
        }
        if let Err(error) = result
            && !error.is::<InteractiveCancelled>()
        {
            writeln!(
                io::stderr().lock(),
                "rat: {}",
                terminal_text(&format!("{error:#}"))
            )?;
        }
    }
}

fn interactive_choices(store: &Store) -> Result<(Vec<InteractiveChoice>, Vec<String>)> {
    let mut choices = vec![
        InteractiveChoice::Add,
        InteractiveChoice::Refresh,
        InteractiveChoice::RunHistory,
    ];
    let mut labels = vec![
        "Add a job".to_owned(),
        "Refresh".to_owned(),
        "Run history (rat)".to_owned(),
    ];
    for job in store.jobs(false)? {
        labels.push(safe_label(&format!(
            "rat job #{}: [{}] next {}: {}",
            job.id,
            state_label(job.state),
            job.next_at.map_or_else(|| "-".to_owned(), format_instant),
            job.definition.command
        )));
        choices.push(InteractiveChoice::Rat(job));
    }
    match crate::native::list_at() {
        Ok(jobs) => {
            for job in jobs {
                let label = if job.queue.is_empty() {
                    format!("at job {}: {}", job.id, job.when)
                } else {
                    format!("at job {} (queue {}): {}", job.id, job.queue, job.when)
                };
                labels.push(safe_label(&label));
                choices.push(InteractiveChoice::At(job));
            }
        }
        Err(error) => {
            labels.push(safe_label(&format!("at unavailable: {error:#}")));
            choices.push(InteractiveChoice::AtUnavailable(format!("{error:#}")));
        }
    }
    match crate::native::read_crontab() {
        Ok(table) => {
            for job in table.jobs {
                let raw_line = original_crontab_line(&table.text, job.line)
                    .unwrap_or("")
                    .to_owned();
                labels.push(safe_label(&format!(
                    "cron line {}: {} {}",
                    job.line, job.schedule, job.command
                )));
                choices.push(InteractiveChoice::Cron { job, raw_line });
            }
        }
        Err(error) => {
            labels.push(safe_label(&format!("cron unavailable: {error:#}")));
            choices.push(InteractiveChoice::CronUnavailable(format!("{error:#}")));
        }
    }
    labels.push("Quit".to_owned());
    choices.push(InteractiveChoice::Quit);
    Ok((choices, labels))
}

fn original_crontab_line(text: &str, line: usize) -> Option<&str> {
    text.split('\n').nth(line.checked_sub(1)?)
}

fn interactive_add_menu(root: &Path, store: &mut Store) -> Result<()> {
    let options = [
        "One-off (at)".to_owned(),
        "Recurring (cron)".to_owned(),
        "rat scheduler".to_owned(),
    ];
    let Some(selected) = select_menu("Add a job", &options, 0)? else {
        return Ok(());
    };
    match selected {
        0 => interactive_add_at(),
        1 => interactive_add_cron(),
        2 => {
            interactive_setup(root)?;
            require_scheduler(root)?;
            interactive_add(store)
        }
        _ => Ok(()),
    }
}

fn interactive_setup(root: &Path) -> Result<()> {
    if !interactive_setup_decided(root)? && !service::installed_for_root(root)? {
        let install = prompt_yes_no(
            "First setup: install rat to start automatically at login?",
            false,
        )?;
        if install {
            service::install(root)?;
            record_interactive_setup_decision(root, true)?;
            print_interactive_message("Installed rat's login service.")?;
        } else {
            record_interactive_setup_decision(root, false)?;
            print_interactive_message(
                "No service was installed. Start the scheduler manually with `rat daemon` when you want scheduled jobs to run.",
            )?;
        }
    }
    Ok(())
}

fn interactive_setup_decided(root: &Path) -> Result<bool> {
    match fs::read(root.join(".first-interactive-setup")) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error).with_context(|| {
            format!(
                "cannot read first-interactive-setup decision in {}",
                root.display()
            )
        }),
    }
}

fn record_interactive_setup_decision(root: &Path, installed: bool) -> Result<()> {
    fs::create_dir_all(root)
        .with_context(|| format!("cannot create data root {}", root.display()))?;
    let decision = if installed {
        "installed\n"
    } else {
        "declined\n"
    };
    fs::write(root.join(".first-interactive-setup"), decision).with_context(|| {
        format!(
            "cannot record first-interactive-setup decision in {}",
            root.display()
        )
    })
}

fn interactive_at_details(job: &crate::native::AtJob) -> Result<()> {
    let script = crate::native::at_script(&job.id)?;
    print_interactive_message(&format!(
        "at job {}\nWhen: {}\nQueue: {}\nScript:\n{}",
        safe_label(&job.id),
        safe_label(&job.when),
        if job.queue.is_empty() {
            "not exposed by atq".to_owned()
        } else {
            safe_label(&job.queue)
        },
        terminal_text(&script)
    ))?;
    let _ = select_menu("at job details", &["Back".to_owned()], 0)?;
    Ok(())
}

fn interactive_cron_details(job: &crate::native::CronJob, raw_line: &str) -> Result<()> {
    print_interactive_message(&format!(
        "Cron line {}\n{}",
        job.line,
        terminal_text(raw_line)
    ))?;
    let _ = select_menu("cron job details", &["Back".to_owned()], 0)?;
    Ok(())
}

fn interactive_rat_job(root: &Path, store: &mut Store, id: i64) -> Result<()> {
    let job = store.get(id)?;
    let actions = vec![
        "Details".to_owned(),
        "Edit job".to_owned(),
        match job.state {
            JobState::Active => "Pause job",
            JobState::Paused => "Resume job",
            JobState::NeedsReview => "Review uncertain run",
            JobState::Completed | JobState::Deleted => "Job unavailable",
        }
        .to_owned(),
        "History".to_owned(),
        "Output".to_owned(),
        "Delete".to_owned(),
        "Back".to_owned(),
    ];
    let Some(selected) = select_menu(&format!("rat job #{}", job.id), &actions, 0)? else {
        return Err(InteractiveCancelled.into());
    };
    let action = actions.get(selected).map(String::as_str);
    match action {
        Some("Details") => print_interactive_job(&job),
        Some("Edit job") => {
            require_scheduler(root)?;
            interactive_edit(store, id)
        }
        Some("Pause job") => {
            store.pause(id)?;
            print_message(&format!("Paused job {id}."))
        }
        Some("Resume job") => {
            require_scheduler(root)?;
            let now = Utc::now();
            let overdue_one_off = job.state == JobState::Paused
                && matches!(job.definition.schedule, Schedule::OneOff { at, .. } if at <= now);
            let confirm = if overdue_one_off {
                prompt_yes_no(
                    "This one-off is overdue and will run immediately. Resume it?",
                    false,
                )?
            } else {
                false
            };
            if overdue_one_off && !confirm {
                print_message("Resume cancelled.")
            } else {
                store.resume(id, now, confirm)?;
                print_message(&format!("Resumed job {id}."))
            }
        }
        Some("Review uncertain run") => {
            if prompt_yes_no(
                "Acknowledge the uncertain occurrence without replaying it?",
                false,
            )? {
                store.resolve(id, Utc::now())?;
                print_message(&format!(
                    "Acknowledged the uncertain occurrence for job {id}; it will not be replayed."
                ))
            } else {
                print_message("Review cancelled.")
            }
        }
        Some("History") => {
            let mut runs = store.history(Some(id))?;
            runs.sort_by_key(|run| run.id);
            print_history(&runs)
        }
        Some("Output") => interactive_output(root, store, Some(id)),
        Some("Delete") => {
            if prompt_yes_no(
                &format!("Delete job {id}? An already-running command will not be stopped."),
                false,
            )? {
                store.delete(id)?;
                print_message(&format!(
                    "Deleted job {id}; an already-running command is not stopped."
                ))
            } else {
                print_message("Delete cancelled.")
            }
        }
        _ => Ok(()),
    }
}

fn interactive_output(root: &Path, store: &Store, job_id: Option<i64>) -> Result<()> {
    let mut runs = store.history(job_id)?;
    runs.sort_by_key(|run| run.id);
    if runs.is_empty() {
        let message = if job_id.is_some() {
            "No run output is available for this job."
        } else {
            "No rat run history is available."
        };
        print_interactive_message(message)?;
        return Ok(());
    }
    let labels = runs
        .iter()
        .map(|run| {
            safe_label(&format!(
                "rat run #{} from job #{} [{}] scheduled {}: {}",
                run.id,
                run.job_id,
                run_status_label(run.status),
                format_instant(run.scheduled_at),
                run.definition.command
            ))
        })
        .collect::<Vec<_>>();
    let Some(selected) = select_menu("Select run output", &labels, labels.len() - 1)? else {
        return Err(InteractiveCancelled.into());
    };
    if let Some(run) = runs.get(selected) {
        print_output(root, run.id)?;
    }
    Ok(())
}

fn print_interactive_job(job: &Job) -> Result<()> {
    let details = JobDetails {
        job,
        sanitize: true,
    };
    let details = if job.definition.env.is_empty() {
        format!("{details}\n")
    } else {
        format!("{details}")
    };
    print_interactive_message(&details)
}

fn select_menu(prompt: &str, items: &[String], default: usize) -> Result<Option<usize>> {
    let theme = SimpleTheme;
    let terminal = dialoguer::console::Term::stdout();
    let result = Select::with_theme(&theme)
        .with_prompt(safe_label(prompt))
        .items(items)
        .clear(false)
        .default(default)
        .interact_on_opt(&terminal);
    terminal.show_cursor()?;
    result.map_err(dialoguer_error)
}

fn dialoguer_error(error: dialoguer::Error) -> anyhow::Error {
    match error {
        dialoguer::Error::IO(error) if error.kind() == io::ErrorKind::Interrupted => {
            InteractiveCancelled.into()
        }
        error @ dialoguer::Error::IO(_) => error.into(),
    }
}

fn safe_label(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .map(|character| {
            if character.is_control() && character != '\n' && character != '\t' {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn print_interactive_message(message: &str) -> Result<()> {
    print_message(&terminal_text(message))
}

fn interactive_add_at() -> Result<()> {
    let when = prompt("One-off time tokens (for example 2h 5m or 20270101T090000Z): ")?;
    let tokens = when
        .split_whitespace()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    ensure!(!tokens.is_empty(), "one-off time is required");
    let default_zone = default_timezone()?;
    let timezone = prompt(&format!("IANA timezone [{default_zone}]: "))?;
    let timezone = if timezone.is_empty() {
        default_zone
    } else {
        validate_timezone(timezone)?
    };
    let command = prompt("Command string (one shell command): ")?;
    validate_command(&command)?;
    print_interactive_message(&format!(
        "Native at preview: time {when:?} in {timezone}; command {command:?}\nNative at uses minute precision and rounds up. Its OS scheduler must be enabled; rat will not start it."
    ))?;
    if !prompt_yes_no("Register native at job?", false)? {
        print_interactive_message("Add cancelled.")?;
        return Ok(());
    }
    let schedule = schedule::parse_one_off(&tokens, Utc::now(), &timezone)?;
    let Schedule::OneOff { at, .. } = schedule else {
        bail!("one-off parser returned a recurring schedule")
    };
    let diagnostics = crate::native::add_at(at, &command)?;
    if diagnostics.trim().is_empty() {
        print_interactive_message("at registration command completed without diagnostics.")?;
    } else {
        print_interactive_message(&format!("at reports (UTC):\n{diagnostics}"))?;
    }
    Ok(())
}

fn interactive_add_cron() -> Result<()> {
    let expected = crate::native::read_crontab()?.text;
    let cron = read_native_cron_schedule()?;
    schedule::parse_recurring(&cron, "UTC")?;
    let command = prompt("Command string (one shell command): ")?;
    validate_command(&command)?;
    print_interactive_message(&format!(
        "Native cron preview: schedule {cron:?}; command {command:?}\nUses the crontab environment and OS scheduler timezone. rat will not start cron or capture run output."
    ))?;
    if !prompt_yes_no("Register native cron job?", false)? {
        print_interactive_message("Add cancelled.")?;
        return Ok(());
    }
    crate::native::add_cron(&expected, &cron, &command)?;
    print_interactive_message("Crontab entry registered; execution is managed by cron.")
}

fn read_native_cron_schedule() -> Result<String> {
    let choices = [
        "Daily".to_owned(),
        "Weekly".to_owned(),
        "Monthly".to_owned(),
        "Custom cron".to_owned(),
    ];
    let selected = select_menu("Recurring cron schedule", &choices, 0)?
        .ok_or_else(|| anyhow::Error::new(InteractiveCancelled))?;
    match selected {
        0 => {
            let (minute, hour) = read_calendar_time()?;
            Ok(format!("{minute} {hour} * * *"))
        }
        1 => {
            let (minute, hour) = read_calendar_time()?;
            let days = prompt("Weekdays (Mon, Tue, Wed, Thu, Fri, Sat, Sun; comma separated): ")?;
            let cron_days = days
                .split(',')
                .map(str::trim)
                .map(weekday_number)
                .collect::<Result<Vec<_>>>()?;
            ensure!(!cron_days.is_empty(), "select at least one weekday");
            Ok(format!("{minute} {hour} * * {}", cron_days.join(",")))
        }
        2 => {
            let (minute, hour) = read_calendar_time()?;
            let day: u8 = prompt("Day of month (1-31): ")?
                .parse()
                .context("day of month must be an integer from 1 to 31")?;
            ensure!((1..=31).contains(&day), "day of month must be from 1 to 31");
            Ok(format!("{minute} {hour} {day} * *"))
        }
        3 => prompt("Cron schedule (five fields): "),
        _ => bail!("invalid recurring schedule choice"),
    }
}
fn interactive_add(store: &mut Store) -> Result<()> {
    let default_zone = default_timezone()?;
    let schedule = read_schedule(&default_zone)?;
    let command = prompt("Command string (one shell command): ")?;
    validate_command(&command)?;
    let env = read_environment()?;
    let confirmation = format!(
        "Add command {:?}, schedule {:?}, timezone {}, and environment {:?}?",
        command, schedule.expression, schedule.timezone, env
    );
    if !prompt_yes_no(&confirmation, false)? {
        print_message("Add cancelled.")?;
        return Ok(());
    }

    // Relative one-off input is resolved only after the user's final confirmation.
    let now = Utc::now();
    let schedule = schedule.parse(now)?;
    let definition = new_definition(command, schedule, env)?;
    let job = store.add(definition, now)?;
    print_saved_job(&job)
}

fn interactive_edit(store: &mut Store, id: i64) -> Result<()> {
    let existing = store.get(id)?;
    let replace_schedule = prompt_yes_no("Replace the schedule?", false)?;
    let schedule = if replace_schedule {
        Some(read_schedule(existing.definition.schedule.timezone())?)
    } else {
        None
    };
    let command_input = prompt(&format!(
        "New command string (blank keeps {:?}): ",
        existing.definition.command
    ))?;
    let command = if command_input.is_empty() {
        existing.definition.command.clone()
    } else {
        validate_command(&command_input)?;
        command_input
    };
    let replace_environment = prompt_yes_no("Replace explicit environment variables?", false)?;
    let env = if replace_environment {
        read_environment()?
    } else {
        existing.definition.env.clone()
    };
    ensure!(
        schedule.is_some()
            || command != existing.definition.command
            || env != existing.definition.env,
        "no changes selected"
    );
    let schedule_preview = schedule.as_ref().map_or_else(
        || format!("{:?}", existing.definition.schedule),
        |draft| format!("{:?}", draft.expression),
    );
    let timezone_preview = schedule.as_ref().map_or_else(
        || existing.definition.schedule.timezone(),
        |draft| draft.timezone.as_str(),
    );
    let confirmation = format!(
        "Save job {id} with command {command:?}, schedule {schedule_preview}, timezone {timezone_preview}, and environment {env:?}?"
    );
    if !prompt_yes_no(&confirmation, false)? {
        print_message("Edit cancelled.")?;
        return Ok(());
    }

    // Resolve a newly entered relative time only after the final confirmation.
    let now = Utc::now();
    let schedule = schedule
        .map(|draft| draft.parse(now))
        .transpose()?
        .unwrap_or_else(|| existing.definition.schedule.clone());
    let definition = NewJob {
        command,
        cwd: existing.definition.cwd,
        path: existing.definition.path,
        env,
        schedule,
    };
    let job = store.edit(id, definition, now)?;
    print_saved_job(&job)
}
#[derive(Debug)]
struct ScheduleDraft {
    expression: DraftExpression,
    timezone: String,
}

#[derive(Debug)]
enum DraftExpression {
    OneOff(Vec<String>),
    Recurring(String),
}

impl ScheduleDraft {
    fn parse(self, now: DateTime<Utc>) -> Result<Schedule> {
        match self.expression {
            DraftExpression::OneOff(tokens) => {
                schedule::parse_one_off(&tokens, now, &self.timezone)
            }
            DraftExpression::Recurring(cron) => schedule::parse_recurring(&cron, &self.timezone),
        }
    }
}

fn read_schedule(default_timezone: &str) -> Result<ScheduleDraft> {
    let kind = prompt("Schedule [oneoff/daily/weekly/monthly]: ")?;
    let expression = match kind.to_ascii_lowercase().as_str() {
        "oneoff" | "one-off" | "once" => {
            let input = prompt("One-off time tokens (for example 2h 5m or 20270101T090000Z): ")?;
            let tokens: Vec<String> = input.split_whitespace().map(str::to_owned).collect();
            ensure!(!tokens.is_empty(), "one-off time is required");
            DraftExpression::OneOff(tokens)
        }
        "daily" => {
            let (minute, hour) = read_calendar_time()?;
            DraftExpression::Recurring(format!("{minute} {hour} * * *"))
        }
        "weekly" => {
            let (minute, hour) = read_calendar_time()?;
            let days = prompt("Weekdays (Mon, Tue, Wed, Thu, Fri, Sat, Sun; comma separated): ")?;
            let cron_days = days
                .split(',')
                .map(str::trim)
                .map(weekday_number)
                .collect::<Result<Vec<_>>>()?;
            ensure!(!cron_days.is_empty(), "select at least one weekday");
            DraftExpression::Recurring(format!("{minute} {hour} * * {}", cron_days.join(",")))
        }
        "monthly" => {
            let (minute, hour) = read_calendar_time()?;
            let day: u8 = prompt("Day of month (1-31): ")?
                .parse()
                .context("day of month must be an integer from 1 to 31")?;
            ensure!((1..=31).contains(&day), "day of month must be from 1 to 31");
            DraftExpression::Recurring(format!("{minute} {hour} {day} * *"))
        }
        _ => bail!("choose oneoff, daily, weekly, or monthly"),
    };
    let timezone = prompt(&format!("IANA timezone [{default_timezone}]: "))?;
    let timezone = if timezone.is_empty() {
        default_timezone.to_owned()
    } else {
        validate_timezone(timezone)?
    };
    Ok(ScheduleDraft {
        expression,
        timezone,
    })
}

fn read_calendar_time() -> Result<(u8, u8)> {
    let input = prompt("Local time (HH:MM): ")?;
    let (hour, minute) = input
        .split_once(':')
        .with_context(|| format!("invalid local time {input:?}; use HH:MM"))?;
    ensure!(!minute.contains(':'), "calendar time must use HH:MM");
    let hour: u8 = hour.parse().context("hour must be from 0 to 23")?;
    let minute: u8 = minute.parse().context("minute must be from 0 to 59")?;
    ensure!(
        hour <= 23 && minute <= 59,
        "calendar time must use hour 00-23 and minute 00-59"
    );
    Ok((minute, hour))
}

fn weekday_number(day: &str) -> Result<String> {
    let number = match day.to_ascii_lowercase().as_str() {
        "sun" | "sunday" => 0,
        "mon" | "monday" => 1,
        "tue" | "tues" | "tuesday" => 2,
        "wed" | "wednesday" => 3,
        "thu" | "thur" | "thurs" | "thursday" => 4,
        "fri" | "friday" => 5,
        "sat" | "saturday" => 6,
        _ => bail!("unknown weekday {day:?}; use Mon through Sun"),
    };
    Ok(number.to_string())
}

fn read_environment() -> Result<BTreeMap<String, String>> {
    print_interactive_message(
        "Enter KEY=VALUE environment entries; submit an empty line when finished.",
    )?;
    let mut values = Vec::new();
    loop {
        let value = prompt("env> ")?;
        if value.is_empty() {
            break;
        }
        values.push(value);
    }
    parse_environment(values)
}

struct PromptTerminalGuard {
    fd: std::os::fd::RawFd,
    original: libc::termios,
}

impl PromptTerminalGuard {
    fn disable() -> Result<Self> {
        use std::os::fd::AsRawFd;

        let stdin = io::stdin();
        let fd = stdin.as_raw_fd();
        let mut original = std::mem::MaybeUninit::<libc::termios>::uninit();
        // SAFETY: stdin is an open terminal and `original` is a valid output buffer.
        if unsafe { libc::tcgetattr(fd, original.as_mut_ptr()) } == -1 {
            return Err(io::Error::last_os_error()).context("cannot read terminal echo settings");
        }
        // SAFETY: tcgetattr initialized `original` successfully.
        let original = unsafe { original.assume_init() };
        let mut prompt_mode = original;
        prompt_mode.c_lflag &= !(libc::ECHO | libc::ICANON | libc::ISIG | libc::IEXTEN);
        prompt_mode.c_cc[libc::VMIN] = 1;
        prompt_mode.c_cc[libc::VTIME] = 0;
        // SAFETY: `fd` is stdin and `prompt_mode` is a fully initialized terminal settings value.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw const prompt_mode) } == -1 {
            return Err(io::Error::last_os_error())
                .context("cannot configure prompt terminal mode");
        }
        Ok(Self { fd, original })
    }
}

impl Drop for PromptTerminalGuard {
    fn drop(&mut self) {
        // SAFETY: `fd` is the open stdin descriptor and `original` was read from it.
        let _ = unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &raw const self.original) };
    }
}

fn prompt(message: &str) -> Result<String> {
    let mut terminal = dialoguer::console::Term::stdout();
    let terminal_guard = PromptTerminalGuard::disable()?;
    let result = (|| {
        ensure!(terminal.is_term(), "input requires a terminal");
        write!(terminal, "{}", safe_label(message))?;
        terminal.flush()?;
        let mut input = String::new();
        loop {
            let key = match terminal.read_key_raw() {
                Ok(key) => key,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                    terminal.write_line("")?;
                    return Err(InteractiveCancelled.into());
                }
                Err(error) => return Err(error.into()),
            };
            match key {
                dialoguer::console::Key::Char('\u{4}')
                | dialoguer::console::Key::CtrlC
                | dialoguer::console::Key::Escape => {
                    terminal.write_line("")?;
                    return Err(InteractiveCancelled.into());
                }
                dialoguer::console::Key::Char(character) if !character.is_control() => {
                    input.push(character);
                    write!(terminal, "{character}")?;
                    terminal.flush()?;
                }
                dialoguer::console::Key::Backspace => {
                    if let Some(character) = input.pop() {
                        let width =
                            dialoguer::console::measure_text_width(&character.to_string()).max(1);
                        for _ in 0..width {
                            write!(terminal, "\x08")?;
                        }
                        for _ in 0..width {
                            write!(terminal, " ")?;
                        }
                        for _ in 0..width {
                            write!(terminal, "\x08")?;
                        }
                        terminal.flush()?;
                    }
                }
                dialoguer::console::Key::Enter => {
                    terminal.write_line("")?;
                    return Ok(input);
                }
                _ => {}
            }
        }
    })();
    drop(terminal_guard);
    terminal.show_cursor()?;
    result
}

fn prompt_yes_no(message: &str, default: bool) -> Result<bool> {
    let theme = SimpleTheme;
    let terminal = dialoguer::console::Term::stdout();
    let result = Confirm::with_theme(&theme)
        .with_prompt(safe_label(message))
        .default(default)
        .wait_for_newline(true)
        .interact_on_opt(&terminal);
    terminal.show_cursor()?;
    result
        .map_err(dialoguer_error)?
        .ok_or_else(|| InteractiveCancelled.into())
}
