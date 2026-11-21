//! Reading the `Retry-After` header that comes with a failed response.

use std::time::{Duration, SystemTime};

/// How long a `Retry-After` header asks the caller to wait, as of `now`.
///
/// The header holds a number of seconds or an HTTP date. This reads it as
/// llm-deepseek does (`llm-deepseek/src/transport.ts:36-42`): the seconds
/// may have a decimal part, and a wait that is zero, already past, too long
/// to count, or unreadable gives `None`, so the caller falls back to its
/// own backoff.
///
/// `now` is a parameter so that tests can pin the clock. Callers pass
/// [`SystemTime::now`].
pub fn parse_retry_after(value: &str, now: SystemTime) -> Option<Duration> {
    let value = value.trim();
    let wait = if is_seconds(value) {
        // Too many digits parse as infinity, which `try_from_secs_f64`
        // refuses.
        let seconds: f64 = value.parse().ok()?;
        Duration::try_from_secs_f64(seconds).ok()?
    } else {
        // `duration_since` fails for a date already past.
        let date = httpdate::parse_http_date(value).ok()?;
        date.duration_since(now).ok()?
    };
    (!wait.is_zero()).then_some(wait)
}

/// Digits, then optionally a point and more digits, as in `7` or `1.5`.
/// Rust's float parser would also take `-1`, `1e3` and `inf`; the header
/// allows none of them.
fn is_seconds(value: &str) -> bool {
    let digits = |part: &str| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit());
    match value.split_once('.') {
        Some((whole, fraction)) => digits(whole) && digits(fraction),
        None => digits(value),
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use super::parse_retry_after;

    /// 2027-01-15 08:00:00 UTC, a time to read dates against.
    fn now() -> SystemTime {
        SystemTime::UNIX_EPOCH + Duration::from_secs(1_800_000_000)
    }

    fn read(value: &str) -> Option<Duration> {
        parse_retry_after(value, now())
    }

    #[test]
    fn whole_seconds() {
        assert_eq!(read("7"), Some(Duration::from_secs(7)));
        assert_eq!(read(" 7 "), Some(Duration::from_secs(7)));
    }

    #[test]
    fn a_decimal_number_of_seconds() {
        assert_eq!(read("1.5"), Some(Duration::from_millis(1500)));
    }

    #[test]
    fn zero_means_no_wait() {
        assert_eq!(read("0"), None);
        assert_eq!(read("0.0"), None);
    }

    /// dsh's "parses a future Retry-After HTTP date"
    /// (`llm-deepseek/tests/runtime.spec.ts`), three seconds ahead.
    #[test]
    fn a_future_http_date() {
        let date = httpdate::fmt_http_date(now() + Duration::from_secs(3));
        assert_eq!(read(&date), Some(Duration::from_secs(3)));
    }

    #[test]
    fn a_past_http_date_means_no_wait() {
        let date = httpdate::fmt_http_date(SystemTime::UNIX_EPOCH);
        assert_eq!(read(&date), None);
        assert_eq!(read(&httpdate::fmt_http_date(now())), None);
    }

    /// dsh's "omits zero, non-finite, invalid, and past Retry-After values",
    /// and the forms Rust would parse but the header doesn't allow.
    #[test]
    fn garbage_means_no_wait() {
        for value in [
            "",
            "not-a-date",
            &"9".repeat(400),
            "-1",
            "1e3",
            "inf",
            "NaN",
            "1.",
            ".5",
        ] {
            assert_eq!(read(value), None, "{value}");
        }
    }
}
