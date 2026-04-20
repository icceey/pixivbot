//! Pixiv 链接解析与处理
//!
//! 处理用户发送的 Pixiv 作品链接和作者链接

use booru_client::BooruEngineType;
use regex::Regex;
use std::sync::LazyLock;

use crate::booru::BooruSiteRegistry;

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
        if let (Some(full_match), Some(id_str)) = (caps.get(0), caps.get(1)) {
            if let Ok(id) = id_str.as_str().parse::<u64>() {
                links.push((full_match.start(), PixivLink::Illust(id)));
            }
        }
    }

    // 解析用户链接
    for caps in USER_REGEX.captures_iter(text) {
        if let (Some(full_match), Some(id_str)) = (caps.get(0), caps.get(1)) {
            if let Ok(id) = id_str.as_str().parse::<u64>() {
                links.push((full_match.start(), PixivLink::User(id)));
            }
        }
    }

    links.sort_by_key(|(start, _)| *start);
    links.into_iter().map(|(_, link)| link).collect()
}

/// 一条 Booru 站点帖子引用，用于跨模块传递解析结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BooruPostRef {
    pub site_name: String,
    pub post_id: u64,
}

/// 从文本中解析所有已配置 Booru 站点的帖子链接
///
/// 仅匹配 `registry` 中已配置的站点，URL 格式按 `engine_type` 区分：
/// - Moebooru: `{host}/post/show/{id}`
/// - Danbooru: `{host}/posts/{id}`
/// - Gelbooru: `{host}/index.php?...id={id}`
///
/// 同一帖子多次出现仅返回一次，按文本中首次出现位置排序。
pub fn parse_booru_post_links(text: &str, registry: &BooruSiteRegistry) -> Vec<BooruPostRef> {
    let mut found: Vec<(usize, BooruPostRef)> = Vec::new();

    for site in registry.iter() {
        let cfg = &site.config;
        let host = match url::Url::parse(&cfg.base_url)
            .ok()
            .and_then(|u| u.host_str().map(|h| h.to_lowercase()))
        {
            Some(h) => h,
            None => continue,
        };
        let host_esc = regex::escape(&host);
        let pattern = match cfg.engine_type {
            BooruEngineType::Moebooru => format!(r"https?://{host_esc}/post/show/(\d+)"),
            BooruEngineType::Danbooru => format!(r"https?://{host_esc}/posts/(\d+)"),
            BooruEngineType::Gelbooru => {
                format!(r"https?://{host_esc}/index\.php\?[^\s]*\bid=(\d+)")
            }
        };
        let re = match Regex::new(&pattern) {
            Ok(r) => r,
            Err(_) => continue,
        };
        for caps in re.captures_iter(text) {
            if let (Some(m), Some(id_str)) = (caps.get(0), caps.get(1)) {
                if let Ok(id) = id_str.as_str().parse::<u64>() {
                    found.push((
                        m.start(),
                        BooruPostRef {
                            site_name: cfg.name.clone(),
                            post_id: id,
                        },
                    ));
                }
            }
        }
    }

    found.sort_by_key(|(start, _)| *start);
    let mut seen = std::collections::HashSet::new();
    found
        .into_iter()
        .filter(|(_, r)| seen.insert((r.site_name.clone(), r.post_id)))
        .map(|(_, r)| r)
        .collect()
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
    fn test_parse_mixed_links_preserves_appearance_order() {
        let text = "作者: https://www.pixiv.net/users/456 作品: https://www.pixiv.net/artworks/123 作者: https://www.pixiv.net/users/789";
        let links = parse_pixiv_links(text);

        assert_eq!(links.len(), 3);

        match &links[0] {
            PixivLink::User(id) => assert_eq!(*id, 456),
            _ => panic!("Expected first link to be User"),
        }

        match &links[1] {
            PixivLink::Illust(id) => assert_eq!(*id, 123),
            _ => panic!("Expected second link to be Illust"),
        }

        match &links[2] {
            PixivLink::User(id) => assert_eq!(*id, 789),
            _ => panic!("Expected third link to be User"),
        }
    }

    #[test]
    fn test_parse_en_links() {
        let text =
            "https://www.pixiv.net/en/artworks/126608911 https://www.pixiv.net/en/users/33611048";
        let links = parse_pixiv_links(text);
        assert_eq!(links.len(), 2);
    }

    use crate::booru::BooruSiteRegistry;
    use crate::config::BooruSiteConfig;

    fn site(name: &str, base: &str, eng: BooruEngineType) -> BooruSiteConfig {
        BooruSiteConfig {
            name: name.to_string(),
            engine_type: eng,
            base_url: base.to_string(),
            username: None,
            api_key: None,
            min_interval_sec: 1800,
            max_interval_sec: 3600,
            page_limit: 20,
            bypass: None,
        }
    }

    #[test]
    fn parse_booru_links_moebooru() {
        let reg = BooruSiteRegistry::from_configs(&[site(
            "yandere",
            "https://yande.re",
            BooruEngineType::Moebooru,
        )]);
        let text = "see https://yande.re/post/show/123456 cool";
        let refs = parse_booru_post_links(text, &reg);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].site_name, "yandere");
        assert_eq!(refs[0].post_id, 123456);
    }

    #[test]
    fn parse_booru_links_danbooru() {
        let reg = BooruSiteRegistry::from_configs(&[site(
            "danbooru",
            "https://danbooru.donmai.us",
            BooruEngineType::Danbooru,
        )]);
        let refs = parse_booru_post_links("x https://danbooru.donmai.us/posts/9876 y", &reg);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].post_id, 9876);
    }

    #[test]
    fn parse_booru_links_gelbooru_query() {
        let reg = BooruSiteRegistry::from_configs(&[site(
            "gel",
            "https://gelbooru.com",
            BooruEngineType::Gelbooru,
        )]);
        let refs = parse_booru_post_links(
            "https://gelbooru.com/index.php?page=post&s=view&id=4242",
            &reg,
        );
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].post_id, 4242);
    }

    #[test]
    fn parse_booru_links_dedup_and_unconfigured_skipped() {
        let reg = BooruSiteRegistry::from_configs(&[site(
            "yandere",
            "https://yande.re",
            BooruEngineType::Moebooru,
        )]);
        let text = "https://yande.re/post/show/1 https://yande.re/post/show/1 https://other.example/post/show/2";
        let refs = parse_booru_post_links(text, &reg);
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].post_id, 1);
    }
}
