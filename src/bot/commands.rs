use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone)]
#[command(rename_rule = "lowercase", description = "Supported commands:")]
pub enum Command {
    #[command(description = "Show help text")]
    Help,
    #[command(description = "Subscribe to an author or ranking")]
    Sub(String),
    #[command(description = "Unsubscribe from a task")]
    Unsub(String),
    #[command(description = "List active subscriptions")]
    List,
}
