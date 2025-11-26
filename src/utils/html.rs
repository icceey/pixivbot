/// Extract tag names from tags and format for display
/// 
/// Converts tag names by replacing spaces with underscores.
/// Does NOT add hashtags or markdown escaping - that should be done by the caller.
/// 
/// # Example
/// ```
/// use pixivbot::utils::html::format_tags;
/// 
/// let tags = vec!["原神", "Genshin Impact"];
/// let formatted = format_tags(&tags);
/// // Returns: vec!["原神", "Genshin_Impact"]
/// ```
pub fn format_tags<T: AsRef<str>>(tags: &[T]) -> Vec<String> {
    tags.iter()
        .map(|tag| tag.as_ref().replace(' ', "_"))
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
        assert_eq!(result, vec!["R-18", "test+tag"]);
    }
}
