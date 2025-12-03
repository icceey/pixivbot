/// Special characters that should be removed from tags.
/// These characters are not compatible with Telegram hashtags or cause matching issues.
const SPECIAL_CHARS: &[char] = &[' ', '-', '(', ')', '・', '/', ':', '…', '「', '」', '_'];

/// Remove special characters from a tag string.
/// Used by both normalize_tag and format_tags.
fn remove_special_chars(tag: &str) -> String {
    tag.chars().filter(|c| !SPECIAL_CHARS.contains(c)).collect()
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
/// use crate::pixiv_client::Illust;
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
pub fn format_tags_escaped(illust: &crate::pixiv_client::Illust) -> String {
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
    fn test_format_tags_empty() {
        let tags: Vec<String> = vec![];
        assert_eq!(format_tags(&tags), Vec::<String>::new());
    }

    #[test]
    fn test_format_tags_simple() {
        let tags = vec!["原神", "Genshin"];
        let result = format_tags(&tags);
        assert_eq!(result, vec!["原神", "Genshin"]);
    }

    #[test]
    fn test_format_tags_with_spaces() {
        let tags = vec!["Genshin Impact", "Game Art"];
        let result = format_tags(&tags);
        assert_eq!(result, vec!["GenshinImpact", "GameArt"]);
    }

    #[test]
    fn test_format_tags_no_escape() {
        let tags = vec!["R-18", "test+tag"];
        let result = format_tags(&tags);
        assert_eq!(result, vec!["R18", "test+tag"]);
    }

    #[test]
    fn test_format_tags_special_chars() {
        let tags = vec!["R-18", "test-tag", "tag(test)", "foo)bar"];
        let result = format_tags(&tags);
        assert_eq!(result, vec!["R18", "testtag", "tagtest", "foobar"]);
    }

    #[test]
    fn test_format_tags_mixed() {
        let tags = vec!["Genshin Impact", "R-18", "tag(test)"];
        let result = format_tags(&tags);
        assert_eq!(result, vec!["GenshinImpact", "R18", "tagtest"]);
    }

    #[test]
    fn test_format_tags_japanese_chars() {
        let tags = vec!["「テスト」", "テスト…", "ヴァイオレット・エヴァーガーデン"];
        let result = format_tags(&tags);
        assert_eq!(
            result,
            vec!["テスト", "テスト", "ヴァイオレットエヴァーガーデン"]
        );
    }

    #[test]
    fn test_normalize_tag_lowercase() {
        assert_eq!(normalize_tag("R-18"), "r18");
        assert_eq!(normalize_tag("NSFW"), "nsfw");
    }

    #[test]
    fn test_normalize_tag_special_chars() {
        assert_eq!(normalize_tag("R-18"), "r18");
        assert_eq!(normalize_tag("R_18"), "r18");
        assert_eq!(normalize_tag("r-18"), "r18");
        assert_eq!(normalize_tag("r_18"), "r18");
    }

    #[test]
    fn test_normalize_tag_spaces() {
        assert_eq!(normalize_tag("Genshin Impact"), "genshinimpact");
        assert_eq!(normalize_tag("Genshin_Impact"), "genshinimpact");
    }

    #[test]
    fn test_normalize_tag_match() {
        // Test that different variations normalize to the same value
        let filter = normalize_tag("R-18");
        assert_eq!(normalize_tag("R-18"), filter);
        assert_eq!(normalize_tag("R_18"), filter);
        assert_eq!(normalize_tag("r-18"), filter);
        assert_eq!(normalize_tag("r_18"), filter);
    }

    #[test]
    fn test_normalize_tag_japanese_chars() {
        assert_eq!(normalize_tag("「テスト」"), "テスト");
        assert_eq!(normalize_tag("テスト…"), "テスト");
    }
}
