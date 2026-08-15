//! Go v1.0.0 `/configure_recap` command and callback presentation primitives.

use std::sync::Arc;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use teloxide::{
    payloads::{EditMessageTextSetters, SendMessageSetters},
    prelude::{Bot, Requester, ResponseResult},
    types::{
        CallbackQuery, ChatMemberStatus, InlineKeyboardButton, InlineKeyboardMarkup, Me, Message,
        ParseMode, ReplyParameters,
    },
};
use tracing::error;

use crate::{
    bot::{
        context::AppContext,
        handlers::{recap_manual::to_go_json, recap_subscription::is_group_anonymous_bot},
    },
    db::{codec, feature_flags, models::AutoRecapSendMode, recap_options},
    redis::{keys, recap_state::RecapStateStore},
    services::autorecap::queue_next_auto_recap,
};

pub const ROUTE_NOP: &str = "nop";
const CONFIGURE_HEADER: &str = "好的。请在下面点击你想配置的选项进行操作吧。";
const GROUP_ONLY: &str = "只有在群组和超级群组内才可以配置聊天记录回顾功能哦！";
const BOT_ADMIN_REQUIRED: &str = "抱歉，此操作无法进行，现在机器人不是<b>群组管理员</b>，已经不会记录任何聊天记录了。如果需要配置聊天记录回顾功能，<b>请先将机器人设为群组管理员</b>，然后再次执行命令后再试";
const ACTOR_ADMIN_REQUIRED: &str =
    "抱歉，此操作无法进行，需要<b>管理员</b>权限才能配置聊天记录回顾功能。";
const CONFIGURE_UNAVAILABLE: &str = "暂时无法配置聊天记录回顾功能，请稍后再试！";
const APPLY_CONFIG_ERROR: &str = "好的。请在下面点击你想配置的选项进行操作吧。\n\n应用聊天记录回顾功能的配置时出现了问题，请稍后再试！";
const CREATOR_MODE_REQUIRED: &str = "好的。请在下面点击你想配置的选项进行操作吧。\n\n抱歉，只有群组创建者才可以配置聊天记录回顾模式。";
const CREATOR_RATE_REQUIRED: &str = "好的。请在下面点击你想配置的选项进行操作吧。\n\n抱歉，只有群组创建者才可以配置每天自动创建聊天回顾的频率次数。";
const APPLY_PIN_ERROR: &str = "好的。请在下面点击你想配置的选项进行操作吧。\n\n应用聊天记录回顾消息置顶功能的配置时出现了问题，请稍后再试！";

fn display_rate(rates_per_day: i64) -> i64 {
    if rates_per_day == 0 { 4 } else { rates_per_day }
}

/// Current values rendered by Go's `/configure_recap` keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigureRecapView {
    pub chat_id: i64,
    pub from_id: i64,
    pub recap_enabled: bool,
    pub send_mode: i64,
    pub rates_per_day: i64,
    pub pin_enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct ToggleActionData {
    status: bool,
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct AssignModeActionData {
    mode: i64,
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct CompleteActionData {
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct RatesActionData {
    rates: i64,
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct PinActionData {
    status: bool,
    #[serde(rename = "chatId")]
    chat_id: i64,
}

async fn callback<T>(
    state: &(impl RecapStateStore + ?Sized),
    route: &str,
    data: &T,
) -> Result<String>
where
    T: Serialize,
{
    state.put_callback(route, &to_go_json(data)?).await
}

async fn reply(
    bot: &Bot,
    message: &Message,
    text: &str,
    html: bool,
    markup: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    let mut request = bot
        .send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id));
    if html {
        request = request.parse_mode(ParseMode::Html);
    }
    if let Some(markup) = markup {
        request = request.reply_markup(markup);
    }
    request.await?;
    Ok(())
}

/// Handle Go's `/configure_recap` command with its bot and actor permission
/// checks. A missing options row is represented only by an in-memory default.
pub async fn handle_configure_recap(
    bot: Bot,
    message: Message,
    me: Me,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if !message.chat.is_group() && !message.chat.is_supergroup() {
        return reply(&bot, &message, GROUP_ONLY, false, None).await;
    }

    let bot_is_admin = match bot.get_chat_member(message.chat.id, me.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Administrator,
        Err(error) => {
            error!(?error, "failed to check recap configuration bot permission");
            return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
        }
    };
    if !bot_is_admin {
        return reply(&bot, &message, BOT_ADMIN_REQUIRED, true, None).await;
    }

    let Some(actor) = message.from.as_ref() else {
        return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
    };
    if !is_group_anonymous_bot(actor) {
        let actor_is_admin = match bot.get_chat_member(message.chat.id, actor.id).await {
            Ok(member) => matches!(
                member.status(),
                ChatMemberStatus::Owner | ChatMemberStatus::Administrator
            ),
            Err(error) => {
                error!(
                    ?error,
                    "failed to check recap configuration actor permission"
                );
                return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
            }
        };
        if !actor_is_admin {
            return reply(&bot, &message, ACTOR_ADMIN_REQUIRED, true, None).await;
        }
    }

    let chat_id = message.chat.id.0;
    let chat_title = message.chat.title().unwrap_or_default();
    let recap_enabled =
        match feature_flags::has_recap_enabled(&context.db, chat_id, chat_title).await {
            Ok(enabled) => enabled,
            Err(error) => {
                error!(?error, "failed to load recap feature status");
                return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
            }
        };
    let options = match recap_options::find_one(&context.db, chat_id).await {
        Ok(options) => options,
        Err(error) => {
            error!(?error, "failed to load recap options");
            return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
        }
    };
    let (send_mode, rates_per_day, pin_enabled) = options.map_or((0, 4, false), |options| {
        (
            options.auto_recap_send_mode,
            options.auto_recap_rates_per_day,
            options.pin_auto_recap_message,
        )
    });
    let Some(state) = context.recap_state.as_deref() else {
        error!("recap state is unavailable for recap configuration");
        return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
    };
    let keyboard = match build_configure_keyboard(
        state,
        ConfigureRecapView {
            chat_id,
            from_id: i64::try_from(actor.id.0).unwrap_or(i64::MAX),
            recap_enabled,
            send_mode,
            rates_per_day,
            pin_enabled,
        },
    )
    .await
    {
        Ok(keyboard) => keyboard,
        Err(error) => {
            error!(?error, "failed to build recap configuration keyboard");
            return reply(&bot, &message, CONFIGURE_UNAVAILABLE, false, None).await;
        }
    };

    reply(&bot, &message, CONFIGURE_HEADER, false, Some(keyboard)).await
}

fn callback_origin_is_anonymous(callback: &CallbackQuery) -> bool {
    callback
        .message
        .as_ref()
        .and_then(|message| message.regular_message())
        .and_then(Message::reply_to_message)
        .and_then(|message| message.from.as_ref())
        .is_some_and(is_group_anonymous_bot)
}

async fn edit_configuration(
    bot: &Bot,
    callback: &CallbackQuery,
    text: &str,
    markup: Option<InlineKeyboardMarkup>,
) -> ResponseResult<()> {
    let Some(message) = callback.message.as_ref() else {
        return Ok(());
    };
    let mut request = bot
        .edit_message_text(message.chat().id, message.id(), text)
        .parse_mode(ParseMode::Html);
    if let Some(markup) = markup {
        request = request.reply_markup(markup);
    }
    request.await?;
    Ok(())
}

/// Apply Go's recap feature toggle. Enabling creates a usable options row and
/// schedules the deterministic TimeCapsule member; disabling leaves any queued
/// member untouched until the worker pops it and observes the disabled flag.
pub async fn handle_toggle_callback(
    bot: Bot,
    callback: CallbackQuery,
    me: Me,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let data = match serde_json::from_str::<ToggleActionData>(&payload_json) {
        Ok(data) => data,
        Err(error) => {
            error!(?error, "failed to bind recap toggle callback payload");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(message) = callback
        .message
        .as_ref()
        .and_then(|message| message.regular_message())
    else {
        return Ok(());
    };
    if message.chat.id.0 != data.chat_id
        || (i64::try_from(callback.from.id.0).unwrap_or(i64::MAX) != data.from_id
            && !callback_origin_is_anonymous(&callback))
    {
        return Ok(());
    }

    let bot_is_admin = match bot.get_chat_member(message.chat.id, me.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Administrator,
        Err(error) => {
            error!(?error, "failed to check recap toggle bot permission");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    if !bot_is_admin {
        return edit_configuration(&bot, &callback, BOT_ADMIN_REQUIRED, None).await;
    }
    let actor_is_admin = if is_group_anonymous_bot(&callback.from) {
        true
    } else {
        match bot.get_chat_member(message.chat.id, callback.from.id).await {
            Ok(member) => matches!(
                member.status(),
                ChatMemberStatus::Owner | ChatMemberStatus::Administrator
            ),
            Err(error) => {
                error!(?error, "failed to check recap toggle actor permission");
                return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
            }
        }
    };
    if !actor_is_admin {
        return edit_configuration(&bot, &callback, ACTOR_ADMIN_REQUIRED, None).await;
    }

    let chat_type = if message.chat.is_group() {
        "group"
    } else {
        "supergroup"
    };
    let chat_title = message.chat.title().unwrap_or_default();
    let result = async {
        let options = recap_options::find_one_or_create(&context.db, data.chat_id).await?;
        if data.status {
            feature_flags::enable_recap(&context.db, data.chat_id, chat_type, chat_title).await?;
            let state = context
                .recap_state
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("recap state is unavailable"))?;
            queue_next_auto_recap(
                state,
                data.chat_id,
                options.auto_recap_rates_per_day,
                context.config.timezone_shift_seconds,
                codec::now_unix_millis(),
            )
            .await;
        } else {
            feature_flags::disable_recap(&context.db, data.chat_id, chat_type, chat_title).await?;
        }
        Result::<_, anyhow::Error>::Ok(options)
    }
    .await;
    let options = match result {
        Ok(options) => options,
        Err(error) => {
            error!(?error, "failed to apply recap toggle");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(state) = context.recap_state.as_deref() else {
        return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
    };
    let keyboard = match build_configure_keyboard(
        state,
        ConfigureRecapView {
            chat_id: data.chat_id,
            from_id: data.from_id,
            recap_enabled: data.status,
            send_mode: options.auto_recap_send_mode,
            rates_per_day: display_rate(options.auto_recap_rates_per_day),
            pin_enabled: options.pin_auto_recap_message,
        },
    )
    .await
    {
        Ok(keyboard) => keyboard,
        Err(error) => {
            error!(?error, "failed to rebuild recap toggle keyboard");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let text = if data.status {
        format!(
            "{CONFIGURE_HEADER}\n\n聊天记录回顾功能已开启，开启后将会自动收集群组中的聊天记录并定时发送聊天回顾快报。"
        )
    } else {
        format!(
            "{CONFIGURE_HEADER}\n\n聊天记录回顾功能已关闭，关闭后将不会再收集群组中的聊天记录了。"
        )
    };
    edit_configuration(&bot, &callback, &text, Some(keyboard)).await
}

/// Apply the creator-only public/private delivery mode without changing the
/// automatic recap queue.
pub async fn handle_assign_mode_callback(
    bot: Bot,
    callback: CallbackQuery,
    me: Me,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let data = match serde_json::from_str::<AssignModeActionData>(&payload_json) {
        Ok(data) => data,
        Err(error) => {
            error!(?error, "failed to bind recap mode callback payload");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(message) = callback
        .message
        .as_ref()
        .and_then(|message| message.regular_message())
    else {
        return Ok(());
    };
    if message.chat.id.0 != data.chat_id
        || (i64::try_from(callback.from.id.0).unwrap_or(i64::MAX) != data.from_id
            && !callback_origin_is_anonymous(&callback))
    {
        return Ok(());
    }
    let bot_is_admin = match bot.get_chat_member(message.chat.id, me.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Administrator,
        Err(error) => {
            error!(?error, "failed to check recap mode bot permission");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    if !bot_is_admin {
        return edit_configuration(&bot, &callback, BOT_ADMIN_REQUIRED, None).await;
    }
    let actor_is_owner = match bot.get_chat_member(message.chat.id, callback.from.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Owner,
        Err(error) => {
            error!(?error, "failed to check recap mode actor permission");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    if !actor_is_owner {
        return edit_configuration(&bot, &callback, CREATOR_MODE_REQUIRED, None).await;
    }
    let Some(mode) = AutoRecapSendMode::from_stored(data.mode) else {
        return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
    };
    if let Err(error) = recap_options::set_send_mode(&context.db, data.chat_id, mode).await {
        error!(?error, "failed to store recap delivery mode");
        return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
    }
    let chat_title = message.chat.title().unwrap_or_default();
    let recap_enabled =
        match feature_flags::has_recap_enabled(&context.db, data.chat_id, chat_title).await {
            Ok(enabled) => enabled,
            Err(error) => {
                error!(
                    ?error,
                    "failed to reload recap feature status after mode update"
                );
                return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
            }
        };
    let options = match recap_options::find_one(&context.db, data.chat_id).await {
        Ok(Some(options)) => options,
        Ok(None) => return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await,
        Err(error) => {
            error!(?error, "failed to reload recap options after mode update");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(state) = context.recap_state.as_deref() else {
        return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
    };
    let keyboard = match build_configure_keyboard(
        state,
        ConfigureRecapView {
            chat_id: data.chat_id,
            from_id: data.from_id,
            recap_enabled,
            send_mode: options.auto_recap_send_mode,
            rates_per_day: display_rate(options.auto_recap_rates_per_day),
            pin_enabled: options.pin_auto_recap_message,
        },
    )
    .await
    {
        Ok(keyboard) => keyboard,
        Err(error) => {
            error!(?error, "failed to rebuild recap mode keyboard");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let text = if data.mode == AutoRecapSendMode::Publicly.as_stored() {
        format!(
            "{CONFIGURE_HEADER}\n\n聊天记录回顾模式已切换为<b>公开</b>，将会自动收集群组中的聊天记录并定时发送聊天回顾快报。"
        )
    } else {
        format!(
            "{CONFIGURE_HEADER}\n\n聊天记录回顾模式已切换为<b>私聊</b>，将会自动收集群组中的聊天记录并定时发送聊天回顾快报给通过 /subscribe_recap 命令订阅了本群组聊天回顾用户。"
        )
    };
    edit_configuration(&bot, &callback, &text, Some(keyboard)).await
}

/// Apply the creator-only daily frequency and immediately rescore the one
/// deterministic queue member, even while recap is currently disabled.
pub async fn handle_rates_callback(
    bot: Bot,
    callback: CallbackQuery,
    me: Me,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let data = match serde_json::from_str::<RatesActionData>(&payload_json) {
        Ok(data) => data,
        Err(error) => {
            error!(?error, "failed to bind recap rate callback payload");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(message) = callback
        .message
        .as_ref()
        .and_then(|message| message.regular_message())
    else {
        return Ok(());
    };
    if message.chat.id.0 != data.chat_id
        || (i64::try_from(callback.from.id.0).unwrap_or(i64::MAX) != data.from_id
            && !callback_origin_is_anonymous(&callback))
    {
        return Ok(());
    }
    let bot_is_admin = match bot.get_chat_member(message.chat.id, me.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Administrator,
        Err(error) => {
            error!(?error, "failed to check recap rate bot permission");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    if !bot_is_admin {
        return edit_configuration(&bot, &callback, BOT_ADMIN_REQUIRED, None).await;
    }
    let actor_is_owner = match bot.get_chat_member(message.chat.id, callback.from.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Owner,
        Err(error) => {
            error!(?error, "failed to check recap rate actor permission");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    if !actor_is_owner {
        return edit_configuration(&bot, &callback, CREATOR_RATE_REQUIRED, None).await;
    }
    if let Err(error) =
        recap_options::set_rates_per_day(&context.db, data.chat_id, data.rates).await
    {
        error!(?error, "failed to store recap daily rate");
        return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
    }
    let options = match recap_options::find_one(&context.db, data.chat_id).await {
        Ok(Some(options)) => options,
        Ok(None) => return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await,
        Err(error) => {
            error!(?error, "failed to reload recap options after rate update");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(state) = context.recap_state.as_deref() else {
        return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
    };
    queue_next_auto_recap(
        state,
        data.chat_id,
        options.auto_recap_rates_per_day,
        context.config.timezone_shift_seconds,
        codec::now_unix_millis(),
    )
    .await;
    let chat_title = message.chat.title().unwrap_or_default();
    let recap_enabled =
        match feature_flags::has_recap_enabled(&context.db, data.chat_id, chat_title).await {
            Ok(enabled) => enabled,
            Err(error) => {
                error!(
                    ?error,
                    "failed to reload recap feature status after rate update"
                );
                return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
            }
        };
    let keyboard = match build_configure_keyboard(
        state,
        ConfigureRecapView {
            chat_id: data.chat_id,
            from_id: data.from_id,
            recap_enabled,
            send_mode: options.auto_recap_send_mode,
            rates_per_day: options.auto_recap_rates_per_day,
            pin_enabled: options.pin_auto_recap_message,
        },
    )
    .await
    {
        Ok(keyboard) => keyboard,
        Err(error) => {
            error!(?error, "failed to rebuild recap rate keyboard");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let schedule = match data.rates {
        2 => "<b>08:00</b>，<b>20:00</b>",
        3 => "<b>00:00</b>，<b>08:00</b>，<b>16:00</b>",
        _ => "<b>02:00</b>，<b>08:00</b>，<b>14:00</b>，<b>20:00</b>",
    };
    let text = format!(
        "{CONFIGURE_HEADER}\n\n每天自动创建聊天回顾的频率次数已设定为 <b>{}</b>，将会自动收集群组中的聊天记录并在 {schedule} 发送聊天回顾快报。",
        data.rates
    );
    edit_configuration(&bot, &callback, &text, Some(keyboard)).await
}

/// Apply Go's creator-only pin option, including its visible wiring quirk:
/// `status` is reused as both the recap-enabled and pin-selected UI state.
pub async fn handle_pin_callback(
    bot: Bot,
    callback: CallbackQuery,
    me: Me,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let data = match serde_json::from_str::<PinActionData>(&payload_json) {
        Ok(data) => data,
        Err(error) => {
            error!(?error, "failed to bind recap pin callback payload");
            return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
        }
    };
    let Some(message) = callback
        .message
        .as_ref()
        .and_then(|message| message.regular_message())
    else {
        return Ok(());
    };
    let bot_is_admin = match bot.get_chat_member(message.chat.id, me.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Administrator,
        Err(error) => {
            error!(?error, "failed to check recap pin bot permission");
            return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
        }
    };
    if !bot_is_admin {
        return edit_configuration(&bot, &callback, BOT_ADMIN_REQUIRED, None).await;
    }
    let actor_is_owner = match bot.get_chat_member(message.chat.id, callback.from.id).await {
        Ok(member) => member.status() == ChatMemberStatus::Owner,
        Err(error) => {
            error!(?error, "failed to check recap pin actor permission");
            return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
        }
    };
    if !actor_is_owner {
        return Ok(());
    }
    let chat_id = message.chat.id.0;
    let options = match recap_options::find_one(&context.db, chat_id).await {
        Ok(Some(options)) => options,
        Ok(None) => match recap_options::find_one_or_create(&context.db, chat_id).await {
            Ok(options) => options,
            Err(error) => {
                error!(?error, "failed to create recap options for pin update");
                return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
            }
        },
        Err(error) => {
            error!(?error, "failed to load recap options before pin update");
            return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
        }
    };
    let updated = if data.status {
        recap_options::set_pin_enabled(&context.db, chat_id).await
    } else {
        recap_options::set_pin_disabled(&context.db, chat_id).await
    };
    if let Err(error) = updated {
        error!(?error, "failed to store recap pin option");
        return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
    }
    let Some(state) = context.recap_state.as_deref() else {
        return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
    };
    let keyboard = match build_configure_keyboard(
        state,
        ConfigureRecapView {
            chat_id,
            from_id: i64::try_from(callback.from.id.0).unwrap_or(i64::MAX),
            // Pinned Go passes the action's pin status as recap status.
            recap_enabled: data.status,
            send_mode: options.auto_recap_send_mode,
            rates_per_day: display_rate(options.auto_recap_rates_per_day),
            pin_enabled: data.status,
        },
    )
    .await
    {
        Ok(keyboard) => keyboard,
        Err(error) => {
            error!(?error, "failed to rebuild recap pin keyboard");
            return edit_configuration(&bot, &callback, APPLY_PIN_ERROR, None).await;
        }
    };
    let text = if data.status {
        format!(
            "{CONFIGURE_HEADER}\n\n聊天记录回顾消息置顶功能已开启，开启后将会自动收集群组中的聊天记录并定时发送聊天回顾快报。"
        )
    } else {
        format!(
            "{CONFIGURE_HEADER}\n\n聊天记录回顾消息置顶功能已关闭，关闭后将不会再收集群组中的聊天记录了。"
        )
    };
    edit_configuration(&bot, &callback, &text, Some(keyboard)).await
}

/// Finish configuration by deleting the settings message and its original
/// command. Go checks only the actor here and treats both deletions as
/// best-effort Telegram operations.
pub async fn handle_complete_callback(
    bot: Bot,
    callback: CallbackQuery,
    payload_json: String,
) -> ResponseResult<()> {
    let data = match serde_json::from_str::<CompleteActionData>(&payload_json) {
        Ok(data) => data,
        Err(error) => {
            error!(?error, "failed to bind recap complete callback payload");
            return edit_configuration(&bot, &callback, APPLY_CONFIG_ERROR, None).await;
        }
    };
    let Some(message) = callback
        .message
        .as_ref()
        .and_then(|message| message.regular_message())
    else {
        return Ok(());
    };
    if message.chat.id.0 != data.chat_id
        || (i64::try_from(callback.from.id.0).unwrap_or(i64::MAX) != data.from_id
            && !callback_origin_is_anonymous(&callback))
    {
        return Ok(());
    }
    let actor_is_admin = if is_group_anonymous_bot(&callback.from) {
        true
    } else {
        match bot.get_chat_member(message.chat.id, callback.from.id).await {
            Ok(member) => matches!(
                member.status(),
                ChatMemberStatus::Owner | ChatMemberStatus::Administrator
            ),
            Err(error) => {
                error!(?error, "failed to check recap complete actor permission");
                return edit_configuration(&bot, &callback, CONFIGURE_UNAVAILABLE, None).await;
            }
        }
    };
    if !actor_is_admin {
        return Ok(());
    }
    let original_message_id = message.reply_to_message().map(|message| message.id);
    if let Err(error) = bot.delete_message(message.chat.id, message.id).await {
        error!(?error, "failed to delete recap configuration message");
    }
    if let Some(message_id) = original_message_id
        && let Err(error) = bot.delete_message(message.chat.id, message_id).await
    {
        error!(?error, "failed to delete recap configuration command");
    }
    Ok(())
}

/// Build Go's five-row disabled or nine-row enabled configuration keyboard.
///
/// Callback payload field order and compact camel-case JSON are wire format:
/// changing either changes the SHA-256 action hash shown to Telegram.
pub async fn build_configure_keyboard(
    state: &(impl RecapStateStore + ?Sized),
    view: ConfigureRecapView,
) -> Result<InlineKeyboardMarkup> {
    let nop = state.put_callback(ROUTE_NOP, r#""""#).await?;
    let toggle_on = callback(
        state,
        keys::ROUTE_CONFIGURE_TOGGLE,
        &ToggleActionData {
            status: true,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let toggle_off = callback(
        state,
        keys::ROUTE_CONFIGURE_TOGGLE,
        &ToggleActionData {
            status: false,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let public = callback(
        state,
        keys::ROUTE_CONFIGURE_ASSIGN_MODE,
        &AssignModeActionData {
            mode: 0,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let private = callback(
        state,
        keys::ROUTE_CONFIGURE_ASSIGN_MODE,
        &AssignModeActionData {
            mode: 1,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let complete = callback(
        state,
        keys::ROUTE_CONFIGURE_COMPLETE,
        &CompleteActionData {
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    // Go allocates pin callbacks before returning its disabled keyboard even
    // though those buttons are not visible in that state.
    let pin_on = callback(
        state,
        keys::ROUTE_CONFIGURE_PIN,
        &PinActionData {
            status: true,
            chat_id: view.chat_id,
        },
    )
    .await?;
    let pin_off = callback(
        state,
        keys::ROUTE_CONFIGURE_PIN,
        &PinActionData {
            status: false,
            chat_id: view.chat_id,
        },
    )
    .await?;

    let selected = |selected: bool, label: &str| {
        if selected {
            format!("🔘 {label}")
        } else {
            label.to_owned()
        }
    };
    let mut rows = vec![
        vec![InlineKeyboardButton::callback(
            "🔈 聊天记录回顾",
            nop.clone(),
        )],
        vec![
            InlineKeyboardButton::callback(selected(view.recap_enabled, "开启"), toggle_on),
            InlineKeyboardButton::callback(selected(!view.recap_enabled, "关闭"), toggle_off),
        ],
        vec![InlineKeyboardButton::callback(
            "📩 聊天记录回顾投递方式",
            nop.clone(),
        )],
        vec![
            InlineKeyboardButton::callback(selected(view.send_mode == 0, "公开"), public),
            InlineKeyboardButton::callback(selected(view.send_mode == 1, "私聊"), private),
        ],
    ];
    if !view.recap_enabled {
        rows.push(vec![InlineKeyboardButton::callback("✅ 完成", complete)]);
        return Ok(InlineKeyboardMarkup::new(rows));
    }

    let rate_two = callback(
        state,
        keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
        &RatesActionData {
            rates: 2,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let rate_three = callback(
        state,
        keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
        &RatesActionData {
            rates: 3,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let rate_four = callback(
        state,
        keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
        &RatesActionData {
            rates: 4,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    rows.extend([
        vec![InlineKeyboardButton::callback(
            "🛎️ 每天自动创建回顾次数",
            nop.clone(),
        )],
        vec![
            InlineKeyboardButton::callback(selected(view.rates_per_day == 2, "2 次"), rate_two),
            InlineKeyboardButton::callback(selected(view.rates_per_day == 3, "3 次"), rate_three),
            InlineKeyboardButton::callback(selected(view.rates_per_day == 4, "4 次"), rate_four),
        ],
        vec![InlineKeyboardButton::callback("🪧 置顶聊天记录回顾", nop)],
        vec![
            InlineKeyboardButton::callback(selected(view.pin_enabled, "开启"), pin_on),
            InlineKeyboardButton::callback(selected(!view.pin_enabled, "关闭"), pin_off),
        ],
        vec![InlineKeyboardButton::callback("✅ 完成", complete)],
    ]);
    Ok(InlineKeyboardMarkup::new(rows))
}
