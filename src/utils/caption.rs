use crate::utils::tag;
use pixiv_client::Illust;
use teloxide::utils::markdown;

pub const MAX_PER_GROUP: usize = 10;

pub fn build_illust_caption(illust: &Illust) -> String {
    let page_info = if illust.is_multi_page() {
        format!(" \\({} photos\\)", illust.page_count)
    } else {
        String::new()
    };

    build_standard_caption("🎨", illust, &page_info)
}

pub fn build_ugoira_caption(illust: &Illust) -> String {
    build_standard_caption("🎞️", illust, "")
}

pub fn build_continuation_caption(
    illust: &Illust,
    already_sent_count: usize,
    total_pages: usize,
) -> String {
    let total_batches = total_pages.div_ceil(MAX_PER_GROUP);
    let current_batch = (already_sent_count / MAX_PER_GROUP) + 1;
    let tags = tag::format_tags_escaped(illust);

    format!(
        "🎨 {} \\(continued {}/{}\\)\nby *{}*\n\n🔗 [来源](https://pixiv\\.net/artworks/{}){}",
        markdown::escape(&illust.title),
        current_batch,
        total_batches,
        markdown::escape(&illust.user.name),
        illust.id,
        tags
    )
}

pub fn build_ranking_title(mode: &str, count: usize) -> String {
    format!(
        "📊 *{} Ranking* \\- {} new\\!\n\n",
        markdown::escape(&mode.replace('_', " ").to_uppercase()),
        count
    )
}

pub fn build_ranking_caption(title: &str, index: usize, illust: &Illust) -> String {
    let tags = tag::format_tags_escaped(illust);
    let title_line = if illust.is_ugoira() {
        format!("🎞️ {}", markdown::escape(&illust.title))
    } else {
        markdown::escape(&illust.title)
    };

    let base_caption = format!(
        "{}\nby *{}* \\(ID: `{}`\\)\n\n❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}",
        title_line,
        markdown::escape(&illust.user.name),
        illust.user.id,
        illust.total_bookmarks,
        illust.id,
        tags
    );

    if index == 0 {
        format!("{}{}", title, base_caption)
    } else {
        base_caption
    }
}

/// Build caption for a booru post (MarkdownV2 format)
pub fn build_booru_caption(
    post: &booru_client::BooruPost,
    site_name: &str,
    base_url: &str,
    engine_type: booru_client::BooruEngineType,
) -> String {
    let rating_emoji = match post.rating {
        booru_client::BooruRating::Safe | booru_client::BooruRating::General => "🟢",
        booru_client::BooruRating::Sensitive => "🟠",
        booru_client::BooruRating::Questionable => "🟡",
        booru_client::BooruRating::Explicit => "🔴",
    };

    let clean_base = base_url.trim_end_matches('/');
    let post_url = format!("{}{}", clean_base, engine_type.post_path(post.id));

    let tag_list: Vec<&str> = post.tags.split_whitespace().take(5).collect();
    let tags_display = if tag_list.is_empty() {
        String::new()
    } else {
        let sanitized = tag::format_tags(&tag_list);
        let formatted: Vec<String> = sanitized
            .iter()
            .map(|t| format!("\\#{}", markdown::escape(t)))
            .collect();
        format!("\n\n{}", formatted.join("  "))
    };

    format!(
        "🏷 *{}* \\| {}\n\n⭐ {} \\| ❤️ {} \\| {} {}\n🔗 [来源]({}){}\n",
        markdown::escape(site_name),
        markdown::escape(&format!("#{}", post.id)),
        markdown::escape(&post.score.to_string()),
        markdown::escape(&post.fav_count.to_string()),
        rating_emoji,
        markdown::escape(post.rating.as_short_str()),
        markdown::escape_link_url(&post_url),
        tags_display
    )
}

fn build_standard_caption(prefix: &str, illust: &Illust, title_suffix: &str) -> String {
    let tags = tag::format_tags_escaped(illust);

    format!(
        "{} {}{}\nby *{}* \\(ID: `{}`\\)\n\n👀 {} \\| ❤️ {} \\| 🔗 [来源](https://pixiv\\.net/artworks/{}){}",
        prefix,
        markdown::escape(&illust.title),
        title_suffix,
        markdown::escape(&illust.user.name),
        illust.user.id,
        illust.total_view,
        illust.total_bookmarks,
        illust.id,
        tags
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_illust(
        illust_type: &str,
        title: &str,
        author_name: &str,
        page_count: u32,
        total_view: u64,
        total_bookmarks: u64,
        tags: &[&str],
    ) -> Illust {
        let meta_pages = if page_count > 1 {
            (0..page_count)
                .map(|page| {
                    json!({
                        "image_urls": {
                            "square_medium": format!("square-{page}"),
                            "medium": format!("medium-{page}"),
                            "large": format!("large-{page}"),
                            "original": format!("original-{page}")
                        }
                    })
                })
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };

        serde_json::from_value(json!({
            "id": 12345,
            "title": title,
            "type": illust_type,
            "image_urls": {
                "square_medium": "square",
                "medium": "medium",
                "large": "large",
                "original": "original"
            },
            "caption": "",
            "restrict": 0,
            "user": {
                "id": 67890,
                "name": author_name,
                "account": "author"
            },
            "tags": tags
                .iter()
                .map(|name| json!({ "name": name, "translated_name": null }))
                .collect::<Vec<_>>(),
            "create_date": "2026-01-01T00:00:00+00:00",
            "page_count": page_count,
            "width": 100,
            "height": 100,
            "sanity_level": 2,
            "x_restrict": 0,
            "series": null,
            "meta_single_page": {
                "original_image_url": "original"
            },
            "meta_pages": meta_pages,
            "total_view": total_view,
            "total_bookmarks": total_bookmarks,
            "is_bookmarked": false,
            "visible": true,
            "is_muted": false,
            "total_comments": 0
        }))
        .unwrap()
    }

    #[test]
    fn build_illust_caption_for_single_page_matches_golden_output() {
        let illust = make_illust("illust", "Still", "Author", 1, 123, 45, &[]);

        assert_eq!(
            build_illust_caption(&illust),
            "🎨 Still\nby *Author* \\(ID: `67890`\\)\n\n👀 123 \\| ❤️ 45 \\| 🔗 [来源](https://pixiv\\.net/artworks/12345)"
        );
    }

    #[test]
    fn build_illust_caption_for_multi_page_matches_golden_output() {
        let illust = make_illust(
            "illust",
            "Multi",
            "Author",
            3,
            123,
            45,
            &["Genshin Impact", "R-18"],
        );

        assert_eq!(
            build_illust_caption(&illust),
            "🎨 Multi \\(3 photos\\)\nby *Author* \\(ID: `67890`\\)\n\n👀 123 \\| ❤️ 45 \\| 🔗 [来源](https://pixiv\\.net/artworks/12345)\n\n\\#GenshinImpact  \\#R18"
        );
    }

    #[test]
    fn build_ugoira_caption_matches_golden_output() {
        let illust = make_illust("ugoira", "Animated", "Author", 1, 123, 45, &[]);

        assert_eq!(
            build_ugoira_caption(&illust),
            "🎞️ Animated\nby *Author* \\(ID: `67890`\\)\n\n👀 123 \\| ❤️ 45 \\| 🔗 [来源](https://pixiv\\.net/artworks/12345)"
        );
    }

    #[test]
    fn build_continuation_caption_matches_golden_output() {
        let illust = make_illust("illust", "Paged Work", "Artist", 23, 123, 45, &["Series A"]);

        assert_eq!(
            build_continuation_caption(&illust, 10, 23),
            "🎨 Paged Work \\(continued 2/3\\)\nby *Artist*\n\n🔗 [来源](https://pixiv\\.net/artworks/12345)\n\n\\#SeriesA"
        );
    }

    #[test]
    fn build_ranking_title_matches_golden_output() {
        assert_eq!(
            build_ranking_title("day_ai", 2),
            "📊 *DAY AI Ranking* \\- 2 new\\!\n\n"
        );
    }

    #[test]
    fn build_ranking_caption_for_first_item_prepends_title_once() {
        let illust = make_illust("illust", "Still", "Author", 1, 123, 45, &[]);
        let title = build_ranking_title("day", 2);

        assert_eq!(
            build_ranking_caption(&title, 0, &illust),
            "📊 *DAY Ranking* \\- 2 new\\!\n\nStill\nby *Author* \\(ID: `67890`\\)\n\n❤️ 45 \\| 🔗 [来源](https://pixiv\\.net/artworks/12345)"
        );
    }

    #[test]
    fn build_ranking_caption_for_non_first_ugoira_matches_golden_output() {
        let illust = make_illust("ugoira", "Animated", "Author", 1, 123, 45, &[]);

        assert_eq!(
            build_ranking_caption("ignored", 1, &illust),
            "🎞️ Animated\nby *Author* \\(ID: `67890`\\)\n\n❤️ 45 \\| 🔗 [来源](https://pixiv\\.net/artworks/12345)"
        );
    }

    #[test]
    fn caption_builders_escape_markdown_sensitive_text() {
        let illust = make_illust("illust", "_[]()!", "A_B(C)!", 1, 123, 45, &["tag(test)"]);

        assert_eq!(
            build_illust_caption(&illust),
            "🎨 \\_\\[\\]\\(\\)\\!\nby *A\\_B\\(C\\)\\!* \\(ID: `67890`\\)\n\n👀 123 \\| ❤️ 45 \\| 🔗 [来源](https://pixiv\\.net/artworks/12345)\n\n\\#tagtest"
        );
    }

    fn make_booru_post(
        id: u64,
        tags: &str,
        score: i32,
        fav_count: i32,
        rating: booru_client::BooruRating,
    ) -> booru_client::BooruPost {
        booru_client::BooruPost {
            id,
            tags: tags.to_string(),
            score,
            fav_count,
            file_url: Some("https://example.com/file.jpg".to_string()),
            sample_url: Some("https://example.com/sample.jpg".to_string()),
            preview_url: Some("https://example.com/preview.jpg".to_string()),
            rating,
            width: 1920,
            height: 1080,
            md5: None,
            source: None,
            created_at: None,
            file_size: None,
            file_ext: None,
            status: None,
        }
    }

    #[test]
    fn build_booru_caption_moebooru_post_url() {
        let post = make_booru_post(
            12345,
            "landscape sky",
            100,
            50,
            booru_client::BooruRating::Safe,
        );
        let caption = build_booru_caption(
            &post,
            "konachan",
            "https://konachan.com",
            booru_client::BooruEngineType::Moebooru,
        );
        assert!(caption.contains("konachan"));
        assert!(caption.contains("\\#landscape"));
        assert!(caption.contains("\\#sky"));
        assert!(caption.contains("/post/show/12345"));
        assert!(caption.contains("🟢"));
    }

    #[test]
    fn build_booru_caption_danbooru_post_url() {
        let post = make_booru_post(99, "art", 10, 5, booru_client::BooruRating::Questionable);
        let caption = build_booru_caption(
            &post,
            "danbooru",
            "https://danbooru.donmai.us",
            booru_client::BooruEngineType::Danbooru,
        );
        assert!(caption.contains("/posts/99"));
        assert!(caption.contains("🟡"));
    }

    #[test]
    fn build_booru_caption_gelbooru_post_url_escapes_query_string() {
        let post = make_booru_post(42, "test", 0, 0, booru_client::BooruRating::Explicit);
        let caption = build_booru_caption(
            &post,
            "gelbooru",
            "https://gelbooru.com",
            booru_client::BooruEngineType::Gelbooru,
        );
        assert!(caption.contains("page=post"));
        assert!(caption.contains("id=42"));
        assert!(caption.contains("🔴"));
    }

    #[test]
    fn build_booru_caption_escapes_markdown_in_tags() {
        let post = make_booru_post(
            1,
            "tag-with-dash tag_underscore",
            5,
            3,
            booru_client::BooruRating::General,
        );
        let caption = build_booru_caption(
            &post,
            "test_site",
            "https://example.com",
            booru_client::BooruEngineType::Moebooru,
        );
        assert!(caption.contains("\\#tagwithdash"));
        assert!(caption.contains("\\#tag\\_underscore"));
        assert!(caption.contains("test\\_site"));
    }

    #[test]
    fn build_booru_caption_escapes_negative_score() {
        let post = make_booru_post(1, "test", -5, 0, booru_client::BooruRating::Safe);
        let caption = build_booru_caption(
            &post,
            "danbooru",
            "https://danbooru.donmai.us",
            booru_client::BooruEngineType::Danbooru,
        );
        // The `-` in `-5` must be escaped for MarkdownV2
        assert!(caption.contains("\\-5"));
    }
}
