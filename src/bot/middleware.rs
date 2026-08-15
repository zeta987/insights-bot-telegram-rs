//! Go v1.0.0 message middleware, ported from
//! `internal/bots/telegram/middlewares/record_messsages.go` and
//! `internal/bots/telegram/middlewares/sync_with_edit_messages.go`.
//!
//! Go runs every middleware synchronously with a no-op `next`, then dispatches
//! the update independently after the middleware loop. The router therefore
//! awaits these taps before its migration and command branches. An early
//! return, disabled chat, or storage failure skips only persistence and never
//! suppresses the later Telegram dispatch.
//!
//! # Logging
//!
//! Go logs `err.Error()` and a debug line carrying the chat identifier and the
//! message text. Every line below is a fixed string instead: no chat or user
//! identifier, no name, no message text, no URL, no serialized payload, and no
//! formatted error reaches the log.

use std::sync::Arc;

use tracing::warn;

use crate::{
    bot::context::AppContext,
    db::{Database, chat_history, feature_flags},
    redis::recap_state::RecapStateStore,
    services::message_capture::{
        CapturedMessage, DynMessagePreprocessor, captured_message_from_teloxide,
        private_forwarded_replay_entry,
    },
};

/// Telegram's wire spellings for the chat types Go's `RecordMessage` accepts.
const CHAT_TYPE_GROUP: &str = "group";
const CHAT_TYPE_SUPERGROUP: &str = "supergroup";
const CHAT_TYPE_PRIVATE: &str = "private";

/// Go's `RecordMessage`, entered from the router with the typed update.
pub async fn record_message(ctx: Arc<AppContext>, msg: teloxide::types::Message) {
    let sender_user_id = msg.from.as_ref().map(|user| user.id.0 as i64);
    let captured = captured_message_from_teloxide(&msg);

    record_captured_message(
        &ctx.db,
        ctx.recap_state.as_ref(),
        ctx.message_preprocessor.as_deref(),
        &captured,
        sender_user_id,
    )
    .await;
}

/// Go's `SyncWithEditedMessage`, entered from the router with the typed update.
pub async fn record_edited_message(ctx: Arc<AppContext>, msg: teloxide::types::Message) {
    let captured = captured_message_from_teloxide(&msg);

    record_captured_edited_message(&ctx.db, ctx.message_preprocessor.as_deref(), &captured).await;
}

/// The transport-free half of Go's `RecordMessage`.
///
/// `sender_user_id` is `None` when Telegram sent no `from`, which is Go's
/// `message.From == nil`. Go dereferences it unconditionally on the private
/// path; a nil `from` is skipped here rather than panicking the process.
pub async fn record_captured_message(
    db: &Database,
    recap_state: Option<&Arc<dyn RecapStateStore>>,
    preprocessor: Option<&DynMessagePreprocessor>,
    message: &CapturedMessage,
    sender_user_id: Option<i64>,
) {
    match message.chat.kind.as_str() {
        CHAT_TYPE_GROUP | CHAT_TYPE_SUPERGROUP => {
            record_group_message(db, preprocessor, message).await;
        }
        CHAT_TYPE_PRIVATE => {
            record_private_message(recap_state, preprocessor, message, sender_user_id).await;
        }
        // Go's `lo.Contains` gate drops every other chat type, channels
        // included, before it reads the feature flag.
        _ => {}
    }
}

/// Go's group branch: the feature flag, then `SaveOneTelegramChatHistory`.
async fn record_group_message(
    db: &Database,
    preprocessor: Option<&DynMessagePreprocessor>,
    message: &CapturedMessage,
) {
    match feature_flags::has_recap_enabled(db, message.chat.id, &message.chat.title).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            warn!("failed to read the chat histories recap feature flag");
            return;
        }
    }

    let Some(preprocessor) = preprocessor else {
        warn!("message preprocessing is not configured; skipping chat history persistence");
        return;
    };

    let captured = match preprocessor.capture_message(message).await {
        Ok(Some(captured)) => captured,
        // Go returns early on an empty extraction without storing anything.
        Ok(None) => return,
        Err(_) => {
            warn!("failed to preprocess a group message; skipping chat history persistence");
            return;
        }
    };

    if chat_history::save_one(db, &captured).await.is_err() {
        warn!("failed to persist a chat history row");
    }
}

/// Go's private branch: `SaveOneTelegramPrivateForwardedReplayChatHistory`.
async fn record_private_message(
    recap_state: Option<&Arc<dyn RecapStateStore>>,
    preprocessor: Option<&DynMessagePreprocessor>,
    message: &CapturedMessage,
    sender_user_id: Option<i64>,
) {
    let Some(sender_user_id) = sender_user_id else {
        return;
    };
    let Some(recap_state) = recap_state else {
        return;
    };

    match recap_state.forwarded_active(sender_user_id).await {
        Ok(true) => {}
        Ok(false) => return,
        Err(_) => {
            warn!("failed to read the forwarded replay session state");
            return;
        }
    }

    let Some(preprocessor) = preprocessor else {
        warn!("message preprocessing is not configured; skipping forwarded replay capture");
        return;
    };

    // Go calls the guarded `extractTextFromMessage` here, the same entry point
    // the group path uses, so the both-empty guard applies.
    let text = match preprocessor.extract_text_guarded(Some(message)).await {
        Ok(text) => text,
        Err(_) => {
            warn!("failed to preprocess a private message; skipping forwarded replay capture");
            return;
        }
    };
    if text.is_empty() {
        return;
    }

    let entry = private_forwarded_replay_entry(message, &text);
    let Ok(json) = serde_json::to_string(&entry).map(|json| {
        json.replace('&', "\\u0026")
            .replace('<', "\\u003c")
            .replace('>', "\\u003e")
            .replace('\u{2028}', "\\u2028")
            .replace('\u{2029}', "\\u2029")
    }) else {
        warn!("failed to serialize a forwarded replay entry");
        return;
    };

    // `append_forwarded` already refreshes both session TTLs, which is Go's
    // pair of `EXPIRE` calls after the `ZADD`.
    if recap_state
        .append_forwarded(sender_user_id, entry.chatted_at, &json)
        .await
        .is_err()
    {
        warn!("failed to append a forwarded replay entry");
    }
}

/// The transport-free half of Go's `SyncWithEditedMessage`.
///
/// Go checks neither the chat type nor the feature flag on this path, so an
/// edit in any chat rewrites whatever row already matches the pair. A chat that
/// never persisted the message simply updates nothing.
pub async fn record_captured_edited_message(
    db: &Database,
    preprocessor: Option<&DynMessagePreprocessor>,
    message: &CapturedMessage,
) {
    let Some(preprocessor) = preprocessor else {
        warn!("message preprocessing is not configured; skipping the edited message sync");
        return;
    };

    let edited = match preprocessor.capture_edited_message(Some(message)).await {
        Ok(Some(edited)) => edited,
        // Go returns early when neither the text nor the caption survives.
        Ok(None) => return,
        Err(_) => {
            warn!("failed to preprocess an edited message; skipping the edited message sync");
            return;
        }
    };

    if chat_history::update_one_text(db, edited.chat_id, edited.message_id, &edited.text)
        .await
        .is_err()
    {
        warn!("failed to rewrite an edited chat history row");
    }
}
