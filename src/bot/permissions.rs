//! Chat-local Telegram privileges must never be written to the global user role.
use super::notifier::ThrottledBot;
use crate::db::repo::Repo;
use teloxide::prelude::*;
use teloxide::types::{Chat, User};
use tracing::error;

/// `from` on a sender_chat message may be a compatibility bot, not the actor.
/// Forward origins are deliberately irrelevant: the sender is the actor.
pub(super) fn message_actor(msg: &Message) -> Option<&User> {
    if msg.sender_chat.is_some() {
        return None;
    }
    msg.from.as_ref().filter(|user| !user.is_bot)
}

pub(super) async fn can_manage_chat(
    bot: &ThrottledBot,
    repo: &Repo,
    chat: &Chat,
    user_id: UserId,
) -> bool {
    match repo.get_user(user_id.0 as i64).await {
        Ok(Some(user)) if user.role.is_admin() => return true,
        Ok(_) => {}
        Err(e) => {
            error!("Failed to check global role: {:#}", e);
            return false;
        }
    }
    if !(chat.is_group() || chat.is_supergroup()) {
        return false;
    }
    match bot.get_chat_member(chat.id, user_id).await {
        Ok(member) => member.user.id == user_id && member.is_privileged(),
        Err(e) => {
            error!("Failed to check chat administrator: {:#}", e);
            false
        }
    }
}
