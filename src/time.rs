#![allow(dead_code)]

use std::time::Duration as StdDuration;

use anyhow::{Result, anyhow, bail};
use chrono::{DateTime, Duration, LocalResult, NaiveDate, NaiveDateTime, NaiveTime, TimeZone, Utc};
use chrono_tz::Tz;

const DEFAULT_LOOKBACK_MINUTES: i64 = 30;

pub fn default_query_window(end: DateTime<Utc>) -> (DateTime<Utc>, DateTime<Utc>) {
    let start = end - Duration::minutes(DEFAULT_LOOKBACK_MINUTES);
    (start, end)
}

pub fn parse_relative_duration(input: &str) -> Result<Duration> {
    let (value, unit) = split_value_and_unit(input)?;

    let amount: i64 = value
        .parse()
        .map_err(|_| anyhow!("invalid duration value: {value}"))?;
    if amount <= 0 {
        bail!("duration must be greater than zero");
    }

    let unit = unit.to_ascii_lowercase();
    match unit.as_str() {
        "ms" => Ok(Duration::milliseconds(amount)),
        "s" => Ok(Duration::seconds(amount)),
        "m" => Ok(Duration::minutes(amount)),
        "h" => Ok(Duration::hours(amount)),
        "d" => Ok(Duration::days(amount)),
        _ => bail!("unsupported duration unit: {unit}"),
    }
}

pub fn parse_std_duration(input: &str) -> Result<StdDuration> {
    let (value, unit) = split_value_and_unit(input)?;

    let amount: u64 = value
        .parse()
        .map_err(|_| anyhow!("invalid duration value: {value}"))?;
    if amount == 0 {
        return Ok(StdDuration::from_secs(0));
    }

    let unit = unit.to_ascii_lowercase();
    match unit.as_str() {
        "ms" => Ok(StdDuration::from_millis(amount)),
        "s" => Ok(StdDuration::from_secs(amount)),
        "m" => Ok(StdDuration::from_secs(
            amount
                .checked_mul(60)
                .ok_or_else(|| anyhow!("duration is too large"))?,
        )),
        "h" => Ok(StdDuration::from_secs(
            amount
                .checked_mul(60)
                .and_then(|minutes| minutes.checked_mul(60))
                .ok_or_else(|| anyhow!("duration is too large"))?,
        )),
        "d" => Ok(StdDuration::from_secs(
            amount
                .checked_mul(60)
                .and_then(|minutes| minutes.checked_mul(60))
                .and_then(|hours| hours.checked_mul(24))
                .ok_or_else(|| anyhow!("duration is too large"))?,
        )),
        _ => bail!("unsupported duration unit: {unit}"),
    }
}

pub fn parse_time_reference(
    input: &str,
    timezone: Tz,
    now: DateTime<Utc>,
) -> Result<DateTime<Utc>> {
    let normalized = input.trim();
    if normalized.is_empty() {
        bail!("time reference must not be empty");
    }

    if let Ok(parsed) = DateTime::parse_from_rfc3339(normalized) {
        return Ok(parsed.with_timezone(&Utc));
    }

    let lowercase = normalized.to_ascii_lowercase();

    if lowercase == "now" {
        return Ok(now);
    }

    if lowercase == "today" {
        let today = now.with_timezone(&timezone).date_naive();
        return local_datetime_to_utc(timezone, today, NaiveTime::MIN);
    }

    if lowercase == "yesterday" {
        let today = now.with_timezone(&timezone).date_naive();
        let yesterday = today - Duration::days(1);
        return local_datetime_to_utc(timezone, yesterday, NaiveTime::MIN);
    }

    if let Some(since_time) = lowercase.strip_prefix("since ") {
        let parsed_time = parse_time_of_day(since_time)?;
        let local_now = now.with_timezone(&timezone);
        let mut date = local_now.date_naive();
        let mut parsed = local_datetime_to_utc(timezone, date, parsed_time)?;

        if parsed > now {
            date -= Duration::days(1);
            parsed = local_datetime_to_utc(timezone, date, parsed_time)?;
        }

        return Ok(parsed);
    }

    parse_relative_duration(&lowercase).map(|duration| now - duration)
}

pub fn resolve_time_range(
    start: Option<&str>,
    end: Option<&str>,
    timezone: Tz,
    now: DateTime<Utc>,
) -> Result<(DateTime<Utc>, DateTime<Utc>)> {
    let end_time = match end {
        Some(raw) => parse_time_reference(raw, timezone, now)?,
        None => now,
    };

    let start_time = match start {
        Some(raw) => parse_time_reference(raw, timezone, end_time)?,
        None => default_query_window(end_time).0,
    };

    if start_time > end_time {
        bail!("start time must be less than or equal to end time");
    }

    Ok((start_time, end_time))
}

fn split_value_and_unit(input: &str) -> Result<(String, String)> {
    let compact = input
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .collect::<String>();

    if compact.is_empty() {
        bail!("duration must not be empty");
    }

    let split_index = compact
        .char_indices()
        .find_map(|(index, character)| {
            if character.is_ascii_digit() {
                None
            } else {
                Some(index)
            }
        })
        .ok_or_else(|| anyhow!("duration must include a unit suffix"))?;

    let value = compact[..split_index].to_string();
    let unit = compact[split_index..].to_string();
    if value.is_empty() || unit.is_empty() {
        bail!("duration must include a numeric value and a unit suffix");
    }

    Ok((value, unit))
}

fn parse_time_of_day(input: &str) -> Result<NaiveTime> {
    let compact = input.trim().replace(' ', "").to_ascii_lowercase();

    if compact.ends_with("am") || compact.ends_with("pm") {
        let meridiem_index = compact
            .len()
            .checked_sub(2)
            .ok_or_else(|| anyhow!("unsupported time-of-day format: {input}"))?;
        let (time_part, meridiem) = compact.split_at(meridiem_index);

        let (hour_text, minute_text) = if let Some((hour, minute)) = time_part.split_once(':') {
            (hour, minute)
        } else {
            (time_part, "0")
        };

        let hour_12 = hour_text
            .parse::<u32>()
            .map_err(|_| anyhow!("unsupported time-of-day format: {input}"))?;
        let minute = minute_text
            .parse::<u32>()
            .map_err(|_| anyhow!("unsupported time-of-day format: {input}"))?;

        if !(1..=12).contains(&hour_12) || minute > 59 {
            bail!("unsupported time-of-day format: {input}");
        }

        let mut hour_24 = hour_12 % 12;
        if meridiem == "pm" {
            hour_24 += 12;
        }

        return NaiveTime::from_hms_opt(hour_24, minute, 0)
            .ok_or_else(|| anyhow!("unsupported time-of-day format: {input}"));
    }

    for format in ["%H:%M", "%H"] {
        if let Ok(time) = NaiveTime::parse_from_str(&compact, format) {
            return Ok(time);
        }
    }

    bail!("unsupported time-of-day format: {input}")
}

fn local_datetime_to_utc(timezone: Tz, date: NaiveDate, time: NaiveTime) -> Result<DateTime<Utc>> {
    let local_datetime = NaiveDateTime::new(date, time);
    let zoned = match timezone.from_local_datetime(&local_datetime) {
        LocalResult::Single(value) => value,
        LocalResult::Ambiguous(first, _) => first,
        LocalResult::None => bail!("time does not exist in timezone due to DST transition"),
    };

    Ok(zoned.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use std::time::Duration as StdDuration;

    use chrono::{Duration, TimeZone, Utc};
    use chrono_tz::America::New_York;

    use crate::time::{
        default_query_window, parse_relative_duration, parse_std_duration, parse_time_reference,
        resolve_time_range,
    };

    #[test]
    fn default_window_is_30_minutes() {
        let end = Utc::now();
        let (start, returned_end) = default_query_window(end);
        assert_eq!(returned_end, end);
        assert_eq!(end - start, Duration::minutes(30));
    }

    #[test]
    fn parses_relative_duration_units() {
        assert_eq!(
            parse_relative_duration("5m").expect("valid"),
            Duration::minutes(5)
        );
        assert_eq!(
            parse_relative_duration("250ms").expect("valid"),
            Duration::milliseconds(250)
        );
    }

    #[test]
    fn parses_std_duration_units() {
        assert_eq!(
            parse_std_duration("30s").expect("valid"),
            StdDuration::from_secs(30)
        );
        assert_eq!(
            parse_std_duration("2m").expect("valid"),
            StdDuration::from_secs(120)
        );
    }

    #[test]
    fn parses_since_time_reference() {
        let now = Utc
            .with_ymd_and_hms(2026, 2, 18, 20, 0, 0)
            .single()
            .expect("fixed timestamp");

        let parsed = parse_time_reference("since 2pm", New_York, now).expect("parse time");
        let expected = Utc
            .with_ymd_and_hms(2026, 2, 18, 19, 0, 0)
            .single()
            .expect("fixed timestamp");

        assert_eq!(parsed, expected);
    }

    #[test]
    fn resolves_default_window_when_start_and_end_missing() {
        let now = Utc
            .with_ymd_and_hms(2026, 2, 18, 12, 0, 0)
            .single()
            .expect("fixed timestamp");

        let (start, end) = resolve_time_range(None, None, New_York, now).expect("valid range");
        assert_eq!(end, now);
        assert_eq!(end - start, Duration::minutes(30));
    }

    #[test]
    fn rejects_inverted_ranges() {
        let now = Utc
            .with_ymd_and_hms(2026, 2, 18, 12, 0, 0)
            .single()
            .expect("fixed timestamp");

        let error = resolve_time_range(
            Some("2026-02-18T13:00:00Z"),
            Some("2026-02-18T12:00:00Z"),
            New_York,
            now,
        )
        .expect_err("invalid range should fail");

        assert!(
            error
                .to_string()
                .contains("start time must be less than or equal to end time")
        );
    }
}
