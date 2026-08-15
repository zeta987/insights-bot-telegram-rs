//! Group to supergroup migration.
//!
//! Ported from Go v1.0.0
//! `internal/bots/telegram/handlers/chatmigrate/chatmigrate.go`. Go's
//! `OnChatMigrationFrom` fires on the **new supergroup** side, when the
//! service message carries `migrate_from_chat_id` (`chatmigrate.go:56-68`);
//! the old group's own `migrate_to_chat_id` service message is never the
//! trigger. The five migration steps run in a fixed order with no
//! transaction around them and no early exit: `fo.May0` logs a failing step
//! and the next one still runs.
//!
//! After migration, Go looks up the new chat's stored language and sends a
//! best-effort HTML notification into the new supergroup
//! (`chatmigrate.go:148-166`); a send failure is only logged, never
//! surfaced to the caller.
//!
//! The set of tables migrated is Go's, and so is the set it leaves alone.
//! `telegram_chats` (this schema's `chats`), `sent_messages`, both feedback
//! reaction tables, the token-usage metric table, and every Redis key are
//! untouched, because Go never rewrites them on an upgrade. `recap_configs`
//! has no Go counterpart and is deliberately untouched.

use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{Me, ParseMode},
};
use tracing::{info, warn};

use crate::{
    bot::context::AppContext, config::Locale, db::feature_flags,
    services::message_capture::full_name_from_first_and_last_name,
};

pub struct MigrationHandlers;

impl MigrationHandlers {
    /// Handle chat migration (group → supergroup upgrade).
    ///
    /// Telegram delivers the `migrate_from_chat_id` service message to the
    /// **new** supergroup; a message that carries only `migrate_to_chat_id`
    /// (the old group's side) is not the trigger, matching Go's
    /// `MigrateFromChatID == 0` early return.
    pub async fn handle_chat_migration(
        bot: Bot,
        msg: Message,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let Some(old_chat_id) = msg.migrate_from_chat_id() else {
            return Ok(());
        };
        let old_chat_id = old_chat_id.0;
        let new_chat_id = msg.chat.id.0;

        info!(
            old_chat_id = old_chat_id,
            new_chat_id = new_chat_id,
            "received chat migration event"
        );

        // Go's parity set, and only that set. Each step logs its own failure and
        // none blocks the next, so there is no aggregate outcome to branch on.
        // `recap_configs` has no Go counterpart and is deliberately untouched.
        crate::db::migration::migrate_chat_data(&ctx.db, old_chat_id, new_chat_id).await;

        // Go `chatmigrate.go:148-166`: a best-effort HTML notification into the
        // new supergroup once migration finishes. A send failure is only
        // logged, never surfaced to the caller.
        Self::notify_new_supergroup(&bot, &ctx, new_chat_id, &me).await;

        Ok(())
    }

    async fn notify_new_supergroup(bot: &Bot, ctx: &AppContext, new_chat_id: i64, me: &Me) {
        let language = match feature_flags::find_language(&ctx.db, new_chat_id, "").await {
            Ok(language) => language,
            Err(source) => {
                warn!(
                    ?source,
                    new_chat_id, "failed to find language for the migrated group"
                );
                crate::db::models::DEFAULT_FEATURE_LANGUAGE.to_owned()
            }
        };
        let full_name = full_name_from_first_and_last_name(
            &me.first_name,
            me.last_name.as_deref().unwrap_or_default(),
        );
        let text = ctx.i18n.t(
            locale_from_code(&language),
            "migration.notification",
            &[("Name", full_name.as_str()), ("Username", me.username())],
        );

        if let Err(source) = bot
            .send_message(ChatId(new_chat_id), text)
            .parse_mode(ParseMode::Html)
            .await
        {
            warn!(
                ?source,
                new_chat_id, "failed to send chat migration notification"
            );
        }
    }
}

/// Map a stored feature-flag language code to a [`Locale`], defaulting to
/// English exactly like [`Locale::from_lookup`] does for an unrecognized code.
fn locale_from_code(code: &str) -> Locale {
    match code {
        "zh-Hans" => Locale::ZhHans,
        "zh-Hant" => Locale::ZhHant,
        _ => Locale::En,
    }
}
