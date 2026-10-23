use crate::bot::notifier::ThrottledBot;
use crate::bot::BotHandler;
use std::path::Path;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

/// 计算目录的总大小（递归）
fn calculate_dir_size(path: &Path) -> u64 {
    if !path.exists() || !path.is_dir() {
        return 0;
    }

    let mut total_size = 0u64;

    if let Ok(entries) = std::fs::read_dir(path) {
        for entry in entries.flatten() {
            let entry_path = entry.path();
            if entry_path.is_file() {
                if let Ok(metadata) = entry.metadata() {
                    total_size += metadata.len();
                }
            } else if entry_path.is_dir() {
                total_size += calculate_dir_size(&entry_path);
            }
        }
    }

    total_size
}

/// 格式化文件大小为人类可读格式
fn format_size(bytes: u64) -> String {
    const KB: u64 = 1024;
    const MB: u64 = KB * 1024;
    const GB: u64 = MB * 1024;

    if bytes >= GB {
        format!("{:.2} GB", bytes as f64 / GB as f64)
    } else if bytes >= MB {
        format!("{:.2} MB", bytes as f64 / MB as f64)
    } else if bytes >= KB {
        format!("{:.2} KB", bytes as f64 / KB as f64)
    } else {
        format!("{} B", bytes)
    }
}

impl BotHandler {
    // ------------------------------------------------------------------------
    // Help Command
    // ------------------------------------------------------------------------

    /// 显示帮助信息
    pub async fn handle_help(&self, bot: ThrottledBot, chat_id: ChatId) -> ResponseResult<()> {
        let help_text = r#"
📚 *PixivBot 帮助*

*可用命令:*

📌 `/sub <id,...> [+tag1 \-tag2]`
   订阅 Pixiv 作者
   \- `<id,...>`: 以逗号分隔的 Pixiv 用户 ID
   \- `\+tag`: 仅包含带有此标签的作品
   \- `\-tag`: 排除带有此标签的作品
   \- 示例: `/sub 123456,789012 \+原神 \-R\-18`

📊 `/subrank <mode> [+tag1 \-tag2]`
   订阅 Pixiv 排行榜
   \- 模式: `day`, `week`, `month`, `day_male`, `day_female`, `week_original`, `week_rookie`, `day_manga`
   \- R18 模式: `day_r18`, `week_r18`, `week_r18g`, `day_male_r18`, `day_female_r18`
   \- `\+tag`: 仅包含带有此标签的作品
   \- `\-tag`: 排除带有此标签的作品
   \- 示例: `/subrank day \+原神`
   \- 排除 AI 生成作品: `/subrank day \-AI生成`
   \- 也可在 /settings 的排除标签或敏感标签中添加 `AI生成`

🔥 `/subtag <标签搜索词> bookmarks=300 [+tag \-tag]`
   订阅标签近似热榜，观察发布后窗口内的作品，收藏达标后推送
   观察窗口由配置文件统一设置，默认 7 天；重复命令可修改收藏阈值并保留推送进度
   `/unsubtag <标签搜索词>` 取消订阅；支持前置 `ch=<频道ID>`

🗑 `/unsub <author_id,...>`
   取消订阅作者
   \- 使用逗号分隔的作者 ID \(Pixiv 用户 ID\)
   \- 示例: `/unsub 123456,789012`

🗑 `/unsubrank <mode>`
   取消订阅排行榜
   \- 示例: `/unsubrank day`

🔑 `/enablechat [chat_id]` / `/disablechat [chat_id]`
   全局管理员可管理聊天；群主和群管理员自动获得当前群的管理权限
   群权限不写入全局角色，不能管理其他聊天或授予全局角色

⚙️ `/settings`
   管理当前聊天的敏感内容模糊、敏感标签及排除标签
   支持当前群管理员修改，提交时重新验证权限
   标签可追加、删减、覆盖或清空；清空需要二次确认
   选择后在原消息显示当前标签；长列表仅截断展示
   输入用中英文逗号分隔，空输入不修改，`clear` 是普通标签
   每阶段限3分钟；可点取消、发送 /cancel 或重开 /settings

👑 `/setadmin <user_id|@username>` / `/unsetadmin <user_id|@username>`
   仅 Owner 可设置或取消全局 Bot Admin，不能修改 Owner
   也可不带参数回复用户消息；转发消息以实际发送者为目标
   回复真人消息设置 Admin 可自动建档；数字 ID 和用户名须已登记
   取消管理员不建档；用户名仅支持已知且能实时验证的用户
   无法验证时请改用数字 ID 或回复；不支持匿名和频道发送者
"#;

        bot.send_message(chat_id, help_text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Info Command
    // ------------------------------------------------------------------------

    /// 显示 Bot 状态信息
    pub async fn handle_info(&self, bot: ThrottledBot, chat_id: ChatId) -> ResponseResult<()> {
        // Gather statistics
        let admin_count = self.repo.count_admin_users().await.unwrap_or(0);
        let enabled_chat_count = self.repo.count_enabled_chats().await.unwrap_or(0);
        let subscription_count = self.repo.count_all_subscriptions().await.unwrap_or(0);
        let task_count = self.repo.count_all_tasks().await.unwrap_or(0);

        // Calculate disk usage for cache and log directories
        let cache_path = Path::new(&self.cache_dir);
        let log_path = Path::new(&self.log_dir);

        let cache_size = calculate_dir_size(cache_path);
        let log_size = calculate_dir_size(log_path);

        let mut message = format!(
            "📊 *PixivBot 状态信息*\n\n\
            👥 管理员人数: `{}`\n\
            💬 启用的聊天数: `{}`\n\
            📋 订阅数: `{}`\n\
            📝 任务数: `{}`\n\n\
            💾 *磁盘占用*\n\
            📁 缓存目录: `{}`\n\
            📄 日志目录: `{}`",
            admin_count,
            enabled_chat_count,
            subscription_count,
            task_count,
            format_size(cache_size),
            format_size(log_size)
        );

        if self.eh_client.is_some() {
            message.push_str("\n\n📥 *EH 下载统计*");
            for (hours, label) in [(24, "最近 24 小时"), (168, "最近 7 天")] {
                let downloaded = match self.repo.get_eh_downloaded_bytes_in_window(hours).await {
                    Ok(bytes) => format_size(bytes as u64),
                    Err(error) => {
                        tracing::error!("Failed to fetch EH downloaded bytes: {:#}", error);
                        "暂不可用".to_string()
                    }
                };
                let gp = match self.repo.get_eh_gp_cost_in_window(hours as u64).await {
                    Ok(cost) => cost.to_string(),
                    Err(error) => {
                        tracing::error!("Failed to fetch EH GP spend: {:#}", error);
                        "暂不可用".to_string()
                    }
                };
                message.push_str(&format!("\n{label}: 下载 `{downloaded}`，GP 消耗 `{gp}`"));
            }
        }

        bot.send_message(chat_id, message)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }
}
