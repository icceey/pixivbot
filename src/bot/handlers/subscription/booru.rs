use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use crate::db::types::{
    BooruFilter, BooruRankingMode, BooruTaskKey, OrderbyKind, TagFilter, TaskType,
};
use crate::utils::args;
use booru_client::{BooruEngineType, BooruRating, PopularScale};
use teloxide::prelude::*;
use teloxide::types::{ChatId, ParseMode, UserId};
use teloxide::utils::markdown;
use tracing::{error, warn};

impl BotHandler {
    pub async fn handle_bsub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                error!(
                    "Failed to resolve subscription target in chat {}: {:#}",
                    chat_id, e
                );
                bot.send_message(chat_id, "❌ 频道ID无效或无法访问").await?;
                return Ok(());
            }
        };

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();

        if parts.is_empty() {
            let available: Vec<&str> = self
                .booru_registry
                .iter()
                .map(|s| s.config.name.as_str())
                .collect();

            bot.send_message(chat_id, build_bsub_usage_message(&available))
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let site_tags_str = parts[0];
        let (site_name, first_tag) = match site_tags_str.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => {
                let available: Vec<&str> = self
                    .booru_registry
                    .iter()
                    .map(|site| site.config.name.as_str())
                    .collect();

                bot.send_message(chat_id, build_bsub_usage_message(&available))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        let site_config = self.booru_registry.get(site_name);

        if site_config.is_none() {
            let available: Vec<String> = self
                .booru_registry
                .iter()
                .map(|s| s.config.name.clone())
                .collect();
            let msg = if available.is_empty() {
                "❌ 未配置任何 Booru 站点".to_string()
            } else {
                format!(
                    "❌ 未找到站点 `{}`\n可用站点: {}",
                    markdown::escape(site_name),
                    available
                        .iter()
                        .map(|s| format!("`{}`", markdown::escape(s)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            bot.send_message(chat_id, msg)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let mut booru_query_tags: Vec<&str> = Vec::new();
        if !first_tag.is_empty() {
            booru_query_tags.push(first_tag);
        }
        let mut filter_arg_parts: Vec<&str> = Vec::new();
        let mut orderby: Option<OrderbyKind> = None;

        for &part in &parts[1..] {
            if let Some(val) = part.strip_prefix("order=") {
                match OrderbyKind::from_str(val) {
                    Some(k) => {
                        orderby = Some(k);
                    }
                    None => {
                        bot.send_message(
                            chat_id,
                            format!("❌ order 值无效: `{}`，可用: score, fav, random", val),
                        )
                        .await?;
                        return Ok(());
                    }
                }
                continue;
            }
            if part.starts_with("score>")
                || part.starts_with("fav>")
                || part.starts_with("rating=")
                || part.starts_with('+')
                || part.starts_with('-')
            {
                filter_arg_parts.push(part);
            } else {
                booru_query_tags.push(part);
            }
        }

        let (booru_filter, tag_filter) = match parse_booru_filter_args(&filter_arg_parts) {
            Ok(result) => result,
            Err(msg) => {
                bot.send_message(chat_id, format!("❌ {}", msg)).await?;
                return Ok(());
            }
        };
        booru_query_tags.sort_unstable();
        let tags = booru_query_tags.join(" ");

        let (task_type, task_value, mode_label) = match orderby {
            None => (
                TaskType::BooruTag,
                BooruTaskKey::new_tag(site_name, &tags, &booru_filter).to_task_value(),
                None,
            ),
            Some(kind) => (
                TaskType::BooruRanking,
                BooruTaskKey::new_ranking(
                    site_name,
                    &tags,
                    BooruRankingMode::Orderby(kind),
                    &booru_filter,
                )
                .to_task_value(),
                Some(format!("order={}", kind.as_str())),
            ),
        };

        let display_name = if tags.is_empty() {
            format!("{} (all)", site_name)
        } else {
            format!("{}:{}", site_name, tags)
        };

        match self
            .create_booru_subscription(
                target_chat_id.0,
                task_type,
                &task_value,
                Some(&display_name),
                tag_filter.clone(),
                booru_filter.clone(),
            )
            .await
        {
            Ok(_) => {
                let label = if mode_label.is_some() {
                    "Booru 排序"
                } else {
                    "Booru 标签"
                };
                let mut msg = format!("✅ 已订阅 {}: *{}*", label, markdown::escape(&display_name));
                if let Some(label) = &mode_label {
                    msg.push_str(&format!("\n🏆 模式: `{}`", markdown::escape(label)));
                }

                if !booru_filter.is_empty() {
                    msg.push_str(&format!(
                        "\n🔧 {}",
                        markdown::escape(&booru_filter.format_for_display())
                    ));
                }
                if !tag_filter.is_empty() {
                    msg.push_str(&format!("\n🏷 {}", tag_filter.format_for_display()));
                }
                if is_channel {
                    msg.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }

                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to subscribe to booru tag {}:{} for chat {}: {:#}",
                    site_name, tags, target_chat_id.0, e
                );
                bot.send_message(chat_id, "❌ 订阅失败").await?;
            }
        }

        Ok(())
    }

    pub async fn handle_bunsub(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                error!(
                    "Failed to resolve subscription target in chat {}: {:#}",
                    chat_id, e
                );
                bot.send_message(chat_id, "❌ 频道ID无效或无法访问").await?;
                return Ok(());
            }
        };

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();

        if parts.is_empty() {
            let available: Vec<&str> = self
                .booru_registry
                .iter()
                .map(|s| s.config.name.as_str())
                .collect();

            bot.send_message(chat_id, build_bunsub_usage_message(&available))
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let site_tags_str = parts[0];
        let (site_name, first_tag) = match site_tags_str.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => {
                let available: Vec<&str> = self
                    .booru_registry
                    .iter()
                    .map(|site| site.config.name.as_str())
                    .collect();

                bot.send_message(chat_id, build_bunsub_usage_message(&available))
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        let mut booru_query_tags: Vec<&str> = Vec::new();
        let mut filter_arg_parts: Vec<&str> = Vec::new();
        let mut orderby: Option<OrderbyKind> = None;
        let mut popular_scale: Option<PopularScale> = None;
        for &part in &parts[1..] {
            if let Some(val) = part.strip_prefix("order=") {
                if let Some(k) = OrderbyKind::from_str(val) {
                    orderby = Some(k);
                }
                continue;
            }
            if let Some(val) = part.strip_prefix("scale=") {
                if let Some(s) = PopularScale::from_str(val) {
                    popular_scale = Some(s);
                }
                continue;
            }
            if part.starts_with("score>")
                || part.starts_with("fav>")
                || part.starts_with("rating=")
                || part.starts_with('+')
                || part.starts_with('-')
            {
                filter_arg_parts.push(part);
                continue;
            }
            booru_query_tags.push(part);
        }
        // Explicit order=/scale= override the ISO8601 heuristic on first_tag,
        // so /bunsub round-trips with /bsub and /brank when the same arg
        // string is reused.
        let mut interval_iso: Option<&str> = None;
        if !first_tag.is_empty() {
            if orderby.is_none()
                && popular_scale.is_none()
                && iso8601_duration::Duration::parse(first_tag).is_ok()
            {
                interval_iso = Some(first_tag);
            } else {
                booru_query_tags.push(first_tag);
            }
        }
        let (booru_filter, _tag_filter) = match parse_booru_filter_args(&filter_arg_parts) {
            Ok(result) => result,
            Err(msg) => {
                bot.send_message(chat_id, format!("❌ {}", msg)).await?;
                return Ok(());
            }
        };
        booru_query_tags.sort_unstable();
        let tags = booru_query_tags.join(" ");

        let (task_type, task_value) = if let Some(iso) = interval_iso {
            (
                TaskType::BooruRanking,
                BooruTaskKey::new_ranking(
                    site_name,
                    "",
                    BooruRankingMode::Interval(iso.to_string()),
                    &booru_filter,
                )
                .to_task_value(),
            )
        } else if let Some(scale) = popular_scale {
            (
                TaskType::BooruRanking,
                BooruTaskKey::new_ranking(
                    site_name,
                    &tags,
                    BooruRankingMode::Popular(scale),
                    &booru_filter,
                )
                .to_task_value(),
            )
        } else if let Some(kind) = orderby {
            (
                TaskType::BooruRanking,
                BooruTaskKey::new_ranking(
                    site_name,
                    &tags,
                    BooruRankingMode::Orderby(kind),
                    &booru_filter,
                )
                .to_task_value(),
            )
        } else {
            (
                TaskType::BooruTag,
                BooruTaskKey::new_tag(site_name, &tags, &booru_filter).to_task_value(),
            )
        };

        match self
            .delete_subscription(target_chat_id.0, task_type, &task_value)
            .await
        {
            Ok(display) => {
                let name = display.unwrap_or_else(|| task_value.clone());
                let mut msg = format!("✅ 已取消订阅: *{}*", markdown::escape(&name));
                if is_channel {
                    msg.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                warn!(
                    "Failed to unsubscribe booru tag {} for chat {}: {:#}",
                    task_value, target_chat_id.0, e
                );
                bot.send_message(chat_id, "❌ 未找到该订阅").await?;
            }
        }

        Ok(())
    }

    pub async fn handle_brank(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
        fixed_scale: Option<PopularScale>,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                error!(
                    "Failed to resolve subscription target in chat {}: {:#}",
                    chat_id, e
                );
                bot.send_message(chat_id, "❌ 频道ID无效或无法访问").await?;
                return Ok(());
            }
        };

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();
        if parts.is_empty() {
            let usage = if fixed_scale.is_some() {
                "❌ 用法: `/brankday|/brankweek|/brankmonth [ch=<频道ID>] <站点名[:标签 [标签2 ...]]> [score>=N] [fav>=N] [rating=s,q,e] [+tag -tag]`"
            } else {
                "❌ 用法: `/brank [ch=<频道ID>] <站点名[:标签 [标签2 ...]]> scale=day|week|month [score>=N] [fav>=N] [rating=s,q,e] [+tag -tag]`"
            };
            bot.send_message(chat_id, usage)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let site_tags_str = parts[0];
        let (site_name, first_tag) = match site_tags_str.split_once(':') {
            Some((site, tags)) => (site, tags),
            None => (site_tags_str, ""),
        };

        let site = match self.booru_registry.get(site_name) {
            Some(site) => site,
            None => {
                let available: Vec<String> = self
                    .booru_registry
                    .iter()
                    .map(|s| s.config.name.clone())
                    .collect();
                let msg = if available.is_empty() {
                    "❌ 未配置任何 Booru 站点".to_string()
                } else {
                    format!(
                        "❌ 未找到站点 `{}`\n可用站点: {}",
                        markdown::escape(site_name),
                        available
                            .iter()
                            .map(|s| format!("`{}`", markdown::escape(s)))
                            .collect::<Vec<_>>()
                            .join(", ")
                    )
                };
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            }
        };

        if matches!(site.config.engine_type, BooruEngineType::Gelbooru) {
            bot.send_message(chat_id, "❌ Gelbooru 不支持周榜/日榜/月榜")
                .await?;
            return Ok(());
        }

        let mut booru_query_tags: Vec<&str> = Vec::new();
        if !first_tag.is_empty() {
            booru_query_tags.push(first_tag);
        }
        let mut filter_arg_parts: Vec<&str> = Vec::new();
        let mut dynamic_scale: Option<PopularScale> = None;

        for &part in &parts[1..] {
            if let Some(val) = part.strip_prefix("scale=") {
                if fixed_scale.is_some() {
                    continue;
                }
                dynamic_scale = PopularScale::from_str(val);
                if dynamic_scale.is_none() {
                    bot.send_message(
                        chat_id,
                        format!(
                            "❌ scale 值无效: `{}`，可用值: day, week, month",
                            markdown::escape(val)
                        ),
                    )
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                    return Ok(());
                }
                continue;
            }

            if part.starts_with("score>")
                || part.starts_with("fav>")
                || part.starts_with("rating=")
                || part.starts_with('+')
                || part.starts_with('-')
            {
                filter_arg_parts.push(part);
            } else {
                booru_query_tags.push(part);
            }
        }

        let scale = if let Some(scale) = fixed_scale {
            scale
        } else {
            let Some(scale) = dynamic_scale else {
                bot.send_message(chat_id, "❌ 缺少参数 `scale=day|week|month`")
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
                return Ok(());
            };
            scale
        };

        let (booru_filter, tag_filter) = match parse_booru_filter_args(&filter_arg_parts) {
            Ok(result) => result,
            Err(msg) => {
                bot.send_message(chat_id, format!("❌ {}", msg)).await?;
                return Ok(());
            }
        };

        booru_query_tags.sort_unstable();
        let tags = booru_query_tags.join(" ");

        if !tags.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 排行榜模式不支持搜索标签 (Pixiv Popular API 仅按时间窗口返回热门作品)\n\
                 如需按标签过滤，请使用 `+tag` / `-tag` 进行客户端过滤",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        let task_value = BooruTaskKey::new_ranking(
            site_name,
            &tags,
            BooruRankingMode::Popular(scale),
            &booru_filter,
        )
        .to_task_value();

        let display_name = if tags.is_empty() {
            format!("{} ({})", site_name, scale.as_str())
        } else {
            format!("{}:{} ({})", site_name, tags, scale.as_str())
        };

        match self
            .create_booru_subscription(
                target_chat_id.0,
                TaskType::BooruRanking,
                &task_value,
                Some(&display_name),
                tag_filter.clone(),
                booru_filter.clone(),
            )
            .await
        {
            Ok(_) => {
                let mut msg = format!(
                    "✅ 已订阅 {} {}榜",
                    markdown::escape(site_name),
                    markdown::escape(scale.as_str())
                );
                if !tags.is_empty() {
                    msg.push_str(&format!("\n🏷 标签: `{}`", markdown::escape(&tags)));
                }
                if !booru_filter.is_empty() {
                    msg.push_str(&format!(
                        "\n🔧 {}",
                        markdown::escape(&booru_filter.format_for_display())
                    ));
                }
                if !tag_filter.is_empty() {
                    msg.push_str(&format!("\n🏷 {}", tag_filter.format_for_display()));
                }
                if is_channel {
                    msg.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to subscribe to booru ranking {}:{} ({}) for chat {}: {:#}",
                    site_name,
                    tags,
                    scale.as_str(),
                    target_chat_id.0,
                    e
                );
                bot.send_message(chat_id, "❌ 订阅失败").await?;
            }
        }

        Ok(())
    }

    pub async fn handle_brand(
        &self,
        bot: ThrottledBot,
        chat_id: ChatId,
        user_id: Option<UserId>,
        args_str: String,
    ) -> ResponseResult<()> {
        let parsed = args::parse_args(&args_str);

        let (target_chat_id, is_channel) = match self
            .resolve_subscription_target(&bot, chat_id, user_id, &parsed)
            .await
        {
            Ok(result) => result,
            Err(e) => {
                error!(
                    "Failed to resolve subscription target in chat {}: {:#}",
                    chat_id, e
                );
                bot.send_message(chat_id, "❌ 频道ID无效或无法访问").await?;
                return Ok(());
            }
        };

        let parts: Vec<&str> = parsed.remaining.split_whitespace().collect();
        if parts.is_empty() {
            bot.send_message(
                chat_id,
                "❌ 用法: `/brand [ch=<频道ID>] <站点名:ISO8601间隔> [score>=N] [fav>=N] [rating=s,q,e] [+tag -tag]`",
            )
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
            return Ok(());
        }

        let site_interval = parts[0];
        let (site_name, iso_str) = match site_interval.split_once(':') {
            Some((site, iso)) if !site.is_empty() && !iso.is_empty() => (site, iso),
            _ => {
                bot.send_message(
                    chat_id,
                    "❌ 格式: `站点名:ISO8601间隔`，例如 `konachan:PT1H`",
                )
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
                return Ok(());
            }
        };

        if self.booru_registry.get(site_name).is_none() {
            let available: Vec<String> = self
                .booru_registry
                .iter()
                .map(|s| s.config.name.clone())
                .collect();
            let msg = if available.is_empty() {
                "❌ 未配置任何 Booru 站点".to_string()
            } else {
                format!(
                    "❌ 未找到站点 `{}`\n可用站点: {}",
                    markdown::escape(site_name),
                    available
                        .iter()
                        .map(|s| format!("`{}`", markdown::escape(s)))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            bot.send_message(chat_id, msg)
                .parse_mode(ParseMode::MarkdownV2)
                .await?;
            return Ok(());
        }

        let parsed_duration = match iso8601_duration::Duration::parse(iso_str) {
            Ok(d) => d,
            Err(_) => {
                bot.send_message(
                    chat_id,
                    "❌ 无效的 ISO8601 间隔格式（例: PT1H, PT30M, P1D）",
                )
                .await?;
                return Ok(());
            }
        };
        let std_duration = match parsed_duration.to_std() {
            Some(d) => d,
            None => {
                bot.send_message(
                    chat_id,
                    "❌ 无效的 ISO8601 间隔格式（例: PT1H, PT30M, P1D）",
                )
                .await?;
                return Ok(());
            }
        };
        let interval = match chrono::Duration::from_std(std_duration) {
            Ok(d) => d,
            Err(_) => {
                bot.send_message(
                    chat_id,
                    "❌ 无效的 ISO8601 间隔格式（例: PT1H, PT30M, P1D）",
                )
                .await?;
                return Ok(());
            }
        };

        let min_interval = chrono::Duration::minutes(5);
        let max_interval = chrono::Duration::days(30);
        if interval < min_interval || interval > max_interval {
            bot.send_message(chat_id, "❌ 间隔超出范围，需在 5 分钟到 30 天之间")
                .await?;
            return Ok(());
        }

        let (booru_filter, tag_filter) = match parse_booru_filter_args(&parts[1..]) {
            Ok(result) => result,
            Err(msg) => {
                bot.send_message(chat_id, format!("❌ {}", msg)).await?;
                return Ok(());
            }
        };

        let task_value = BooruTaskKey::new_ranking(
            site_name,
            "",
            BooruRankingMode::Interval(iso_str.to_string()),
            &booru_filter,
        )
        .to_task_value();
        let display_name = format!("{} (interval:{})", site_name, iso_str);

        match self
            .create_booru_subscription(
                target_chat_id.0,
                TaskType::BooruRanking,
                &task_value,
                Some(&display_name),
                tag_filter.clone(),
                booru_filter.clone(),
            )
            .await
        {
            Ok(_) => {
                let mut msg = format!(
                    "✅ 已订阅 {} 随机推送（每 {}）",
                    markdown::escape(site_name),
                    markdown::escape(iso_str)
                );
                if !booru_filter.is_empty() {
                    msg.push_str(&format!(
                        "\n🔧 {}",
                        markdown::escape(&booru_filter.format_for_display())
                    ));
                }
                if !tag_filter.is_empty() {
                    msg.push_str(&format!("\n🏷 {}", tag_filter.format_for_display()));
                }
                if is_channel {
                    msg.push_str(&format!("\n📢 频道: `{}`", target_chat_id.0));
                }
                bot.send_message(chat_id, msg)
                    .parse_mode(ParseMode::MarkdownV2)
                    .await?;
            }
            Err(e) => {
                error!(
                    "Failed to subscribe to booru interval {}:{} for chat {}: {:#}",
                    site_name, iso_str, target_chat_id.0, e
                );
                bot.send_message(chat_id, "❌ 订阅失败").await?;
            }
        }

        Ok(())
    }
}

fn parse_booru_filter_args(args: &[&str]) -> Result<(BooruFilter, TagFilter), String> {
    let mut score_min = None;
    let mut fav_count_min = None;
    let mut allowed_ratings = Vec::new();
    let mut tag_parts = Vec::new();

    for &arg in args {
        // score>=N means "score >= N", score>N means "score > N" (stored as N+1)
        if let Some(val) = arg.strip_prefix("score>=") {
            score_min =
                Some(val.parse::<i32>().map_err(|_| {
                    format!("score 参数无效: `{}`，需要整数，例如 `score>=50`", arg)
                })?);
            continue;
        }
        if let Some(val) = arg.strip_prefix("score>") {
            let v = val
                .parse::<i32>()
                .map_err(|_| format!("score 参数无效: `{}`，需要整数，例如 `score>=50`", arg))?;
            score_min = Some(v.saturating_add(1));
            continue;
        }
        // fav>=N means "fav >= N", fav>N means "fav > N" (stored as N+1)
        if let Some(val) = arg.strip_prefix("fav>=") {
            fav_count_min = Some(
                val.parse::<i32>()
                    .map_err(|_| format!("fav 参数无效: `{}`，需要整数，例如 `fav>=10`", arg))?,
            );
            continue;
        }
        if let Some(val) = arg.strip_prefix("fav>") {
            let v = val
                .parse::<i32>()
                .map_err(|_| format!("fav 参数无效: `{}`，需要整数，例如 `fav>=10`", arg))?;
            fav_count_min = Some(v.saturating_add(1));
            continue;
        }
        if let Some(val) = arg.strip_prefix("rating=") {
            for r in val.split(',') {
                match r.trim() {
                    "s" | "safe" => allowed_ratings.push(BooruRating::Safe),
                    "g" | "general" => allowed_ratings.push(BooruRating::General),
                    "se" | "sensitive" => allowed_ratings.push(BooruRating::Sensitive),
                    "q" | "questionable" => allowed_ratings.push(BooruRating::Questionable),
                    "e" | "explicit" => allowed_ratings.push(BooruRating::Explicit),
                    other => {
                        return Err(format!(
                            "rating 值无效: `{}`，可用值: s/safe, g/general, se/sensitive, q/questionable, e/explicit",
                            other
                        ));
                    }
                }
            }
            continue;
        }
        tag_parts.push(arg);
    }

    let booru_filter = BooruFilter::new(score_min, fav_count_min, allowed_ratings);
    let tag_filter = TagFilter::parse_from_args(&tag_parts);

    Ok((booru_filter, tag_filter))
}

fn build_bsub_usage_message(site_names: &[&str]) -> String {
    let Some(first_site) = site_names.first() else {
        return "❌ 未配置任何 Booru 站点".to_string();
    };

    let available_sites = site_names
        .iter()
        .map(|site| markdown::escape(site))
        .collect::<Vec<_>>()
        .join(", ");
    let first_site = markdown::escape(first_site);

    format!(
        "❌ 用法: `/bsub [ch=<频道ID>] <站点名:标签 [标签2 ...]> [score>=N] [fav>=N] [rating=s,q,e]`\n\n可用站点: {}\n\n示例:\n`/bsub {}:landscape`\n`/bsub {}:blue_sky clouds`\n`/bsub {}: score>=50`\n`/bsub {}:blue_sky rating=s`",
        available_sites, first_site, first_site, first_site, first_site
    )
}

fn build_bunsub_usage_message(site_names: &[&str]) -> String {
    let Some(first_site) = site_names.first() else {
        return "❌ 未配置任何 Booru 站点".to_string();
    };

    let available_sites = site_names
        .iter()
        .map(|site| markdown::escape(site))
        .collect::<Vec<_>>()
        .join(", ");
    let first_site = markdown::escape(first_site);

    format!(
        "❌ 用法: `/bunsub [ch=<频道ID>] <站点名[:标签]> [order=...|scale=day|week|month|<ISO间隔>] [过滤条件]`\n\n可用站点: {}\n\n示例:\n`/bunsub {}:landscape`\n`/bunsub {}:landscape scale=day`\n`/bunsub {}:PT1H`",
        available_sites, first_site, first_site, first_site
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_filters_and_tags_split() {
        let args = vec!["+good", "score>=50", "fav>=10", "rating=s,q", "-bad"];
        let (booru, tags) = parse_booru_filter_args(&args).unwrap();
        assert_eq!(booru.score_min, Some(50));
        assert_eq!(booru.fav_count_min, Some(10));
        assert_eq!(
            booru.allowed_ratings,
            vec![BooruRating::Safe, BooruRating::Questionable]
        );
        assert!(!tags.is_empty());
    }

    #[test]
    fn invalid_numeric_returns_error() {
        let args = vec!["score>=abc"];
        let result = parse_booru_filter_args(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("score"));

        let args = vec!["fav>=xyz"];
        let result = parse_booru_filter_args(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("fav"));
    }

    #[test]
    fn invalid_rating_returns_error() {
        let args = vec!["rating=bad"];
        let result = parse_booru_filter_args(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("rating"));
    }

    #[test]
    fn multiple_ratings_parsed() {
        let args = vec!["rating=s,q,e"];
        let (booru, _) = parse_booru_filter_args(&args).unwrap();
        assert_eq!(
            booru.allowed_ratings,
            vec![
                BooruRating::Safe,
                BooruRating::Questionable,
                BooruRating::Explicit
            ]
        );
    }

    #[test]
    fn sensitive_rating_parsed() {
        let args = vec!["rating=se"];
        let (booru, _) = parse_booru_filter_args(&args).unwrap();
        assert_eq!(booru.allowed_ratings, vec![BooruRating::Sensitive]);

        let args = vec!["rating=sensitive"];
        let (booru, _) = parse_booru_filter_args(&args).unwrap();
        assert_eq!(booru.allowed_ratings, vec![BooruRating::Sensitive]);
    }

    #[test]
    fn empty_args_returns_defaults() {
        let args: Vec<&str> = vec![];
        let (booru, tags) = parse_booru_filter_args(&args).unwrap();
        assert!(booru.is_empty());
        assert!(tags.is_empty());
    }

    #[test]
    fn strict_greater_than_stored_as_plus_one() {
        let args = vec!["score>49", "fav>9"];
        let (booru, _) = parse_booru_filter_args(&args).unwrap();
        assert_eq!(booru.score_min, Some(50));
        assert_eq!(booru.fav_count_min, Some(10));
    }
}
