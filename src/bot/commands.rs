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
    #[command(
        description = "使 Bot 接收在私聊中转发给 Bot 的消息，并在发送 /recap_forwarded 后开始总结"
    )]
    RecapForwardedStart,
    #[command(description = "使 Bot 停止接收在私聊中转发给 Bot 的消息，对已经转发过的消息进行总结")]
    RecapForwarded,
    #[command(description = "订阅当前群组的定时聊天回顾")]
    SubscribeRecap,
    #[command(description = "取消订阅当前群组的定时聊天回顾")]
    UnsubscribeRecap,
}

/// Parse commands with Go telegram-bot-api's mention behavior.
///
/// Go removes everything after the first `@` in the command token without
/// checking that the mention names this bot. Remaining command arguments are
/// preserved for handlers that consume them and ignored by unit variants.
#[must_use]
pub fn parse_go_command(text: &str, bot_name: &str) -> Option<Command> {
    let (token, arguments) = text
        .split_once(' ')
        .map_or((text, None), |(token, arguments)| (token, Some(arguments)));
    let token = token.split_once('@').map_or(token, |(command, _)| command);
    let normalized = arguments.map_or_else(
        || token.to_owned(),
        |arguments| format!("{token} {arguments}"),
    );
    Command::parse(&normalized, bot_name).ok()
}
