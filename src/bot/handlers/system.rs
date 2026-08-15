use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{ChatMemberStatus, Me, MessageEntityKind, ParseMode, ReplyParameters},
};
use tracing::error;

use crate::bot::context::AppContext;

pub struct SystemHandlers;

impl SystemHandlers {
    pub async fn handle_start(
        bot: Bot,
        msg: Message,
        arguments: String,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        if should_suppress_group_command(&bot, &msg, &me, &[format!("start@{}", me.username())])
            .await
        {
            return Ok(());
        }
        if crate::bot::handlers::recap_subscription::handle_start_continuation(
            &bot, &msg, &arguments, &ctx,
        )
        .await?
        {
            return Ok(());
        }
        send_help_command(&bot, &msg, &me, &ctx).await
    }

    pub async fn handle_help(
        bot: Bot,
        msg: Message,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        send_help_command(&bot, &msg, &me, &ctx).await
    }

    pub async fn handle_cancel(bot: Bot, msg: Message, ctx: Arc<AppContext>) -> ResponseResult<()> {
        let text = ctx.i18n.t(ctx.config.locale, "bot.cancel", &[]);
        bot.send_message(msg.chat.id, text).await?;
        Ok(())
    }
}

async fn should_suppress_group_command(
    bot: &Bot,
    message: &Message,
    me: &Me,
    allowed_commands: &[String],
) -> bool {
    let is_administrator = match bot.get_chat_member(message.chat.id, me.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Administrator,
        Err(source) => {
            error!(
                ?source,
                chat_id = message.chat.id.0,
                "failed to check whether the bot is an administrator for /start"
            );
            false
        }
    };
    if !is_administrator {
        return false;
    }
    if !message.chat.is_group() && !message.chat.is_supergroup() {
        return false;
    }
    let command = command_with_at(message);
    !allowed_commands
        .iter()
        .any(|allowed| command.as_deref() == Some(allowed.as_str()))
}

fn command_with_at(message: &Message) -> Option<String> {
    let entity = message.parse_entities()?.into_iter().next()?;
    if entity.start() != 0 || !matches!(entity.kind(), MessageEntityKind::BotCommand) {
        return None;
    }
    entity.text().strip_prefix('/').map(str::to_owned)
}

async fn send_help_command(
    bot: &Bot,
    message: &Message,
    me: &Me,
    context: &AppContext,
) -> ResponseResult<()> {
    if should_suppress_group_command(
        bot,
        message,
        me,
        &[
            format!("help@{}", me.username()),
            format!("start@{}", me.username()),
        ],
    )
    .await
    {
        return Ok(());
    }
    send_help_reply(bot, message, context).await
}

async fn send_help_reply(bot: &Bot, message: &Message, context: &AppContext) -> ResponseResult<()> {
    let text = context.i18n.t(context.config.locale, "system.help", &[]);
    bot.send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}
