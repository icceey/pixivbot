/// Normalize a tag string for comparison purposes
///
/// Converts the tag to lowercase and replaces special characters with underscores
/// so that tags like "R-18", "R_18", "r-18" all match.
///
/// # Example
/// ```
/// use pixivbot::utils::html::normalize_tag;
///
/// assert_eq!(normalize_tag("R-18"), "r_18");
/// assert_eq!(normalize_tag("R_18"), "r_18");
/// assert_eq!(normalize_tag("Genshin Impact"), "genshin_impact");
/// ```
pub fn normalize_tag(tag: &str) -> String {
    tag.chars()
        .map(|c| {
            let c_lower = c.to_lowercase().next().unwrap_or(c);
            match c_lower {
                ' ' | '-' | '(' | ')' | '・' | '/' | ':' => '_',
                _ => c_lower,
            }
        })
        .collect()
}

/// Extract tag names from tags and format for display
///
/// Converts tag names by replacing spaces with underscores and removing special characters
/// that Telegram doesn't recognize in hashtags (e.g., `-`, `(`, `)`).
/// Does NOT add hashtags or markdown escaping - that should be done by the caller.
///
/// # Example
/// ```
/// use pixivbot::utils::html::format_tags;
///
/// let tags = vec!["原神", "Genshin Impact", "R-18", "test-tag(test)"];
/// let formatted = format_tags(&tags);
/// // Returns: vec!["原神", "Genshin_Impact", "R18", "testtagtest"]
/// ```
pub fn format_tags<T: AsRef<str>>(tags: &[T]) -> Vec<String> {
    tags.iter()
        .map(|tag| {
            let tag_str = tag.as_ref();
            // Replace spaces with underscores
            let mut result = tag_str.replace(' ', "_");
            // Remove special characters that Telegram doesn't recognize in hashtags
            result = result.replace('-', "");
            result = result.replace('(', "");
            result = result.replace(')', "");
            result = result.replace('・', "");
            result = result.replace('/', "");
            result = result.replace(':', "");
            result
        })
        .collect()
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
        assert_eq!(result, vec!["Genshin_Impact", "Game_Art"]);
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
        assert_eq!(result, vec!["Genshin_Impact", "R18", "tagtest"]);
    }

    #[test]
    fn test_normalize_tag_lowercase() {
        assert_eq!(normalize_tag("R-18"), "r_18");
        assert_eq!(normalize_tag("NSFW"), "nsfw");
    }

    #[test]
    fn test_normalize_tag_special_chars() {
        assert_eq!(normalize_tag("R-18"), "r_18");
        assert_eq!(normalize_tag("R_18"), "r_18");
        assert_eq!(normalize_tag("r-18"), "r_18");
        assert_eq!(normalize_tag("r_18"), "r_18");
    }

    #[test]
    fn test_normalize_tag_spaces() {
        assert_eq!(normalize_tag("Genshin Impact"), "genshin_impact");
        assert_eq!(normalize_tag("Genshin_Impact"), "genshin_impact");
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
}
