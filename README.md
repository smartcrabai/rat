# rat

rat is a single-user tool for scheduling local shell commands. It targets macOS and Linux and manages one-shot and calendar-based recurring schedules as Jobs.

## Install

Prebuilt binaries support Apple Silicon macOS and Linux x86_64/ARM64 with glibc 2.35 or later. SQLite is bundled.

### Homebrew

```sh
brew install smartcrabai/tap/rat
```

### Shell installer

```sh
curl --proto '=https' --tlsv1.2 -LsSf https://github.com/smartcrabai/rat/releases/latest/download/rat-installer.sh | sh
```

The installer uses `$CARGO_HOME/bin` (default: `~/.cargo/bin`). Follow its instructions to update `PATH`, or open a new shell.

Run `rat --help` to check the installation. Versioned downloads and checksums are available on [GitHub Releases](https://github.com/smartcrabai/rat/releases).

### Build from source

A Rust toolchain and a C compiler are required.

```sh
cargo install --path .
```

## Use and start

Starting `rat` opens the job list, where you can view job details, edit or delete jobs, and add jobs. `rat list` displays the list without an interactive interface. Jobs can also be paused and resumed from the list or the non-interactive CLI.

Before using these examples, either run `rat service install` or start `rat daemon` in another terminal:

```sh
rat add 2h 5m -- 'printf "hello\n"'
rat add --cron '0 9 * * 1-5' --timezone Asia/Tokyo -- 'date'
```

Everything after `--` is treated as one command string interpreted by the shell. Arguments are not reconstructed after shell expansion.

rat saves and manages job definitions and determines when they run. The OS is used to start rat's scheduler when the user logs in; it is not used to register individual jobs. `at` and `cron` are not runtime dependencies. The scheduler continues running after the terminal is closed and resumes after login, but execution while logged out and waking hardware from sleep are not guaranteed.

During initial interactive setup, rat asks before configuring automatic startup at login. macOS uses launchd; Linux uses systemd --user. On Linux without systemd --user, the scheduler can be started manually. The non-interactive CLI never silently configures a persistent service. If required setup is missing, it returns an error with explicit guidance.

`rat add`, `rat edit`, and `rat resume` require a scheduler that is actually running. The presence of an automatic-start configuration alone is not enough. Check or remove the service with `rat service status` and `rat service uninstall`. The manually started `rat daemon` runs in the foreground. Use the service for login-time startup independent of a terminal.

A Job is a scheduled command; a Run is one execution of a Job. Job IDs and Run IDs are distinct. Common non-interactive commands:

```sh
rat list --all --json
rat show 1 --json
rat edit 1 --at 30m -- 'date'
rat pause 1
rat resume 1 --confirm-overdue
rat review 1 --acknowledge
rat history 1 --json
rat output 1
rat delete 1
```

The numeric arguments to `show`, `edit`, `pause`, `resume`, `review`, `history`, and `delete` are Job IDs. The numeric argument to `output` is a Run ID. `review --acknowledge` marks an uncertain scheduled occurrence as reviewed without replaying it. `--confirm-overdue` explicitly consents to immediately running an overdue one-shot Job. The interactive interface also describes commands through `help`; use `history` and `output` to inspect Runs and their results.

Data is stored in this order of preference: `RAT_HOME`, `XDG_DATA_HOME/rat`, then `HOME/.local/share/rat`. The directory is accessible only to the user. Job definitions and Run history are stored in SQLite, and output is stored in separate files.

## Schedules

### One-shot

One-shot date and time syntax is based on [the pinned zz commit](https://github.com/smartcrabai/zz/tree/23868142c15b1f3bb64e607762d722ea660e17e0).

- A single integer means a relative number of seconds. Independent hour, minute, and second tokens can be combined, as in `2h 5m`; `2h5m` is not valid.
- Local times use `HH:MM` or `HH:MM:SS`. Absolute dates use `YYYYMMDDThhmmss+HHMM`, `YYYYMMDDThhmmss-HHMM`, or `YYYYMMDDThhmmssZ`. Seconds can be specified. Natural-language date and time parsing is not supported.
- Negative numbers, out-of-range values, overflow, and absolute dates already in the past when a new Job is registered are rejected. Zero means run immediately. A time without a date means the next occurrence of that time.
- Resolved UTC timestamps must use years `0000` through `9999`, including relative delays and offset conversions.
- Relative dates are calculated from the final confirmation of registration, and the resolved absolute date and time are shown in the registration result.

### Recurring

Recurring schedules can be selected from an interactive daily, weekly, or monthly calendar, or specified with a standard five-field cron expression. Elapsed-time interval repetition is not supported. Each Job's timezone is saved and displayed when it is registered; changing the OS timezone does not change its schedule. Timezone changes must be made explicitly.

Monthly occurrences whose date does not exist (for example, the 31st in a month without 31 days) and local times that do not exist because of a daylight-saving transition are skipped. When a local time occurs twice as daylight saving time ends, only the first occurrence is used. One-shot local times that do not exist or are ambiguous are rejected at registration.

## Commands

Jobs store arbitrary shell command strings, including pipes and redirects, and run them with `/bin/sh -c`. The working directory and `PATH` at registration are saved. Only additional environment variables explicitly set for the Job are provided. rat does not snapshot the full environment or implicitly load interactive shell configuration or aliases. No interactive terminal is allocated, and standard input is `/dev/null`. Managing interactive sessions is not supported.

Set additional environment variables by repeating `--env KEY=VALUE`:

```sh
rat add 0 --env MODE=nightly -- 'printf "%s\n" "$MODE"'
```

## Execution, pause, and recovery

One-shot schedules that pass while the scheduler is unavailable run after it recovers. Missed recurring occurrences are skipped; the schedule continues with the next future occurrence. Missed occurrences are not replayed in a batch on resume. If a recurring Job is still running at its next scheduled time, that occurrence is skipped rather than queued. Different Jobs can run concurrently.

Ordinary failures are recorded but not retried automatically. A recurring Job continues with its next scheduled occurrence. A one-shot Job is not registered again after either success or failure. A completed one-shot Job is removed from the normal scheduled list and can be viewed in the 30-day history. A new schedule is required to run it again.

A user-paused recurring Job skips occurrences that passed while it was paused. When resuming an overdue one-shot Job, the user must explicitly confirm that it should run immediately. If whether an occurrence ran or what its result was is uncertain, rat does not replay it. The Job enters NeedsReview and remains on hold until the user decides. NeedsReview is separate from ordinary Paused state and pause/resume.

An explicitly edited replacement one-shot schedule remains pending when the uncertain occurrence is acknowledged. Future replacements become active; replacements that became overdue remain paused until `resume --confirm-overdue`.

Editing a running Job affects future schedules. Deleting a Job prevents future starts but does not stop a command that is already running. Its history is not immediately erased and remains available during the retention period.

## History and output

Run history stores start and end times, exit status, standard output, and standard error, and can be viewed in lists and details. History and output are retained for 30 days. Output is capped at 10 MiB combined stdout and stderr per Run, and 1 GiB total. At the total cap, the oldest output is deleted first. Reaching the per-Run cap does not stop the command; truncation or missing output is indicated.

## Development and verification

In addition to Rust unit tests, the project uses Bats for end-to-end tests of the real CLI, shell, and PTY, and a Quint state-transition specification at [spec/scheduler.qnt](spec/scheduler.qnt). Bats requires Bash and Python 3. Tests use a temporary `RAT_HOME` and do not configure an autostart service on the host.

```sh
cargo test --all-features
cargo build --bin rat
bats tests/e2e.bats
cargo fmt -- --check
cargo clippy --all-targets --all-features -- -D warnings
```

Formal specification checks require Node.js 18 or later and Java 21 or later. The Quint version is pinned in `package-lock.json`.

```sh
npm ci
npm run typecheck
npm test
npm run run
npm run verify
```

Quint checks scheduling uniqueness, overlap skips, pause and resume, recovery from NeedsReview, preventing starts after deletion, and the absence of automatic retries. Its state space is a finite model with one one-shot Job, one recurring Job, three recurring occurrences, and times 0 through 3. Randomized runs use 10,000 traces of 20 steps each; TLC bounded verification checks 12 steps. These checks do not establish OS process-start or persistence atomicity, nor do they formally prove the entire Rust implementation. Correspondence with the implementation is checked by unit and end-to-end tests.

## Out of scope

Canceling or forcibly terminating running commands, notifications, importing existing `at` or `crontab` jobs, remote execution, Windows, and multi-user administration are out of scope. Full syntax compatibility with existing commands is not a goal.
