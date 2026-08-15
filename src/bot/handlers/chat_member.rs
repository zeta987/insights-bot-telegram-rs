//! Go v1.0.0 bot-membership updates, `welcome/welcome.go:57-135`.
//!
//! Go registers `OnMyChatMember` and reacts only to the bot's own status
//! becoming exactly `left`; a ban (`kicked`) matches no branch and performs no
//! cleanup, and no branch ever sends a Telegram reply.

use std::sync::Arc;

use teloxide::{prelude::ResponseResult, types::ChatMemberUpdated};

use crate::{bot::context::AppContext, db::chat_cleanup::prune_chat_data_after_bot_left};

/// Handle a `my_chat_member` transition for the bot itself.
///
/// `is_left()` matches only the `Left` variant, so a ban (`kicked`) falls
/// through exactly like Go's unmatched status branch.
pub async fn handle_my_chat_member(
    update: ChatMemberUpdated,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if !update.new_chat_member.is_left() {
        return Ok(());
    }
    prune_chat_data_after_bot_left(&context.db, update.chat.id.0).await;
    Ok(())
}
