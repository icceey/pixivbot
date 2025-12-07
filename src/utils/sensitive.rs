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
