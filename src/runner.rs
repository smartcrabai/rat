//! Background execution and scheduling for rat jobs.

use std::{
    collections::HashMap,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            fs::{OpenOptionsExt, PermissionsExt},
            process::CommandExt,
        },
    },
    path::Path,
    process::{Child, Command, ExitStatus, Stdio},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use signal_hook::{
    consts::signal::{SIGINT, SIGTERM},
    flag, low_level,
};

use crate::{
    model::{JobState, NewJob, Run, RunCompletion, RunStatus},
    store::Store,
};

const MAX_OUTPUT_BYTES: u64 = 10 * 1024 * 1024;
const MAX_READ_PER_TICK: usize = 1024 * 1024;
const PRUNE_OUTPUT_THRESHOLD: u64 = 1024 * 1024;
const PRUNE_INTERVAL: ChronoDuration = ChronoDuration::minutes(1);
const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Owns the scheduler lock, persistent store, and child processes dispatched by this instance.
pub struct Scheduler {
    store: Store,
    _lock: File,
    children: HashMap<i64, RunningChild>,
    detached: HashMap<i64, DetachedCapture>,
    last_tick: DateTime<Utc>,
    last_prune: DateTime<Utc>,
    stored_since_prune: u64,
    dispatch_failed: bool,
    stop_dispatch: Option<Arc<AtomicBool>>,
}

struct RunningChild {
    child: Child,
    capture: Capture,
    uncertain: bool,
}

struct DetachedCapture {
    capture: Capture,
    completion: RunCompletion,
    reported_message_bytes: usize,
}

struct Capture {
    stdout: CaptureStream,
    stderr: CaptureStream,
    stored_bytes: u64,
    truncated: bool,
    output_missing: bool,
    message: Option<String>,
}

struct CaptureStream {
    input: Option<File>,
    output: Option<File>,
    bytes: u64,
    name: &'static str,
}

impl Scheduler {
    /// Acquires the process-wide scheduler lock and performs startup recovery and retention.
    ///
    /// # Errors
    /// Returns an error if another scheduler owns the lock or recovery/retention fails.
    pub fn new(mut store: Store, now: DateTime<Utc>) -> Result<Self> {
        let lock = acquire_lock(store.root())?;
        store.recover(now)?;
        store.skip_missed(now)?;
        let _ = store.prune(now)?;
        Ok(Self {
            store,
            _lock: lock,
            children: HashMap::new(),
            detached: HashMap::new(),
            last_tick: now,
            last_prune: now,
            stored_since_prune: 0,
            dispatch_failed: false,
            stop_dispatch: None,
        })
    }

    /// Returns the persistent store managed by this scheduler.
    #[must_use]
    pub const fn store(&self) -> &Store {
        &self.store
    }

    /// Reports whether child processes or inherited output pipes still need collection.
    #[must_use]
    pub fn has_running(&self) -> bool {
        !self.children.is_empty() || !self.detached.is_empty()
    }

    /// Reaps completed children and launches due jobs without waiting for any child.
    ///
    /// # Errors
    /// Returns an error if persistence, process collection, or launch bookkeeping fails.
    pub fn tick(&mut self, now: DateTime<Utc>) -> Result<()> {
        if self.dispatch_failed {
            return self.collect_children(now).map(|_| ());
        }
        let result = self.dispatch_tick(now);
        if result.is_err() {
            self.dispatch_failed = true;
        }
        result
    }

    fn dispatch_tick(&mut self, now: DateTime<Utc>) -> Result<()> {
        self.collect_children(now)?;
        if now.signed_duration_since(self.last_tick) > ChronoDuration::seconds(2) {
            self.store.skip_missed(now)?;
        }

        let jobs = self.store.due(now)?;
        for job in jobs {
            if self
                .stop_dispatch
                .as_ref()
                .is_some_and(|stopping| stopping.load(Ordering::Relaxed))
            {
                break;
            }
            if job.state != JobState::Active {
                continue;
            }
            let Some(scheduled_at) = job.next_at else {
                continue;
            };
            if scheduled_at > now {
                continue;
            }

            let Some(reservation) = self.store.reserve(job.id, scheduled_at, now)? else {
                continue;
            };
            if reservation.status != RunStatus::Starting {
                continue;
            }
            self.launch(reservation, now)?;
        }

        self.last_tick = now;
        Ok(())
    }

    fn launch(&mut self, reservation: Run, now: DateTime<Utc>) -> Result<()> {
        let run_id = reservation.id;
        let definition = reservation.definition;
        let (stdout_path, stderr_path) = self.store.output_paths(run_id);
        let mut launch_attempted = false;
        let mut spawned = None;
        let result = self.store.launch_reserved(run_id, now, || {
            launch_attempted = true;
            let child = spawn_child(&definition, &stdout_path, &stderr_path)?;
            spawned = Some(child);
            Ok(())
        });

        match result {
            Ok(Some(())) => {
                let child = spawned.ok_or_else(|| {
                    anyhow!("launch transaction succeeded without returning its child handle")
                })?;
                self.children.insert(run_id, child);
            }
            Ok(None) => {
                // Deletion won the reservation boundary; the stored run is Skipped.
            }
            Err(error) => {
                if let Some(mut child) = spawned {
                    child.uncertain = true;
                    child.capture.add_message(&format!(
                        "shell was spawned, but its Running state could not be confirmed: {error:#}"
                    ));
                    self.children.insert(run_id, child);
                    return Err(error).context("recording spawned child as Running");
                }
                if !launch_attempted {
                    return Err(error).context("entering the atomic launch boundary");
                }

                let missing = !stdout_path.exists() || !stderr_path.exists();
                self.store.finish(
                    run_id,
                    RunCompletion {
                        status: RunStatus::Failed,
                        exit_code: None,
                        stdout_bytes: 0,
                        stderr_bytes: 0,
                        truncated: false,
                        output_missing: missing,
                        message: Some(format!("could not start shell: {error:#}")),
                    },
                    now,
                )?;
                let evicted = self.store.prune(now)?;
                self.close_pruned_outputs(&evicted)?;
                self.last_prune = now;
                self.stored_since_prune = 0;
            }
        }
        Ok(())
    }

    fn collect_children(&mut self, now: DateTime<Utc>) -> Result<bool> {
        let mut first_error = None;
        let (completed, mut newly_stored) = self.poll_children(&mut first_error);
        let was_completed = !completed.is_empty();
        self.record_completed(completed, now, &mut first_error);
        newly_stored = newly_stored.saturating_add(self.collect_detached(&mut first_error));
        self.prune_outputs(now, was_completed, newly_stored, &mut first_error);
        match first_error {
            Some(error) => Err(error),
            None => Ok(was_completed),
        }
    }

    fn poll_children(
        &mut self,
        first_error: &mut Option<anyhow::Error>,
    ) -> (Vec<(i64, ExitStatus)>, u64) {
        let mut completed = Vec::new();
        let mut newly_stored = 0_u64;
        for (&run_id, running) in &mut self.children {
            let status = running
                .child
                .try_wait()
                .with_context(|| format!("reaping child for run {run_id}"));
            let (_, stored) = running.capture.drain(MAX_READ_PER_TICK);
            newly_stored = newly_stored.saturating_add(stored);
            match status {
                Ok(Some(status)) => {
                    let (_, stored) = running.capture.drain(MAX_READ_PER_TICK);
                    newly_stored = newly_stored.saturating_add(stored);
                    completed.push((run_id, status));
                }
                Ok(None) => {}
                Err(error) => {
                    first_error.get_or_insert(error);
                }
            }
        }
        (completed, newly_stored)
    }

    fn record_completed(
        &mut self,
        completed: Vec<(i64, ExitStatus)>,
        now: DateTime<Utc>,
        first_error: &mut Option<anyhow::Error>,
    ) {
        for (run_id, status) in completed {
            let Some(running) = self.children.remove(&run_id) else {
                continue;
            };
            let completion = running.capture.completion(status);
            if running.uncertain {
                if let Err(error) = self.store.update_capture(run_id, &completion) {
                    first_error.get_or_insert_with(|| {
                        error.context(format!("updating uncertain output for run {run_id}"))
                    });
                }
            } else if let Err(error) = self.store.finish(run_id, completion.clone(), now) {
                first_error.get_or_insert_with(|| {
                    error.context(format!("recording completion for run {run_id}"))
                });
            }
            if running.capture.has_readers() {
                let reported_message_bytes =
                    running.capture.message.as_ref().map_or(0, String::len);
                self.detached.insert(
                    run_id,
                    DetachedCapture {
                        capture: running.capture,
                        completion,
                        reported_message_bytes,
                    },
                );
            }
        }
    }

    fn collect_detached(&mut self, first_error: &mut Option<anyhow::Error>) -> u64 {
        let mut finished_captures = Vec::new();
        let mut newly_stored = 0_u64;
        for (&run_id, detached) in &mut self.detached {
            let (read, stored) = detached.capture.drain(MAX_READ_PER_TICK);
            newly_stored = newly_stored.saturating_add(stored);
            if read > 0 {
                detached.capture.update_completion(
                    &mut detached.completion,
                    &mut detached.reported_message_bytes,
                );
                if let Err(error) = self.store.update_capture(run_id, &detached.completion) {
                    first_error.get_or_insert_with(|| {
                        error.context(format!("updating detached output for run {run_id}"))
                    });
                }
            }
            if detached.capture.is_complete() {
                finished_captures.push(run_id);
            }
        }
        for run_id in finished_captures {
            self.detached.remove(&run_id);
        }
        newly_stored
    }

    fn prune_outputs(
        &mut self,
        now: DateTime<Utc>,
        was_completed: bool,
        newly_stored: u64,
        first_error: &mut Option<anyhow::Error>,
    ) {
        self.stored_since_prune = self.stored_since_prune.saturating_add(newly_stored);
        if was_completed
            || self.stored_since_prune >= PRUNE_OUTPUT_THRESHOLD
            || now.signed_duration_since(self.last_prune) >= PRUNE_INTERVAL
        {
            match self.store.prune(now) {
                Ok(evicted) => {
                    if let Err(error) = self.close_pruned_outputs(&evicted) {
                        first_error.get_or_insert(error);
                    }
                    self.last_prune = now;
                    self.stored_since_prune = 0;
                }
                Err(error) => {
                    first_error.get_or_insert_with(|| error.context("pruning run outputs"));
                }
            }
        }
    }

    fn close_pruned_outputs(&mut self, evicted: &[i64]) -> Result<()> {
        for &run_id in evicted {
            if let Some(running) = self.children.get_mut(&run_id) {
                running.capture.close_pruned_outputs();
            }
            if let Some(detached) = self.detached.get_mut(&run_id) {
                detached.capture.close_pruned_outputs();
            }
        }
        for &run_id in evicted {
            if let Some(detached) = self.detached.get_mut(&run_id) {
                detached.capture.update_completion(
                    &mut detached.completion,
                    &mut detached.reported_message_bytes,
                );
                self.store
                    .update_capture(run_id, &detached.completion)
                    .with_context(|| format!("updating evicted output for run {run_id}"))?;
            }
        }
        Ok(())
    }
    fn wait_for_running(&mut self) -> Result<()> {
        while self.has_running() {
            if let Err(error) = self.collect_children(Utc::now()) {
                return self.drain_after_error(error);
            }
            if self.has_running() {
                thread::sleep(POLL_INTERVAL);
            }
        }
        Ok(())
    }

    fn drain_after_error(&mut self, original: anyhow::Error) -> Result<()> {
        self.dispatch_failed = true;
        while self.has_running() {
            match self.collect_children(Utc::now()) {
                Ok(_) | Err(_) => {}
            }
            if self.has_running() {
                thread::sleep(POLL_INTERVAL);
            }
        }
        Err(original)
    }
}

impl Capture {
    fn new(
        stdout_input: File,
        stdout_output: File,
        stderr_input: File,
        stderr_output: File,
    ) -> Self {
        Self {
            stdout: CaptureStream::new(stdout_input, stdout_output, "stdout"),
            stderr: CaptureStream::new(stderr_input, stderr_output, "stderr"),
            stored_bytes: 0,
            truncated: false,
            output_missing: false,
            message: None,
        }
    }

    fn drain(&mut self, budget: usize) -> (u64, u64) {
        let mut stdout_budget = budget / 2;
        let mut stderr_budget = budget - stdout_budget;
        let Self {
            stdout,
            stderr,
            stored_bytes,
            truncated,
            output_missing,
            message,
        } = self;
        let (stdout_read, stdout_stored) = stdout.drain(
            stored_bytes,
            truncated,
            output_missing,
            message,
            &mut stdout_budget,
        );
        let (stderr_read, stderr_stored) = stderr.drain(
            stored_bytes,
            truncated,
            output_missing,
            message,
            &mut stderr_budget,
        );
        (
            stdout_read.saturating_add(stderr_read),
            stdout_stored.saturating_add(stderr_stored),
        )
    }

    fn has_readers(&self) -> bool {
        self.stdout.input.is_some() || self.stderr.input.is_some()
    }

    fn is_complete(&self) -> bool {
        !self.has_readers()
    }

    fn completion(&self, status: ExitStatus) -> RunCompletion {
        let exit_code = status.code();
        let (run_status, message) = match exit_code {
            Some(0) => (RunStatus::Succeeded, self.message.clone()),
            Some(code) => {
                let mut message = self.message.clone();
                append_message(&mut message, &format!("shell exited with status {code}"));
                (RunStatus::Failed, message)
            }
            None => {
                let mut message = self.message.clone();
                append_message(&mut message, "shell terminated without an exit code");
                (RunStatus::Failed, message)
            }
        };
        RunCompletion {
            status: run_status,
            exit_code,
            stdout_bytes: self.stdout.bytes,
            stderr_bytes: self.stderr.bytes,
            truncated: self.truncated,
            output_missing: self.output_missing,
            message,
        }
    }

    fn update_completion(
        &self,
        completion: &mut RunCompletion,
        reported_message_bytes: &mut usize,
    ) {
        completion.stdout_bytes = self.stdout.bytes;
        completion.stderr_bytes = self.stderr.bytes;
        completion.truncated = self.truncated;
        completion.output_missing |= self.output_missing;
        if let Some(message) = &self.message
            && message.len() > *reported_message_bytes
        {
            let additional = &message[*reported_message_bytes..];
            append_message(
                &mut completion.message,
                additional.strip_prefix("; ").unwrap_or(additional),
            );
            *reported_message_bytes = message.len();
        }
    }

    fn add_message(&mut self, message: &str) {
        append_message(&mut self.message, message);
    }

    fn close_pruned_outputs(&mut self) {
        let stdout_removed = self.stdout.output.take().is_some();
        let stderr_removed = self.stderr.output.take().is_some();
        if stdout_removed || stderr_removed {
            self.output_missing = true;
            append_message(
                &mut self.message,
                "output capture file was removed while output was being captured",
            );
        }
    }
}

impl CaptureStream {
    fn new(input: File, output: File, name: &'static str) -> Self {
        Self {
            input: Some(input),
            output: Some(output),
            bytes: 0,
            name,
        }
    }

    fn drain(
        &mut self,
        stored_bytes: &mut u64,
        truncated: &mut bool,
        output_missing: &mut bool,
        message: &mut Option<String>,
        budget: &mut usize,
    ) -> (u64, u64) {
        let Some(reader) = self.input.as_mut() else {
            return (0, 0);
        };
        let mut buffer = [0_u8; 16 * 1024];
        let mut read_bytes = 0_u64;
        let mut written_bytes = 0_u64;
        while *budget > 0 {
            let read_limit = buffer.len().min(*budget);
            match reader.read(&mut buffer[..read_limit]) {
                Ok(0) => {
                    self.input.take();
                    return (read_bytes, written_bytes);
                }
                Ok(length) => {
                    *budget -= length;
                    let length_as_u64 = u64::try_from(length).unwrap_or(u64::MAX);
                    self.bytes = self.bytes.saturating_add(length_as_u64);
                    read_bytes = read_bytes.saturating_add(length_as_u64);
                    if let Some(writer) = self.output.as_mut() {
                        let remaining = MAX_OUTPUT_BYTES.saturating_sub(*stored_bytes);
                        let to_store = length.min(usize::try_from(remaining).unwrap_or(usize::MAX));
                        if to_store < length {
                            *truncated = true;
                        }
                        if to_store > 0 {
                            match writer.write_all(&buffer[..to_store]) {
                                Ok(()) => {
                                    let amount = u64::try_from(to_store).unwrap_or(u64::MAX);
                                    *stored_bytes = stored_bytes.saturating_add(amount);
                                    written_bytes = written_bytes.saturating_add(amount);
                                }
                                Err(error) => {
                                    *output_missing = true;
                                    append_message(
                                        message,
                                        &format!("writing {} output failed: {error}", self.name),
                                    );
                                    self.output.take();
                                }
                            }
                        }
                    } else {
                        *output_missing = true;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    return (read_bytes, written_bytes);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    *output_missing = true;
                    append_message(
                        message,
                        &format!("reading {} output failed: {error}", self.name),
                    );
                    self.input.take();
                    return (read_bytes, written_bytes);
                }
            }
        }
        (read_bytes, written_bytes)
    }
}

fn append_message(message: &mut Option<String>, addition: &str) {
    match message {
        Some(existing) => {
            existing.push_str("; ");
            existing.push_str(addition);
        }
        None => *message = Some(addition.to_owned()),
    }
}

fn spawn_child(
    definition: &NewJob,
    stdout_path: &Path,
    stderr_path: &Path,
) -> Result<RunningChild> {
    let stdout_output = open_output(stdout_path)?;
    let stderr_output = open_output(stderr_path)?;
    let (stdout_read, stdout_write) = create_pipe()?;
    let (stderr_read, stderr_write) = create_pipe()?;

    let mut command = Command::new("/bin/sh");
    command
        .arg("-c")
        .arg(&definition.command)
        .current_dir(&definition.cwd)
        .env_clear()
        .env("PATH", &definition.path)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_write))
        .stderr(Stdio::from(stderr_write));
    for (key, value) in &definition.env {
        command.env(key, value);
    }
    command.process_group(0);
    let child = command.spawn().context("spawning /bin/sh")?;
    Ok(RunningChild {
        child,
        capture: Capture::new(stdout_read, stdout_output, stderr_read, stderr_output),
        uncertain: false,
    })
}

fn open_output(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("creating output file {}", path.display()))?;
    file.set_permissions(fs::Permissions::from_mode(0o600))
        .with_context(|| format!("setting private permissions on {}", path.display()))?;
    Ok(file)
}

fn create_pipe() -> Result<(File, File)> {
    let mut descriptors = [0; 2];
    // SAFETY: descriptors points to two writable c_int slots, as required by pipe(2).
    if unsafe { libc::pipe(descriptors.as_mut_ptr()) } == -1 {
        return Err(io::Error::last_os_error()).context("creating output pipe");
    }
    // SAFETY: pipe returned two fresh descriptors owned by this function.
    let read = unsafe { File::from_raw_fd(descriptors[0]) };
    // SAFETY: pipe returned two fresh descriptors owned by this function.
    let write = unsafe { File::from_raw_fd(descriptors[1]) };
    set_cloexec(read.as_raw_fd())?;
    set_cloexec(write.as_raw_fd())?;
    set_nonblocking(read.as_raw_fd())?;
    Ok((read, write))
}

fn set_cloexec(fd: libc::c_int) -> Result<()> {
    // SAFETY: fcntl only inspects/modifies the flags of the valid descriptor supplied here.
    let descriptor_flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if descriptor_flags == -1 {
        return Err(io::Error::last_os_error()).context("reading descriptor flags");
    }
    // SAFETY: fcntl only modifies flags on the valid descriptor supplied here.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, descriptor_flags | libc::FD_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error()).context("setting close-on-exec");
    }
    Ok(())
}

fn set_nonblocking(fd: libc::c_int) -> Result<()> {
    // SAFETY: fcntl only inspects/modifies the status flags of the valid descriptor supplied here.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error()).context("reading pipe status flags");
    }
    // SAFETY: fcntl only modifies flags on the valid descriptor supplied here.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error()).context("setting nonblocking pipe");
    }
    Ok(())
}

fn lock_file(root: &Path, create: bool) -> Result<Option<File>> {
    let path = root.join("scheduler.lock");
    let mut options = OpenOptions::new();
    options.read(true).write(true).mode(0o600);
    if create {
        options.create(true);
    }
    match options.open(&path) {
        Ok(file) => {
            if create {
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .with_context(|| {
                        format!("setting private permissions on {}", path.display())
                    })?;
            }
            Ok(Some(file))
        }
        Err(error) if !create && error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => {
            Err(error).with_context(|| format!("opening scheduler lock {}", path.display()))
        }
    }
}

fn acquire_lock(root: &Path) -> Result<File> {
    let lock =
        lock_file(root, true)?.ok_or_else(|| anyhow!("scheduler lock file was not created"))?;
    // SAFETY: lock is an open file descriptor and flock does not retain the pointer.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::WouldBlock {
            bail!(
                "another rat scheduler already holds the lock for {}",
                root.display()
            );
        }
        return Err(error).with_context(|| format!("locking scheduler root {}", root.display()));
    }
    Ok(lock)
}

/// Reports whether another process currently holds the scheduler's advisory lock.
#[must_use]
pub fn scheduler_available(root: &Path) -> bool {
    let Ok(Some(lock)) = lock_file(root, false) else {
        return false;
    };
    // SAFETY: lock is an open file descriptor and flock does not retain the pointer.
    if unsafe { libc::flock(lock.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == -1 {
        return io::Error::last_os_error().kind() == io::ErrorKind::WouldBlock;
    }
    // SAFETY: this descriptor owns the lock acquired immediately above.
    unsafe {
        libc::flock(lock.as_raw_fd(), libc::LOCK_UN);
    }
    false
}

/// Runs a daemon until interrupted, or dispatches currently due work with `once`.
///
/// In either mode, already dispatched shell processes are allowed to finish before return.
///
/// # Errors
/// Returns an error if opening the store, acquiring the scheduler lock, installing signal
/// handlers, or scheduler persistence/process bookkeeping fails.
pub fn run(root: &Path, once: bool) -> Result<()> {
    let store = Store::open(root)?;
    let mut scheduler = Scheduler::new(store, Utc::now())?;
    let stopping = Arc::new(AtomicBool::new(false));
    let _signals = SignalRegistrations::install(Arc::clone(&stopping))?;
    scheduler.stop_dispatch = Some(Arc::clone(&stopping));

    if let Err(error) = scheduler.tick(Utc::now()) {
        return scheduler.drain_after_error(error);
    }
    if once {
        return scheduler.wait_for_running();
    }

    loop {
        if stopping.load(Ordering::Relaxed) {
            return scheduler.wait_for_running();
        }
        if let Err(error) = scheduler.tick(Utc::now()) {
            return scheduler.drain_after_error(error);
        }
        thread::sleep(POLL_INTERVAL);
    }
}

struct SignalRegistrations {
    interrupt: signal_hook::SigId,
    terminate: signal_hook::SigId,
}

impl SignalRegistrations {
    fn install(stopping: Arc<AtomicBool>) -> Result<Self> {
        let interrupt = flag::register(SIGINT, Arc::clone(&stopping))?;
        match flag::register(SIGTERM, stopping) {
            Ok(terminate) => Ok(Self {
                interrupt,
                terminate,
            }),
            Err(error) => {
                low_level::unregister(interrupt);
                Err(error).context("registering SIGTERM handler")
            }
        }
    }
}

impl Drop for SignalRegistrations {
    fn drop(&mut self) {
        low_level::unregister(self.interrupt);
        low_level::unregister(self.terminate);
    }
}
