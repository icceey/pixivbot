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
pub fn normalize_tag(tag: &str) -> String {
    remove_special_chars(tag).to_lowercase()
}

/// Extract tag names from tags and format for display
///
/// Removes special characters that Telegram doesn't recognize in hashtags.
/// Does NOT add hashtags or markdown escaping - that should be done by the caller.
pub fn format_tags<T: AsRef<str>>(tags: &[T]) -> Vec<String> {
    tags.iter()
        .map(|tag| remove_special_chars(tag.as_ref()))
        .collect()
}

/// Prepend the platform AI label, deduplicating that label using filter normalization.
pub fn pixiv_tag_names(illust: &pixiv_client::Illust) -> impl Iterator<Item = &str> {
    let is_ai = illust.illust_ai_type == 2;
    is_ai.then_some("AI生成").into_iter().chain(
        illust
            .tags
            .iter()
            .map(|tag| tag.name.as_str())
            .filter(move |name| !is_ai || normalize_tag(name) != "ai生成"),
    )
}

/// Format tags for display
///
/// Adds hashtags and escapes for Telegram MarkdownV2.
/// Returns a string like `\n\n\#tag1  \#tag2`
pub fn format_tags_escaped(illust: &pixiv_client::Illust) -> String {
    use teloxide::utils::markdown;

    let tag_names: Vec<&str> = pixiv_tag_names(illust).collect();
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
