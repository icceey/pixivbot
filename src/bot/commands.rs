use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "支持的命令:")]
pub enum Command {
    #[command(description = "显示帮助信息")]
    Help,
    #[command(description = "订阅作者\n  用法: /sub <id,...> [+tag1 -tag2]")]
    Sub(String),
    #[command(description = "订阅排行榜\n  用法: /subrank <mode>")]
    SubRank(String),
    #[command(description = "取消订阅作者\n  用法: /unsub <author_id,...>")]
    Unsub(String),
    #[command(description = "取消订阅排行榜\n  用法: /unsubrank <mode>")]
    UnsubRank(String),
    #[command(description = "列出当前订阅")]
    List,
    #[command(description = "[仅Owner] 设置用户为管理员\n  用法: /setadmin <user_id>")]
    SetAdmin(String),
    #[command(description = "[仅Owner] 移除用户管理员角色\n  用法: /unsetadmin <user_id>")]
    UnsetAdmin(String),
    #[command(description = "[仅Admin] 启用聊天\n  用法: /enablechat [chat_id]")]
    EnableChat(String),
    #[command(description = "[仅Admin] 禁用聊天\n  用法: /disablechat [chat_id]")]
    DisableChat(String),
    #[command(description = "启用或禁用敏感内容模糊\n  用法: /blursensitive <on|off>")]
    BlurSensitive(String),
    #[command(description = "设置排除的标签\n  用法: /excludetags <tag1,tag2,...>")]
    ExcludeTags(String),
    #[command(description = "清除所有排除的标签")]
    ClearExcludedTags,
    #[command(description = "显示聊天设置")]
    Settings,
}
