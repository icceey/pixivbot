use crate::db::entities::{chats, users};
use crate::db::repo::Repo;
use crate::db::types::{Tags, UserRole};
use anyhow::{Context, Result};
use std::sync::Arc;
use teloxide::dispatching::DpHandlerDescription;
use teloxide::dptree::{self, Handler};
use teloxide::prelude::*;
use teloxide::types::Me;
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

impl UserChatContext {
    pub fn user_role(&self) -> &UserRole {
        &self.user.role
    }

    pub fn chat_enabled(&self) -> bool {
        self.chat.enabled
    }
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

    // Get or create user - require user info to exist
    let user = msg
        .from
        .as_ref()
        .context("Message has no user information")?;

    let user_id = user.id.0 as i64;
    let username = user.username.clone();

    // Check if user already exists
    let user_model = match repo.get_user(user_id).await.context("Failed to get user")? {
        Some(existing_user) => existing_user,
        None => {
            // New user - determine role
            let role = if handler.owner_id == Some(user_id) {
                UserRole::Owner
            } else {
                UserRole::User
            };

            info!("Creating new user {} with role {:?}", user_id, role);

            repo.upsert_user(user_id, username, role)
                .await
                .context("Failed to upsert user")?
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
    ctx.chat_enabled() || (chat_id.is_user() && ctx.user_role().is_admin())
}

// ============================================================================
// 消息过滤器
// ============================================================================

/// 混合命令过滤器
///
/// 根据聊天类型应用不同的命令解析策略：
/// - **私聊**: 接受 `/cmd` 和 `/cmd@bot` (宽松)
/// - **群组**: 只接受 `/cmd@bot` (严格)
///
/// **依赖要求:**
/// - `Message` - 当前消息
/// - `Me` - Bot 信息
/// - `String` - 消息文本
///
/// **注入依赖:**
/// - `C` - 解析后的命令类型
#[must_use]
pub fn filter_hybrid_command<C, Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    C: BotCommands + Send + Sync + 'static,
    Output: Send + Sync + 'static,
{
    dptree::filter_map(move |message: Message, me: Me, text: String| {
        let bot_name = me.user.username.expect("Bots must have a username");

        // 解析命令（验证格式正确性）
        let cmd = C::parse(&text, &bot_name).ok()?;

        // 私聊：接受所有格式
        if message.chat.is_private() {
            return Some(cmd);
        }

        // 群组：只接受带 @bot 的命令
        // 技巧：用空字符串解析，如果成功说明是裸命令（如 /start），失败说明带点名（如 /start@bot）
        let is_bare_command = C::parse(&text, "").is_ok();
        (!is_bare_command).then_some(cmd)
    })
}

/// 过滤相关消息
///
/// 根据聊天类型判断消息是否需要处理：
/// - **私聊**: 总是相关
/// - **群组**: 只有 @Bot 或回复 Bot 的消息才相关
///
/// **依赖要求:**
/// - `Message` - 当前消息
/// - `Me` - Bot 信息
/// - `String` - 消息文本
#[must_use]
pub fn filter_relevant_message<Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    Output: Send + Sync + 'static,
{
    dptree::filter_map(move |message: Message, me: Me| {
        // 私聊总是处理
        if message.chat.is_private() {
            return Some(message);
        }

        // 群组：检查是否回复 Bot
        let is_reply_to_bot = message
            .reply_to_message()
            .and_then(|reply| reply.from.as_ref())
            .map(|user| user.id == me.user.id)
            .unwrap_or(false);

        is_reply_to_bot.then_some(message)
    })
}
