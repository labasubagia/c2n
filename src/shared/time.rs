//! Timezone and timestamp helpers.

use std::str::FromStr;

use chrono::{
    DateTime, Duration, FixedOffset, NaiveDate, NaiveDateTime, SecondsFormat, TimeZone, Utc,
};
use chrono_tz::Tz;

#[derive(Debug, thiserror::Error)]
pub enum TimeError {
    #[error("invalid timezone: {0}")]
    InvalidTimezone(String),
    #[error("invalid timestamp: {0}")]
    InvalidTimestamp(String),
    #[error("timestamp is out of range: {0}")]
    OutOfRange(String),
    #[error("local time does not exist in timezone {0}")]
    InvalidLocalTime(String),
}

/// Resolve an IANA timezone name, falling back to the system timezone and then UTC.
pub fn resolve_timezone(name: Option<&str>) -> Result<Tz, TimeError> {
    if let Some(name) = name.filter(|value| !value.trim().is_empty()) {
        return Tz::from_str(name.trim()).map_err(|_| TimeError::InvalidTimezone(name.to_owned()));
    }

    if let Ok(system) = iana_time_zone::get_timezone()
        && let Ok(zone) = Tz::from_str(&system)
    {
        return Ok(zone);
    }

    Ok(Tz::UTC)
}

/// Format a duration using the same compact style as the Python tool.
pub fn format_duration(total_seconds: f64) -> String {
    let total = total_seconds.max(0.0) as i64;
    let hours = total / 3600;
    let minutes = (total % 3600) / 60;
    let seconds = total % 60;

    let mut parts = Vec::new();
    if hours > 0 {
        parts.push(format!("{hours}h"));
        if minutes > 0 {
            parts.push(format!("{minutes}m"));
        }
    } else {
        if minutes > 0 {
            parts.push(format!("{minutes}m"));
        }
        if seconds > 0 || parts.is_empty() {
            parts.push(format!("{seconds}s"));
        }
    }
    parts.join(" ")
}

/// Format a UTC instant consistently for storage and SQL comparisons.
pub fn utc_iso(timestamp: DateTime<Utc>) -> String {
    timestamp.to_rfc3339_opts(SecondsFormat::Micros, true)
}

/// Current UTC time formatted with microsecond precision.
pub fn now_utc_iso() -> String {
    utc_iso(Utc::now())
}

/// Convert a Unix timestamp in seconds to an RFC 3339 UTC string.
pub fn timestamp_to_utc_iso(seconds: f64) -> Result<String, TimeError> {
    if !seconds.is_finite() {
        return Err(TimeError::InvalidTimestamp(seconds.to_string()));
    }
    let micros = (seconds * 1_000_000.0).round();
    if micros < i64::MIN as f64 || micros > i64::MAX as f64 {
        return Err(TimeError::OutOfRange(seconds.to_string()));
    }
    let timestamp = DateTime::<Utc>::from_timestamp_micros(micros as i64)
        .ok_or_else(|| TimeError::OutOfRange(seconds.to_string()))?;
    Ok(utc_iso(timestamp))
}

/// Parse an RFC 3339 timestamp and normalize it to UTC.
pub fn parse_utc(value: &str) -> Result<DateTime<Utc>, TimeError> {
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(parsed) = DateTime::<FixedOffset>::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f%:z") {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(naive) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f") {
        return Ok(Utc.from_utc_datetime(&naive));
    }
    Err(TimeError::InvalidTimestamp(value.to_owned()))
}

/// Return the UTC start and end of a local calendar day.
///
/// The end is the next local midnight, not simply the start plus 24 hours, so
/// daylight-saving transitions produce 23-hour or 25-hour days.
pub fn local_day_bounds_utc(
    date: NaiveDate,
    timezone: Tz,
) -> Result<(DateTime<Utc>, DateTime<Utc>), TimeError> {
    let start_naive = date
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| TimeError::InvalidLocalTime(date.to_string()))?;
    let end_naive = (date + Duration::days(1))
        .and_hms_opt(0, 0, 0)
        .ok_or_else(|| TimeError::InvalidLocalTime(date.to_string()))?;

    let start = timezone
        .from_local_datetime(&start_naive)
        .earliest()
        .ok_or_else(|| TimeError::InvalidLocalTime(date.to_string()))?
        .with_timezone(&Utc);
    let end = timezone
        .from_local_datetime(&end_naive)
        .earliest()
        .ok_or_else(|| TimeError::InvalidLocalTime(date.to_string()))?
        .with_timezone(&Utc);
    Ok((start, end))
}

/// Format an instant in a timezone.
pub fn format_local(timestamp: DateTime<Utc>, timezone: Tz, format: &str) -> String {
    timestamp
        .with_timezone(&timezone)
        .format(format)
        .to_string()
}

/// Format an instant for user-facing output.
pub fn format_local_datetime(timestamp: DateTime<Utc>, timezone: Tz) -> String {
    format_local(timestamp, timezone, "%Y-%m-%d %H:%M:%S")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_durations_like_python() {
        assert_eq!(format_duration(0.0), "0s");
        assert_eq!(format_duration(59.9), "59s");
        assert_eq!(format_duration(60.0), "1m");
        assert_eq!(format_duration(61.0), "1m 1s");
        assert_eq!(format_duration(3600.0), "1h");
        assert_eq!(format_duration(3661.0), "1h 1m");
        assert_eq!(format_duration(-10.0), "0s");
    }

    #[test]
    fn parses_timestamps_with_microseconds() {
        let parsed = parse_utc("2026-01-02T03:04:05.123456+00:00").unwrap();
        assert_eq!(
            parsed.to_rfc3339_opts(SecondsFormat::Micros, true),
            "2026-01-02T03:04:05.123456Z"
        );
    }

    #[test]
    fn converts_unix_timestamp() {
        assert_eq!(
            timestamp_to_utc_iso(0.0).unwrap(),
            "1970-01-01T00:00:00.000000Z"
        );
    }

    #[test]
    fn local_day_bounds_follow_dst_transitions() {
        let zone: Tz = "America/New_York".parse().unwrap();
        let (start, end) =
            local_day_bounds_utc(NaiveDate::from_ymd_opt(2026, 3, 8).unwrap(), zone).unwrap();
        assert_eq!((end - start).num_hours(), 23);

        let (start, end) =
            local_day_bounds_utc(NaiveDate::from_ymd_opt(2026, 11, 1).unwrap(), zone).unwrap();
        assert_eq!((end - start).num_hours(), 25);
    }
}
