use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Supported commands / 支持的命令:")]
pub enum Command {
    #[command(description = "Show help text / 显示帮助信息")]
    Help,
    #[command(description = "Subscribe to author or ranking / 订阅作者或排行榜\n  Usage: /sub author:123456 or /sub ranking:daily")]
    Sub(String),
    #[command(description = "Unsubscribe from author / 取消订阅作者\n  Usage: /unsub <author_id>")]
    Unsub(String),
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
}
