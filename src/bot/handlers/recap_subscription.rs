//! Go v1.0.0 private recap and automatic-recap subscription handlers.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{
        InlineKeyboardButton, InlineKeyboardButtonKind, InlineKeyboardMarkup, Me, MessageId,
        ParseMode, ReplyParameters, User,
    },
};
use tracing::{error, warn};

use crate::{
    bot::{
        context::AppContext,
        handlers::recap_manual::{
            AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS, build_select_hour_keyboard,
            build_vote_keyboard, escape_html, to_go_json,
        },
    },
    db::{feature_flags, models::ReactionCounts, subscribers},
    redis::{
        keys::{self, StartContextDomain},
        recap_state::RecapStateStore,
    },
    services::telegram_rich_message::{
        PlainMessageRequest, TelegramRichMessageClient, TelegramRichMessageError,
    },
};

const GROUP_ANONYMOUS_BOT_ID: u64 = 1_087_968_824;

/// Go's shared private `/start` context, including its serialized field order.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PrivateSubscriptionStartContext {
    pub chat_id: i64,
    pub chat_title: String,
}

/// Go's `recap.UnsubscribeRecapActionData` wire payload.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct UnsubscribeRecapActionData {
    #[serde(rename = "chatId")]
    pub chat_id: i64,
    #[serde(rename = "chatTitle")]
    pub chat_title: String,
    #[serde(rename = "fromId")]
    pub from_id: i64,
}

/// Serialize the start context with Go `encoding/json` HTML-safe escaping.
pub fn encode_start_context(chat_id: i64, chat_title: &str) -> Result<String> {
    to_go_json(&PrivateSubscriptionStartContext {
        chat_id,
        chat_title: chat_title.to_owned(),
    })
}

/// Match Go's four-field `GroupAnonymousBot` predicate exactly.
#[must_use]
pub fn is_group_anonymous_bot(user: &User) -> bool {
    user.id.0 == GROUP_ANONYMOUS_BOT_ID
        && user.is_bot
        && user.username.as_deref() == Some("GroupAnonymousBot")
        && user.first_name == "Group"
}

/// Build the subscriber-only recap keyboard: votes followed by unsubscribe.
pub async fn build_subscriber_vote_keyboard(
    state: &dyn RecapStateStore,
    chat_id: i64,
    chat_title: &str,
    from_id: i64,
    log_id: &str,
    counts: ReactionCounts,
) -> Result<InlineKeyboardMarkup> {
    let mut markup = build_vote_keyboard(state, chat_id, log_id, counts).await?;
    let payload = to_go_json(&UnsubscribeRecapActionData {
        chat_id,
        chat_title: chat_title.to_owned(),
        from_id,
    })?;
    let wire = state
        .put_callback(keys::ROUTE_UNSUBSCRIBE_RECAP, &payload)
        .await?;
    markup
        .inline_keyboard
        .push(vec![InlineKeyboardButton::callback("取消订阅", wire)]);
    Ok(markup)
}

/// Handle the mode-1 `/recap` branch before the public command limiter.
pub async fn handle_private_recap_command(
    bot: &Bot,
    message: &Message,
    me: &Me,
    context: &AppContext,
) -> ResponseResult<()> {
    let Some(from) = message.from.as_ref() else {
        return Ok(());
    };
    let Some(from_id) = telegram_user_id(from) else {
        return Ok(());
    };
    let chat_id = message.chat.id.0;

    if is_group_anonymous_bot(from) {
        send_reply_and_track(
            bot,
            message,
            "匿名管理员无法在设定为私聊回顾模式的群组内请求创建聊天记录回顾哦！如果需要创建聊天记录回顾，必须先将发送角色切换为普通用户然后再试哦。",
            false,
            false,
            from_id,
            context,
        )
        .await;
        return Ok(());
    }

    let Some(state) = context.recap_state.as_deref() else {
        error!(chat_id, "private recap state store is unavailable");
        send_plain_reply(bot, message, "聊天记录回顾生成失败，请稍后再试！", false).await?;
        return Ok(());
    };
    let chat_title = message.chat.title().unwrap_or_default();
    let keyboard = match build_select_hour_keyboard(
        state,
        chat_id,
        chat_title,
        AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS,
    )
    .await
    {
        Ok(keyboard) => keyboard,
        Err(source) => {
            error!(?source, chat_id, "failed to build private recap selector");
            send_plain_reply(bot, message, "聊天记录回顾生成失败，请稍后再试！", false).await?;
            return Ok(());
        }
    };

    let text = private_recap_selector_text(chat_title);
    let client =
        TelegramRichMessageClient::new(context.raw_telegram_http.clone(), &context.config.telegram);
    let result = client
        .send_plain(PlainMessageRequest {
            chat_id: from_id,
            text: &text,
            parse_mode: Some("HTML"),
            reply_markup: Some(&keyboard),
            ..Default::default()
        })
        .await;
    if result.is_ok() {
        if let Err(source) = bot.delete_message(message.chat.id, message.id).await {
            warn!(?source, chat_id, "failed to delete private recap command");
        }
        cleanup_delete_later(bot, state, from_id).await;
        return Ok(());
    }
    let source = result.expect_err("the successful branch returned above");

    let token = StartContextDomain::PrivateSubscription.token(chat_id);
    let payload = match encode_start_context(chat_id, chat_title) {
        Ok(payload) => payload,
        Err(source) => {
            error!(
                ?source,
                chat_id, "failed to encode private recap start context"
            );
            send_plain_reply(bot, message, "聊天记录回顾生成失败，请稍后再试！", false).await?;
            return Ok(());
        }
    };
    if let Err(context_error) = state
        .put_start_context(StartContextDomain::PrivateSubscription, &token, &payload)
        .await
    {
        error!(
            ?context_error,
            chat_id, "failed to store private recap start context"
        );
        send_plain_reply(bot, message, "聊天记录回顾生成失败，请稍后再试！", false).await?;
        return Ok(());
    }

    let guidance = if is_cannot_initiate(&source) {
        Some(private_never_started_guidance(me.username(), &token))
    } else if is_blocked(&source) {
        Some(private_blocked_guidance(me.username(), &token))
    } else {
        None
    };
    if let Some(guidance) = guidance {
        send_reply_and_track(bot, message, &guidance, true, true, from_id, context).await;
    } else {
        error!(error = %source, chat_id, "failed to send private recap selector");
    }
    Ok(())
}

/// Handle `/subscribe_recap` with Go's DM-before-database ordering.
pub async fn handle_subscribe_recap_command(
    bot: Bot,
    message: Message,
    me: Me,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if !message.chat.is_group() && !message.chat.is_supergroup() {
        send_plain_reply(
            &bot,
            &message,
            "只有在群组和超级群组内才可以订阅定时的聊天记录回顾哦！",
            false,
        )
        .await?;
        return Ok(());
    }
    let Some(from) = message.from.as_ref() else {
        return Ok(());
    };
    let Some(from_id) = telegram_user_id(from) else {
        return Ok(());
    };
    if is_group_anonymous_bot(from) {
        send_reply_and_track(
            &bot,
            &message,
            "匿名管理员无法订阅定时的聊天记录回顾哦！如果需要订阅定时的聊天记录回顾，必须先将发送角色切换为普通用户然后再试哦。",
            false,
            false,
            from_id,
            &context,
        )
        .await;
        return Ok(());
    }

    let chat_id = message.chat.id.0;
    let chat_title = message.chat.title().unwrap_or_default();
    match feature_flags::has_recap_enabled(&context.db, chat_id, chat_title).await {
        Ok(true) => {}
        Ok(false) => {
            send_reply_and_track(
                &bot,
                &message,
                "聊天记录回顾功能在当前群组尚未启用，需要在群组管理员通过 /configure_recap 命令配置功能启用后才可以订阅聊天回顾哦。",
                false,
                false,
                from_id,
                &context,
            )
            .await;
            return Ok(());
        }
        Err(source) => {
            error!(
                ?source,
                chat_id, "failed to read recap subscription feature flag"
            );
            send_reply_and_track(
                &bot,
                &message,
                subscription_error_message(),
                false,
                false,
                from_id,
                &context,
            )
            .await;
            return Ok(());
        }
    }

    let text = subscribed_message(chat_title);
    let client =
        TelegramRichMessageClient::new(context.raw_telegram_http.clone(), &context.config.telegram);
    let send_result = client
        .send_plain(PlainMessageRequest {
            chat_id: from_id,
            text: &text,
            parse_mode: Some("HTML"),
            ..Default::default()
        })
        .await;
    if send_result.is_ok() {
        if let Err(source) = subscribers::subscribe(&context.db, chat_id, from_id).await {
            error!(
                ?source,
                chat_id, from_id, "failed to subscribe to automatic recaps"
            );
            send_reply_and_track(
                &bot,
                &message,
                subscription_error_message(),
                false,
                false,
                from_id,
                &context,
            )
            .await;
            return Ok(());
        }
        if let Err(source) = bot.delete_message(message.chat.id, message.id).await {
            warn!(
                ?source,
                chat_id, "failed to delete recap subscription command"
            );
        }
        if let Some(state) = context.recap_state.as_deref() {
            cleanup_delete_later(&bot, state, from_id).await;
        }
        return Ok(());
    }
    let source = send_result.expect_err("the successful branch returned above");

    let Some(state) = context.recap_state.as_deref() else {
        error!(chat_id, "recap subscription state store is unavailable");
        send_plain_reply(&bot, &message, subscription_error_message(), false).await?;
        return Ok(());
    };
    let token = StartContextDomain::SubscribeRecap.token(chat_id);
    let payload = match encode_start_context(chat_id, chat_title) {
        Ok(payload) => payload,
        Err(context_error) => {
            error!(
                ?context_error,
                chat_id, "failed to encode subscription start context"
            );
            send_reply_and_track(
                &bot,
                &message,
                subscription_error_message(),
                false,
                false,
                from_id,
                &context,
            )
            .await;
            return Ok(());
        }
    };
    if let Err(context_error) = state
        .put_start_context(StartContextDomain::SubscribeRecap, &token, &payload)
        .await
    {
        error!(
            ?context_error,
            chat_id, "failed to store subscription start context"
        );
        send_reply_and_track(
            &bot,
            &message,
            subscription_error_message(),
            false,
            false,
            from_id,
            &context,
        )
        .await;
        return Ok(());
    }

    let guidance = if is_cannot_initiate(&source) {
        Some(subscription_never_started_guidance(me.username(), &token))
    } else if is_blocked(&source) {
        Some(subscription_blocked_guidance(me.username(), &token))
    } else {
        None
    };
    if let Some(guidance) = guidance {
        send_reply_and_track(&bot, &message, &guidance, true, true, from_id, &context).await;
    } else {
        error!(error = %source, chat_id, "failed to send recap subscription confirmation");
    }
    Ok(())
}

/// Handle both recap `/start` context namespaces in Go registration order.
pub async fn handle_start_continuation(
    bot: &Bot,
    message: &Message,
    arguments: &str,
    context: &AppContext,
) -> ResponseResult<bool> {
    if arguments.split(' ').count() != 1 {
        return Ok(false);
    }
    let Some(state) = context.recap_state.as_deref() else {
        return Ok(false);
    };

    let private_context = match state
        .get_start_context(StartContextDomain::PrivateSubscription, arguments)
        .await
    {
        Ok(Some(payload)) => {
            match serde_json::from_str::<PrivateSubscriptionStartContext>(&payload) {
                Ok(start_context) => Some(start_context),
                Err(source) => {
                    error!(?source, "failed to decode private recap start context");
                    None
                }
            }
        }
        Ok(None) => None,
        Err(source) => {
            error!(?source, "failed to read private recap start context");
            None
        }
    };
    if let Some(start_context) = private_context {
        let keyboard = match build_select_hour_keyboard(
            state,
            start_context.chat_id,
            &start_context.chat_title,
            AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS,
        )
        .await
        {
            Ok(keyboard) => keyboard,
            Err(source) => {
                error!(?source, "failed to build private recap start selector");
                send_plain_reply(bot, message, "聊天记录回顾生成失败，请稍后再试！", false).await?;
                return Ok(true);
            }
        };
        if let Some(from_id) = message.from.as_ref().and_then(telegram_user_id) {
            cleanup_delete_later(bot, state, from_id).await;
        }
        bot.send_message(
            message.chat.id,
            private_recap_selector_text(&start_context.chat_title),
        )
        .reply_parameters(ReplyParameters::new(message.id))
        .reply_markup(keyboard)
        .parse_mode(ParseMode::Html)
        .await?;
        return Ok(true);
    }

    let Some(from) = message.from.as_ref() else {
        return Ok(false);
    };
    if is_group_anonymous_bot(from) {
        return Ok(false);
    }
    let Some(from_id) = telegram_user_id(from) else {
        return Ok(false);
    };
    let subscription_context = match state
        .get_start_context(StartContextDomain::SubscribeRecap, arguments)
        .await
    {
        Ok(Some(payload)) => {
            match serde_json::from_str::<PrivateSubscriptionStartContext>(&payload) {
                Ok(start_context) => Some(start_context),
                Err(source) => {
                    error!(?source, "failed to decode subscription start context");
                    None
                }
            }
        }
        Ok(None) => None,
        Err(source) => {
            error!(?source, "failed to read subscription start context");
            None
        }
    };
    let Some(start_context) = subscription_context else {
        return Ok(false);
    };
    if let Err(source) = subscribers::subscribe(&context.db, start_context.chat_id, from_id).await {
        error!(?source, "failed to subscribe from start context");
        send_plain_reply(bot, message, subscription_error_message(), false).await?;
        return Ok(true);
    }
    cleanup_delete_later(bot, state, from_id).await;
    bot.send_message(
        message.chat.id,
        subscribed_message(&start_context.chat_title),
    )
    .parse_mode(ParseMode::Html)
    .await?;
    Ok(true)
}

/// Handle `/unsubscribe_recap` with the database side effect first.
pub async fn handle_unsubscribe_recap_command(
    bot: Bot,
    message: Message,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if !message.chat.is_group() && !message.chat.is_supergroup() {
        send_plain_reply(
            &bot,
            &message,
            "只有在群组和超级群组内才可以取消订阅定时的聊天记录回顾哦！",
            false,
        )
        .await?;
        return Ok(());
    }
    let Some(from) = message.from.as_ref() else {
        return Ok(());
    };
    let chat_id = message.chat.id.0;
    if is_group_anonymous_bot(from) {
        if let Err(source) = bot.delete_message(message.chat.id, message.id).await {
            warn!(
                ?source,
                chat_id, "failed to delete anonymous unsubscribe command"
            );
        }
        return Ok(());
    }
    let Some(from_id) = telegram_user_id(from) else {
        return Ok(());
    };
    if let Err(source) = subscribers::unsubscribe(&context.db, chat_id, from_id).await {
        error!(
            ?source,
            chat_id, from_id, "failed to unsubscribe from automatic recaps"
        );
        send_reply_and_track(
            &bot,
            &message,
            subscription_error_message(),
            false,
            false,
            from_id,
            &context,
        )
        .await;
        return Ok(());
    }
    if let Err(source) = bot.delete_message(message.chat.id, message.id).await {
        warn!(?source, chat_id, "failed to delete unsubscribe command");
    }

    let text = unsubscribed_message(message.chat.title().unwrap_or_default());
    let client =
        TelegramRichMessageClient::new(context.raw_telegram_http.clone(), &context.config.telegram);
    if let Err(source) = client
        .send_plain(PlainMessageRequest {
            chat_id: from_id,
            text: &text,
            parse_mode: Some("HTML"),
            ..Default::default()
        })
        .await
    {
        if is_cannot_initiate(&source) || is_blocked(&source) {
            if let Err(delete_error) = bot.delete_message(message.chat.id, message.id).await {
                warn!(
                    ?delete_error,
                    chat_id, "failed to repeat unsubscribe command deletion"
                );
            }
        } else {
            error!(error = %source, chat_id, "failed to send unsubscribe confirmation");
        }
    }
    Ok(())
}

/// Handle the subscriber keyboard's inline unsubscribe action.
pub async fn handle_unsubscribe_callback(
    bot: Bot,
    callback: CallbackQuery,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let Some(message) = callback.message.as_ref() else {
        return Ok(());
    };
    let current_markup = message
        .regular_message()
        .and_then(Message::reply_markup)
        .cloned()
        .unwrap_or_else(empty_keyboard);
    let data = match serde_json::from_str::<UnsubscribeRecapActionData>(&payload_json) {
        Ok(data) => data,
        Err(source) => {
            error!(?source, "failed to decode unsubscribe callback payload");
            edit_unsubscribe_error(&bot, message).await;
            return Ok(());
        }
    };
    let Some(from_id) = telegram_user_id(&callback.from) else {
        return Ok(());
    };
    if data.from_id != from_id {
        warn!(from_id, "unsubscribe callback actor does not match payload");
        return Ok(());
    }
    if let Err(source) = subscribers::unsubscribe(&context.db, data.chat_id, from_id).await {
        error!(?source, "failed to unsubscribe from callback");
        edit_unsubscribe_error(&bot, message).await;
        return Ok(());
    }

    let clicked_wire = callback.data.as_deref().unwrap_or_default();
    let mut updated_markup = current_markup;
    for row in &mut updated_markup.inline_keyboard {
        if let Some(index) = row.iter().position(|button| {
            matches!(
                &button.kind,
                InlineKeyboardButtonKind::CallbackData(data) if data == clicked_wire
            )
        }) {
            row.remove(index);
        }
    }
    updated_markup.inline_keyboard.retain(|row| !row.is_empty());
    if let Err(source) = bot
        .edit_message_reply_markup(message.chat().id, message.id())
        .reply_markup(updated_markup)
        .await
    {
        warn!(?source, "failed to remove unsubscribe callback button");
    }
    if let Err(source) = bot
        .send_message(
            message.chat().id,
            format!(
                "已成功取消订阅群组 <b>{}</b> 的定时聊天回顾。",
                escape_html(&data.chat_title)
            ),
        )
        .parse_mode(ParseMode::Html)
        .await
    {
        warn!(?source, "failed to send unsubscribe confirmation");
    }
    Ok(())
}

/// Remove one subscription row when an ordinary member leaves the group.
pub async fn handle_chat_member_left(
    message: Message,
    left_user: User,
    me: Me,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if left_user.id == me.id {
        return Ok(());
    }
    let Some(left_user_id) = telegram_user_id(&left_user) else {
        return Ok(());
    };
    if let Err(source) =
        subscribers::unsubscribe(&context.db, message.chat.id.0, left_user_id).await
    {
        error!(
            ?source,
            chat_id = message.chat.id.0,
            left_user_id,
            "failed to unsubscribe departed member"
        );
    }
    Ok(())
}

fn telegram_user_id(user: &User) -> Option<i64> {
    i64::try_from(user.id.0).ok()
}

fn private_recap_selector_text(chat_title: &str) -> String {
    format!(
        "您正在请求为群组 <b>{}</b> 创建聊天回顾。\n请问您要为过去几个小时内的聊天创建回顾呢？",
        escape_html(chat_title)
    )
}

fn is_cannot_initiate(error: &TelegramRichMessageError) -> bool {
    matches!(
        error,
        TelegramRichMessageError::Api {
            code: 403,
            description,
            ..
        } if description == "Forbidden: bot can't initiate conversation with a user"
    )
}

fn is_blocked(error: &TelegramRichMessageError) -> bool {
    matches!(
        error,
        TelegramRichMessageError::Api {
            code: 403,
            description,
            ..
        } if description == "Forbidden: bot was blocked by the user"
    )
}

fn private_never_started_guidance(username: &str, token: &str) -> String {
    format!(
        "抱歉，在给您发送引导您创建聊天回顾的消息时出现了问题，这似乎是因为您<b>从未</b>和本 Bot（@{username}） <b>发起过对话</b>导致的。\n\n由于当前群组的聊天回顾功能已经被<b>群组创建者</b>设定为<b>私聊订阅模式</b>，Bot 需要通过私聊的方式向您发送引导您创建聊天回顾的消息，届时，您需要完成以下任一一个操作后方可继续创建聊天回顾：\n1. <b>点击链接</b> https://t.me/{username}?start={token} 与 Bot 开始对话就能继续原先的 /recap 命令操作；\n2. 点击 Bot 头像并且开始对话，然后在群组内重新发送 /recap 命令来创建聊天回顾。"
    )
}

fn private_blocked_guidance(username: &str, token: &str) -> String {
    format!(
        "抱歉，在给您发送引导您创建聊天回顾的消息时出现了问题，这似乎是因为您已将本 Bot（@{username}）<b>停用</b>或是添加到了<b>黑名单</b>中导致的。\n\n由于当前群组的聊天回顾功能已经被<b>群组创建者</b>设定为<b>私聊订阅模式</b>，Bot 需要通过私聊的方式向您发送引导您创建聊天回顾的消息，届时，您需要根据下面的提示进行操作：\n1. 将 Bot 从<b>黑名单中移除</b>；\n2. <b>点击链接</b> https://t.me/{username}?start={token} 继续创建聊天回顾，或是在群组内重新发送 /recap 命令来创建聊天回顾。"
    )
}

fn subscription_never_started_guidance(username: &str, token: &str) -> String {
    format!(
        "抱歉，在为您订阅本群组定时聊天回顾时出现了问题，这似乎是因为您<b>从未</b>和本 Bot（@{username}） <b>发起过对话</b>导致的。\n\n订阅群组的聊天回顾需要 Bot 需要有权限通过私聊的方式向您定期发送聊天回顾，届时，您需要完成以下任一一个操作后方可完成订阅：\n1. <b>点击链接</b> https://t.me/{username}?start={token} 与 Bot 开始对话；\n2. 点击 Bot 头像并且开始对话，然后在群组内重新发送 /subscribe_recap 命令来订阅本群组的定时聊天回顾。"
    )
}

fn subscription_blocked_guidance(username: &str, token: &str) -> String {
    format!(
        "抱歉，在为您订阅本群组定时聊天回顾时出现了问题，这似乎是因为您已将本 Bot（@{username}）<b>停用</b>或是添加到了<b>黑名单</b>中导致的。\n\n订阅群组的聊天回顾需要 Bot 需要有权限通过私聊的方式向您定期发送聊天回顾，届时，您需要根据下面的提示进行操作：\n1. 将 Bot 从<b>黑名单中移除</b>；\n2. <b>点击链接</b> https://t.me/{username}?start={token} 继续订阅本群组的定时聊天回顾操作，或是在群组内重新发送 /subscribe_recap 命令来订阅本群组的定时聊天回顾。"
    )
}

fn subscription_error_message() -> &'static str {
    "订阅群组定时聊天回顾时出现问题，请稍后再试！"
}

fn subscribed_message(chat_title: &str) -> String {
    format!(
        "您已成功订阅群组 <b>{}</b> 的定时聊天回顾！",
        escape_html(chat_title)
    )
}

fn unsubscribed_message(chat_title: &str) -> String {
    format!(
        "您已成功取消订阅群组 <b>{}</b> 的定时聊天回顾！",
        escape_html(chat_title)
    )
}

fn empty_keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new())
}

/// Go `processExceptionError` (`pkg/bots/tgbot/handler.go:117-156`) ignores
/// `ExceptionError.replyMarkup` entirely for the edit branch and only ever
/// calls `NewEditMessageText(chatID, editMessage.MessageID, message)`, so the
/// callback_query.go:381-400 `WithReplyMarkup(...)` calls are dead code. The
/// wire edit here must stay bare: no reply markup, no parse mode.
async fn edit_unsubscribe_error(bot: &Bot, message: &teloxide::types::MaybeInaccessibleMessage) {
    if let Err(source) = bot
        .edit_message_text(
            message.chat().id,
            message.id(),
            "取消订阅时出现了问题，请稍后再试！",
        )
        .await
    {
        warn!(?source, "failed to edit unsubscribe callback error");
    }
}

async fn send_plain_reply(
    bot: &Bot,
    message: &Message,
    text: &str,
    html: bool,
) -> ResponseResult<Message> {
    let request = bot
        .send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id));
    if html {
        request.parse_mode(ParseMode::Html).await
    } else {
        request.await
    }
}

async fn send_reply_and_track(
    bot: &Bot,
    message: &Message,
    text: &str,
    include_original: bool,
    html: bool,
    from_id: i64,
    context: &AppContext,
) {
    let sent = send_plain_reply(bot, message, text, html).await;
    let Some(state) = context.recap_state.as_deref() else {
        return;
    };
    if include_original
        && let Err(source) = state
            .push_delete_later(from_id, message.chat.id.0, message.id.0)
            .await
    {
        error!(?source, "failed to track original delete-later message");
    }
    if let Ok(sent) = sent
        && let Err(source) = state
            .push_delete_later(from_id, sent.chat.id.0, sent.id.0)
            .await
    {
        error!(?source, "failed to track response delete-later message");
    }
}

async fn cleanup_delete_later(bot: &Bot, state: &dyn RecapStateStore, from_id: i64) {
    let drained = match state.drain_delete_later_for_delivery(from_id).await {
        Ok(drained) => drained,
        Err(source) => {
            error!(?source, from_id, "failed to drain delete-later messages");
            return;
        }
    };
    for (chat_id, message_id) in drained.messages {
        if let Err(source) = bot
            .delete_message(ChatId(chat_id), MessageId(message_id))
            .await
        {
            warn!(
                ?source,
                chat_id, message_id, "failed to delete deferred message"
            );
        }
    }
    if let Some(source) = drained.delete_error {
        error!(?source, from_id, "failed to clear delete-later messages");
    }
}
