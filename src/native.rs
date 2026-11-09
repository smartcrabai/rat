use std::{
    env::VarError,
    io::Write,
    process::{Command, Output, Stdio},
};

use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::{DateTime, Datelike, Timelike, Utc};

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AtJob {
    pub(crate) id: String,
    pub(crate) when: String,
    /// Empty when the native queue listing omits queue metadata.
    pub(crate) queue: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct CronJob {
    pub(crate) line: usize,
    pub(crate) schedule: String,
    pub(crate) command: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Crontab {
    pub(crate) text: String,
    pub(crate) jobs: Vec<CronJob>,
}

pub(crate) fn list_at() -> Result<Vec<AtJob>> {
    let output = Command::new("atq")
        .env("LC_ALL", "C")
        .output()
        .context("failed to run atq")?;
    ensure_success("atq", &output)?;
    parse_at_queue(&String::from_utf8_lossy(&output.stdout))
}

pub(crate) fn at_script(id: &str) -> Result<String> {
    ensure!(is_job_id(id), "invalid at job id");
    let output = Command::new("at")
        .arg("-c")
        .arg(id)
        .output()
        .context("failed to run at -c")?;
    ensure_success("at -c", &output)?;
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

pub(crate) fn read_crontab() -> Result<Crontab> {
    let output = Command::new("crontab")
        .arg("-l")
        .env("LC_ALL", "C")
        .output()
        .context("failed to run crontab -l")?;

    let text = if output.status.success() {
        String::from_utf8(output.stdout).context("crontab -l returned non-UTF-8 content")?
    } else if is_no_crontab_message(&String::from_utf8_lossy(&output.stderr)) {
        String::new()
    } else {
        return Err(command_error("crontab -l", &output));
    };

    Ok(Crontab {
        jobs: parse_crontab(&text),
        text,
    })
}

pub(crate) fn add_at(at: DateTime<Utc>, command: &str) -> Result<String> {
    validate_command(command)?;
    let timestamp = at_timestamp(at)?;
    let timezone = match std::env::var("TZ") {
        Ok(timezone) => Some(timezone),
        Err(VarError::NotPresent) => None,
        Err(VarError::NotUnicode(_)) => {
            bail!("cannot preserve non-UTF-8 TZ while scheduling at job")
        }
    };
    let input = at_input(command, timezone.as_deref());
    let output = run_with_input("at", &["-t", &timestamp], &input, Some(("TZ", "UTC")))?;
    ensure_success("at -t", &output)?;
    Ok(command_diagnostics(&output))
}
fn at_input(command: &str, timezone: Option<&str>) -> Vec<u8> {
    let mut input = String::with_capacity(command.len() + timezone.map_or(10, str::len) + 20);
    if let Some(timezone) = timezone {
        input.push_str("export TZ=");
        input.push_str(&shell_quote(timezone));
    } else {
        input.push_str("unset TZ");
    }
    input.push('\n');
    input.push_str(command);
    if !input.ends_with('\n') {
        input.push('\n');
    }
    input.into_bytes()
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

pub(crate) fn add_cron(expected: &str, schedule: &str, command: &str) -> Result<()> {
    validate_cron_input(schedule, command)?;
    let current = read_crontab()?.text;
    ensure_crontab_unchanged(expected, &current)?;
    let updated = append_cron(&current, schedule, command);
    let output = run_with_input("crontab", &["-"], updated.as_bytes(), Some(("LC_ALL", "C")))?;
    ensure_success("crontab -", &output)
}

fn parse_at_queue(text: &str) -> Result<Vec<AtJob>> {
    let mut jobs = Vec::new();
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let fields = line.split_whitespace().collect::<Vec<_>>();
        ensure!(
            fields.len() >= 2 && is_job_id(fields[0]),
            "could not parse atq line {}: {line:?}",
            index + 1
        );

        let owner_end = if fields
            .last()
            .is_some_and(|field| field.bytes().all(|byte| byte.is_ascii_alphabetic()))
        {
            fields.len() - 1
        } else {
            fields.len()
        };
        let queue_index = fields[1..owner_end].iter().position(|field| {
            *field == "=" || (field.len() == 1 && field.as_bytes()[0].is_ascii_alphabetic())
        });
        let (queue, when_fields) = if let Some(index) = queue_index {
            let queue = fields[index + 1].to_owned();
            (queue, &fields[1..=index])
        } else {
            (String::new(), &fields[1..owner_end])
        };
        ensure!(
            !when_fields.is_empty(),
            "could not parse atq time on line {}: {line:?}",
            index + 1
        );
        jobs.push(AtJob {
            id: fields[0].to_owned(),
            when: when_fields.join(" "),
            queue,
        });
    }
    Ok(jobs)
}

fn parse_crontab(text: &str) -> Vec<CronJob> {
    text.split('\n')
        .enumerate()
        .filter_map(|(index, raw_line)| {
            let line = raw_line.strip_suffix('\r').unwrap_or(raw_line);
            parse_crontab_line(line).map(|(schedule, command)| CronJob {
                line: index + 1,
                schedule,
                command: decode_cron_command(&command).unwrap_or(command),
            })
        })
        .collect()
}

fn parse_crontab_line(line: &str) -> Option<(String, String)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') || is_environment_assignment(line) {
        return None;
    }

    let (first, rest) = take_field(line)?;
    if first.starts_with('@') {
        if !matches!(
            first,
            "@yearly"
                | "@annually"
                | "@monthly"
                | "@weekly"
                | "@daily"
                | "@midnight"
                | "@hourly"
                | "@reboot"
        ) {
            return None;
        }
        let command = rest.trim_start();
        return (!command.is_empty()).then(|| (first.to_owned(), command.to_owned()));
    }

    let mut fields = vec![first];
    let mut remainder = rest;
    for _ in 1..5 {
        let (field, rest) = take_field(remainder)?;
        fields.push(field);
        remainder = rest;
    }
    let command = remainder.trim_start();
    if command.is_empty() {
        return None;
    }
    Some((fields.join(" "), command.to_owned()))
}

fn is_environment_assignment(line: &str) -> bool {
    let Some((name, _)) = line.split_once('=') else {
        return false;
    };
    let mut bytes = name.trim_end().bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn take_field(input: &str) -> Option<(&str, &str)> {
    let input = input.trim_start();
    let end = input.find(char::is_whitespace).unwrap_or(input.len());
    (end > 0).then(|| (&input[..end], &input[end..]))
}

const CRON_COMMAND_PREFIX: &str = "exec \"$SHELL\" -c '";

fn append_cron(text: &str, schedule: &str, command: &str) -> String {
    let schedule = schedule.split_whitespace().collect::<Vec<_>>().join(" ");
    let capacity = text
        .len()
        .saturating_add(schedule.len())
        .saturating_add(command.len().saturating_mul(4))
        .saturating_add(64);
    let mut result = String::with_capacity(capacity);
    result.push_str(text);
    if !text.is_empty() && !text.ends_with('\n') {
        result.push('\n');
    }
    result.push_str(&schedule);
    result.push(' ');
    append_cron_command(&mut result, command);
    result.push('\n');
    result
}

fn append_cron_command(output: &mut String, command: &str) {
    if !command.contains('%') && !command.contains('\\') {
        output.push_str(command);
        return;
    }
    output.push_str(CRON_COMMAND_PREFIX);
    for character in command.chars() {
        match character {
            '\'' => output.push_str("'\\''"),
            '%' => output.push_str("'\\%'"),
            '\\' => output.push_str("\\''"),
            _ => output.push(character),
        }
    }
    output.push('\'');
}
// Reverse only the exact escaped single-line form written by append_cron_command.
fn decode_cron_command(command: &str) -> Option<String> {
    let body = command
        .strip_prefix(CRON_COMMAND_PREFIX)?
        .strip_suffix('\'')?;
    let mut chars = body.chars();
    let mut decoded = String::with_capacity(body.len());
    let mut has_wrapper_trigger = false;
    while let Some(character) = chars.next() {
        match character {
            '\'' => {
                if chars.next()? != '\\' {
                    return None;
                }
                match chars.next()? {
                    '\'' => {
                        if chars.next()? != '\'' {
                            return None;
                        }
                        decoded.push('\'');
                    }
                    '%' => {
                        if chars.next()? != '\'' {
                            return None;
                        }
                        decoded.push('%');
                        has_wrapper_trigger = true;
                    }
                    _ => return None,
                }
            }
            '\\' => {
                if chars.next()? != '\'' || chars.next()? != '\'' {
                    return None;
                }
                decoded.push('\\');
                has_wrapper_trigger = true;
            }
            '\0' | '\n' | '\r' => return None,
            _ => decoded.push(character),
        }
    }
    (has_wrapper_trigger && !decoded.trim().is_empty()).then_some(decoded)
}
fn validate_cron_input(schedule: &str, command: &str) -> Result<()> {
    ensure!(
        !schedule
            .bytes()
            .any(|byte| matches!(byte, b'\n' | b'\r' | 0)),
        "cron schedule must not contain CR, LF, or NUL"
    );
    ensure!(
        schedule.split_whitespace().count() == 5,
        "cron schedule must contain exactly five fields"
    );
    crate::schedule::parse_recurring(schedule, "UTC")?;
    validate_command(command)
}

fn validate_command(command: &str) -> Result<()> {
    ensure!(!command.trim().is_empty(), "command must not be empty");
    ensure!(
        !command
            .bytes()
            .any(|byte| matches!(byte, b'\n' | b'\r' | 0)),
        "command must not contain CR, LF, or NUL"
    );
    Ok(())
}

fn at_timestamp(at: DateTime<Utc>) -> Result<String> {
    let at = if at.second() == 0 && at.nanosecond() == 0 {
        at
    } else {
        at.checked_add_signed(chrono::Duration::minutes(1))
            .context("at time is outside the supported date range")?
            .with_second(0)
            .context("at time is outside the supported date range")?
            .with_nanosecond(0)
            .context("at time is outside the supported date range")?
    };
    ensure!(
        (1..=9999).contains(&at.year()),
        "at time year must be between 0001 and 9999"
    );
    Ok(format!(
        "{:04}{:02}{:02}{:02}{:02}",
        at.year(),
        at.month(),
        at.day(),
        at.hour(),
        at.minute()
    ))
}

fn ensure_crontab_unchanged(expected: &str, current: &str) -> Result<()> {
    ensure!(
        expected == current,
        "crontab changed since it was listed; refusing to overwrite it"
    );
    Ok(())
}

fn is_no_crontab_message(stderr: &str) -> bool {
    let lines = stderr.trim().lines().collect::<Vec<_>>();
    if lines.len() != 1 {
        return false;
    }
    let message = lines[0].trim();
    let message = message.strip_prefix("crontab: ").unwrap_or(message);
    message
        .strip_prefix("no crontab for ")
        .is_some_and(|user| !user.trim().is_empty())
}

fn is_job_id(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit())
}

fn run_with_input(
    program: &str,
    args: &[&str],
    input: &[u8],
    env: Option<(&str, &str)>,
) -> Result<Output> {
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((name, value)) = env {
        command.env(name, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to run {program}"))?;
    let write_result = child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow!("{program} stdin was not piped"))?
        .write_all(input);
    drop(child.stdin.take());
    if let Err(error) = write_result {
        let _ = child.kill();
        let _ = child.wait();
        return Err(error).with_context(|| format!("failed to write to {program} stdin"));
    }
    child
        .wait_with_output()
        .with_context(|| format!("failed waiting for {program}"))
}

fn ensure_success(program: &str, output: &Output) -> Result<()> {
    if output.status.success() {
        Ok(())
    } else {
        Err(command_error(program, output))
    }
}

fn command_error(program: &str, output: &Output) -> anyhow::Error {
    let diagnostic = command_diagnostics(output);
    let status = output.status.code().map_or_else(
        || "terminated by signal".to_owned(),
        |code| format!("exit status {code}"),
    );
    anyhow!(
        "{program} failed ({status}){}",
        diagnostic_suffix(&diagnostic)
    )
}

fn command_diagnostics(output: &Output) -> String {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    match (stdout.is_empty(), stderr.is_empty()) {
        (true, true) => String::new(),
        (false, true) => stdout.into_owned(),
        (true, false) => stderr.into_owned(),
        (false, false) => format!(
            "{stdout}{}{}",
            if stdout.ends_with('\n') { "" } else { "\n" },
            stderr
        ),
    }
}

fn diagnostic_suffix(diagnostic: &str) -> String {
    if diagnostic.is_empty() {
        String::new()
    } else {
        format!(": {}", diagnostic.trim_end())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cron_jobs_and_skips_quoted_environment_assignments() {
        let text = concat!(
            "# keep this\n",
            "MAILTO=jobs@example.test\n",
            "NOTE=\"one two three four five six\"\n",
            "PATH = '/usr/bin /bin'\n",
            "0 2 * * 1 echo weekly  \n",
            "@reboot /bin/true\n",
            "unknown line\n",
        );
        let jobs = parse_crontab(text);
        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].line, 5);
        assert_eq!(jobs[0].schedule, "0 2 * * 1");
        assert_eq!(jobs[0].command, "echo weekly  ");
        assert_eq!(jobs[1].line, 6);
        assert_eq!(jobs[1].schedule, "@reboot");
        assert_eq!(jobs[1].command, "/bin/true");
    }
    #[test]
    fn appends_entry_without_rewriting_existing_bytes() {
        let expected = "# retained\nMAILTO=me@example.test\n0 1 * * * echo old";
        let updated = append_cron(expected, "15 3 * * 1", "echo new");
        assert_eq!(updated, format!("{expected}\n15 3 * * 1 echo new\n"));
        assert!(ensure_crontab_unchanged(expected, "different").is_err());
    }

    #[test]
    fn rejects_invalid_native_inputs() {
        assert!(validate_cron_input("* * * *", "echo x").is_err());
        assert!(validate_cron_input("* * * * *\n", "echo x").is_err());
        assert!(validate_cron_input("* * * * *", "echo x\nunsafe").is_err());
        assert!(validate_cron_input("* * * * *", "echo x\runsafe").is_err());
        assert!(validate_cron_input("* * * * *", "echo x\0unsafe").is_err());
        assert!(!is_no_crontab_message("permission denied"));
        assert!(is_no_crontab_message("crontab: no crontab for rat\n"));
    }

    #[test]
    fn parses_atq_queues_without_including_owner_in_time() -> Result<()> {
        let jobs = parse_at_queue(concat!(
            "12 2026-11-08 10:15:00 a admin\n",
            "13 Tue Nov  8 10:20:00 2026\n",
            "14 Fri Jan  1 09:00:00 2027 = a\n",
        ))?;
        assert_eq!(jobs.len(), 3);
        assert_eq!(jobs[0].queue, "a");
        assert_eq!(jobs[0].when, "2026-11-08 10:15:00");
        assert_eq!(jobs[1].queue, "");
        assert_eq!(jobs[1].when, "Tue Nov 8 10:20:00 2026");
        assert_eq!(jobs[2].queue, "=");
        assert_eq!(jobs[2].when, "Fri Jan 1 09:00:00 2027");
        Ok(())
    }

    #[test]
    fn rounds_at_time_up_to_the_next_whole_minute() -> Result<()> {
        let exact_minute = DateTime::parse_from_rfc3339("2026-11-08T10:15:00Z")?.to_utc();
        assert_eq!(at_timestamp(exact_minute)?, "202611081015");

        let fractional = DateTime::parse_from_rfc3339("2026-11-08T10:15:00.001Z")?.to_utc();
        assert_eq!(at_timestamp(fractional)?, "202611081016");

        let rollover = DateTime::parse_from_rfc3339("2026-11-08T10:15:59.999Z")?.to_utc();
        assert_eq!(at_timestamp(rollover)?, "202611081016");
        Ok(())
    }

    #[test]
    fn at_input_restores_the_callers_timezone() -> Result<()> {
        let input = at_input("printf '%s' \"$TZ\"", Some("Test'TZ"));
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(String::from_utf8(input)?)
            .env("TZ", "UTC")
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout.as_slice(), b"Test'TZ");

        let input = at_input("printf '%s' \"${TZ-unset}\"", None);
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(String::from_utf8(input)?)
            .env("TZ", "UTC")
            .output()?;
        assert!(output.status.success());
        assert_eq!(output.stdout.as_slice(), b"unset");
        Ok(())
    }

    // BSD cron also consumes the escape before backslashes; cronie only does so for percent.
    #[test]
    fn encoded_cron_command_survives_bsd_and_cronie_consumption_and_shells() -> Result<()> {
        let commands = [
            (
                r#"/usr/bin/printf '%s' "literal %; backslashes\\\\; apostrophe'""#,
                true,
            ),
            (r#"/usr/bin/printf "backslash-only\\\\""#, false),
        ];

        let mut shells = vec!["/bin/sh"];
        let csh = if cfg!(target_os = "macos") {
            assert!(
                std::path::Path::new("/bin/csh").is_file(),
                "macOS csh must be available"
            );
            Some("/bin/csh")
        } else {
            ["/bin/csh", "/bin/tcsh", "/usr/bin/csh", "/usr/bin/tcsh"]
                .into_iter()
                .find(|shell| std::path::Path::new(shell).is_file())
        };
        if let Some(csh) = csh {
            shells.push(csh);
        }
        let home = tempfile::tempdir()?;

        let backslash_probe = r"/usr/bin/printf '\%s' 'a\\b'";
        let (bsd_probe, bsd_stdin) = consume_cron_command(backslash_probe, true);
        let (cronie_probe, cronie_stdin) = consume_cron_command(backslash_probe, false);
        assert!(bsd_stdin.is_empty() && cronie_stdin.is_empty());
        let bsd_output = run_cron_shell("/bin/sh", &bsd_probe, home.path())?;
        let cronie_output = run_cron_shell("/bin/sh", &cronie_probe, home.path())?;
        assert!(bsd_output.status.success());
        assert!(cronie_output.status.success());
        assert_eq!(bsd_output.stdout, b"a\\b");
        assert_eq!(cronie_output.stdout, b"a\\\\b");

        for (original, has_percent) in commands {
            assert_eq!(original.contains('%'), has_percent);
            let entry = append_cron("", "* * * * *", original);
            let cron_command = entry
                .strip_prefix("* * * * * ")
                .and_then(|line| line.strip_suffix('\n'))
                .ok_or_else(|| anyhow!("appended cron entry has an invalid shape"))?;
            for (cron_flavor, unescape_backslashes) in [("BSD cron", true), ("cronie", false)] {
                let (consumed_command, stdin) =
                    consume_cron_command(cron_command, unescape_backslashes);
                assert!(stdin.is_empty(), "{cron_flavor} split the encoded command");
                for shell in shells.iter().copied() {
                    let raw = run_cron_shell(shell, original, home.path())?;
                    let encoded = run_cron_shell(shell, &consumed_command, home.path())?;
                    assert!(
                        raw.status.success(),
                        "raw command failed under {shell}: {}",
                        String::from_utf8_lossy(&raw.stderr)
                    );
                    assert!(
                        !raw.stdout.is_empty(),
                        "raw command emitted nothing under {shell}"
                    );
                    if has_percent {
                        assert!(
                            raw.stdout.contains(&b'%')
                                && raw.stdout.contains(&b'\\')
                                && raw.stdout.contains(&b'\''),
                            "raw command under {shell} omitted a cron-sensitive character"
                        );
                    } else {
                        assert!(
                            raw.stdout.contains(&b'\\'),
                            "backslash-only command omitted its backslash under {shell}"
                        );
                    }
                    assert!(
                        encoded.status.success(),
                        "{cron_flavor} command failed under {shell}: {}",
                        String::from_utf8_lossy(&encoded.stderr)
                    );
                    assert_eq!(
                        encoded.stdout, raw.stdout,
                        "{cron_flavor} under {shell} changed the command output"
                    );
                    assert_eq!(
                        encoded.stderr, raw.stderr,
                        "{cron_flavor} under {shell} changed the command diagnostics"
                    );
                }
            }
        }
        Ok(())
    }

    fn run_cron_shell(shell: &str, command: &str, home: &std::path::Path) -> Result<Output> {
        Ok(Command::new(shell)
            .arg("-c")
            .arg(command)
            .env("SHELL", shell)
            .env("HOME", home)
            .env("TMPDIR", home)
            .output()?)
    }

    fn consume_cron_command(command: &str, unescape_backslashes: bool) -> (String, String) {
        let mut shell_command = String::with_capacity(command.len());
        let mut stdin = String::new();
        let mut in_stdin = false;
        let mut escaped = false;
        for character in command.chars() {
            if in_stdin {
                stdin.push(if character == '%' { '\n' } else { character });
            } else if escaped {
                if character == '%' || (unescape_backslashes && character == '\\') {
                    shell_command.push(character);
                } else {
                    shell_command.push('\\');
                    shell_command.push(character);
                }
                escaped = false;
            } else if character == '\\' {
                escaped = true;
            } else if character == '%' {
                in_stdin = true;
            } else {
                shell_command.push(character);
            }
        }
        if escaped {
            shell_command.push('\\');
        }
        (shell_command, stdin)
    }
}
