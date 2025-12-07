pub mod commands;
mod handler;
mod handlers;
pub mod link_handler;
pub mod notifier;

use crate::config::TelegramConfig;
use crate::db::repo::Repo;
use crate::db::types::UserRole;
use crate::pixiv::client::PixivClient;
use anyhow::Result;
use std::sync::Arc;
use teloxide::dispatching::{Dispatcher, DpHandlerDescription, UpdateFilterExt};
use teloxide::dptree;
use teloxide::prelude::*;
use teloxide::types::{BotCommandScope, Me};
use teloxide::utils::command::BotCommands;
use tracing::info;

pub use commands::Command;
pub use handler::BotHandler;

/// Handler 返回类型
type HandlerResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

pub async fn run(
    bot: Bot,
    config: TelegramConfig,
    repo: Arc<Repo>,
    pixiv_client: Arc<tokio::sync::RwLock<PixivClient>>,
    notifier: notifier::Notifier,
    sensitive_tags: Vec<String>,
) -> Result<()> {
    info!("Starting Telegram Bot...");

    // Parse bot mode from config
    let is_public_mode = config.bot_mode.is_public();

    info!(
        "Bot mode: {:?} (new chats will be {} by default)",
        config.bot_mode,
        if is_public_mode {
            "enabled"
        } else {
            "disabled"
        }
    );

    let handler = BotHandler::new(
        repo.clone(),
        pixiv_client.clone(),
        notifier.clone(),
        sensitive_tags,
        config.owner_id,
        is_public_mode,
    );

    info!("✅ Bot initialized, starting command handler");

    // 设置命令可见性
    setup_commands(&bot, &repo).await;

    // 构建 handler 树
    let handler_tree = build_handler_tree();

    // 使用 Dispatcher
    Dispatcher::builder(bot, handler_tree)
        .dependencies(dptree::deps![handler, repo, notifier])
        .default_handler(|_| async {})
        .enable_ctrlc_handler()
        .build()
        .dispatch()
        .await;

    Ok(())
}

/// 构建消息处理树
fn build_handler_tree(
) -> teloxide::dispatching::UpdateHandler<Box<dyn std::error::Error + Send + Sync + 'static>> {
    let command_handler = Message::filter_text()
        .chain(filter_hybrid_command::<Command, HandlerResult>())
        .endpoint(handle_command);

    let message_handler = Message::filter_text()
        .chain(filter_relevant_message::<HandlerResult>())
        .endpoint(handle_message);

    dptree::entry()
        .chain(Update::filter_message())
        .branch(command_handler)
        .branch(message_handler)
}

/// 处理命令
async fn handle_command(
    bot: Bot,
    msg: Message,
    cmd: Command,
    handler: BotHandler,
) -> HandlerResult {
    handler.handle_command(bot, msg, cmd).await?;
    Ok(())
}

/// 处理普通消息（检查 Pixiv 链接）
async fn handle_message(
    bot: Bot,
    msg: Message,
    handler: BotHandler,
    text: String,
) -> HandlerResult {
    handler.handle_message(bot, msg, &text).await?;
    Ok(())
}

/// 设置命令可见性
/// - 普通用户看到基础命令
/// - 数据库中的 Admin 用户看到管理员命令
/// - 数据库中的 Owner 用户看到所有命令
async fn setup_commands(bot: &Bot, repo: &Repo) {
    // 1. 设置默认命令（所有用户都能看到的基础命令）
    if let Err(e) = bot
        .set_my_commands(Command::user_commands())
        .scope(BotCommandScope::Default)
        .await
    {
        tracing::warn!("Failed to set default commands: {:#}", e);
    } else {
        info!("✅ Set default commands for all users");
    }

    // 2. 从数据库获取所有管理员用户，为他们设置对应的命令可见性
    match repo.get_admin_users().await {
        Ok(admin_users) => {
            for user in admin_users {
                let commands = match user.role {
                    UserRole::Owner => Command::owner_commands(),
                    UserRole::Admin => Command::admin_commands(),
                    UserRole::User => continue, // 不应该出现，但以防万一
                };

                if let Err(e) = bot
                    .set_my_commands(commands)
                    .scope(BotCommandScope::Chat {
                        chat_id: teloxide::types::Recipient::Id(teloxide::types::ChatId(user.id)),
                    })
                    .await
                {
                    tracing::warn!(
                        "Failed to set commands for {:?} {}: {:#}",
                        user.role,
                        user.id,
                        e
                    );
                } else {
                    info!("✅ Set {:?} commands for user_id: {}", user.role, user.id);
                }
            }
        }
        Err(e) => {
            tracing::warn!("Failed to get admin users from database: {:#}", e);
        }
    }
}

/// 一个混合命令过滤器：
/// - 私聊：接受 `/cmd` 和 `/cmd@bot` (宽松模式)
/// - 群组：只接受 `/cmd@bot` (严格模式)
///
/// 依赖要求：
/// - [`teloxide::types::Message`]
/// - [`teloxide::types::Me`] (Teloxide Dispatcher 默认会提供)
#[must_use]
pub fn filter_hybrid_command<C, Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    C: BotCommands + Send + Sync + 'static,
    Output: Send + Sync + 'static,
{
    dptree::filter_map(move |message: Message, me: Me, text: String| {
        let bot_name = me.user.username.expect("Bots must have a username");
        // let text =  message.text().or_else(|| message.caption())?;

        // 2. 尝试用标准方式解析 (这一步验证命令是否存在，且格式正确)
        // 此时 /start 和 /start@mybot 都会解析成功
        let cmd = C::parse(&text, &bot_name).ok()?;

        // 3. 如果是私聊，直接通过，不纠结是否有点名
        if message.chat.is_private() {
            return Some(cmd);
        }

        // 4.如果是群组，应用严格模式：必须是显式点名
        // 技巧：尝试用空字符串作为 bot_name 去解析。
        // - 如果是 "/start"，用 "" 解析会【成功】，说明它是裸命令 -> 我们要拒绝
        // - 如果是 "/start@mybot"，用 "" 解析会【失败】(因为后缀不匹配)，说明它有点名 -> 我们要接受
        let is_bare_command = C::parse(&text, "").is_ok();

        if is_bare_command {
            // 是裸命令 (如 /start)，在群组里忽略
            return None;
        }

        // 是点名命令 (如 /start@mybot)，通过
        Some(cmd)
    })
}

/// 过滤“相关”消息：
/// 1. 私聊消息 -> 总是相关
/// 2. 群组消息 -> 只有当 @Bot 或 回复 Bot 时才相关
///
/// 返回值：如果相关，返回消息文本(String)；否则被过滤掉。
#[must_use]
pub fn filter_relevant_message<Output>() -> Handler<'static, Output, DpHandlerDescription>
where
    Output: Send + Sync + 'static,
{
    dptree::filter_map(move |message: Message, me: Me, text: String| {
        // let text = message.text().or_else(|| message.caption())?;

        // 1. 私聊：直接通过
        if message.chat.is_private() {
            return Some(message);
        }

        // 2. 群组：检查是否有点名 (@BotUsername)
        let bot_name = me.user.username.expect("Bots must have a username");
        let mention_str = format!("@{}", bot_name);

        // 大小写不敏感检查
        if text.to_lowercase().contains(&mention_str.to_lowercase()) {
            return Some(message);
        }

        // 3. 群组：检查是否是回复 (Reply) 给 Bot 的
        if let Some(reply) = message.reply_to_message() {
            // 如果被回复的消息的发送者是 Bot 自己
            if let Some(user) = reply.from.as_ref() {
                if user.id == me.user.id {
                    return Some(message);
                }
            }
        }

        // 既不是私聊，也没 @Bot，也没回复 Bot -> 忽略
        None
    })
}
