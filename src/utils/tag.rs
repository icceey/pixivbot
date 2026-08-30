use regex::Regex;
use std::sync::LazyLock;

/// Regex pattern to match non-word characters.
/// The regex crate has Unicode support enabled by default, so \w matches Unicode word characters.
/// Only keeps: Letters, Numbers, Underscores, and Unicode word characters (e.g., Chinese, Japanese).
static NON_WORD_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"[^\w]").expect("BUG: Failed to compile hardcoded regex pattern [^\\w]")
});

/// Remove non-word characters from a tag string using whitelist approach.
/// Only keeps letters, numbers, underscores, and Unicode characters.
/// Used by both normalize_tag and format_tags.
fn remove_special_chars(tag: &str) -> String {
    NON_WORD_PATTERN.replace_all(tag, "").to_string()
}

/// Normalize a tag string for comparison purposes
///
/// Converts the tag to lowercase and removes special characters
/// so that tags like "R-18", "R18", "r-18" all match.
///
/// # Example
/// ```
/// use pixivbot::utils::html::normalize_tag;
///
/// assert_eq!(normalize_tag("R-18"), "r18");
/// assert_eq!(normalize_tag("R_18"), "r18");
/// assert_eq!(normalize_tag("Genshin Impact"), "genshinimpact");
/// ```
pub fn normalize_tag(tag: &str) -> String {
    remove_special_chars(tag).to_lowercase()
}

/// Extract tag names from tags and format for display
///
/// Removes special characters that Telegram doesn't recognize in hashtags.
/// Does NOT add hashtags or markdown escaping - that should be done by the caller.
///
/// # Example
/// ```
/// use pixivbot::utils::html::format_tags;
///
/// let tags = vec!["原神", "Genshin Impact", "R-18", "test-tag(test)"];
/// let formatted = format_tags(&tags);
/// // Returns: vec!["原神", "GenshinImpact", "R18", "testtagtest"]
/// ```
pub fn format_tags<T: AsRef<str>>(tags: &[T]) -> Vec<String> {
    tags.iter()
        .map(|tag| remove_special_chars(tag.as_ref()))
        .collect()
}

/// Format tags for display
///
/// Adds hashtags and escapes for Telegram MarkdownV2.
/// Returns a string like `\n\n\#tag1  \#tag2`
/// # Example
/// ```
/// use pixivbot::utils::tag::format_tags_escaped;
/// use pixiv_client::Illust;
/// let illust = Illust {
///     tags: vec![
///         pixivbot::pixiv::model::Tag { name: "原神".to_string() },
///         pixivbot::pixiv::model::Tag { name: "Genshin Impact".to_string() },
///     ],
///     ..Default::default()
/// };
/// let formatted = format_tags_escaped(&illust);
/// // Returns: "\n\n\#原神  \#GenshinImpact"
/// ```
pub fn format_tags_escaped(illust: &pixiv_client::Illust) -> String {
    use teloxide::utils::markdown;

    let tag_names: Vec<&str> = illust.tags.iter().map(|t| t.name.as_str()).collect();
    let formatted = format_tags(&tag_names);

    if formatted.is_empty() {
        return String::new();
    }

    let escaped: Vec<String> = formatted
        .iter()
        .map(|t| markdown::escape(format!("#{}", t).as_str()))
        .collect();

    format!("\n\n{}", escaped.join("  "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_tag_special_chars() {
        assert_eq!(normalize_tag("R-18"), "r18");
        // Underscores are kept (part of \w in regex)
        assert_eq!(normalize_tag("R_18"), "r_18");
        assert_eq!(normalize_tag("r-18"), "r18");
        assert_eq!(normalize_tag("r_18"), "r_18");
    }

    #[test]
    fn test_normalize_tag_japanese_chars() {
        assert_eq!(normalize_tag("「テスト」"), "テスト");
        assert_eq!(normalize_tag("テスト…"), "テスト");
    }

    #[test]
    fn test_format_tags_requirements() {
        // Hello World -> HelloWorld
        assert_eq!(format_tags(&["Hello World"]), vec!["HelloWorld"]);

        // C++ & Rust -> CRust
        assert_eq!(format_tags(&["C++ & Rust"]), vec!["CRust"]);

        // User-Name -> UserName
        assert_eq!(format_tags(&["User-Name"]), vec!["UserName"]);

        // 你好，世界！ -> 你好世界
        assert_eq!(format_tags(&["你好，世界！"]), vec!["你好世界"]);
    }

    #[test]
    fn test_format_tags_unicode_support() {
        // Test various Unicode scripts
        assert_eq!(format_tags(&["日本語"]), vec!["日本語"]);
        assert_eq!(format_tags(&["中文测试"]), vec!["中文测试"]);
        assert_eq!(format_tags(&["한국어"]), vec!["한국어"]);
        assert_eq!(format_tags(&["Русский"]), vec!["Русский"]);
    }
}
