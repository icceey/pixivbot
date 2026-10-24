use crate::db::entities::{chats, users};
use crate::db::repo::Repo;
use crate::db::types::{Tags, UserRole};
use anyhow::{Context, Result};
use std::sync::Arc;
use teloxide::dispatching::DpHandlerDescription;
use teloxide::dptree::{self, Handler};
use teloxide::prelude::*;
use teloxide::types::{Me, MessageEntityKind, MessageEntityRef};
use teloxide::utils::command::BotCommands;
use tracing::{error, info};

// ============================================================================
// UserChatContext - 用户和聊天上下文
// ============================================================================

/// 用户和聊天上下文，在中间件中填充，传递给后续处理器
#[derive(Clone, Debug)]
pub struct UserChatContext {
    pub user: users::Model,
    pub chat: chats::Model,
}

// ============================================================================
// 中间件过滤器
// ============================================================================

/// 确保用户和聊天存在于数据库
///
/// 从消息中提取用户和聊天信息，在数据库中创建或更新记录，
/// 然后将 `UserChatContext` 注入到依赖链中供后续处理器使用。
///
/// **依赖要求:**
/// - `Message` - 当前消息
/// - `Arc<Repo>` - 数据库仓库
/// - `BotHandler` - Bot 处理器（获取配置）
///
/// **注入依赖:**
/// - `UserChatContext` - 用户和聊天上下文
#[must_use]
pub fn filter_user_chat<Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    Output: Send + Sync + 'static,
{
    dptree::filter_map_async(
        move |message: Message, repo: Arc<Repo>, handler: super::BotHandler| async move {
            match ensure_user_and_chat(&message, &repo, &handler).await {
                Ok(ctx) => {
                    info!(
                        "User {} (role: {:?}) in chat {} (enabled: {})",
                        ctx.user.id, ctx.user.role, ctx.chat.id, ctx.chat.enabled
                    );
                    Some(ctx)
                }
                Err(e) => {
                    error!("Failed to ensure user/chat: {:#}", e);
                    None
                }
            }
        },
    )
}

/// 检查聊天是否可访问
///
/// 基于用户角色和聊天状态判断是否允许处理该消息：
/// - 聊天已启用 → 允许
/// - 私聊 + Admin/Owner → 允许
/// - 其他情况 → 过滤掉
///
/// **依赖要求:**
/// - `UserChatContext` - 用户和聊天上下文
/// - `Message` - 当前消息（用于获取 chat_id）
#[must_use]
pub fn filter_chat_accessible<Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    Output: Send + Sync + 'static,
{
    dptree::filter(move |ctx: UserChatContext, msg: Message| is_chat_accessible(msg.chat.id, &ctx))
}

// ============================================================================
// 辅助函数
// ============================================================================

/// 确保用户和聊天在数据库中存在
///
/// 如果用户或聊天不存在，则创建新记录；Owner 用户会被自动识别。
/// 如果消息没有用户信息，返回错误。
async fn ensure_user_and_chat(
    msg: &Message,
    repo: &Repo,
    handler: &super::BotHandler,
) -> Result<UserChatContext> {
    let user = super::permissions::message_actor(msg)
        .context("Message has no identifiable human sender")?;
    let chat_id = msg.chat.id.0;
    let chat_type = match msg.chat.is_group() || msg.chat.is_supergroup() {
        true => "group",
        false => "private",
    };
    let chat_title = msg.chat.title().map(|s| s.to_string());

    // Convert default sensitive tags to Tags for new chats
    let default_sensitive_tags = Tags::from(handler.default_sensitive_tags.clone());

    // Upsert chat - new chats get enabled status based on bot mode
    let chat = repo
        .upsert_chat(
            chat_id,
            chat_type.to_string(),
            chat_title,
            handler.is_public_mode,
            default_sensitive_tags,
        )
        .await
        .context("Failed to upsert chat")?;

    // Get or create the identified sender.
    let user_id = user.id.0 as i64;
    let username = user.username.clone();

    // Check if user already exists
    let user_model = match repo.get_user(user_id).await.context("Failed to get user")? {
        Some(existing_user) if existing_user.username == username => existing_user,
        Some(existing_user) => repo
            .upsert_user(user_id, username, existing_user.role)
            .await
            .context("Failed to refresh username")?,
        None => {
            // New user - determine role
            if handler.owner_id == Some(user_id) {
                // If owner_id is configured and matches this user, assign Owner role
                repo.upsert_user(user_id, username, UserRole::Owner)
                    .await
                    .context("Failed to create owner user")?
            } else if handler.owner_id.is_none() {
                // Auto-assignment logic: use database transaction for atomicity
                let user = repo
                    .create_user_with_auto_owner(user_id, username)
                    .await
                    .context("Failed to create user with auto-owner check")?;

                if user.role.is_owner() {
                    info!(
                        "No owner configured and no owner exists, assigned owner role to first user {}",
                        user_id
                    );
                }

                user
            } else {
                // Regular user
                repo.upsert_user(user_id, username, UserRole::User)
                    .await
                    .context("Failed to create user")?
            }
        }
    };

    Ok(UserChatContext {
        user: user_model,
        chat,
    })
}

/// 检查聊天是否可访问
fn is_chat_accessible(chat_id: ChatId, ctx: &UserChatContext) -> bool {
    // 聊天已启用或私聊 Admin/Owner
    ctx.chat.enabled || (chat_id.is_user() && ctx.user.role.is_admin())
}

fn message_mentions_bot(message: &Message, me: &Me) -> bool {
    let Some(text) = message.text() else {
        return false;
    };
    let Some(entities) = message.entities() else {
        return false;
    };

    let bot_mention = me
        .user
        .username
        .as_ref()
        .map(|username| format!("@{username}"));

    MessageEntityRef::parse(text, entities)
        .into_iter()
        .any(|entity| match entity.kind() {
            MessageEntityKind::Mention => bot_mention
                .as_ref()
                .is_some_and(|mention| entity.text().eq_ignore_ascii_case(mention)),
            MessageEntityKind::TextMention { user } => user.id == me.user.id,
            _ => false,
        })
}

// ============================================================================
// 消息过滤器
// ============================================================================

/// 解析裸命令及发给当前 Bot 的命令；mention 要求由聊天设置过滤器判断。
#[must_use]
pub fn filter_hybrid_command<C, Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    C: BotCommands + Send + Sync + 'static,
    Output: Send + Sync + 'static,
{
    dptree::filter_map(move |me: Me, text: String| {
        let bot_name = me.user.username.expect("Bots must have a username");
        C::parse(&text, &bot_name).ok()
    })
}

/// 检查命令的 @mention 要求
///
/// 此过滤器在 `filter_user_chat` 之后执行，可以访问 chat 设置。
/// 根据全局 `require_mention_in_group` 和 chat 级别的 `allow_without_mention` 设置决定是否过滤裸命令。
///
/// **依赖要求:**
/// - `UserChatContext` - 用户和聊天上下文
/// - `C` - 解析后的命令
/// - `Message` - 当前消息
/// - `BotHandler` - Bot 处理器（获取配置）
///
/// **注入依赖:**
/// - `C` - 解析后的命令类型（过滤后）
#[must_use]
pub fn filter_mention_requirement<C, Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    C: BotCommands + Send + Sync + Clone + 'static,
    Output: Send + Sync + 'static,
{
    dptree::filter_map(
        move |ctx: UserChatContext,
              cmd: C,
              message: Message,
              text: String,
              handler: super::BotHandler| {
            // 判断是否为裸命令（不带 @bot）
            // 用空字符串作为 bot_name 解析：
            // - 裸命令 "/start" 解析成功 → is_bare_command = true
            // - 带 @bot 的命令 "/start@mybot" 解析失败 → is_bare_command = false
            let is_bare_command = C::parse(&text, "").is_ok();

            let should_accept = message.chat.is_private()
                || !handler.require_mention_in_group
                || ctx.chat.allow_without_mention
                || !is_bare_command;

            should_accept.then_some(cmd)
        },
    )
}

/// 检查消息的 @mention 要求
///
/// 此过滤器在 `filter_user_chat` 之后执行，可以访问 chat 设置。
/// 根据全局 `require_mention_in_group` 和 chat 级别的 `allow_without_mention` 设置决定是否过滤消息。
///
/// **依赖要求:**
/// - `UserChatContext` - 用户和聊天上下文
/// - `Message` - 当前消息
/// - `Me` - Bot 信息
/// - `BotHandler` - Bot 处理器（获取配置）
///
/// **注入依赖:**
/// - `String` - 消息文本（用于后续处理）
#[must_use]
pub fn filter_message_mention_requirement<Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    Output: Send + Sync + 'static,
{
    dptree::filter_map(
        move |ctx: UserChatContext, message: Message, me: Me, handler: super::BotHandler| {
            // 检查是否回复 Bot
            let is_reply_to_bot = message
                .reply_to_message()
                .and_then(|reply| reply.from.as_ref())
                .map(|user| user.id == me.user.id)
                .unwrap_or(false);

            let is_mentioned = message_mentions_bot(&message, &me);

            let should_process = message.chat.is_private()
                || !handler.require_mention_in_group
                || ctx.chat.allow_without_mention
                || is_reply_to_bot
                || is_mentioned;

            if should_process {
                message.text().map(|t| t.to_string())
            } else {
                None
            }
        },
    )
}
