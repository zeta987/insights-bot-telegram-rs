//! Bot-left-chat cleanup.
//!
//! Ported from Go v1.0.0 `internal/bots/telegram/handlers/welcome/welcome.go`,
//! whose `handleBotLeftChat` runs five independent steps in a fixed order with
//! no transaction around them and no early exit: `fo.May0` logs a failing step
//! and the next one still runs.
//!
//! Four tables lose their rows outright and the recap log keeps its rows with
//! only the two text columns blanked, so a feedback button minted before the
//! bot left still resolves. Everything else survives untouched:
//! `telegram_chats` (this schema's `chats`), `sent_messages`, both feedback
//! reaction tables, the token-usage metric table, and every Redis key.

use anyhow::Result;
use tracing::{error, info};

use crate::db::{Database, chat_history, feature_flags, recap_logs, recap_options, subscribers};

/// Drop the recap data this service owns for a chat the bot has left.
///
/// Every step is attempted even after an earlier one failed and nothing is
/// reported back, which is exactly what `fo.May0` does: it logs and continues,
/// and the handler has no failure to react to. Returning `()` keeps a caller
/// from inventing a recovery path Go does not have.
pub async fn prune_chat_data_after_bot_left(db: &Database, chat_id: i64) {
    record(
        "subscribers",
        chat_id,
        subscribers::delete_all_by_chat_id(db, chat_id).await,
    );
    record(
        "feature flags",
        chat_id,
        feature_flags::delete_by_chat_id(db, chat_id).await,
    );
    record(
        "recap options",
        chat_id,
        recap_options::delete_by_chat_id(db, chat_id).await,
    );
    record(
        "chat histories",
        chat_id,
        chat_history::delete_all_by_chat_id(db, chat_id).await,
    );
    record(
        "recap log content",
        chat_id,
        recap_logs::prune_content_by_chat_id(db, chat_id).await,
    );

    info!(
        chat_id = chat_id,
        "finished pruning the owned chat data sets"
    );
}

/// Log one completed step, whether or not it succeeded.
fn record(step: &'static str, chat_id: i64, outcome: Result<()>) {
    match outcome {
        Ok(()) => info!(step = step, chat_id = chat_id, "pruned one chat data set"),
        Err(error) => error!(
            step = step,
            chat_id = chat_id,
            error = ?error,
            "failed to prune one chat data set"
        ),
    }
}
