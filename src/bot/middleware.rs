use std::sync::Arc;

use teloxide::types::Message;
use tracing::{debug, warn};

use crate::{bot::context::AppContext, db::chat_history, db::models::MessageKind};

/// Record a message to the database. Called from the router as a side effect.
pub async fn record_message(ctx: Arc<AppContext>, msg: Message) {
    let is_group_chat = msg.chat.is_group() || msg.chat.is_supergroup();
    if !is_group_chat {
        return;
    }

    match chat_history::is_recap_enabled(&ctx.db.pool, msg.chat.id.0).await {
        Ok(false) => return,
        Ok(true) => {}
        Err(err) => {
            warn!("failed to determine recap enablement: {err:?}");
            return;
        }
    }

    let text = msg
        .text()
        .map(|s| s.to_string())
        .or_else(|| msg.caption().map(|s| s.to_string()));

    let kind = if text.is_some() {
        MessageKind::Text
    } else if msg.photo().is_some() {
        MessageKind::Photo
    } else if msg.video().is_some() {
        MessageKind::Video
    } else if msg.audio().is_some() {
        MessageKind::Audio
    } else if msg.voice().is_some() {
        MessageKind::Voice
    } else if msg.document().is_some() {
        MessageKind::Document
    } else if msg.sticker().is_some() {
        MessageKind::Sticker
    } else {
        MessageKind::Other
    };

    let created_at = msg.date.timestamp();
    let chat_id = msg.chat.id.0;
    let from_id = msg.from.as_ref().map(|u| u.id.0 as i64);
    // Extract full name (first_name + last_name)
    let from_full_name = msg.from.as_ref().map(|u| {
        let mut name = u.first_name.clone();
        if let Some(ref last) = u.last_name {
            name.push(' ');
            name.push_str(last);
        }
        name
    });
    let from_username = msg.from.as_ref().and_then(|u| u.username.clone());

    let preview = text.clone().unwrap_or_else(|| "<non-text>".to_string());
    let from = msg
        .from
        .as_ref()
        .map(|u| u.username.clone().unwrap_or_else(|| u.first_name.clone()))
        .unwrap_or_else(|| "<unknown>".to_string());

    debug!(
        chat_id = chat_id,
        message_id = msg.id.0,
        kind = ?kind,
        from = %from,
        preview = %preview,
        "recording message"
    );

    // Record to chat_histories
    if let Err(err) = chat_history::insert_message(
        &ctx.db.pool,
        chat_id,
        msg.id.0 as i64,
        from_id,
        from_full_name,
        from_username,
        kind,
        text.clone(),
        None,
        created_at,
    )
    .await
    {
        warn!("record_message failed: {err:?}");
    }
}

/// Update an edited message in the database. Called from the router as a side effect.
pub async fn record_edited_message(ctx: Arc<AppContext>, msg: Message) {
    let is_group_chat = msg.chat.is_group() || msg.chat.is_supergroup();
    if !is_group_chat {
        return;
    }

    match chat_history::is_recap_enabled(&ctx.db.pool, msg.chat.id.0).await {
        Ok(false) => return,
        Ok(true) => {}
        Err(err) => {
            warn!("failed to determine recap enablement: {err:?}");
            return;
        }
    }

    let new_text = msg
        .text()
        .map(|s| s.to_string())
        .or_else(|| msg.caption().map(|s| s.to_string()));

    if let Some(text) = new_text {
        debug!(
            chat_id = msg.chat.id.0,
            message_id = msg.id.0,
            "updating edited message"
        );

        if let Err(err) = chat_history::update_message_text(
            &ctx.db.pool,
            msg.chat.id.0,
            msg.id.0 as i64,
            &text,
        )
        .await
        {
            warn!("record_edited_message failed: {err:?}");
        }
    }
}
