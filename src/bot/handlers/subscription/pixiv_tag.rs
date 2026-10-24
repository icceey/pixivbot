use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::db::repo::pixiv_tag::TagSettings;
use crate::db::types::{TagFilter, TaskType};
use crate::utils::args::{self, ParsedArgs};
use teloxide::prelude::*;
use teloxide::types::UserId;
use tracing::error;

const USAGE: &str = "用法：/subtag [ch=<频道ID>] <单个精确标签> [b=500] [l=<图片数>] [+tag -tag]";

fn parse_subscription(
    parsed: &ParsedArgs,
    default_min_bookmarks: i64,
) -> Result<(String, TagSettings, TagFilter), &'static str> {
    let mut bookmarks = parsed.get_any(&["b", "bookmarks"]);
    let mut limit = parsed.get_any(&["l", "limit"]);
    if parsed.get("b").is_some() && parsed.get("bookmarks").is_some() {
        return Err("收藏阈值参数重复");
    }
    if parsed.get("l").is_some() && parsed.get("limit").is_some() {
        return Err("图片数量参数重复");
    }
    if parsed.params.keys().any(|key| {
        !matches!(
            key.as_str(),
            "ch" | "channel" | "b" | "bookmarks" | "l" | "limit"
        )
    }) {
        return Err("存在未知参数");
    }
    let mut words = Vec::new();
    let mut filters = Vec::new();
    for word in parsed.remaining.split_whitespace() {
        if let Some(value) = word
            .strip_prefix("b=")
            .or_else(|| word.strip_prefix("bookmarks="))
        {
            if bookmarks.replace(value).is_some() {
                return Err("收藏阈值参数重复");
            }
        } else if let Some(value) = word
            .strip_prefix("l=")
            .or_else(|| word.strip_prefix("limit="))
        {
            if limit.replace(value).is_some() {
                return Err("图片数量参数重复");
            }
        } else if word.starts_with(['+', '-']) {
            filters.push(word);
        } else if word.contains('=') {
            return Err("存在未知参数");
        } else {
            words.push(word);
        }
    }
    if words.is_empty() {
        return Err("请指定标签");
    }
    let query = words.join(" ");
    let min_bookmarks: i64 = bookmarks
        .map(str::parse)
        .transpose()
        .map_err(|_| "收藏阈值须为正整数")?
        .unwrap_or(default_min_bookmarks);
    if min_bookmarks <= 0 {
        return Err("收藏阈值须为正整数");
    }
    let max_pages: Option<i64> = limit
        .map(str::parse)
        .transpose()
        .map_err(|_| "图片数量须为正整数")?;
    if max_pages.is_some_and(|value| value <= 0) {
        return Err("图片数量须为正整数");
    }
    Ok((
        query,
        TagSettings {
            min_bookmarks,
            max_pages,
        },
        TagFilter::parse_from_args(&filters),
    ))
}

impl BotHandler {
    pub async fn handle_sub_tag(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);
        let (query, settings, filter) =
            match parse_subscription(&parsed, self.pixiv_tag_min_bookmarks) {
                Ok(value) => value,
                Err(reason) => {
                    bot.send_message(
                        chat_id,
                        format!(
                            "❌ {reason}\n{USAGE}\n\n参数：\n\
                         - b / bookmarks：收藏数达到此值才会推送，默认{}。\n\
                         - l / limit：图片数量超过此值的作品不推送，省略时不限数量。\n\
                         - +tag：除了订阅的标签外，作品还需带有至少一个指定标签才推送。\n\
                         - -tag：带有任意一个指定标签的作品不会推送。\n\
                         收藏数和图片数量须为正整数。",
                            self.pixiv_tag_min_bookmarks
                        ),
                    )
                    .await?;
                    return Ok(());
                }
            };
        let (target, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(target) => target,
            Err(e) => {
                error!("Tag subscription target failed: {:#}", e);
                bot.send_message(chat_id, "❌ 频道ID无效或无法访问").await?;
                return Ok(());
            }
        };
        let result = async {
            let task = self
                .repo
                .get_or_create_task(TaskType::PixivTag, query.clone(), None)
                .await?;
            self.repo
                .upsert_pixiv_tag_subscription(target.0, task.id, filter, &settings)
                .await
        }
        .await;
        match result {
            Ok(()) => {
                let limit = settings
                    .max_pages
                    .map(|limit| format!("，图片不超过 {limit} 张"))
                    .unwrap_or_default();
                let channel = if is_channel {
                    format!("\n📢 频道：{}", target.0)
                } else {
                    String::new()
                };
                bot.send_message(
                    chat_id,
                    format!(
                        "✅ 已订阅标签热榜：{query}\n收藏达到 {} 时推送{limit}{channel}",
                        settings.min_bookmarks
                    ),
                )
                .await?;
            }
            Err(e) => {
                error!("Failed to save tag subscription: {:#}", e);
                bot.send_message(chat_id, "❌ 保存标签热榜订阅失败").await?;
            }
        }
        Ok(())
    }

    pub async fn handle_unsub_tag(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);
        let query = parsed
            .remaining
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        if query.is_empty() {
            bot.send_message(
                chat_id,
                "用法：/unsubtag [ch=<频道ID>] <list 中的标签搜索词>",
            )
            .await?;
            return Ok(());
        }
        let (target, _) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(target) => target,
            Err(e) => {
                error!("Tag unsubscribe target failed: {:#}", e);
                bot.send_message(chat_id, "❌ 频道ID无效或无法访问").await?;
                return Ok(());
            }
        };
        match self
            .delete_subscription(target.0, TaskType::PixivTag, &query)
            .await
        {
            Ok(_) => {
                bot.send_message(chat_id, format!("✅ 已取消标签热榜订阅：{query}"))
                    .await?;
            }
            Err(e) => {
                error!("Failed to remove tag subscription: {:#}", e);
                bot.send_message(chat_id, "❌ 取消失败，请检查标签搜索词是否与 /list 一致")
                    .await?;
            }
        }
        Ok(())
    }
}
