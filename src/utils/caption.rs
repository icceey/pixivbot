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

    let metrics = if engine_type.supports_fav_count() {
        format!(
            "⭐ {} \\| ❤️ {} \\|",
            markdown::escape(&post.score.to_string()),
            markdown::escape(&post.fav_count.to_string())
        )
    } else {
        format!("⭐ {} \\|", markdown::escape(&post.score.to_string()))
    };

    format!(
        "🏷 *{}* \\| {}\n\n{} {} {} \\| 🔗 [来源]({}){}\n",
        markdown::escape(site_name),
        markdown::escape(&format!("#{}", post.id)),
        metrics,
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
