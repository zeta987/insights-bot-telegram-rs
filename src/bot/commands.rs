use teloxide::utils::command::BotCommands;

#[derive(BotCommands, Clone, Debug)]
#[command(
    rename_rule = "snake_case",
    description = "Available commands",
    separator = " "
)]
pub enum Command {
    #[command(description = "Show welcome message")]
    Start(String),
    #[command(description = "Show help")]
    Help,
    #[command(description = "Cancel current operation")]
    Cancel,
    #[command(description = "Generate chat recap")]
    Recap,
    #[command(description = "Configure recap settings")]
    ConfigureRecap,
    #[command(description = "订阅当前群组的定时聊天回顾")]
    SubscribeRecap,
    #[command(description = "取消订阅当前群组的定时聊天回顾")]
    UnsubscribeRecap,
}
