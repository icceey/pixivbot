use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Supported commands / 支持的命令:")]
pub enum Command {
    #[command(description = "Show help text / 显示帮助信息")]
    Help,
    #[command(description = "Subscribe to author(s) / 订阅作者\n  Usage: /sub <id,...> [+tag1 -tag2]")]
    Sub(String),
    #[command(description = "Subscribe to ranking / 订阅排行榜\n  Usage: /subrank <mode>")]
    SubRank(String),
    #[command(description = "Unsubscribe from author(s) / 取消订阅作者\n  Usage: /unsub <author_id,...>")]
    Unsub(String),
    #[command(description = "Unsubscribe from ranking / 取消订阅排行榜\n  Usage: /unsubrank <mode>")]
    UnsubRank(String),
    #[command(description = "List active subscriptions / 列出当前订阅")]
    List,
    #[command(description = "[Owner Only] Set user as admin / 设置用户为管理员\n  Usage: /setadmin <user_id>")]
    SetAdmin(String),
    #[command(description = "[Owner Only] Remove user admin role / 移除用户管理员角色\n  Usage: /unsetadmin <user_id>")]
    UnsetAdmin(String),
    #[command(description = "[Admin Only] Enable chat / 启用聊天\n  Usage: /enablechat [chat_id]")]
    EnableChat(String),
    #[command(description = "[Admin Only] Disable chat / 禁用聊天\n  Usage: /disablechat [chat_id]")]
    DisableChat(String),
    #[command(description = "Enable/disable sensitive content blur / 启用或禁用敏感内容模糊\n  Usage: /blursensitive <on|off>")]
    BlurSensitive(String),
    #[command(description = "Set excluded tags / 设置排除的标签\n  Usage: /excludetags <tag1,tag2,...>")]
    ExcludeTags(String),
    #[command(description = "Clear all excluded tags / 清除所有排除的标签")]
    ClearExcludedTags,
    #[command(description = "Show chat settings / 显示聊天设置")]
    Settings,
}
