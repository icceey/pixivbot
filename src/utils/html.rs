use regex::Regex;

/// Clean HTML content to plain text
/// 
/// This function:
/// - Replaces HTML line breaks with newlines
/// - Removes common HTML tags (p, strong, em)
/// - Decodes HTML entities (&nbsp;, &amp;, &lt;, &gt;)
/// - Removes anchor tags with their content
/// - Returns trimmed text, or empty string if no content
/// 
/// # Example
/// ```
/// use pixivbot::utils::html::clean_description;
/// 
/// let html = "Hello<br />World<strong>!</strong>";
/// let cleaned = clean_description(html);
/// // Returns: "Hello\nWorld!"
/// ```
pub fn clean_description(html: &str) -> String {
    if html.is_empty() {
        return String::new();
    }
    
    // Replace HTML tags and entities
    let mut clean_text = html
        .replace("<br />", "\n")
        .replace("<br>", "\n")
        .replace("<p>", "")
        .replace("</p>", "\n")
        .replace("<strong>", "")
        .replace("</strong>", "")
        .replace("<em>", "")
        .replace("</em>", "")
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">");
    
    // Remove anchor tags with their content
    let re = Regex::new(r"<a\s+[^>]*>.*?</a>").unwrap();
    clean_text = re.replace_all(&clean_text, "").to_string();
    
    // Return trimmed text
    clean_text.trim().to_string()
}

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
    fn test_clean_description_empty() {
        assert_eq!(clean_description(""), "");
        assert_eq!(clean_description("   "), "");
    }

    #[test]
    fn test_clean_description_simple() {
        let html = "Hello World";
        let result = clean_description(html);
        assert_eq!(result, "Hello World");
    }

    #[test]
    fn test_clean_description_with_br() {
        let html = "Line1<br />Line2<br>Line3";
        let result = clean_description(html);
        assert_eq!(result, "Line1\nLine2\nLine3");
    }

    #[test]
    fn test_clean_description_with_tags() {
        let html = "<p>Hello</p><strong>World</strong><em>!</em>";
        let result = clean_description(html);
        assert_eq!(result, "Hello\nWorld!");
    }

    #[test]
    fn test_clean_description_with_entities() {
        let html = "Hello&nbsp;World&amp;&lt;&gt;";
        let result = clean_description(html);
        assert_eq!(result, "Hello World&<>");
    }

    #[test]
    fn test_clean_description_with_anchor() {
        let html = "Text <a href=\"link\">remove this</a> keep this";
        let result = clean_description(html);
        assert_eq!(result, "Text  keep this");
    }

    #[test]
    fn test_clean_description_no_escape() {
        let html = "Price: $9.99! (sale)";
        let result = clean_description(html);
        assert_eq!(result, "Price: $9.99! (sale)");
    }

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
