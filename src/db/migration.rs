//! Group to supergroup migration.
//!
//! Ported from Go v1.0.0
//! `internal/bots/telegram/handlers/chatmigrate/chatmigrate.go`, which runs five
//! independent steps in a fixed order with no transaction around them and no
//! early exit: `fo.May0` logs a failing step and the next one still runs.
//!
//! The set of tables is Go's, and so is the set it leaves alone. `telegram_chats`
//! (this schema's `chats`), `sent_messages`, both feedback reaction tables, the
//! token-usage metric table, and every Redis key are untouched, because Go never
//! rewrites them on an upgrade.

use anyhow::Result;
use tracing::{error, info};

use crate::db::{Database, chat_history, feature_flags, recap_logs, recap_options, subscribers};

/// Move every Go-parity row of `from_chat_id` onto `to_chat_id`.
///
/// Every step is attempted even after an earlier one failed and nothing is
/// reported back, which is exactly what `fo.May0` does: it logs and continues,
/// and the handler has no failure to react to. Returning `()` keeps a caller
/// from inventing a recovery path Go does not have.
///
/// `recap_configs` is deliberately absent. It has no Go counterpart, so the
/// parity orchestrator must not touch it.
pub async fn migrate_chat_data(db: &Database, from_chat_id: i64, to_chat_id: i64) {
    record(
        "feature flags",
        from_chat_id,
        to_chat_id,
        feature_flags::migrate_chat_id(db, from_chat_id, to_chat_id).await,
    );
    record(
        "recap options",
        from_chat_id,
        to_chat_id,
        recap_options::migrate_chat_id(db, from_chat_id, to_chat_id).await,
    );
    record(
        "subscribers",
        from_chat_id,
        to_chat_id,
        subscribers::migrate_chat_id(db, from_chat_id, to_chat_id).await,
    );
    record(
        "chat histories",
        from_chat_id,
        to_chat_id,
        chat_history::migrate_chat_id(db, from_chat_id, to_chat_id).await,
    );
    record(
        "recap logs",
        from_chat_id,
        to_chat_id,
        recap_logs::migrate_chat_id(db, from_chat_id, to_chat_id).await,
    );

    info!(
        from_chat_id = from_chat_id,
        to_chat_id = to_chat_id,
        "finished migrating the chat data sets to the supergroup"
    );
}

/// Log one completed step, whether or not it succeeded.
fn record(step: &'static str, from_chat_id: i64, to_chat_id: i64, outcome: Result<()>) {
    match outcome {
        Ok(()) => info!(
            step = step,
            from_chat_id = from_chat_id,
            to_chat_id = to_chat_id,
            "migrated one chat data set"
        ),
        Err(error) => error!(
            step = step,
            from_chat_id = from_chat_id,
            to_chat_id = to_chat_id,
            error = ?error,
            "failed to migrate one chat data set"
        ),
    }
}
