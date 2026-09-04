//! RFC 9110 `Retry-After` delay-seconds and HTTP-date parsing.

use std::time::{Duration, SystemTime};

/// A parsed `Retry-After` value: delay-seconds or an absolute HTTP-date.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetryAfter {
    /// Integer delay-seconds, including zero.
    DelaySeconds(u64),
    /// HTTP-date instant (IMF-fixdate, RFC 850, or asctime).
    HttpDate(SystemTime),
}

impl RetryAfter {
    /// Wait from `now`. A past HTTP-date is [`Duration::ZERO`].
    #[must_use]
    pub fn wait_from(self, now: SystemTime) -> Duration {
        match self {
            Self::DelaySeconds(seconds) => Duration::from_secs(seconds),
            Self::HttpDate(when) => when.duration_since(now).unwrap_or(Duration::ZERO),
        }
    }

    /// Positive finite provider delay in milliseconds, matching TypeScript
    /// `providerRetryAfterMs`: zero, past, and overflowing waits are omitted.
    #[must_use]
    pub fn provider_retry_after_ms(self, now: SystemTime) -> Option<u64> {
        match self {
            Self::DelaySeconds(0) => None,
            Self::DelaySeconds(seconds) => seconds.checked_mul(1_000),
            Self::HttpDate(when) => {
                let wait = when.duration_since(now).ok()?;
                let millis = u64::try_from(wait.as_millis()).ok()?;
                (millis > 0).then_some(millis)
            }
        }
    }
}

/// Parse RFC 9110 `Retry-After`: all-digit delay-seconds, else HTTP-date.
#[must_use]
pub fn parse_retry_after(value: &str) -> Option<RetryAfter> {
    if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) {
        return value.parse::<u64>().ok().map(RetryAfter::DelaySeconds);
    }
    parse_http_date(value).map(RetryAfter::HttpDate)
}

/// Positive provider `Retry-After` delay in milliseconds from `now`.
#[must_use]
pub fn provider_retry_after_ms(value: &str, now: SystemTime) -> Option<u64> {
    parse_retry_after(value)?.provider_retry_after_ms(now)
}

fn parse_http_date(value: &str) -> Option<SystemTime> {
    parse_imf_fixdate(value)
        .or_else(|| parse_rfc850_date(value))
        .or_else(|| parse_asctime_date(value))
}

fn parse_imf_fixdate(value: &str) -> Option<SystemTime> {
    // Wed, 21 Oct 2015 07:28:00 GMT
    let rest = value.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let day = parts.next()?.parse::<u32>().ok()?;
    let month = month_from_name(parts.next()?)?;
    let year = parts.next()?.parse::<i32>().ok()?;
    let time = parts.next()?;
    let zone = parts.next()?;
    if parts.next().is_some() || !zone.eq_ignore_ascii_case("GMT") {
        return None;
    }
    let (hour, minute, second) = parse_hms(time)?;
    civil_to_system_time(year, month, day, hour, minute, second)
}

fn parse_rfc850_date(value: &str) -> Option<SystemTime> {
    // Sunday, 06-Nov-94 08:49:37 GMT
    let rest = value.split_once(", ")?.1;
    let mut parts = rest.split(' ');
    let date = parts.next()?;
    let time = parts.next()?;
    let zone = parts.next()?;
    if parts.next().is_some() || !zone.eq_ignore_ascii_case("GMT") {
        return None;
    }
    let mut date_parts = date.split('-');
    let day = date_parts.next()?.parse::<u32>().ok()?;
    let month = month_from_name(date_parts.next()?)?;
    let year_token = date_parts.next()?;
    if date_parts.next().is_some() {
        return None;
    }
    let year = match year_token.len() {
        2 => 1900 + year_token.parse::<i32>().ok()?,
        4 => year_token.parse::<i32>().ok()?,
        _ => return None,
    };
    let (hour, minute, second) = parse_hms(time)?;
    civil_to_system_time(year, month, day, hour, minute, second)
}

fn parse_asctime_date(value: &str) -> Option<SystemTime> {
    // Sun Nov  6 08:49:37 1994
    let mut parts = value.split_whitespace();
    let _weekday = parts.next()?;
    let month = month_from_name(parts.next()?)?;
    let day = parts.next()?.parse::<u32>().ok()?;
    let time = parts.next()?;
    let year = parts.next()?.parse::<i32>().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let (hour, minute, second) = parse_hms(time)?;
    civil_to_system_time(year, month, day, hour, minute, second)
}

fn parse_hms(value: &str) -> Option<(u32, u32, u32)> {
    let mut parts = value.split(':');
    let hour = parts.next()?.parse::<u32>().ok()?;
    let minute = parts.next()?.parse::<u32>().ok()?;
    let second = parts.next()?.parse::<u32>().ok()?;
    if parts.next().is_some() || hour > 23 || minute > 59 || second > 60 {
        return None;
    }
    Some((hour, minute, second.min(59)))
}

fn month_from_name(name: &str) -> Option<u32> {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    MONTHS
        .iter()
        .position(|month| month.eq_ignore_ascii_case(name))
        .map(|index| u32::try_from(index + 1).expect("month index 1..=12"))
}

fn civil_to_system_time(
    year: i32,
    month: u32,
    day: u32,
    hour: u32,
    minute: u32,
    second: u32,
) -> Option<SystemTime> {
    if !(1..=12).contains(&month) || day == 0 || day > 31 {
        return None;
    }
    let days = i64::from(days_from_civil(year, month, day));
    let unix = days
        .checked_mul(86_400)?
        .checked_add(i64::from(hour) * 3_600)?
        .checked_add(i64::from(minute) * 60)?
        .checked_add(i64::from(second))?;
    if unix >= 0 {
        Some(SystemTime::UNIX_EPOCH + Duration::from_secs(unix as u64))
    } else {
        Some(SystemTime::UNIX_EPOCH - Duration::from_secs(unix.unsigned_abs()))
    }
}

/// Days since 1970-01-01 (Howard Hinnant civil calendar).
fn days_from_civil(mut year: i32, month: u32, day: u32) -> i32 {
    year -= i32::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = (year - era * 400) as u32;
    let shifted_month = if month > 2 { month - 3 } else { month + 9 };
    let doy = (153 * shifted_month + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe as i32 - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_seconds_include_zero_and_reject_non_digits() {
        assert_eq!(parse_retry_after("0"), Some(RetryAfter::DelaySeconds(0)));
        assert_eq!(parse_retry_after("2"), Some(RetryAfter::DelaySeconds(2)));
        assert_eq!(parse_retry_after("02"), Some(RetryAfter::DelaySeconds(2)));
        assert_eq!(parse_retry_after("+2"), None);
        assert_eq!(parse_retry_after("2.0"), None);
        assert_eq!(parse_retry_after(""), None);
        assert_eq!(parse_retry_after(&"9".repeat(400)), None);
    }

    #[test]
    fn imf_fixdate_is_unix_1445412480() {
        let parsed = parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT");
        let RetryAfter::HttpDate(when) = parsed.expect("IMF-fixdate") else {
            panic!("expected HTTP-date");
        };
        assert_eq!(
            when.duration_since(SystemTime::UNIX_EPOCH)
                .expect("after epoch")
                .as_secs(),
            1_445_412_480
        );
    }

    #[test]
    fn rfc850_and_asctime_parse() {
        assert!(matches!(
            parse_retry_after("Sunday, 06-Nov-94 08:49:37 GMT"),
            Some(RetryAfter::HttpDate(_))
        ));
        assert!(matches!(
            parse_retry_after("Sun Nov  6 08:49:37 1994"),
            Some(RetryAfter::HttpDate(_))
        ));
        assert_eq!(parse_retry_after("not-a-date"), None);
    }

    #[test]
    fn provider_omits_zero_past_and_invalid() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        assert_eq!(provider_retry_after_ms("0", now), None);
        assert_eq!(provider_retry_after_ms("2", now), Some(2_000));
        assert_eq!(
            provider_retry_after_ms("Wed, 21 Oct 2015 07:28:00 GMT", now),
            None
        );
        let future = now + Duration::from_secs(3);
        let http_date = httpdate_gmt(future);
        assert_eq!(provider_retry_after_ms(&http_date, now), Some(3_000));
        assert_eq!(provider_retry_after_ms("not-a-date", now), None);
    }

    #[test]
    fn wait_from_past_http_date_is_zero() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000);
        let parsed = parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT").unwrap();
        assert_eq!(parsed.wait_from(now), Duration::ZERO);
        assert_eq!(
            parse_retry_after("2").unwrap().wait_from(now),
            Duration::from_secs(2)
        );
    }

    fn httpdate_gmt(when: SystemTime) -> String {
        let secs = when
            .duration_since(SystemTime::UNIX_EPOCH)
            .expect("after epoch")
            .as_secs();
        // 2015-10-21 07:28:00Z = 1445412480; format via known civil conversion tests.
        let days = (secs / 86_400) as i32;
        let tod = secs % 86_400;
        let (year, month, day) = civil_from_days(days);
        let hour = tod / 3_600;
        let minute = (tod % 3_600) / 60;
        let second = tod % 60;
        format!(
            "Thu, {day:02} {} {year} {hour:02}:{minute:02}:{second:02} GMT",
            MONTH_NAMES[(month - 1) as usize]
        )
    }

    const MONTH_NAMES: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];

    fn civil_from_days(days: i32) -> (i32, u32, u32) {
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = (z - era * 146_097) as u32;
        let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe as i32 + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        (y + i32::from(m <= 2), m, d)
    }
}
