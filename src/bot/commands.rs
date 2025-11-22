use teloxide::prelude::*;
use teloxide::types::{Message, ChatKind};
use serde_json::json;
use std::sync::Arc;

use crate::config::Config;
use crate::error::{AppError, Result};
use crate::db::repo::{chats, users, tasks, subscriptions};

pub struct BotHandler {
    config: Arc<Config>,
    db: sea_orm::DatabaseConnection,
}

impl BotHandler {
    pub fn new(config: Arc<Config>, db: sea_orm::DatabaseConnection) -> Self {
        Self { config, db }
    }
    
    pub async fn run(self) {
        let bot = Bot::new(&self.config.telegram.bot_token);
        let handler = Arc::new(self);
        
        // Start polling for updates
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(1000)).await;
        }
    }
}

async fn command_handler(
    bot: Bot,
    message: Message,
    handler: Arc<BotHandler>,
) -> Result<()> {
    // Only process text messages
    if message.text().is_none() {
        return Ok(());
    }
    
    let text = message.text().unwrap();
    
    // Parse command
    if let Some(command) = parse_command(text, "") {
        match command {
            PixivCommand::Start => {
                let chat_type = match message.chat.kind {
                    ChatKind::Private(_) => "private",
                    _ => "unknown",
                };
                
                // Register user if not exists
                let user_id = message.from.map(|u| u.id.0);
                
                let is_admin = if let (Some(user_id), Some(owner_id)) = (user_id, handler.config.telegram.owner_id) {
                    user_id as i64 == owner_id
                } else {
                    false
                };
                
                if let Some(user_id) = user_id {
                    if let Err(_) = users::find_by_id(&handler.db, user_id as i64).await {
                        // User doesn't exist, create it
                        if let Err(e) = users::create_if_not_exists(
                            &handler.db,
                            user_id as i64,
                            Some(format!("{}", user_id)),
                            is_admin,
                        ).await {
                            tracing::error!("Failed to create user: {}", e);
                        }
                    }
                }
                
                let response = format!(
                    "👋 欢迎使用 Pixiv Bot！\n\n\
                    📝 可用命令：\n\
                    /sub_author <作者ID> [标签...] - 订阅作者（可添加标签过滤）\n\
                    /sub_ranking <mode> - 订阅排行榜\n\
                    /unsub <订阅ID> - 取消订阅\n\
                    /list - 查看订阅列表\n\
                    /help - 查看帮助\n\n\
                    🤖 当前聊天类型: {}",
                    chat_type
                );
                
                bot.send_message(message.chat.id, response).await?;
            }
            
            PixivCommand::Help => {
                let response = "🤖 Pixiv Bot 帮助\n\n\
                📝 订阅命令：\n\
                /sub_author <作者ID> [标签...] - 订阅作者，可添加多个标签进行过滤（如：/sub_author 123456 萝莉 白丝）\n\
                /sub_ranking <mode> - 订阅排行榜，mode可选: daily, weekly, monthly, male, female, rookie\n\n\
                📋 管理命令：\n\
                /unsub <订阅ID> - 取消指定订阅\n\
                /list - 查看当前聊天的所有订阅\n\n\
                ⚙️ 设置命令：\n\
                /set_interval <小时> - 设置检查间隔（默认2小时）\n\
                /set_timezone <时区> - 设置时区（如：Asia/Shanghai）\n\n\
                📊 排行榜模式：\n\
                daily - 日榜\n\
                weekly - 周榜\n\
                monthly - 月榜\n\
                male - 男性向\n\
                female - 女性向\n\
                rookie - 新人榜\n\n\
                💡 标签过滤：\n\
                支持多个标签，用空格分隔\n\
                标签会进行OR匹配，即作品包含任一标签就会推送\n\
                也可以使用负标签进行排除（如：-R18）";
                
                bot.send_message(message.chat.id, response).await?;
            }
            
            PixivCommand::List => {
                match chats::find_by_id(&handler.db, message.chat.id.0).await {
                    Ok(chat) => {
                        if let Some(chat) = chat {
                            match subscriptions::find_by_chat(&handler.db, chat.id).await {
                                Ok(subscriptions) => {
                                    if subscriptions.is_empty() {
                                        bot.send_message(message.chat.id, "📭 当前没有订阅").await?;
                                    } else {
                                        let mut response = String::from("📋 当前订阅列表：\n\n");
                                        for sub in &subscriptions {
                                            match tasks::find_by_id(&handler.db, sub.task_id).await {
                                                Ok(task) => {
                                                    if let Some(task) = task {
                                                        let task_type = if task.r#type == "ranking" { "排行榜" } else { "作者" };
                                                        
                                                        let target = if task.r#type == "ranking" {
                                                            task.value.clone()
                                                        } else {
                                                            format!("作者 {}", task.value)
                                                        };
                                                        
                                                        response.push_str(&format!(
                                                            "ID: {} | {} | {}\n",
                                                            sub.id, target, task_type
                                                        ));
                                                    }
                                                }
                                                Err(e) => {
                                                    tracing::error!("Failed to get task {}: {}", sub.task_id, e);
                                                }
                                            }
                                        }
                                        
                                        bot.send_message(message.chat.id, response).await?;
                                    }
                                }
                                Err(e) => {
                                    tracing::error!("Failed to get subscriptions: {}", e);
                                    bot.send_message(message.chat.id, "❌ 获取订阅列表失败").await?;
                                }
                            }
                        } else {
                            // Chat doesn't exist, create it
                            if let Err(e) = chats::create_if_not_exists(&handler.db, message.chat.id.0, "unknown", None).await {
                                tracing::error!("Failed to create chat: {}", e);
                                bot.send_message(message.chat.id, "❌ 注册聊天失败").await?;
                            } else {
                                bot.send_message(message.chat.id, "📭 当前没有订阅").await?;
                            }
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to get chat: {}", e);
                        bot.send_message(message.chat.id, "❌ 获取聊天信息失败").await?;
                    }
                }
            }
            
            PixivCommand::SubAuthor { target, tags } => {
                // Parse author ID
                let author_id = match target.parse::<u64>() {
                    Ok(id) => id,
                    Err(_) => {
                        bot.send_message(message.chat.id, "❌ 无效的作者ID，请使用纯数字ID").await?;
                        return Ok(());
                    }
                };
                
                // Get or create chat
                let chat = match chats::find_by_id(&handler.db, message.chat.id.0).await {
                    Ok(chat) => {
                        if let Some(chat) = chat {
                            chat
                        } else {
                            let chat_type = match message.chat.kind {
                                ChatKind::Private(_) => "private",
                                _ => "unknown",
                            };
                            
                            chats::create_if_not_exists(&handler.db, message.chat.id.0, chat_type, None).await?
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to get chat: {}", e);
                        bot.send_message(message.chat.id, "❌ 获取聊天信息失败").await?;
                        return Ok(());
                    }
                };
                
                // Create filters
                let (include_tags, exclude_tags) = parse_tags(&tags);
                let filters = json!({
                    "include_tags": include_tags,
                    "exclude_tags": exclude_tags
                });
                
                // Create task
                let task = tasks::create(
                    &handler.db,
                    "author",
                    &target,
                    2 * 3600, // default interval 2 hours in seconds
                    message.from.map(|u| u.id.0 as i64),
                ).await?;
                
                // Create subscription
                subscriptions::create_or_update(
                    &handler.db,
                    chat.id,
                    task.id,
                    Some(filters),
                ).await?;
                
                bot.send_message(message.chat.id, &format!("✅ 已订阅作者 {}", author_id)).await?;
            }
            
            PixivCommand::SubRanking { mode } => {
                // Validate ranking mode
                if !["daily", "weekly", "monthly", "male", "female", "rookie"].contains(&mode.as_str()) {
                    bot.send_message(message.chat.id, "❌ 无效的排行榜模式，可选: daily, weekly, monthly, male, female, rookie").await?;
                    return Ok(());
                }
                
                // Get or create chat
                let chat = match chats::find_by_id(&handler.db, message.chat.id.0).await {
                    Ok(chat) => {
                        if let Some(chat) = chat {
                            chat
                        } else {
                            let chat_type = match message.chat.kind {
                                ChatKind::Private(_) => "private",
                                _ => "unknown",
                            };
                            
                            chats::create_if_not_exists(&handler.db, message.chat.id.0, chat_type, None).await?
                        }
                    }
                    Err(e) => {
                        tracing::error!("Failed to get chat: {}", e);
                        bot.send_message(message.chat.id, "❌ 获取聊天信息失败").await?;
                        return Ok(());
                    }
                };
                
                // Create task
                let task = tasks::create(
                    &handler.db,
                    "ranking",
                    &mode,
                    12 * 3600, // default interval 12 hours in seconds
                    message.from.map(|u| u.id.0 as i64),
                ).await?;
                
                // Create subscription
                subscriptions::create_or_update(
                    &handler.db,
                    chat.id,
                    task.id,
                    None, // No filters for rankings
                ).await?;
                
                bot.send_message(message.chat.id, &format!("✅ 已订阅排行榜 {}", mode)).await?;
            }
            
            PixivCommand::Unsub { subscription_id } => {
                match subscriptions::delete(&handler.db, subscription_id).await {
                    Ok(_) => {
                        bot.send_message(message.chat.id, "✅ 已取消订阅").await?;
                    }
                    Err(_) => {
                        bot.send_message(message.chat.id, "❌ 订阅不存在或取消失败").await?;
                    }
                }
            }
        }
    }
    
    Ok(())
}

// Command parsing
fn parse_command(text: &str, _bot_name: &str) -> Option<PixivCommand> {
    if !text.starts_with('/') {
        return None;
    }
    
    let parts: Vec<&str> = text.split_whitespace().collect();
    if parts.is_empty() {
        return None;
    }
    
    match parts[0] {
        "/start" => Some(PixivCommand::Start),
        "/help" => Some(PixivCommand::Help),
        "/list" => Some(PixivCommand::List),
        "/sub_author" => {
            if parts.len() < 2 {
                return None;
            }
            
            let target = parts[1].to_string();
            let tags = if parts.len() > 2 {
                parts[2..].iter().map(|s| s.to_string()).collect()
            } else {
                vec![]
            };
            
            Some(PixivCommand::SubAuthor { target, tags })
        }
        "/sub_ranking" => {
            if parts.len() < 2 {
                return None;
            }
            
            let mode = parts[1].to_string();
            Some(PixivCommand::SubRanking { mode })
        }
        "/unsub" => {
            if parts.len() < 2 {
                return None;
            }
            
            let subscription_id = match parts[1].parse::<i32>() {
                Ok(id) => id,
                Err(_) => return None,
            };
            
            Some(PixivCommand::Unsub { subscription_id })
        }
        _ => None,
    }
}

// Tag parsing
fn parse_tags(tags: &[String]) -> (Vec<String>, Vec<String>) {
    let mut include_tags = Vec::new();
    let mut exclude_tags = Vec::new();
    
    for tag in tags {
        if tag.starts_with('-') && tag.len() > 1 {
            exclude_tags.push(tag[1..].to_string());
        } else {
            include_tags.push(tag.clone());
        }
    }
    
    (include_tags, exclude_tags)
}

// Command enums
#[derive(Debug)]
enum PixivCommand {
    Start,
    Help,
    List,
    SubAuthor { target: String, tags: Vec<String> },
    SubRanking { mode: String },
    Unsub { subscription_id: i32 },
}

// Utility functions for subscription management
pub fn should_send_work(work: &serde_json::Value, filters: &serde_json::Value) -> bool {
    // Get work tags
    let work_tags = work.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>())
        .unwrap_or_default();
    
    // Get include and exclude tags from filters
    let include_tags = filters.get("include_tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>())
        .unwrap_or_default();
    
    let exclude_tags = filters.get("exclude_tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<_>>())
        .unwrap_or_default();
    
    // Check if work should be sent
    let include_match = include_tags.is_empty() || include_tags.iter().any(|tag| work_tags.contains(&tag.to_string()));
    let exclude_match = exclude_tags.iter().any(|tag| work_tags.contains(&tag.to_string()));
    
    include_match && !exclude_match
}

pub fn format_work_message(work: &serde_json::Value) -> String {
    let id = work.get("id").and_then(|v| v.as_u64()).unwrap_or(0);
    let title = work.get("title").and_then(|v| v.as_str()).unwrap_or("未知作品");
    let author = work.get("user")
        .and_then(|u| u.as_object())
        .and_then(|u| u.get("name"))
        .and_then(|n| n.as_str())
        .unwrap_or("未知作者");
    
    let tags = work.get("tags")
        .and_then(|v| v.as_array())
        .map(|arr| arr.iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join(", "))
        .unwrap_or_default();
    
    let url = format!("https://www.pixiv.net/artworks/{}", id);
    
    format!(
        "🎨 {}\n👤 {}\n🏷️ {}\n🔗 {}",
        title, author, tags, url
    )
}