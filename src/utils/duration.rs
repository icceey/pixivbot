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
    fn friendly_durations_encode_and_roundtrip_as_seconds() {
        for (input, seconds, key) in [
            ("1h", 3600, "3600s"),
            ("30m", 1800, "1800s"),
            ("1d", 86400, "86400s"),
            ("45s", 45, "45s"),
            ("2h30m", 9000, "9000s"),
            ("1d2h30m45s", 95445, "95445s"),
            ("  1h  ", 3600, "3600s"),
        ] {
            let duration = chrono::Duration::seconds(seconds);
            assert_eq!(parse_duration(input), Some(duration), "{input}");
            assert_eq!(duration_to_key(duration), key);
            assert_eq!(parse_duration_key(key), Some(duration), "{key}");
        }
    }

    #[test]
    fn friendly_durations_reject_malformed_or_zero_values() {
        for input in ["", "1", "h", "1x", "1h2", "abc", "0s"] {
            assert_eq!(parse_duration(input), None, "{input}");
        }
    }

    #[test]
    fn stored_duration_keys_require_canonical_seconds() {
        for key in ["1h", "60m", "0s", "060s", "abc"] {
            assert_eq!(parse_duration_key(key), None, "{key}");
        }
    }
}
