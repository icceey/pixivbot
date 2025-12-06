use crate::bot::BotHandler;
use teloxide::prelude::*;
use teloxide::types::ParseMode;

impl BotHandler {
    // ------------------------------------------------------------------------
    // Help Command
    // ------------------------------------------------------------------------

    /// 显示帮助信息
    pub async fn handle_help(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
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

🗑 `/unsub <author_id,...>`
   取消订阅作者
   \- 使用逗号分隔的作者 ID \(Pixiv 用户 ID\)
   \- 示例: `/unsub 123456,789012`

🗑 `/unsubrank <mode>`
   取消订阅排行榜
   \- 示例: `/unsubrank day`

🔒 `/blursensitive <on|off>`
   启用或禁用敏感内容模糊
   \- 示例: `/blursensitive on`

🏷 `/sensitivetags <tag1,tag2,...>`
   设置此聊天的敏感标签
   \- 示例: `/sensitivetags R\-18,R\-18G`

🗑 `/clearsensitivetags`
   清除所有敏感标签

🚫 `/excludetags <tag1,tag2,...>`
   设置此聊天的全局排除标签
   \- 示例: `/excludetags R\-18,gore`

🗑 `/clearexcludedtags`
   清除所有排除的标签
"#;

        bot.send_message(chat_id, help_text)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;
        Ok(())
    }

    // ------------------------------------------------------------------------
    // Info Command
    // ------------------------------------------------------------------------

    /// 显示 Bot 状态信息（仅管理员可用）
    pub async fn handle_info(&self, bot: Bot, chat_id: ChatId) -> ResponseResult<()> {
        // Gather statistics
        let admin_count = self.repo.count_admin_users().await.unwrap_or(0);
        let enabled_chat_count = self.repo.count_enabled_chats().await.unwrap_or(0);
        let subscription_count = self.repo.count_all_subscriptions().await.unwrap_or(0);
        let task_count = self.repo.count_all_tasks().await.unwrap_or(0);

        let message = format!(
            "📊 *PixivBot 状态信息*\n\n\
            👥 管理员人数: `{}`\n\
            💬 启用的聊天数: `{}`\n\
            📋 订阅数: `{}`\n\
            📝 任务数: `{}`",
            admin_count, enabled_chat_count, subscription_count, task_count
        );

        bot.send_message(chat_id, message)
            .parse_mode(ParseMode::MarkdownV2)
            .await?;

        Ok(())
    }
}
