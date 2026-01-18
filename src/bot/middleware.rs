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
    ctx.chat_enabled() || (chat_id.is_user() && ctx.user_role().is_admin())
}

// ============================================================================
// 可测试的辅助函数
// ============================================================================

/// 判断命令是否应该被接受（基于 @mention 要求）
///
/// 此函数封装了 mention 过滤的核心逻辑，便于单元测试。
///
/// # 参数
/// - `is_private`: 是否为私聊
/// - `require_mention_in_group`: 全局配置是否要求群组中 @bot
/// - `allow_without_mention`: chat 级别设置是否允许不带 @bot
/// - `is_bare_command`: 命令是否为裸命令（不带 @bot）
///
/// # 返回
/// - `true`: 命令应该被接受
/// - `false`: 命令应该被过滤
#[inline]
fn should_accept_command(
    is_private: bool,
    require_mention_in_group: bool,
    allow_without_mention: bool,
    is_bare_command: bool,
) -> bool {
    // 私聊：总是接受
    if is_private {
        return true;
    }

    // 群组：全局配置不需要 @bot，接受所有格式
    if !require_mention_in_group {
        return true;
    }

    // 全局配置需要 @bot，检查 chat 级别设置
    if allow_without_mention {
        // chat 允许不带 @bot
        return true;
    }

    // 需要 @bot：只接受带 @bot 的命令
    !is_bare_command
}

/// 判断消息是否应该被处理（基于 @mention 要求）
///
/// 此函数封装了消息 mention 过滤的核心逻辑，便于单元测试。
///
/// # 参数
/// - `is_private`: 是否为私聊
/// - `require_mention_in_group`: 全局配置是否要求群组中 @bot
/// - `allow_without_mention`: chat 级别设置是否允许不带 @bot
/// - `is_reply_to_bot`: 消息是否为回复 bot 的消息
///
/// # 返回
/// - `true`: 消息应该被处理
/// - `false`: 消息应该被忽略
#[inline]
fn should_process_message(
    is_private: bool,
    require_mention_in_group: bool,
    allow_without_mention: bool,
    is_reply_to_bot: bool,
) -> bool {
    // 私聊：总是处理
    if is_private {
        return true;
    }

    // 群组：全局配置不需要 @bot，处理所有消息
    if !require_mention_in_group {
        return true;
    }

    // 全局配置需要 @bot，检查 chat 级别设置
    if allow_without_mention {
        // chat 允许不带 @bot
        return true;
    }

    // 需要 @bot：只处理回复 bot 的消息
    is_reply_to_bot
}

// ============================================================================
// 消息过滤器
// ============================================================================

/// 混合命令过滤器
///
/// 根据聊天类型应用不同的命令解析策略：
/// - **私聊**: 接受 `/cmd` 和 `/cmd@bot` (宽松)
/// - **群组**:
///   - 如果全局 `require_mention_in_group` 为 false: 接受 `/cmd` 和 `/cmd@bot` (宽松)
///   - 如果全局 `require_mention_in_group` 为 true: 先宽松解析，由后续过滤器根据 chat 设置决定
///
/// **依赖要求:**
/// - `Message` - 当前消息
/// - `Me` - Bot 信息
/// - `String` - 消息文本
/// - `BotHandler` - Bot 处理器（获取配置）
///
/// **注入依赖:**
/// - `C` - 解析后的命令类型
#[must_use]
pub fn filter_hybrid_command<C, Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    C: BotCommands + Send + Sync + 'static,
    Output: Send + Sync + 'static,
{
    dptree::filter_map(
        move |message: Message, me: Me, text: String, handler: super::BotHandler| {
            let bot_name = me.user.username.expect("Bots must have a username");

            // 解析命令（验证格式正确性）
            let cmd = C::parse(&text, &bot_name).ok()?;

            // 私聊：接受所有格式
            if message.chat.is_private() {
                return Some(cmd);
            }

            // 群组：根据全局配置决定
            if !handler.require_mention_in_group {
                // 全局配置不需要 @bot，接受所有格式
                return Some(cmd);
            }

            // 全局配置需要 @bot，但需要考虑 chat 级别设置
            // 这里先宽松解析，由后续 filter_mention_requirement 根据 chat 设置过滤
            Some(cmd)
        },
    )
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
/// - `Me` - Bot 信息
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

            let should_accept = should_accept_command(
                message.chat.is_private(),
                handler.require_mention_in_group,
                ctx.chat.allow_without_mention,
                is_bare_command,
            );

            should_accept.then_some(cmd)
        },
    )
}

/// 过滤相关消息
///
/// 根据聊天类型判断消息是否需要处理：
/// - **私聊**: 总是相关
/// - **群组**:
///   - 如果全局 `require_mention_in_group` 为 false: 所有消息都相关
///   - 如果全局 `require_mention_in_group` 为 true: 先宽松检查，由后续过滤器根据 chat 设置决定
///
/// **依赖要求:**
/// - `Message` - 当前消息
/// - `Me` - Bot 信息
/// - `BotHandler` - Bot 处理器（获取配置）
#[must_use]
pub fn filter_relevant_message<Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    Output: Send + Sync + 'static,
{
    dptree::filter_map(move |message: Message, handler: super::BotHandler| {
        // 私聊总是处理
        if message.chat.is_private() {
            return Some(message);
        }

        // 群组：根据全局配置决定
        if !handler.require_mention_in_group {
            // 不需要 @bot，处理所有消息
            return Some(message);
        }

        // 全局配置需要 @bot，但需要考虑 chat 级别设置
        // 这里先宽松检查，由后续 filter_message_mention_requirement 根据 chat 设置过滤
        Some(message)
    })
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

            let should_process = should_process_message(
                message.chat.is_private(),
                handler.require_mention_in_group,
                ctx.chat.allow_without_mention,
                is_reply_to_bot,
            );

            if should_process {
                message.text().map(|t| t.to_string())
            } else {
                None
            }
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ========================================================================
    // should_accept_command 测试
    // ========================================================================

    #[test]
    fn test_command_private_chat_always_accepted() {
        // 私聊中，无论什么配置，命令都应该被接受
        assert!(should_accept_command(true, true, false, true)); // 裸命令
        assert!(should_accept_command(true, true, false, false)); // 带 @bot
        assert!(should_accept_command(true, false, false, true));
        assert!(should_accept_command(true, false, true, true));
    }

    #[test]
    fn test_command_group_global_not_required() {
        // 全局配置不需要 @bot 时，群组中所有命令都被接受
        assert!(should_accept_command(false, false, false, true)); // 裸命令
        assert!(should_accept_command(false, false, false, false)); // 带 @bot
        assert!(should_accept_command(false, false, true, true)); // chat 设置被忽略
    }

    #[test]
    fn test_command_group_global_required_chat_allows() {
        // 全局要求 @bot，但 chat 允许不带 @bot
        assert!(should_accept_command(false, true, true, true)); // 裸命令被接受
        assert!(should_accept_command(false, true, true, false)); // 带 @bot 也被接受
    }

    #[test]
    fn test_command_group_global_required_chat_not_allows() {
        // 全局要求 @bot，chat 也不允许不带 @bot
        assert!(!should_accept_command(false, true, false, true)); // 裸命令被过滤
        assert!(should_accept_command(false, true, false, false)); // 带 @bot 被接受
    }

    // ========================================================================
    // should_process_message 测试
    // ========================================================================

    #[test]
    fn test_message_private_chat_always_processed() {
        // 私聊中，消息都应该被处理
        assert!(should_process_message(true, true, false, false));
        assert!(should_process_message(true, true, false, true));
        assert!(should_process_message(true, false, false, false));
    }

    #[test]
    fn test_message_group_global_not_required() {
        // 全局配置不需要 @bot 时，群组中所有消息都被处理
        assert!(should_process_message(false, false, false, false));
        assert!(should_process_message(false, false, false, true));
        assert!(should_process_message(false, false, true, false));
    }

    #[test]
    fn test_message_group_global_required_chat_allows() {
        // 全局要求 @bot，但 chat 允许不带 @bot
        assert!(should_process_message(false, true, true, false)); // 普通消息被处理
        assert!(should_process_message(false, true, true, true)); // 回复消息也被处理
    }

    #[test]
    fn test_message_group_global_required_chat_not_allows() {
        // 全局要求 @bot，chat 也不允许不带 @bot
        assert!(!should_process_message(false, true, false, false)); // 普通消息被忽略
        assert!(should_process_message(false, true, false, true)); // 回复 bot 的消息被处理
    }
}
