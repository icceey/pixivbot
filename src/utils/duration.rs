/// Parse a duration string accepting either friendly format (`1h`, `30m`, `2h30m`,
/// `1d`) or ISO8601 (`PT1H`, `P1D`). Friendly format supports units `s`/`m`/`h`/`d`
/// in any combination, e.g. `1d2h30m`. Returns `None` on parse failure.
pub fn parse_friendly_or_iso8601(input: &str) -> Option<chrono::Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some(d) = parse_friendly(trimmed) {
        return Some(d);
    }
    iso8601_duration::Duration::parse(trimmed)
        .ok()
        .and_then(|d| d.to_std())
        .and_then(|d| chrono::Duration::from_std(d).ok())
}

fn parse_friendly(input: &str) -> Option<chrono::Duration> {
    let bytes = input.as_bytes();
    let mut total_secs: i64 = 0;
    let mut digits: Option<i64> = None;

    for &b in bytes {
        match b {
            b'0'..=b'9' => {
                let d = (b - b'0') as i64;
                digits = Some(digits.unwrap_or(0).checked_mul(10)?.checked_add(d)?);
            }
            b's' | b'm' | b'h' | b'd' => {
                let n = digits.take()?;
                let mult: i64 = match b {
                    b's' => 1,
                    b'm' => 60,
                    b'h' => 3600,
                    b'd' => 86_400,
                    _ => unreachable!(),
                };
                total_secs = total_secs.checked_add(n.checked_mul(mult)?)?;
            }
            _ => return None,
        }
    }

    if digits.is_some() || total_secs == 0 {
        return None;
    }
    Some(chrono::Duration::seconds(total_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn friendly_basic_units() {
        assert_eq!(
            parse_friendly_or_iso8601("1h"),
            Some(chrono::Duration::hours(1))
        );
        assert_eq!(
            parse_friendly_or_iso8601("30m"),
            Some(chrono::Duration::minutes(30))
        );
        assert_eq!(
            parse_friendly_or_iso8601("1d"),
            Some(chrono::Duration::days(1))
        );
        assert_eq!(
            parse_friendly_or_iso8601("45s"),
            Some(chrono::Duration::seconds(45))
        );
    }

    #[test]
    fn friendly_compound() {
        assert_eq!(
            parse_friendly_or_iso8601("2h30m"),
            Some(chrono::Duration::seconds(2 * 3600 + 30 * 60))
        );
        assert_eq!(
            parse_friendly_or_iso8601("1d2h30m45s"),
            Some(chrono::Duration::seconds(86_400 + 2 * 3600 + 30 * 60 + 45))
        );
    }

    #[test]
    fn iso8601_fallback() {
        assert_eq!(
            parse_friendly_or_iso8601("PT1H"),
            Some(chrono::Duration::hours(1))
        );
        assert_eq!(
            parse_friendly_or_iso8601("PT30M"),
            Some(chrono::Duration::minutes(30))
        );
        assert_eq!(
            parse_friendly_or_iso8601("P1D"),
            Some(chrono::Duration::days(1))
        );
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_friendly_or_iso8601(""), None);
        assert_eq!(parse_friendly_or_iso8601("1"), None);
        assert_eq!(parse_friendly_or_iso8601("h"), None);
        assert_eq!(parse_friendly_or_iso8601("1x"), None);
        assert_eq!(parse_friendly_or_iso8601("1h2"), None);
        assert_eq!(parse_friendly_or_iso8601("abc"), None);
        assert_eq!(parse_friendly_or_iso8601("0s"), None);
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(
            parse_friendly_or_iso8601("  1h  "),
            Some(chrono::Duration::hours(1))
        );
    }
}
