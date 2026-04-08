use super::tag::normalize_tag;
use crate::db::entities::chats;
use pixiv_client::Illust;

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

#[cfg(test)]
mod tests {
    use super::{contains_sensitive_tags, should_blur};
    use crate::db::types::Tags;
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
    fn contains_sensitive_tags_matches_normalized_tags() {
        let illust = make_illust(&["R-18"]);
        assert!(contains_sensitive_tags(&illust, &["r18".to_string()]));
    }

    #[test]
    fn should_blur_returns_false_when_blur_is_disabled() {
        let chat = make_chat(false, &["R-18"]);
        let illust = make_illust(&["R-18"]);
        assert!(!should_blur(&chat, &illust));
    }

    #[test]
    fn should_blur_returns_false_when_no_sensitive_match_exists() {
        let chat = make_chat(true, &["R-18"]);
        let illust = make_illust(&["landscape"]);
        assert!(!should_blur(&chat, &illust));
    }

    #[test]
    fn should_blur_returns_true_when_sensitive_match_exists() {
        let chat = make_chat(true, &["R-18"]);
        let illust = make_illust(&["r18"]);
        assert!(should_blur(&chat, &illust));
    }
}
