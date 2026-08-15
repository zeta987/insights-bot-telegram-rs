//! Go v1.0.0 bot-membership updates, `welcome/welcome.go:57-184`.
//!
//! Go registers `OnMyChatMember` and reacts to the bot's own status becoming
//! exactly `left` (the five-step cleanup) or exactly `member` (the first-join
//! welcome). Any other transition -- a ban (`kicked`), being added directly
//! as `administrator`, or any other status -- matches no Go branch and does
//! nothing.

use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{ChatMemberUpdated, Me, ParseMode},
};
use tracing::warn;

use crate::{
    bot::context::AppContext,
    config::Locale,
    db::{chat_cleanup::prune_chat_data_after_bot_left, feature_flags},
};

/// Handle a `my_chat_member` transition for the bot itself.
///
/// `is_left()` matches only the `Left` variant, so a ban (`kicked`) falls
/// through exactly like Go's unmatched status branch. `is_member()` matches
/// only the plain `Member` variant, so being added directly as
/// `administrator` also falls through untouched (`welcome.go:64-71`).
pub async fn handle_my_chat_member(
    update: ChatMemberUpdated,
    bot: Bot,
    me: Me,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if update.new_chat_member.is_left() {
        prune_chat_data_after_bot_left(&context.db, update.chat.id.0).await;
        return Ok(());
    }
    if update.new_chat_member.is_member() {
        handle_bot_join_chat(&update, &bot, &me, &context).await;
        return Ok(());
    }
    Ok(())
}

/// Go `welcome.go:137-184`: first-join bookkeeping and a best-effort welcome.
///
/// `HasJoinedGroupsBefore` gates the whole function body, not merely the
/// welcome message: a chat the bot has already recorded runs neither the
/// language write nor the send (`welcome.go:143-157`). Go's
/// `SetLanguageForGroups` failure is only logged, and so is `MaySend`'s
/// (`welcome.go:159-183`).
async fn handle_bot_join_chat(
    update: &ChatMemberUpdated,
    bot: &Bot,
    me: &Me,
    context: &AppContext,
) {
    let chat_id = update.chat.id.0;
    let chat_type = chat_type_str(&update.chat);
    let chat_title = update.chat.title().unwrap_or_default();
    // Go: `language := c.Update.MyChatMember.From.LanguageCode`, the raw,
    // possibly-absent Telegram field with no normalisation.
    let language = update.from.language_code.as_deref().unwrap_or_default();

    let has_joined_before =
        match feature_flags::has_joined_before(&context.db, chat_id, chat_title).await {
            Ok(value) => value,
            Err(source) => {
                warn!(
                    ?source,
                    chat_id,
                    chat_title,
                    chat_type,
                    language,
                    "failed to check if bot has joined groups before"
                );
                return;
            }
        };
    if has_joined_before {
        return;
    }

    if let Err(source) =
        feature_flags::set_language(&context.db, chat_id, chat_type, chat_title, language).await
    {
        warn!(
            ?source,
            chat_id, chat_title, chat_type, language, "failed to set language for groups"
        );
    }

    let text = context.i18n.t(
        locale_from_code(language),
        "welcome.message_normal_group",
        &[("Username", me.username())],
    );

    if let Err(source) = bot
        .send_message(ChatId(chat_id), text)
        .parse_mode(ParseMode::Html)
        .await
    {
        warn!(?source, chat_id, "failed to send welcome message");
    }
}

/// Telegram's wire type string for a chat, Go's `telegram.ChatType(...)`.
fn chat_type_str(chat: &teloxide::types::Chat) -> &'static str {
    if chat.is_private() {
        "private"
    } else if chat.is_supergroup() {
        "supergroup"
    } else if chat.is_channel() {
        "channel"
    } else {
        "group"
    }
}

/// Map a stored/raw language code to a [`Locale`], defaulting to English
/// exactly like [`Locale::from_lookup`] does for an unrecognized code.
fn locale_from_code(code: &str) -> Locale {
    match code {
        "zh-Hans" => Locale::ZhHans,
        "zh-Hant" => Locale::ZhHant,
        _ => Locale::En,
    }
}
