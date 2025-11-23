use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(rename_rule = "lowercase", description = "Supported commands / 支持的命令:")]
pub enum Command {
    #[command(description = "Show help text / 显示帮助信息")]
    Help,
    #[command(description = "Subscribe to author or ranking / 订阅作者或排行榜\n  Usage: /sub author:123456 or /sub ranking:daily")]
    Sub(String),
    #[command(description = "Unsubscribe from task / 取消订阅\n  Usage: /unsub author:123456")]
    Unsub(String),
    #[command(description = "List active subscriptions / 列出当前订阅")]
    List,
}
