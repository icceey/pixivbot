/// Serialize a duration to a canonical string of the form `PT{N}S`
/// (total whole seconds). Two equal durations always produce the same string
/// regardless of how they were originally entered (e.g. `1h` and `3600s`
/// both become `"PT3600S"`). Useful for building stable, comparable
/// subscription keys.
pub fn duration_to_canonical_iso8601(d: chrono::Duration) -> String {
    format!("PT{}S", d.num_seconds())
}

/// Parse a friendly duration string (`1h`, `30m`, `2h30m`, `1d`, etc.).
/// Supports units `s`/`m`/`h`/`d` in any combination. Returns `None` on
/// parse failure or if the string uses ISO 8601 format (not supported).
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
    fn rejects_iso8601_input() {
        // ISO 8601 inputs must no longer be accepted — only friendly format is supported
        assert_eq!(parse_duration("PT1H"), None);
        assert_eq!(parse_duration("PT30M"), None);
        assert_eq!(parse_duration("P1D"), None);
        assert_eq!(parse_duration("PT2H30M"), None);
    }

    #[test]
    fn canonical_iso8601_basic() {
        assert_eq!(
            duration_to_canonical_iso8601(chrono::Duration::hours(1)),
            "PT3600S"
        );
        assert_eq!(
            duration_to_canonical_iso8601(chrono::Duration::minutes(30)),
            "PT1800S"
        );
        assert_eq!(
            duration_to_canonical_iso8601(chrono::Duration::days(1)),
            "PT86400S"
        );
        assert_eq!(
            duration_to_canonical_iso8601(chrono::Duration::seconds(45)),
            "PT45S"
        );
    }
}
