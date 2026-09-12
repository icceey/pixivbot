use regex::Regex;
use std::sync::LazyLock;

// Keep the command syntax limited to integer s/m/h/d components.
static DURATION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\A(?:[0-9]+[smhd])+\z").unwrap());

/// Serialize a duration for internal task keys as total whole seconds.
/// Two equal durations always produce the same string regardless of how they
/// were originally entered (e.g. `1h` and `3600s` both become `"3600s"`).
pub fn duration_to_key(d: chrono::Duration) -> String {
    format!("{}s", d.num_seconds())
}

pub fn parse_duration_key(input: &str) -> Option<chrono::Duration> {
    let value = input.strip_suffix('s')?;
    let seconds: i64 = value.parse().ok()?;
    if seconds <= 0 || seconds.to_string() != value {
        return None;
    }
    Some(chrono::Duration::seconds(seconds))
}

/// Supports units `s`/`m`/`h`/`d` in any combination. Returns `None` on
/// parse failure.
pub fn parse_duration(input: &str) -> Option<chrono::Duration> {
    let input = input.trim();
    if !DURATION_PATTERN.is_match(input) {
        return None;
    }
    let duration = humantime::parse_duration(input).ok()?;
    let seconds = i64::try_from(duration.as_secs()).ok()?;
    (seconds > 0).then(|| chrono::Duration::seconds(seconds))
}
