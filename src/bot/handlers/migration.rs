use std::sync::Arc;

use teloxide::prelude::*;
use tracing::info;

use crate::bot::context::AppContext;

pub struct MigrationHandlers;

impl MigrationHandlers {
    /// Handle chat migration (group → supergroup upgrade).
    /// Telegram sends a message with migrate_to_chat_id when this happens.
    pub async fn handle_chat_migration(
        _bot: Bot,
        msg: Message,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let old_chat_id = msg.chat.id.0;
        let Some(new_chat_id) = msg.migrate_to_chat_id() else {
            return Ok(());
        };
        let new_chat_id = new_chat_id.0;

        info!(
            old_chat_id = old_chat_id,
            new_chat_id = new_chat_id,
            "received chat migration event"
        );

        // Go's parity set, and only that set. Each step logs its own failure and
        // none blocks the next, so there is no aggregate outcome to branch on.
        // `recap_configs` has no Go counterpart and is deliberately untouched.
        crate::db::migration::migrate_chat_data(&ctx.db, old_chat_id, new_chat_id).await;

        Ok(())
    }
}
