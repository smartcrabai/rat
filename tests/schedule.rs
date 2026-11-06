use anyhow::{Result, anyhow};
use chrono::{DateTime, Duration, TimeZone, Utc};
use rat::schedule::{Schedule, parse_one_off, parse_recurring};

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
        .ok_or_else(|| anyhow!("invalid UTC test instant"))
}

fn tokens(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).to_owned()).collect()
}

fn one_off_at(schedule: Schedule) -> Result<DateTime<Utc>> {
    match schedule {
        Schedule::OneOff { at, .. } => Ok(at),
        other @ Schedule::Recurring { .. } => {
            Err(anyhow!("expected one-off schedule, got {other:?}"))
        }
    }
}

#[test]
fn separate_duration_tokens_are_summed_but_concatenated_tokens_are_rejected() -> Result<()> {
    let now = utc(2026, 2, 20, 10, 0, 0)?;
    let schedule = parse_one_off(&tokens(&["2h", "5m", "30s"]), now, "UTC")?;

    assert_eq!(one_off_at(schedule)?, now + Duration::seconds(7_530));
    assert!(parse_one_off(&tokens(&["2h5m"]), now, "UTC").is_err());
    Ok(())
}

#[test]
fn zero_is_immediate_while_negative_and_overflowing_durations_are_rejected() -> Result<()> {
    let now = utc(2026, 2, 20, 10, 0, 0)? + Duration::nanoseconds(123_456_789);
    let schedule = parse_one_off(&tokens(&["0"]), now, "UTC")?;

    assert_eq!(one_off_at(schedule)?, now);
    assert!(parse_one_off(&tokens(&["-1"]), now, "UTC").is_err());
    assert!(parse_one_off(&tokens(&["-1h"]), now, "UTC").is_err());
    assert!(parse_one_off(&tokens(&["18446744073709551615"]), now, "UTC").is_err());
    assert!(parse_one_off(&tokens(&["9223372036854775807s", "1s"]), now, "UTC").is_err());
    Ok(())
}

#[test]
fn large_relative_delays_must_resolve_inside_supported_utc_years() -> Result<()> {
    let now = utc(2026, 1, 1, 0, 0, 0)?;

    assert!(parse_one_off(&tokens(&["315576000000"]), now, "UTC").is_err());
    assert!(parse_one_off(&tokens(&["87659999h", "59m", "59s"]), now, "UTC").is_err());
    Ok(())
}

#[test]
fn one_off_parser_accepts_the_last_supported_instant_and_rejects_the_next_second() -> Result<()> {
    let last_supported = utc(9999, 12, 31, 23, 59, 59)? + Duration::nanoseconds(999_999_999);
    let schedule = parse_one_off(&tokens(&["0"]), last_supported, "UTC")?;

    assert_eq!(one_off_at(schedule)?, last_supported);
    assert!(parse_one_off(&tokens(&["1"]), utc(9999, 12, 31, 23, 59, 59)?, "UTC").is_err());
    assert!(parse_one_off(&tokens(&["0"]), utc(0, 1, 1, 0, 0, 0)?, "UTC").is_ok());
    Ok(())
}

#[test]
fn absolute_offsets_must_not_cross_outside_supported_utc_years() -> Result<()> {
    assert!(
        parse_one_off(
            &tokens(&["99991231T233000-0100"]),
            utc(9999, 12, 31, 0, 0, 0)?,
            "UTC"
        )
        .is_err()
    );
    assert!(
        parse_one_off(
            &tokens(&["00000101T003000+0100"]),
            utc(-1, 12, 31, 0, 0, 0)?,
            "UTC"
        )
        .is_err()
    );
    Ok(())
}

#[test]
fn directly_constructed_one_off_cannot_activate_outside_supported_utc_years() -> Result<()> {
    let schedule = Schedule::OneOff {
        at: utc(10000, 1, 1, 0, 0, 0)?,
        timezone: "UTC".to_owned(),
    };

    assert!(schedule.first_at(utc(2026, 2, 20, 10, 0, 0)?).is_err());
    Ok(())
}

#[test]
fn absolute_dates_keep_their_offset_and_past_dates_are_refused() -> Result<()> {
    let now = utc(2026, 7, 1, 12, 0, 0)?;
    let schedule = parse_one_off(&tokens(&["20260701T150000+0200"]), now, "Asia/Tokyo")?;

    assert_eq!(one_off_at(schedule.clone())?, utc(2026, 7, 1, 13, 0, 0)?);
    assert_eq!(schedule.timezone(), "Asia/Tokyo");
    assert!(parse_one_off(&tokens(&["20260630T120000Z"]), now, "UTC").is_err());
    Ok(())
}

#[test]
fn local_clocks_are_strict_and_a_past_clock_moves_to_the_next_calendar_day() -> Result<()> {
    let now = utc(2026, 3, 7, 15, 0, 0)?;
    let schedule = parse_one_off(&tokens(&["09:00"]), now, "America/New_York")?;

    // The next local calendar day crosses the spring DST transition: 09:00 EDT is 13:00 UTC.
    assert_eq!(one_off_at(schedule)?, utc(2026, 3, 8, 13, 0, 0)?);
    assert!(parse_one_off(&tokens(&["9:00"]), now, "America/New_York").is_err());
    assert!(parse_one_off(&tokens(&["09:00:00.5"]), now, "America/New_York").is_err());
    Ok(())
}

#[test]
fn one_off_local_times_in_dst_gaps_and_folds_are_rejected() -> Result<()> {
    let before_spring_gap = utc(2026, 3, 8, 5, 0, 0)?;
    let before_fall_fold = utc(2026, 11, 1, 4, 0, 0)?;

    assert!(parse_one_off(&tokens(&["02:30"]), before_spring_gap, "America/New_York").is_err());
    assert!(parse_one_off(&tokens(&["01:30"]), before_fall_fold, "America/New_York").is_err());
    Ok(())
}

#[test]
fn one_off_first_at_is_immediate_and_has_no_later_occurrence() -> Result<()> {
    let now = utc(2026, 2, 20, 10, 0, 0)?;
    let schedule = parse_one_off(&tokens(&["0"]), now, "UTC")?;

    assert_eq!(schedule.first_at(now)?, now);
    assert_eq!(schedule.next_after(now)?, None);
    assert_eq!(schedule.next_after(now - Duration::seconds(1))?, Some(now));
    Ok(())
}

#[test]
fn recurring_schedules_require_exactly_five_cron_fields() -> Result<()> {
    assert!(parse_recurring("0 9 * *", "UTC").is_err());
    assert!(parse_recurring("0 9 * * * *", "UTC").is_err());
    assert!(parse_recurring("0 9 * * not-a-weekday", "UTC").is_err());

    let schedule = parse_recurring("*/15 9-17 * * 1-5", "UTC")?;
    assert_eq!(schedule.timezone(), "UTC");
    Ok(())
}

#[test]
fn recurring_day_of_month_and_day_of_week_use_or_semantics() -> Result<()> {
    let schedule = parse_recurring("0 9 1 * 1", "UTC")?;
    let after = utc(2026, 6, 2, 8, 0, 0)?;

    // June 8 is Monday (DOW match) even though it is not the first day of the month.
    assert_eq!(schedule.next_after(after)?, Some(utc(2026, 6, 8, 9, 0, 0)?));
    Ok(())
}

#[test]
fn recurring_monthly_dates_skip_months_without_the_requested_day() -> Result<()> {
    let schedule = parse_recurring("0 9 31 * *", "UTC")?;
    let after = utc(2026, 1, 31, 9, 1, 0)?;

    assert_eq!(
        schedule.next_after(after)?,
        Some(utc(2026, 3, 31, 9, 0, 0)?)
    );
    Ok(())
}

#[test]
fn recurrence_uses_its_saved_timezone_not_the_machine_timezone() -> Result<()> {
    let schedule = parse_recurring("0 9 * * *", "America/New_York")?;
    let after = utc(2026, 7, 1, 13, 0, 0)?;

    assert_eq!(schedule.timezone(), "America/New_York");
    assert_eq!(
        schedule.next_after(after)?,
        Some(utc(2026, 7, 2, 13, 0, 0)?)
    );
    assert_eq!(schedule.first_at(after)?, utc(2026, 7, 2, 13, 0, 0)?);
    Ok(())
}

#[test]
fn recurring_local_times_skip_a_dst_gap() -> Result<()> {
    let schedule = parse_recurring("30 2 * * *", "America/New_York")?;
    let before_missing_occurrence = utc(2026, 3, 8, 6, 0, 0)?;

    assert_eq!(
        schedule.next_after(before_missing_occurrence)?,
        Some(utc(2026, 3, 9, 6, 30, 0)?)
    );
    Ok(())
}

#[test]
fn recurring_fold_uses_only_the_first_instant_even_if_queried_between_the_fold_times() -> Result<()>
{
    let schedule = parse_recurring("30 1 * * *", "America/New_York")?;
    let before_first_fold_time = utc(2026, 11, 1, 4, 0, 0)?;
    let between_fold_times = utc(2026, 11, 1, 6, 0, 0)?;

    assert_eq!(
        schedule.next_after(before_first_fold_time)?,
        Some(utc(2026, 11, 1, 5, 30, 0)?)
    );
    assert_eq!(
        schedule.next_after(between_fold_times)?,
        Some(utc(2026, 11, 2, 6, 30, 0)?)
    );
    Ok(())
}

#[test]
fn future_zulu_timestamp_is_an_absolute_instant() -> Result<()> {
    let now = utc(2026, 7, 1, 12, 0, 0)?;
    let schedule = parse_one_off(&tokens(&["20260701T130000Z"]), now, "UTC")?;
    assert_eq!(one_off_at(schedule)?, utc(2026, 7, 1, 13, 0, 0)?);
    Ok(())
}

#[test]
fn local_clock_equal_to_now_selects_the_next_occurrence() -> Result<()> {
    let now = utc(2026, 7, 1, 12, 0, 0)?;
    let schedule = parse_one_off(&tokens(&["12:00:00"]), now, "UTC")?;
    assert_eq!(one_off_at(schedule)?, utc(2026, 7, 2, 12, 0, 0)?);
    Ok(())
}

#[test]
fn dom_wildcard_step_and_weekday_use_vixie_and_semantics() -> Result<()> {
    let schedule = parse_recurring("0 9 */1 * 1", "UTC")?;
    let after = utc(2026, 6, 2, 8, 0, 0)?;

    assert_eq!(schedule.next_after(after)?, Some(utc(2026, 6, 8, 9, 0, 0)?));
    Ok(())
}

#[test]
fn dow_wildcard_step_and_month_day_use_vixie_and_semantics() -> Result<()> {
    let schedule = parse_recurring("0 9 1 * */1", "UTC")?;
    let after = utc(2026, 6, 2, 8, 0, 0)?;

    assert_eq!(schedule.next_after(after)?, Some(utc(2026, 7, 1, 9, 0, 0)?));
    Ok(())
}

#[test]
fn cron_names_cannot_be_adjacent_to_digits_but_lists_ranges_and_steps_remain_valid() -> Result<()> {
    for expression in ["0 9 * JAN2 *", "0 9 * 1JAN *", "0 9 * * MON2"] {
        assert!(
            parse_recurring(expression, "UTC").is_err(),
            "expected malformed cron name to be rejected: {expression}"
        );
    }

    for expression in [
        "0 9 * JAN,MAR MON,WED",
        "0 9 * JAN-MAR MON-FRI",
        "0 9 * */2 MON-FRI",
    ] {
        parse_recurring(expression, "UTC")?;
    }
    Ok(())
}

#[test]
fn future_absolute_timestamps_reject_leap_seconds_and_out_of_range_offsets() -> Result<()> {
    let now = utc(2026, 7, 1, 0, 0, 0)?;

    assert!(parse_one_off(&tokens(&["20260702T235960Z"]), now, "UTC").is_err());
    assert!(parse_one_off(&tokens(&["20260702T120000+2400"]), now, "UTC").is_err());
    Ok(())
}
