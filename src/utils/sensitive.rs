use super::tag::{normalize_tag, pixiv_tag_names};
use crate::db::entities::chats;
use booru_client::BooruRating;
use pixiv_client::Illust;
use std::collections::HashSet;

/// Check if illust contains any sensitive tags (normalized match, case-insensitive)
pub fn contains_sensitive_tags(illust: &Illust, sensitive_tags: &[String]) -> bool {
    let illust_tags: Vec<String> = pixiv_tag_names(illust).map(normalize_tag).collect();

    for sensitive_tag in sensitive_tags {
        let sensitive_normalized = normalize_tag(sensitive_tag);
        if illust_tags.iter().any(|t| t == &sensitive_normalized) {
            return true;
        }
    }

    false
}

pub fn should_blur(chat: &chats::Model, illust: &Illust) -> bool {
    chat.blur_sensitive_tags && contains_sensitive_tags(illust, &chat.sensitive_tags)
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
    let sensitive_set: HashSet<String> = chat
        .sensitive_tags
        .iter()
        .map(|s| normalize_tag(s))
        .collect();
    tags.split_whitespace()
        .map(normalize_tag)
        .any(|pt| sensitive_set.contains(&pt))
}
