use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::db::repo::pixiv_tag::TagSettings;
use crate::db::types::{TagFilter, TaskType};
use crate::utils::args::{self, ParsedArgs};
use teloxide::prelude::*;
use teloxide::types::UserId;
use tracing::error;

const USAGE: &str = "用法：/subtag [ch=<频道ID>] <标签或搜索词> bookmarks=300 [+tag -tag]\n观察窗口统一由配置文件设置，不接受命令参数覆盖；收藏阈值为正整数。重复订阅更新配置并保留推送进度。";

fn parse_subscription(
    parsed: &ParsedArgs,
) -> Result<(String, TagSettings, TagFilter), &'static str> {
    let mut bookmarks = parsed.get("bookmarks");
    if parsed
        .params
        .keys()
        .any(|key| !matches!(key.as_str(), "ch" | "channel" | "bookmarks"))
    {
        return Err("存在未知参数");
    }
    let mut words = Vec::new();
    let mut filters = Vec::new();
    for word in parsed.remaining.split_whitespace() {
        if let Some(value) = word.strip_prefix("bookmarks=") {
            if bookmarks.replace(value).is_some() {
                return Err("收藏阈值参数重复");
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
        return Err("请指定标签或搜索词");
    }
    let query = words.join(" ");
    if query.chars().count() > 128 {
        return Err("标签搜索词不能超过 128 个字符");
    }
    let min_bookmarks: i64 = bookmarks
        .ok_or("请指定 bookmarks 收藏阈值")?
        .parse()
        .map_err(|_| "收藏阈值须为正整数")?;
    if min_bookmarks <= 0 {
        return Err("收藏阈值须为正整数");
    }
    Ok((
        query,
        TagSettings { min_bookmarks },
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
        let (query, settings, filter) = match parse_subscription(&parsed) {
            Ok(value) => value,
            Err(reason) => {
                bot.send_message(chat_id, format!("❌ {reason}\n{USAGE}"))
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
                let channel = if is_channel {
                    format!("\n📢 频道：{}", target.0)
                } else {
                    String::new()
                };
                bot.send_message(chat_id, format!("✅ 已订阅标签热榜：{query}\n收藏达到 {} 时推送，观察窗口由服务器统一配置。{channel}\n已发送及待重试进度保留。",
                    settings.min_bookmarks)).await?;
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
