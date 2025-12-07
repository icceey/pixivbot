//! Pixiv 链接解析与处理
//!
//! 处理用户发送的 Pixiv 作品链接和作者链接

use regex::Regex;
use std::sync::LazyLock;

/// Pixiv 作品链接正则表达式
/// 匹配格式: https://www.pixiv.net/artworks/126608911
static ILLUST_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://(?:www\.)?pixiv\.net/(?:en/)?artworks/(\d+)").unwrap());

/// Pixiv 用户链接正则表达式
/// 匹配格式: https://www.pixiv.net/users/33611048
static USER_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"https?://(?:www\.)?pixiv\.net/(?:en/)?users/(\d+)").unwrap());

/// 解析到的 Pixiv 链接类型
#[derive(Debug, Clone)]
pub enum PixivLink {
    /// 作品链接，包含作品 ID
    Illust(u64),
    /// 用户链接，包含用户 ID
    User(u64),
}

/// 从文本中解析所有 Pixiv 链接
///
/// 返回找到的所有链接（作品和用户链接），按照出现顺序排列
pub fn parse_pixiv_links(text: &str) -> Vec<PixivLink> {
    let mut links = Vec::new();

    // 解析作品链接
    for caps in ILLUST_REGEX.captures_iter(text) {
        if let Some(id_str) = caps.get(1) {
            if let Ok(id) = id_str.as_str().parse::<u64>() {
                links.push(PixivLink::Illust(id));
            }
        }
    }

    // 解析用户链接
    for caps in USER_REGEX.captures_iter(text) {
        if let Some(id_str) = caps.get(1) {
            if let Ok(id) = id_str.as_str().parse::<u64>() {
                links.push(PixivLink::User(id));
            }
        }
    }

    links
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_illust_link() {
        let text = "看看这个作品 https://www.pixiv.net/artworks/126608911 很好看";
        let links = parse_pixiv_links(text);
        assert_eq!(links.len(), 1);
        match &links[0] {
            PixivLink::Illust(id) => assert_eq!(*id, 126608911),
            _ => panic!("Expected Illust link"),
        }
    }

    #[test]
    fn test_parse_user_link() {
        let text = "关注这个作者 https://www.pixiv.net/users/33611048";
        let links = parse_pixiv_links(text);
        assert_eq!(links.len(), 1);
        match &links[0] {
            PixivLink::User(id) => assert_eq!(*id, 33611048),
            _ => panic!("Expected User link"),
        }
    }

    #[test]
    fn test_parse_multiple_links() {
        let text = "作品: https://www.pixiv.net/artworks/123 作者: https://www.pixiv.net/users/456";
        let links = parse_pixiv_links(text);
        assert_eq!(links.len(), 2);
    }

    #[test]
    fn test_parse_en_links() {
        let text =
            "https://www.pixiv.net/en/artworks/126608911 https://www.pixiv.net/en/users/33611048";
        let links = parse_pixiv_links(text);
        assert_eq!(links.len(), 2);
    }
}
