//! Command argument parsing utilities.
//!
//! Provides utilities for parsing key-value parameters from command arguments.
//! Key-value parameters use the format `key=value` and must appear at the beginning
//! of the argument string.

use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;

/// Regex for matching key-value pairs in command arguments.
/// Format: `key=value` where key is alphanumeric (including underscore) and
/// value is alphanumeric or digits (including underscore, dash, and can be empty).
static KV_REGEX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"^(\w+)=([\w\-]*)$").unwrap());

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
    #[allow(dead_code)]
    pub fn get(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(|s| s.as_str())
    }

    /// Check if a parameter exists (even if empty).
    #[allow(dead_code)]
    pub fn has(&self, key: &str) -> bool {
        self.params.contains_key(key)
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
    let parts: Vec<&str> = args.split_whitespace().collect();
    let mut remaining_start = 0;

    for (i, part) in parts.iter().enumerate() {
        if let Some(caps) = KV_REGEX.captures(part) {
            let key = caps.get(1).unwrap().as_str().to_lowercase();
            let value = caps.get(2).unwrap().as_str().to_string();
            params.insert(key, value);
            remaining_start = i + 1;
        } else {
            // Stop parsing key-value pairs once we hit a non-matching argument
            break;
        }
    }

    let remaining = if remaining_start < parts.len() {
        parts[remaining_start..].join(" ")
    } else {
        String::new()
    };

    ParsedArgs { params, remaining }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_args_empty() {
        let parsed = parse_args("");
        assert!(parsed.params.is_empty());
        assert_eq!(parsed.remaining, "");
    }

    #[test]
    fn test_parse_args_no_kv() {
        let parsed = parse_args("123456 +tag1 -tag2");
        assert!(parsed.params.is_empty());
        assert_eq!(parsed.remaining, "123456 +tag1 -tag2");
    }

    #[test]
    fn test_parse_args_single_kv() {
        let parsed = parse_args("channel=123456 789 +tag");
        assert_eq!(parsed.get("channel"), Some("123456"));
        assert_eq!(parsed.remaining, "789 +tag");
    }

    #[test]
    fn test_parse_args_multiple_kv() {
        let parsed = parse_args("ch=123 other=val 789 +tag");
        assert_eq!(parsed.get("ch"), Some("123"));
        assert_eq!(parsed.get("other"), Some("val"));
        assert_eq!(parsed.remaining, "789 +tag");
    }

    #[test]
    fn test_parse_args_case_insensitive_key() {
        let parsed = parse_args("Channel=123 789");
        assert_eq!(parsed.get("channel"), Some("123"));
        assert_eq!(parsed.remaining, "789");
    }

    #[test]
    fn test_parse_args_empty_value() {
        let parsed = parse_args("channel= 789");
        assert_eq!(parsed.get("channel"), Some(""));
        assert_eq!(parsed.remaining, "789");
    }

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
        assert_eq!(parsed.get("ch"), Some("-1001234567890"));
        assert_eq!(parsed.remaining, "789");
    }

    #[test]
    fn test_parse_args_only_kv() {
        let parsed = parse_args("channel=123");
        assert_eq!(parsed.get("channel"), Some("123"));
        assert_eq!(parsed.remaining, "");
    }

    #[test]
    fn test_parse_args_stops_at_non_kv() {
        // Tags like +tag should stop kv parsing
        let parsed = parse_args("channel=123 +tag val=should_not_parse");
        assert_eq!(parsed.get("channel"), Some("123"));
        assert_eq!(parsed.get("val"), None);
        assert_eq!(parsed.remaining, "+tag val=should_not_parse");
    }
}
