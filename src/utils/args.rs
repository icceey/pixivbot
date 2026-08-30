//! Command argument parsing utilities.
//!
//! Provides utilities for parsing key-value parameters from command arguments.
//! Key-value parameters use the format `key=value` and must appear at the beginning
//! of the argument string.

use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Regex for matching key-value pairs at the beginning of command arguments.
/// Format: `key=value` where key is alphanumeric (including underscore) and
/// value can contain alphanumerics, underscores, hyphens (for negative channel IDs like -1001234567890),
/// or start with @ (for channel usernames like @channelname). The value can also be empty.
/// Matches leading whitespace and captures the key-value pair.
static KV_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^\s*(\w+)=(@?[\w\-]*)(?:\s|$)").unwrap());

/// Result of parsing command arguments with key-value parameters.
#[derive(Debug, Clone)]
pub struct ParsedArgs {
    /// Key-value parameters extracted from the beginning of arguments.
    pub params: HashMap<String, String>,
    /// Remaining arguments after key-value parameters are removed.
    pub remaining: String,
}

impl ParsedArgs {
    /// Get a parameter value by key.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    /// Get a parameter value by multiple possible keys (aliases).
    /// Returns the first matching key's value.
    pub fn get_any(&self, keys: &[&str]) -> Option<&str> {
        for key in keys {
            if let Some(value) = self.params.get(*key) {
                return Some(value.as_str());
            }
        }
        None
    }
}

/// Parse command arguments, extracting key-value parameters from the front.
///
/// Key-value parameters must:
/// 1. Appear at the beginning of the argument string
/// 2. Be in the format `key=value`
/// 3. Be separated by whitespace
///
/// Once a non-key-value argument is encountered, all remaining text is treated
/// as regular arguments.
///
/// # Examples
/// ```ignore
/// let parsed = parse_args("channel=123 456789 +tag1 -tag2");
/// assert_eq!(parsed.params.get("channel"), Some(&"123".to_string()));
/// assert_eq!(parsed.remaining, "456789 +tag1 -tag2");
///
/// let parsed = parse_args("ch=-123456 789");
/// assert_eq!(parsed.params.get("ch"), Some(&"-123456".to_string()));
/// ```
pub fn parse_args(args: &str) -> ParsedArgs {
    let mut params = HashMap::new();
    let mut input = args;

    // Use regex to match key-value pairs from the beginning
    while let Some(caps) = KV_REGEX.captures(input) {
        let key = caps.get(1).unwrap().as_str().to_lowercase();
        let value = caps.get(2).unwrap().as_str().to_string();
        params.insert(key, value);

        // Move past the matched portion
        let match_end = caps.get(0).unwrap().end();
        input = &input[match_end..];
    }

    // Trim any remaining leading whitespace from the remaining input
    let remaining = input.trim_start().to_string();

    ParsedArgs { params, remaining }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args_get_any() {
        let parsed = parse_args("ch=123 789");
        assert_eq!(parsed.get_any(&["channel", "ch"]), Some("123"));

        let parsed = parse_args("channel=456 789");
        assert_eq!(parsed.get_any(&["channel", "ch"]), Some("456"));
    }

    #[test]
    fn test_parse_args_negative_number_value() {
        let parsed = parse_args("ch=-1001234567890 789");
        assert_eq!(
            parsed.params.get("ch").map(String::as_str),
            Some("-1001234567890")
        );
        assert_eq!(parsed.remaining, "789");
    }

    #[test]
    fn test_parse_args_username_value() {
        let parsed = parse_args("ch=@mychannel 789");
        assert_eq!(
            parsed.params.get("ch").map(String::as_str),
            Some("@mychannel")
        );
        assert_eq!(parsed.remaining, "789");
    }

    #[test]
    fn test_parse_args_stops_at_non_kv() {
        // Tags like +tag should stop kv parsing
        let parsed = parse_args("channel=123 +tag val=should_not_parse");
        assert_eq!(
            parsed.params.get("channel").map(String::as_str),
            Some("123")
        );
        assert_eq!(parsed.params.get("val"), None);
        assert_eq!(parsed.remaining, "+tag val=should_not_parse");
    }
}
