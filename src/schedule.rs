use std::collections::BTreeSet;

use anyhow::{Context, Result, bail, ensure};
use chrono::{
    DateTime, Datelike, Days, Duration, LocalResult, NaiveDate, NaiveDateTime, TimeZone, Utc,
};
use chrono_tz::Tz;
use croner::{
    errors::CronError,
    parser::{CronParser, Seconds, Year},
};
use serde::{Deserialize, Serialize};

/// A one-off instant or a recurring civil-calendar rule.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Schedule {
    /// One absolute execution instant, interpreted in the saved timezone for display.
    OneOff { at: DateTime<Utc>, timezone: String },
    /// A standard five-field cron rule in its saved IANA timezone.
    Recurring { cron: String, timezone: String },
}

impl Schedule {
    /// Returns the IANA timezone saved with this schedule.
    #[must_use]
    pub fn timezone(&self) -> &str {
        match self {
            Self::OneOff { timezone, .. } | Self::Recurring { timezone, .. } => timezone,
        }
    }

    /// Finds the first scheduled instant strictly after `after`.
    ///
    /// # Errors
    ///
    /// Returns an error if the saved timezone or recurring rule is invalid.
    pub fn next_after(&self, after: DateTime<Utc>) -> Result<Option<DateTime<Utc>>> {
        match self {
            Self::OneOff { at, .. } => Ok((*at > after).then_some(*at)),
            Self::Recurring { cron, timezone } => recurring_next_after(cron, timezone, after),
        }
    }

    /// Returns the first instant to use when activating this schedule.
    ///
    /// A one-off keeps its saved instant, including an immediate or overdue instant. A
    /// recurring schedule always starts strictly after `now`.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported one-off UTC year, invalid saved timezone or recurring
    /// rule, or a recurring schedule with no representable future occurrence.
    pub fn first_at(&self, now: DateTime<Utc>) -> Result<DateTime<Utc>> {
        match self {
            Self::OneOff { at, .. } => validate_one_off_instant(*at),
            Self::Recurring { .. } => self
                .next_after(now)?
                .context("recurring schedule has no representable future occurrence"),
        }
    }
}

/// Parses a one-off time expression using `now` as its final resolution base.
///
/// Accepted expressions are integer seconds, separate nonnegative `h`/`m`/`s` duration
/// tokens, strict local `HH:MM[:SS]` clocks, and strict `YYYYMMDDThhmmssZ` or
/// `YYYYMMDDThhmmss(+|-)HHMM` absolute timestamps.
///
/// # Errors
///
/// Returns an error for malformed or overflowing input, an invalid timezone, an unsupported UTC
/// year, a past absolute instant, or a nonexistent/ambiguous local clock time.
pub fn parse_one_off(tokens: &[String], now: DateTime<Utc>, timezone: &str) -> Result<Schedule> {
    let zone = parse_timezone(timezone)?;
    ensure!(!tokens.is_empty(), "one-off time is required");

    let at = if tokens.len() == 1 {
        let token = tokens[0].as_str();
        if is_ascii_digits(token) {
            now.checked_add_signed(duration_from_seconds(parse_unsigned(token)?)?)
                .context("relative one-off instant is outside the supported date range")?
        } else if is_absolute_timestamp(token) {
            let parsed = parse_absolute_timestamp(token)?;
            ensure!(parsed >= now, "absolute one-off time is in the past");
            parsed
        } else if token.contains(':') {
            parse_local_clock(token, now, zone)?
        } else {
            parse_relative_tokens(tokens, now)?
        }
    } else {
        parse_relative_tokens(tokens, now)?
    };

    let at = validate_one_off_instant(at)?;

    Ok(Schedule::OneOff {
        at,
        timezone: timezone.to_owned(),
    })
}

/// Parses and validates a standard five-field cron expression and saved IANA timezone.
///
/// Day-of-month and day-of-week use Vixie cron semantics: require both fields to match if either
/// begins with `*`, otherwise require either field to match. Fields support wildcards, lists,
/// ranges, and steps; standard month and weekday names are accepted, but Croner-specific
/// extensions are rejected.
///
/// # Errors
///
/// Returns an error if the field count, syntax, values, calendar rule, or timezone is invalid.
pub fn parse_recurring(expression: &str, timezone: &str) -> Result<Schedule> {
    parse_timezone(timezone)?;
    let fields = expression.split_whitespace().collect::<Vec<_>>();
    ensure!(
        fields.len() == 5,
        "cron expression must contain exactly five fields"
    );
    for (index, field) in fields.iter().enumerate() {
        ensure!(
            field.bytes().all(|byte| {
                byte.is_ascii_digit()
                    || b"*/,-".contains(&byte)
                    || (index >= 3 && byte.is_ascii_alphabetic())
            }),
            "cron expression contains a nonstandard extension or invalid character"
        );
    }

    let normalized_month = normalize_cron_names(
        fields[3],
        &[
            ("JAN", 1),
            ("FEB", 2),
            ("MAR", 3),
            ("APR", 4),
            ("MAY", 5),
            ("JUN", 6),
            ("JUL", 7),
            ("AUG", 8),
            ("SEP", 9),
            ("OCT", 10),
            ("NOV", 11),
            ("DEC", 12),
        ],
    )?;
    let normalized_weekday = normalize_cron_names(
        fields[4],
        &[
            ("SUN", 0),
            ("MON", 1),
            ("TUE", 2),
            ("WED", 3),
            ("THU", 4),
            ("FRI", 5),
            ("SAT", 6),
        ],
    )?;

    let _minutes = expand_numeric_field(fields[0], 0, 59)?;
    let _hours = expand_numeric_field(fields[1], 0, 23)?;
    let month_day_values = expand_numeric_field(fields[2], 1, 31)?;
    let months = expand_numeric_field(&normalized_month, 1, 12)?;
    let mut weekday_values = expand_numeric_field(&normalized_weekday, 0, 7)?;
    if weekday_values.remove(&7) {
        weekday_values.insert(0);
    }
    let day_fields_use_and = day_fields_use_and_semantics(fields[2], fields[4]);

    let parser = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .dom_and_dow(day_fields_use_and)
        .alternative_weekdays(false)
        .sloppy_ranges(false)
        .build();
    parser
        .parse(expression)
        .context("invalid five-field cron expression")?;

    ensure!(
        has_calendar_occurrence(
            fields[2],
            &month_day_values,
            &months,
            &normalized_weekday,
            &weekday_values,
        )?,
        "cron expression has no possible calendar date"
    );

    Ok(Schedule::Recurring {
        cron: expression.to_owned(),
        timezone: timezone.to_owned(),
    })
}

fn day_fields_use_and_semantics(day_of_month: &str, day_of_week: &str) -> bool {
    day_of_month.starts_with('*') || day_of_week.starts_with('*')
}

fn parse_timezone(timezone: &str) -> Result<Tz> {
    timezone
        .parse::<Tz>()
        .with_context(|| format!("invalid IANA timezone: {timezone}"))
}

fn is_ascii_digits(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
}

fn parse_unsigned(value: &str) -> Result<u64> {
    ensure!(is_ascii_digits(value), "expected a nonnegative integer");
    value
        .parse::<u64>()
        .with_context(|| format!("integer is out of range: {value}"))
}

fn validate_one_off_instant(at: DateTime<Utc>) -> Result<DateTime<Utc>> {
    ensure!(
        (0..=9999).contains(&at.year()),
        "one-off UTC date year is outside the supported range 0000..=9999"
    );
    Ok(at)
}

fn duration_from_seconds(seconds: u64) -> Result<Duration> {
    let seconds = i64::try_from(seconds).context("relative duration is too large")?;
    Duration::try_seconds(seconds).context("relative duration is too large")
}

fn parse_relative_tokens(tokens: &[String], now: DateTime<Utc>) -> Result<DateTime<Utc>> {
    let mut total_seconds = 0_u64;
    for token in tokens {
        let (digits, multiplier) = match token.as_bytes().last() {
            Some(b'h') => (&token[..token.len() - 1], 3_600_u64),
            Some(b'm') => (&token[..token.len() - 1], 60_u64),
            Some(b's') => (&token[..token.len() - 1], 1_u64),
            _ => bail!("duration tokens must be separate nonnegative h/m/s values"),
        };
        let amount = parse_unsigned(digits)?;
        let seconds = amount
            .checked_mul(multiplier)
            .context("relative duration is too large")?;
        total_seconds = total_seconds
            .checked_add(seconds)
            .context("relative duration is too large")?;
    }

    now.checked_add_signed(duration_from_seconds(total_seconds)?)
        .context("relative one-off instant is outside the supported date range")
}

fn is_absolute_timestamp(value: &str) -> bool {
    let bytes = value.as_bytes();
    (bytes.len() == 16
        && bytes[8] == b'T'
        && bytes[15] == b'Z'
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[9..15].iter().all(u8::is_ascii_digit))
        || (bytes.len() == 20
            && bytes[8] == b'T'
            && matches!(bytes[15], b'+' | b'-')
            && bytes[..8].iter().all(u8::is_ascii_digit)
            && bytes[9..15].iter().all(u8::is_ascii_digit)
            && bytes[16..20].iter().all(u8::is_ascii_digit))
}

fn parse_absolute_timestamp(value: &str) -> Result<DateTime<Utc>> {
    let seconds = value[13..15].parse::<u32>()?;
    ensure!(seconds < 60, "absolute timestamp second is out of range");

    let parsed = if value.ends_with('Z') {
        NaiveDateTime::parse_from_str(value, "%Y%m%dT%H%M%SZ").map(|parsed| parsed.and_utc())
    } else {
        let offset_hour = value[16..18].parse::<u32>()?;
        let offset_minute = value[18..20].parse::<u32>()?;
        ensure!(
            offset_hour < 24 && offset_minute < 60,
            "absolute timestamp offset is out of range"
        );
        DateTime::parse_from_str(value, "%Y%m%dT%H%M%S%z").map(|parsed| parsed.with_timezone(&Utc))
    };
    parsed.with_context(|| format!("invalid absolute one-off timestamp: {value}"))
}

fn parse_local_clock(value: &str, now: DateTime<Utc>, timezone: Tz) -> Result<DateTime<Utc>> {
    let components = value.split(':').collect::<Vec<_>>();
    ensure!(
        (components.len() == 2 || components.len() == 3)
            && components
                .iter()
                .all(|component| component.len() == 2 && is_ascii_digits(component)),
        "local clock must use strict HH:MM or HH:MM:SS syntax"
    );
    let hour = components[0].parse::<u32>()?;
    let minute = components[1].parse::<u32>()?;
    let second = components
        .get(2)
        .map_or(Ok(0), |value| value.parse::<u32>())?;
    ensure!(
        hour < 24 && minute < 60 && second < 60,
        "local clock component is out of range"
    );

    let local_now = now.with_timezone(&timezone).naive_local();
    let mut date = local_now.date();
    let mut candidate = date
        .and_hms_opt(hour, minute, second)
        .context("local clock is invalid")?;
    if candidate <= local_now {
        date = date
            .checked_add_days(Days::new(1))
            .context("local clock date is outside the supported range")?;
        candidate = date
            .and_hms_opt(hour, minute, second)
            .context("local clock is invalid")?;
    }

    match timezone.from_local_datetime(&candidate) {
        LocalResult::Single(local) => Ok(local.with_timezone(&Utc)),
        LocalResult::Ambiguous(_, _) => bail!("local one-off time is ambiguous in {timezone}"),
        LocalResult::None => bail!("local one-off time does not exist in {timezone}"),
    }
}

fn normalize_cron_names(field: &str, names: &[(&str, u32)]) -> Result<String> {
    let mut normalized = String::with_capacity(field.len());
    let bytes = field.as_bytes();
    let mut position = 0;
    while position < bytes.len() {
        if bytes[position].is_ascii_alphabetic() {
            let start = position;
            while position < bytes.len() && bytes[position].is_ascii_alphabetic() {
                position += 1;
            }
            let adjacent_to_digit = (start > 0 && bytes[start - 1].is_ascii_digit())
                || (position < bytes.len() && bytes[position].is_ascii_digit());
            ensure!(
                !adjacent_to_digit,
                "cron names cannot be adjacent to digits"
            );

            let name = field[start..position].to_ascii_uppercase();
            let value = names
                .iter()
                .find_map(|(candidate, value)| (*candidate == name.as_str()).then_some(*value))
                .with_context(|| format!("nonstandard cron name: {name}"))?;
            normalized.push_str(&value.to_string());
        } else {
            normalized.push(char::from(bytes[position]));
            position += 1;
        }
    }
    Ok(normalized)
}

fn parse_u32(value: &str) -> Result<u32> {
    u32::try_from(parse_unsigned(value)?).context("cron field value is out of range")
}

fn expand_numeric_field(field: &str, min: u32, max: u32) -> Result<BTreeSet<u32>> {
    let mut values = BTreeSet::new();
    for part in field.split(',') {
        ensure!(
            !part.is_empty(),
            "cron fields cannot contain empty list items"
        );
        let (range, step) = match part.split_once('/') {
            Some((range, step)) => {
                ensure!(!step.is_empty() && !step.contains('/'), "invalid cron step");
                (range, parse_u32(step)?)
            }
            None => (part, 1),
        };
        ensure!(step > 0, "cron step must be positive");

        let (start, end) = if range == "*" {
            (min, max)
        } else if let Some((start, end)) = range.split_once('-') {
            ensure!(
                !start.is_empty() && !end.is_empty() && !end.contains('-'),
                "invalid cron range"
            );
            (parse_u32(start)?, parse_u32(end)?)
        } else {
            let start = parse_u32(range)?;
            (start, if part.contains('/') { max } else { start })
        };
        ensure!(
            start >= min && end <= max && start <= end,
            "cron field value is out of range"
        );

        let mut value = start;
        loop {
            values.insert(value);
            let Some(next) = value.checked_add(step) else {
                break;
            };
            if next > end {
                break;
            }
            value = next;
        }
    }
    ensure!(!values.is_empty(), "cron field has no values");
    Ok(values)
}

fn has_calendar_occurrence(
    month_day_rule: &str,
    valid_month_dates: &BTreeSet<u32>,
    months: &BTreeSet<u32>,
    weekday_rule: &str,
    valid_sunday_based_days: &BTreeSet<u32>,
) -> Result<bool> {
    let day_fields_use_and = day_fields_use_and_semantics(month_day_rule, weekday_rule);

    // Gregorian dates and weekdays repeat every 400 years; this proves impossibility without
    // asking Croner to search indefinitely for rules such as February 31st.
    for year in 2000..2400 {
        for month in 1..=12 {
            if !months.contains(&month) {
                continue;
            }
            let Some(first_day) = NaiveDate::from_ymd_opt(year, month, 1) else {
                continue;
            };
            let next_month = if month == 12 {
                NaiveDate::from_ymd_opt(year + 1, 1, 1)
            } else {
                NaiveDate::from_ymd_opt(year, month + 1, 1)
            };
            let Some(next_month) = next_month else {
                continue;
            };
            let month_days = u32::try_from((next_month - first_day).num_days())
                .context("invalid month length")?;
            for day in 1..=month_days {
                let date_matches = valid_month_dates.contains(&day);
                let date = first_day
                    .with_day(day)
                    .context("invalid date while checking cron calendar occurrence")?;
                let weekday_matches =
                    valid_sunday_based_days.contains(&date.weekday().num_days_from_sunday());
                let day_matches = if day_fields_use_and {
                    date_matches && weekday_matches
                } else {
                    date_matches || weekday_matches
                };
                if day_matches {
                    return Ok(true);
                }
            }
        }
    }
    Ok(false)
}

fn recurring_next_after(
    cron_expression: &str,
    timezone: &str,
    after: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let zone = parse_timezone(timezone)?;
    let mut fields = cron_expression.split_whitespace();
    let month_day_field = fields.nth(2).unwrap_or_default();
    let weekday_field = fields.nth(1).unwrap_or_default();
    let day_fields_use_and = day_fields_use_and_semantics(month_day_field, weekday_field);
    let parser = CronParser::builder()
        .seconds(Seconds::Disallowed)
        .year(Year::Disallowed)
        .dom_and_dow(day_fields_use_and)
        .alternative_weekdays(false)
        .sloppy_ranges(false)
        .build();
    let cron = parser
        .parse(cron_expression)
        .context("invalid saved cron expression")?;
    let mut civil_after = after.with_timezone(&zone).naive_local();

    loop {
        let candidate = match cron.find_next_occurrence(&civil_after, false) {
            Ok(candidate) => candidate,
            Err(CronError::InvalidTime | CronError::TimeSearchLimitExceeded) => return Ok(None),
            Err(error) => return Err(error).context("could not find next cron occurrence"),
        };
        match zone.from_local_datetime(&candidate) {
            LocalResult::Single(local) => {
                let instant = local.with_timezone(&Utc);
                if instant > after {
                    return Ok(Some(instant));
                }
            }
            LocalResult::Ambiguous(first, second) => {
                let first = first.with_timezone(&Utc);
                let second = second.with_timezone(&Utc);
                let instant = first.min(second);
                if instant > after {
                    return Ok(Some(instant));
                }
            }
            LocalResult::None => {}
        }
        civil_after = candidate;
    }
}
