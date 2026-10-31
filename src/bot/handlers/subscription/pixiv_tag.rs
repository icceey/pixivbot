use crate::bot::{notifier::ThrottledBot, BotHandler};
use crate::db::repo::pixiv_tag::TagSettings;
use crate::db::types::{TagFilter, TaskType};
use crate::utils::args::{self, ParsedArgs};
use teloxide::prelude::*;
use teloxide::types::UserId;
use tracing::error;

const USAGE: &str = "用法：/subtag [ch=<频道ID>] <单个精确标签> [v=<浏览量>] [b=<收藏量>] [p=<收藏率>] [l=<图片数>] [+tag -tag]";

fn parse_subscription(
    parsed: &ParsedArgs,
) -> Result<(String, TagSettings, TagFilter), &'static str> {
    let mut params: Vec<_> = parsed
        .params
        .iter()
        .filter(|(key, _)| !matches!(key.as_str(), "ch" | "channel"))
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let mut words = Vec::new();
    let mut filters = Vec::new();
    for word in parsed.remaining.split_whitespace() {
        if word.starts_with(['+', '-']) {
            filters.push(word);
        } else if let Some(param) = word.split_once('=') {
            params.push(param);
        } else {
            words.push(word);
        }
    }
    let mut views = None;
    let mut bookmarks = None;
    let mut rate = None;
    let mut limit = None;
    for (key, value) in params {
        let (param, duplicate_error) = match key {
            "v" => (&mut views, "浏览量参数重复"),
            "b" | "bookmarks" => (&mut bookmarks, "收藏阈值参数重复"),
            "p" => (&mut rate, "收藏率参数重复"),
            "l" | "limit" => (&mut limit, "图片数量参数重复"),
            _ => return Err("存在未知参数"),
        };
        if param.replace(value).is_some() {
            return Err(duplicate_error);
        }
    }
    if words.is_empty() {
        return Err("请指定标签");
    }
    let query = words.join(" ");
    let mut min_views: Option<i64> = views
        .map(str::parse)
        .transpose()
        .map_err(|_| "浏览量须为正整数")?;
    if min_views.is_some_and(|value| value <= 0) {
        return Err("浏览量须为正整数");
    }
    let min_bookmarks: Option<i64> = bookmarks
        .map(str::parse)
        .transpose()
        .map_err(|_| "收藏阈值须为正整数")?;
    if min_bookmarks.is_some_and(|value| value <= 0) {
        return Err("收藏阈值须为正整数");
    }
    let mut min_bookmark_rate: Option<f64> = rate
        .map(str::parse)
        .transpose()
        .map_err(|_| "收藏率须为 0 到 1 之间的小数，例如 p=0.15 表示 15%")?;
    if min_bookmark_rate.is_some_and(|value| !(0.0..=1.0).contains(&value)) {
        return Err("收藏率须为 0 到 1 之间的小数，例如 p=0.15 表示 15%");
    }
    if min_views.is_none() && min_bookmarks.is_none() {
        if min_bookmark_rate.is_some() {
            return Err("请至少设置浏览量 v 或收藏量 b");
        }
        min_views = Some(3000);
        min_bookmark_rate = Some(0.15);
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
            min_views,
            min_bookmarks,
            min_bookmark_rate,
            max_pages,
        },
        TagFilter::parse_from_args(&filters),
    ))
}

pub(super) fn format_settings(settings: &TagSettings) -> String {
    let mut conditions = Vec::new();
    if let Some(min) = settings.min_views {
        conditions.push(format!("浏览 ≥ {min}"));
    }
    if let Some(min) = settings.min_bookmarks {
        conditions.push(format!("收藏 ≥ {min}"));
    }
    if let Some(min) = settings.min_bookmark_rate {
        conditions.push(format!("收藏率 ≥ {min}"));
    }
    if let Some(limit) = settings.max_pages {
        conditions.push(format!("图片 ≤ {limit}"));
    }
    conditions.join("，")
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
                bot.send_message(
                    chat_id,
                    format!(
                        "❌ {reason}\n{USAGE}\n\n参数：\n\
                         - v：最低浏览量，须为正整数。\n\
                         - b / bookmarks：最低收藏量，须为正整数，省略时不过滤收藏量。\n\
                         - p：最低收藏率（收藏量 / 浏览量），范围 0 到 1，0.15 表示 15%，可省略。\n\
                         - v 和 b 至少设置一项；未指定 v、b、p 时默认 v=3000、p=0.15。所有条件需同时满足。\n\
                         - l / limit：图片数量超过此值的作品不推送，省略时不限数量。\n\
                         - +tag：除了订阅的标签外，作品还需带有至少一个指定标签才推送。\n\
                         - -tag：带有任意一个指定标签的作品不会推送。"
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
                let channel = if is_channel {
                    format!("\n📢 频道：{}", target.0)
                } else {
                    String::new()
                };
                bot.send_message(
                    chat_id,
                    format!(
                        "✅ 已订阅标签热榜：{query}\n筛选条件：{}{channel}",
                        format_settings(&settings)
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
