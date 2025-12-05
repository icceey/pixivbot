//! Tag filter utilities for filtering Pixiv illusts.
//!
//! This module provides a unified `TagFilter` type that handles:
//! - Parsing from command arguments (e.g., `/sub 123 +tag1 -tag2`)
//! - Parsing from database JSON
//! - Converting to JSON for database storage
//! - Formatting for display in Telegram messages
//! - Filtering Illust objects

use crate::pixiv_client::Illust;
use crate::utils::tag;
use sea_orm::FromJsonQueryResult;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::ops::{Deref, DerefMut};
use teloxide::utils::markdown;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
#[serde(transparent)]
pub struct Tags(pub Vec<String>);

impl Deref for Tags {
    type Target = Vec<String>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl DerefMut for Tags {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Vec<String>> for Tags {
    fn from(tags: Vec<String>) -> Self {
        Tags(tags)
    }
}

/// A unified tag filter for include/exclude filtering.
///
/// Tags are stored in their original form for display purposes.
/// Normalization is done on-the-fly during matching for case-insensitive comparison.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, FromJsonQueryResult)]
pub struct TagFilter {
    #[serde(default)]
    include: Vec<String>,
    #[serde(default)]
    exclude: Vec<String>,
}

impl TagFilter {
    /// Parse filter tags from command arguments.
    ///
    /// Format: `+tag1 -tag2 tag3` (tags without prefix are treated as include)
    ///
    /// # Example
    /// ```ignore
    /// let filter = TagFilter::parse_from_args(&["+原神", "-R-18", "cute"]);
    /// // include: ["原神", "cute"], exclude: ["R-18"]
    /// ```
    pub fn parse_from_args(args: &[&str]) -> Self {
        let mut include = Vec::new();
        let mut exclude = Vec::new();

        for arg in args {
            if let Some(stripped) = arg.strip_prefix('+') {
                if !stripped.is_empty() {
                    include.push(stripped.to_string());
                }
            } else if let Some(stripped) = arg.strip_prefix('-') {
                if !stripped.is_empty() {
                    exclude.push(stripped.to_string());
                }
            } else if !arg.is_empty() {
                include.push(arg.to_string());
            }
        }

        Self { include, exclude }
    }

    /// Create a TagFilter from chat excluded_tags (exclude only).
    pub fn from_excluded_tags(excluded_tags: &Tags) -> Self {
        if excluded_tags.is_empty() {
            return Self::default();
        }

        Self {
            include: Vec::new(),
            exclude: excluded_tags.0.clone(),
        }
    }

    /// Create a TagFilter from chat excluded_tags JSON (exclude only).
    ///
    /// Expected JSON format: `["tag1", "tag2", "tag3"]`
    #[allow(dead_code)]
    pub fn from_excluded_json(excluded_tags: &Option<Value>) -> Self {
        let Some(tags) = excluded_tags else {
            return Self::default();
        };

        let exclude: Vec<String> = serde_json::from_value(tags.clone()).unwrap_or_default();

        Self {
            include: Vec::new(),
            exclude,
        }
    }

    /// Check if this filter has any restrictions.
    pub fn is_empty(&self) -> bool {
        self.include.is_empty() && self.exclude.is_empty()
    }

    /// Convert to JSON Value for database storage.
    ///
    /// Returns `None` if the filter is empty.
    pub fn to_json(&self) -> Option<Value> {
        if self.is_empty() {
            None
        } else {
            serde_json::to_value(self).ok()
        }
    }

    /// Format for display in Telegram messages (MarkdownV2 escaped).
    ///
    /// Returns a string like `\+tag1 \+tag2 \-tag3`
    pub fn format_for_display(&self) -> String {
        let mut parts = Vec::new();

        if !self.include.is_empty() {
            let include_str = self
                .include
                .iter()
                .map(|s| markdown::escape(format!("+{}", s).as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            parts.push(include_str);
        }

        if !self.exclude.is_empty() {
            let exclude_str = self
                .exclude
                .iter()
                .map(|s| markdown::escape(format!("-{}", s).as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            parts.push(exclude_str);
        }

        parts.join(" ")
    }

    /// Check if an illust matches this filter.
    ///
    /// - If exclude tags are specified, the illust must NOT contain any of them.
    /// - If include tags are specified, the illust must contain at least one of them.
    /// - Tags are compared case-insensitively after normalization.
    pub fn matches(&self, illust: &Illust) -> bool {
        // Early return if no filter
        if self.is_empty() {
            return true;
        }

        // Normalize illust tags once
        let illust_tags: Vec<String> = illust
            .tags
            .iter()
            .map(|t| tag::normalize_tag(&t.name))
            .collect();

        // Check exclude tags first (must not contain any)
        for exclude_tag in &self.exclude {
            let normalized = tag::normalize_tag(exclude_tag);
            if illust_tags.iter().any(|t| t == &normalized) {
                return false;
            }
        }

        // Check include tags (must contain at least one if specified)
        if !self.include.is_empty() {
            for include_tag in &self.include {
                let normalized = tag::normalize_tag(include_tag);
                if illust_tags.iter().any(|t| t == &normalized) {
                    return true;
                }
            }
            return false;
        }

        true
    }

    /// Filter illusts using this tag filter.
    ///
    /// Works with any iterator that yields references to Illust.
    pub fn filter<'a, I>(&self, iter: I) -> Vec<&'a Illust>
    where
        I: IntoIterator<Item = &'a Illust>,
    {
        if self.is_empty() {
            return iter.into_iter().collect();
        }
        iter.into_iter().filter(|i| self.matches(i)).collect()
    }

    /// Merge another filter into this one (combine include/exclude lists).
    pub fn merge(&mut self, other: &TagFilter) {
        self.include.extend(other.include.iter().cloned());
        self.exclude.extend(other.exclude.iter().cloned());
    }

    /// Create a merged filter from two filters.
    pub fn merged(&self, other: &TagFilter) -> Self {
        let mut result = self.clone();
        result.merge(other);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_from_args_empty() {
        let filter = TagFilter::parse_from_args(&[]);
        assert!(filter.is_empty());
    }

    #[test]
    fn test_parse_from_args_include_only() {
        let filter = TagFilter::parse_from_args(&["tag1", "+tag2"]);
        assert_eq!(filter.include, vec!["tag1", "tag2"]);
        assert!(filter.exclude.is_empty());
    }

    #[test]
    fn test_parse_from_args_exclude_only() {
        let filter = TagFilter::parse_from_args(&["-tag1", "-tag2"]);
        assert!(filter.include.is_empty());
        assert_eq!(filter.exclude, vec!["tag1", "tag2"]);
    }

    #[test]
    fn test_parse_from_args_mixed() {
        let filter = TagFilter::parse_from_args(&["+原神", "-R-18", "cute"]);
        assert_eq!(filter.include, vec!["原神", "cute"]);
        assert_eq!(filter.exclude, vec!["R-18"]);
    }

    #[test]
    fn test_parse_from_args_ignores_empty() {
        let filter = TagFilter::parse_from_args(&["+", "-", ""]);
        assert!(filter.is_empty());
    }

    #[test]
    fn test_to_json_and_from_json() {
        let original = TagFilter::parse_from_args(&["+tag1", "-tag2"]);
        let json = original.to_json();
        assert!(json.is_some());

        let restored: TagFilter = serde_json::from_value(json.unwrap()).unwrap();
        assert_eq!(restored.include, original.include);
        assert_eq!(restored.exclude, original.exclude);
    }

    #[test]
    fn test_from_excluded_json() {
        let json = Some(serde_json::json!(["R-18", "gore"]));
        let filter = TagFilter::from_excluded_json(&json);
        assert!(filter.include.is_empty());
        assert_eq!(filter.exclude, vec!["R-18", "gore"]);
    }

    #[test]
    fn test_format_for_display() {
        let filter = TagFilter::parse_from_args(&["+原神", "-R-18"]);
        let display = filter.format_for_display();
        assert!(display.contains("\\+原神"));
        assert!(display.contains("\\-R\\-18"));
    }

    #[test]
    fn test_merge() {
        let mut filter1 = TagFilter::parse_from_args(&["+tag1"]);
        let filter2 = TagFilter::parse_from_args(&["-tag2"]);
        filter1.merge(&filter2);
        assert_eq!(filter1.include, vec!["tag1"]);
        assert_eq!(filter1.exclude, vec!["tag2"]);
    }
}
