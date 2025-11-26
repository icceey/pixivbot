use teloxide::types::BotCommand;
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

impl Command {
    /// 获取普通用户可见的命令列表
    pub fn user_commands() -> Vec<BotCommand> {
        vec![
            BotCommand::new("help", "显示帮助信息"),
            BotCommand::new("sub", "订阅作者 - /sub <id,...> [+tag1 -tag2]"),
            BotCommand::new("subrank", "订阅排行榜 - /subrank <mode>"),
            BotCommand::new("unsub", "取消订阅作者 - /unsub <author_id,...>"),
            BotCommand::new("unsubrank", "取消订阅排行榜 - /unsubrank <mode>"),
            BotCommand::new("list", "列出当前订阅"),
            BotCommand::new(
                "blursensitive",
                "启用或禁用敏感内容模糊 - /blursensitive <on|off>",
            ),
            BotCommand::new(
                "excludetags",
                "设置排除的标签 - /excludetags <tag1,tag2,...>",
            ),
            BotCommand::new("clearexcludedtags", "清除所有排除的标签"),
            BotCommand::new("settings", "显示聊天设置"),
        ]
    }

    /// 获取管理员可见的命令列表（包含普通命令 + 管理员命令）
    pub fn admin_commands() -> Vec<BotCommand> {
        let mut cmds = Self::user_commands();
        cmds.extend([
            BotCommand::new("enablechat", "[Admin] 启用聊天 - /enablechat [chat_id]"),
            BotCommand::new("disablechat", "[Admin] 禁用聊天 - /disablechat [chat_id]"),
        ]);
        cmds
    }

    /// 获取 Owner 可见的完整命令列表（包含所有命令）
    pub fn owner_commands() -> Vec<BotCommand> {
        let mut cmds = Self::admin_commands();
        cmds.extend([
            BotCommand::new("setadmin", "[Owner] 设置管理员 - /setadmin <user_id>"),
            BotCommand::new("unsetadmin", "[Owner] 移除管理员 - /unsetadmin <user_id>"),
        ]);
        cmds
    }
}
