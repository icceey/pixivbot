use super::tag::normalize_tag;
use crate::db::entities::chats;
use booru_client::BooruRating;
use pixiv_client::Illust;
use std::collections::HashSet;

/// Get sensitive tags list from chat settings
pub fn get_chat_sensitive_tags(chat: &chats::Model) -> &[String] {
    &chat.sensitive_tags
}

/// Check if illust contains any sensitive tags (normalized match, case-insensitive)
pub fn contains_sensitive_tags(illust: &Illust, sensitive_tags: &[String]) -> bool {
    let illust_tags: Vec<String> = illust
        .tags
        .iter()
        .map(|tag| normalize_tag(&tag.name))
        .collect();

    for sensitive_tag in sensitive_tags {
        let sensitive_normalized = normalize_tag(sensitive_tag);
        if illust_tags.iter().any(|t| t == &sensitive_normalized) {
            return true;
        }
    }

    false
}

pub fn should_blur(chat: &chats::Model, illust: &Illust) -> bool {
    chat.blur_sensitive_tags && contains_sensitive_tags(illust, get_chat_sensitive_tags(chat))
}

pub fn should_blur_booru(chat: &chats::Model, tags: &str, rating: BooruRating) -> bool {
    if !chat.blur_sensitive_tags {
        return false;
    }
    match rating {
        BooruRating::General | BooruRating::Safe => tags_match_sensitive(chat, tags),
        BooruRating::Sensitive | BooruRating::Questionable | BooruRating::Explicit => true,
    }
}

fn tags_match_sensitive(chat: &chats::Model, tags: &str) -> bool {
    let sensitive_set: HashSet<String> = get_chat_sensitive_tags(chat)
        .iter()
        .map(|s| normalize_tag(s))
        .collect();
    tags.split_whitespace()
        .map(normalize_tag)
        .any(|pt| sensitive_set.contains(&pt))
}

#[cfg(test)]
mod tests {
    use super::{should_blur, should_blur_booru};
    use crate::db::entities::chats;
    use crate::db::types::Tags;
    use booru_client::BooruRating;
    use pixiv_client::Illust;
    use serde_json::json;

    fn make_chat(blur_sensitive_tags: bool, sensitive_tags: &[&str]) -> chats::Model {
        chats::Model {
            id: 1,
            r#type: "private".to_string(),
            title: Some("test".to_string()),
            enabled: true,
            blur_sensitive_tags,
            excluded_tags: Tags::default(),
            sensitive_tags: Tags(sensitive_tags.iter().map(|s| s.to_string()).collect()),
            created_at: chrono::Utc::now().naive_utc(),
            allow_without_mention: false,
        }
    }

    fn make_illust(tags: &[&str]) -> Illust {
        serde_json::from_value(json!({
            "id": 12345,
            "title": "Title",
            "type": "illust",
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
                "name": "Author",
                "account": "author"
            },
            "tags": tags
                .iter()
                .map(|name| json!({ "name": name, "translated_name": null }))
                .collect::<Vec<_>>(),
            "create_date": "2026-01-01T00:00:00+00:00",
            "page_count": 1,
            "width": 100,
            "height": 100,
            "sanity_level": 2,
            "x_restrict": 0,
            "series": null,
            "meta_single_page": {
                "original_image_url": "original"
            },
            "meta_pages": [],
            "total_view": 1,
            "total_bookmarks": 2,
            "is_bookmarked": false,
            "visible": true,
            "is_muted": false,
            "total_comments": 0
        }))
        .unwrap()
    }

    #[test]
    fn pixiv_blur_requires_enabled_setting_and_normalized_tag_match() {
        for (enabled, tags, expected) in [
            (false, "R-18", false),
            (true, "landscape", false),
            (true, "r18", true),
        ] {
            assert_eq!(
                should_blur(&make_chat(enabled, &["R-18"]), &make_illust(&[tags])),
                expected,
                "enabled={enabled}, tags={tags}"
            );
        }
    }

    #[test]
    fn booru_blur_combines_chat_setting_rating_and_tags() {
        for (rating, expected) in [
            (BooruRating::Safe, false),
            (BooruRating::General, false),
            (BooruRating::Sensitive, true),
            (BooruRating::Questionable, true),
            (BooruRating::Explicit, true),
        ] {
            assert!(
                !should_blur_booru(&make_chat(false, &["nude"]), "nude", rating),
                "{rating:?}"
            );
            assert!(
                should_blur_booru(&make_chat(true, &["nude"]), "nude landscape", rating),
                "{rating:?}"
            );
            assert_eq!(
                should_blur_booru(&make_chat(true, &["nude"]), "landscape sky", rating),
                expected,
                "{rating:?}"
            );
        }
    }
}
