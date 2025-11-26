/// Escape special characters for Telegram MarkdownV2 format
///
/// This function escapes all special characters that have meaning in Telegram's MarkdownV2:
/// _ * [ ] ( ) ~ ` > # + - = | { } . !
///
/// Use this when you need to display user-generated content (like titles, names, tags)
/// in Telegram messages with MarkdownV2 parse mode.
///
/// # Example
/// ```
/// use pixivbot::utils::markdown::escape;
///
/// let title = "Hello *World*!";
/// let escaped = escape(title);
/// assert_eq!(escaped, "Hello \\*World\\*\\!");
/// ```
pub fn escape(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('_', "\\_")
        .replace('*', "\\*")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace('(', "\\(")
        .replace(')', "\\)")
        .replace('~', "\\~")
        .replace('`', "\\`")
        .replace('>', "\\>")
        .replace('#', "\\#")
        .replace('+', "\\+")
        .replace('-', "\\-")
        .replace('=', "\\=")
        .replace('|', "\\|")
        .replace('{', "\\{")
        .replace('}', "\\}")
        .replace('.', "\\.")
        .replace('!', "\\!")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_special_chars() {
        assert_eq!(escape("Hello World"), "Hello World");
        assert_eq!(escape("Hello_World"), "Hello\\_World");
        assert_eq!(escape("*bold*"), "\\*bold\\*");
        assert_eq!(escape("test-123"), "test\\-123");
        assert_eq!(escape("price: $9.99!"), "price: $9\\.99\\!");
    }

    #[test]
    fn test_escape_complex_text() {
        let input = "Artist (Name) - Work #123 [2024]";
        let expected = "Artist \\(Name\\) \\- Work \\#123 \\[2024\\]";
        assert_eq!(escape(input), expected);
    }

    #[test]
    fn test_escape_backslash() {
        assert_eq!(escape("path\\to\\file"), "path\\\\to\\\\file");
    }
}
