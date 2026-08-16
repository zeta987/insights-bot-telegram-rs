use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{ChatMemberStatus, Me, MessageEntityKind, ParseMode, ReplyParameters},
};
use tracing::error;

use crate::{
    bot::{context::AppContext, handlers::recap_manual::escape_html},
    config::Locale,
    i18n::I18n,
};

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
    send_help_reply(bot, message, me, context).await
}

async fn send_help_reply(
    bot: &Bot,
    message: &Message,
    me: &Me,
    context: &AppContext,
) -> ResponseResult<()> {
    let locale = locale_for_message(message, context);
    let text = build_help_message(&context.i18n, locale, me.username());
    bot.send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id))
        .parse_mode(ParseMode::Html)
        .await?;
    Ok(())
}

/// Go `pkg/bots/tgbot/context.go:137-156`: resolve the locale from the
/// *sender's* per-message Telegram `language_code`, not a value pinned at
/// startup. Go always falls back to the literal `"en"` for a missing sender
/// or empty code; this port falls back to `AppConfig::locale` instead, so an
/// operator's `INSIGHTS_LANG` choice still governs when Telegram sends
/// nothing usable.
fn locale_for_message(message: &Message, context: &AppContext) -> Locale {
    let code = message
        .from
        .as_ref()
        .and_then(|user| user.language_code.as_deref());
    Locale::from_language_code(code, context.config.locale)
}

/// A command's help text as rendered in the composed `/help` listing. Go
/// sources this two different ways depending on which command group it
/// belongs to (see [`CommandGroup`]).
enum HelpText {
    /// Looked up per-message via [`I18n::t`] -- Go's basic-group
    /// `HelpMessage(c)` closures, which call `c.T(...)`
    /// (`pkg/bots/tgbot/dispatcher.go:62-64`, `help_command.go`,
    /// `start_command.go`).
    Localized(&'static str),
    /// A fixed string regardless of locale -- Go's recap-group
    /// `HelpMessage` closures in
    /// `internal/bots/telegram/handlers/recap/recap.go:41-86`, which return
    /// Simplified Chinese literals directly instead of calling `c.T(...)`.
    /// This is a documented Go quirk (the recap group's name and every
    /// command's help text render in Simplified Chinese even when the rest
    /// of `/help` is in English or Traditional Chinese) that this port
    /// reproduces exactly rather than "fixing".
    Fixed(&'static str),
}

struct CommandEntry {
    command: &'static str,
    help: HelpText,
}

struct CommandGroup {
    name: HelpText,
    commands: &'static [CommandEntry],
}

/// Go `pkg/bots/tgbot/dispatcher.go:59-65`: the basic command group, in
/// registration order (help, cancel, start).
const BASIC_GROUP: CommandGroup = CommandGroup {
    name: HelpText::Localized("system.commands.groups.basic.name"),
    commands: &[
        CommandEntry {
            command: "help",
            help: HelpText::Localized("system.commands.groups.basic.commands.help.help"),
        },
        CommandEntry {
            command: "cancel",
            help: HelpText::Localized("system.commands.groups.basic.commands.cancel.help"),
        },
        CommandEntry {
            command: "start",
            help: HelpText::Localized("system.commands.groups.basic.commands.start.help"),
        },
    ],
};

/// Go `internal/bots/telegram/handlers/recap/recap.go:41-86`, in
/// registration order. `/smr` (Go's `summarization` group) has no port yet
/// and is deliberately excluded, matching this repository's decided feature
/// scope.
const RECAP_GROUP: CommandGroup = CommandGroup {
    name: HelpText::Fixed("聊天回顾"),
    commands: &[
        CommandEntry {
            command: "recap",
            help: HelpText::Fixed("总结过去的聊天记录并生成回顾快报"),
        },
        CommandEntry {
            command: "configure_recap",
            help: HelpText::Fixed(
                "配置聊天记录回顾（需要管理权限，<b>请在配置的时候尽量避免使用匿名用户身份或者其他群组的身份进行配置，可能会导致权限检查异常而配置失败。</b>）",
            ),
        },
        CommandEntry {
            command: "recap_forwarded_start",
            help: HelpText::Fixed(
                "使 Bot 接收在私聊中转发给 Bot 的消息，并在发送 /recap_forwarded 后开始总结",
            ),
        },
        CommandEntry {
            command: "recap_forwarded",
            help: HelpText::Fixed(
                "使 Bot 停止接收在私聊中转发给 Bot 的消息，对已经转发过的消息进行总结",
            ),
        },
        CommandEntry {
            command: "subscribe_recap",
            help: HelpText::Fixed("订阅当前群组的定时聊天回顾"),
        },
        CommandEntry {
            command: "unsubscribe_recap",
            help: HelpText::Fixed("取消订阅当前群组的定时聊天回顾"),
        },
    ],
};

/// Registration order matches Go: the basic group is registered inside
/// `NewDispatcher()` before any module's `Install()` runs
/// (`dispatcher.go:38-72`), and recap is the only other module this port
/// carries.
const COMMAND_GROUPS: &[CommandGroup] = &[BASIC_GROUP, RECAP_GROUP];

fn resolve_help_text(i18n: &I18n, locale: Locale, help: &HelpText) -> String {
    match help {
        HelpText::Localized(key) => i18n.t(locale, key, &[]),
        HelpText::Fixed(text) => (*text).to_owned(),
    }
}

/// Go `pkg/bots/tgbot/help_command.go:44-101`: compose the `/help` body by
/// joining each registered command group's `<b>name</b>` header with its
/// `/cmd@bot - help` lines, then splice the result into the outer help
/// message template.
pub(crate) fn build_help_message(i18n: &I18n, locale: Locale, bot_username: &str) -> String {
    let group_blocks: Vec<String> = COMMAND_GROUPS
        .iter()
        .map(|group| {
            let name = resolve_help_text(i18n, locale, &group.name);
            let command_lines: Vec<String> = group
                .commands
                .iter()
                .map(|entry| {
                    let help = resolve_help_text(i18n, locale, &entry.help);
                    let mut line = format!("/{}@{bot_username}", entry.command);
                    if !help.is_empty() {
                        line.push_str(" - ");
                        line.push_str(&help);
                    }
                    line
                })
                .collect();
            if name.is_empty() {
                command_lines.join("\n")
            } else {
                format!(
                    "<b>{}</b>\n\n{}",
                    escape_html(&name),
                    command_lines.join("\n")
                )
            }
        })
        .collect();

    i18n.t(
        locale,
        "system.commands.groups.basic.commands.help.message",
        &[("Commands", &group_blocks.join("\n\n"))],
    )
}
