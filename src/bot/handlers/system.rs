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

    pub async fn handle_cancel(
        bot: Bot,
        msg: Message,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let administrator_gate_failed =
            match group_command_suppressed(&bot, &msg, &me, &[format!("cancel@{}", me.username())])
                .await
            {
                Ok(true) => return Ok(()),
                Ok(false) => false,
                Err(source) => {
                    error!(?source, "failed to apply the /cancel administrator gate");
                    true
                }
            };

        let Some(user_id) = msg
            .from
            .as_ref()
            .and_then(|user| i64::try_from(user.id.0).ok())
        else {
            send_cancel_message(&bot, &msg, "发生了一些错误，请稍后再试", false).await;
            return Ok(());
        };
        let Some(state) = ctx.recap_state.as_deref() else {
            error!("forwarded recap state store is unavailable for /cancel");
            send_cancel_message(&bot, &msg, "发生了一些错误，请稍后再试", false).await;
            return Ok(());
        };
        let (text, reply) = match state.cancel_forwarded(user_id).await {
            Ok(true) => (
                "好的，已经帮你把消息清理掉了，如果需要总结转发的消息，可以再次发送 /recap_forwarded_start 开始操作。".to_owned(),
                true,
            ),
            Ok(false) if administrator_gate_failed => {
                ("发生了一些错误，请稍后再试".to_owned(), false)
            }
            Ok(false) => (ctx.i18n.t(ctx.config.locale, "system.cancel", &[]), true),
            Err(source) => {
                error!(?source, user_id, "failed to cancel forwarded recap session");
                ("发生了一些错误，请稍后再试".to_owned(), false)
            }
        };
        send_cancel_message(&bot, &msg, &text, reply).await;
        Ok(())
    }
}

async fn send_cancel_message(bot: &Bot, message: &Message, text: &str, reply: bool) {
    let request = bot.send_message(message.chat.id, text);
    let result = if reply {
        request
            .reply_parameters(ReplyParameters::new(message.id))
            .await
    } else {
        request.await
    };
    if let Err(source) = result {
        error!(?source, "failed to send /cancel response");
    }
}

async fn should_suppress_group_command(
    bot: &Bot,
    message: &Message,
    me: &Me,
    allowed_commands: &[String],
) -> bool {
    match group_command_suppressed(bot, message, me, allowed_commands).await {
        Ok(suppressed) => suppressed,
        Err(source) => {
            error!(
                ?source,
                chat_id = message.chat.id.0,
                "failed to check whether the bot is an administrator for a command"
            );
            false
        }
    }
}

async fn group_command_suppressed(
    bot: &Bot,
    message: &Message,
    me: &Me,
    allowed_commands: &[String],
) -> Result<bool, teloxide::RequestError> {
    let is_administrator = bot.get_chat_member(message.chat.id, me.id).await?.status()
        == ChatMemberStatus::Administrator;
    if !is_administrator {
        return Ok(false);
    }
    if !message.chat.is_group() && !message.chat.is_supergroup() {
        return Ok(false);
    }
    let command = command_with_at(message);
    Ok(!allowed_commands
        .iter()
        .any(|allowed| command.as_deref() == Some(allowed.as_str())))
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
