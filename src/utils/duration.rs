/// Serialize a duration for internal task keys as total whole seconds.
/// Two equal durations always produce the same string regardless of how they
/// were originally entered (e.g. `1h` and `3600s` both become `"3600s"`).
pub fn duration_to_key(d: chrono::Duration) -> String {
    format!("{}s", d.num_seconds())
}

pub fn parse_duration_key(input: &str) -> Option<chrono::Duration> {
    let seconds = input.strip_suffix('s')?;
    if seconds.is_empty()
        || (seconds.len() > 1 && seconds.starts_with('0'))
        || !seconds.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let seconds: i64 = seconds.parse().ok()?;
    if seconds <= 0 {
        return None;
    }
    Some(chrono::Duration::seconds(seconds))
}

/// Supports units `s`/`m`/`h`/`d` in any combination. Returns `None` on
/// parse failure.
pub fn parse_duration(input: &str) -> Option<chrono::Duration> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    parse_friendly(trimmed)
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
        assert_eq!(parse_duration("1h"), Some(chrono::Duration::hours(1)));
        assert_eq!(parse_duration("30m"), Some(chrono::Duration::minutes(30)));
        assert_eq!(parse_duration("1d"), Some(chrono::Duration::days(1)));
        assert_eq!(parse_duration("45s"), Some(chrono::Duration::seconds(45)));
    }

    #[test]
    fn friendly_compound() {
        assert_eq!(
            parse_duration("2h30m"),
            Some(chrono::Duration::seconds(2 * 3600 + 30 * 60))
        );
        assert_eq!(
            parse_duration("1d2h30m45s"),
            Some(chrono::Duration::seconds(86_400 + 2 * 3600 + 30 * 60 + 45))
        );
    }

    #[test]
    fn rejects_invalid() {
        assert_eq!(parse_duration(""), None);
        assert_eq!(parse_duration("1"), None);
        assert_eq!(parse_duration("h"), None);
        assert_eq!(parse_duration("1x"), None);
        assert_eq!(parse_duration("1h2"), None);
        assert_eq!(parse_duration("abc"), None);
        assert_eq!(parse_duration("0s"), None);
    }

    #[test]
    fn whitespace_trimmed() {
        assert_eq!(parse_duration("  1h  "), Some(chrono::Duration::hours(1)));
    }

    #[test]
    fn duration_key_basic() {
        assert_eq!(duration_to_key(chrono::Duration::hours(1)), "3600s");
        assert_eq!(duration_to_key(chrono::Duration::minutes(30)), "1800s");
        assert_eq!(duration_to_key(chrono::Duration::days(1)), "86400s");
        assert_eq!(duration_to_key(chrono::Duration::seconds(45)), "45s");
    }

    #[test]
    fn duration_key_roundtrip() {
        for (input, expected_secs) in &[("1h", 3600i64), ("30m", 1800), ("1d", 86400)] {
            let d = parse_duration(input).unwrap();
            let stored = duration_to_key(d);
            let recovered = parse_duration_key(&stored).unwrap();
            assert_eq!(
                recovered.num_seconds(),
                *expected_secs,
                "round-trip failed for input {input}"
            );
        }
    }

    #[test]
    fn parse_duration_key_requires_seconds_suffix() {
        assert_eq!(
            parse_duration_key("3600s"),
            Some(chrono::Duration::hours(1))
        );
        assert_eq!(parse_duration_key("1h"), None);
        assert_eq!(parse_duration_key("60m"), None);
        assert_eq!(parse_duration_key("0s"), None);
        assert_eq!(parse_duration_key("060s"), None);
        assert_eq!(parse_duration_key("abc"), None);
    }
}
