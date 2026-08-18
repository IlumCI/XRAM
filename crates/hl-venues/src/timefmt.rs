//! RFC 3339 parsing, without a datetime dependency.
//!
//! Every API we read stamps things like `2026-08-18T09:27:00Z`, and all we need from it
//! is epoch milliseconds.

/// Parse `YYYY-MM-DDTHH:MM:SS[.fff]Z` into milliseconds since the Unix epoch.
///
/// Only the UTC (`Z`) form is accepted, because that is what these APIs emit and
/// silently mis-parsing an offset would corrupt every latency we measure.
pub fn parse_rfc3339_utc(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b' ') {
        return None;
    }
    if !s.ends_with('Z') {
        return None;
    }
    let num = |a: usize, z: usize| s.get(a..z)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, sec) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if !(1..=12).contains(&mo) || !(1..=31).contains(&d) || h > 23 || mi > 59 || sec > 60 {
        return None;
    }
    let days = days_from_civil(y, mo as u32, d as u32);
    let secs = days * 86_400 + h * 3600 + mi * 60 + sec;
    (secs >= 0).then_some(secs as u64 * 1000)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_known_instants() {
        assert_eq!(parse_rfc3339_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_rfc3339_utc("2000-01-01T00:00:00Z"), Some(946_684_800_000));
        assert_eq!(parse_rfc3339_utc("2026-08-18T09:27:00Z"), Some(1_787_045_220_000));
    }

    #[test]
    fn handles_leap_days_and_fractional_seconds() {
        assert_eq!(parse_rfc3339_utc("2024-02-29T12:00:00Z"), Some(1_709_208_000_000));
        assert_eq!(
            parse_rfc3339_utc("2026-08-18T09:27:00.123Z"),
            parse_rfc3339_utc("2026-08-18T09:27:00Z")
        );
    }

    #[test]
    fn rejects_what_it_cannot_faithfully_read() {
        // An offset form would parse to the wrong instant, so it is refused outright.
        assert_eq!(parse_rfc3339_utc("2026-08-18T09:27:00+02:00"), None);
        assert_eq!(parse_rfc3339_utc("not a date"), None);
        assert_eq!(parse_rfc3339_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_rfc3339_utc(""), None);
    }

    #[test]
    fn ordering_is_preserved() {
        let a = parse_rfc3339_utc("2026-08-18T09:00:00Z").unwrap();
        let b = parse_rfc3339_utc("2026-08-18T09:00:01Z").unwrap();
        assert_eq!(b - a, 1000);
    }
}
