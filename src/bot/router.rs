use std::sync::Arc;

use teloxide::{RequestError, dispatching::DefaultKey, dptree, prelude::*, types::Me};

use crate::bot::{
    commands::{Command, parse_go_command},
    context::AppContext,
    handlers::{
        chat_member, migration::MigrationHandlers, recap::RecapHandlers, recap_forwarded,
        recap_subscription, system::SystemHandlers,
    },
    middleware,
};

pub fn build_dispatcher(
    bot: Bot,
    ctx: Arc<AppContext>,
) -> Dispatcher<Bot, RequestError, DefaultKey> {
    let commands = dptree::entry()
        .filter_map(|message: Message, me: Me| {
            message
                .text()
                .or_else(|| message.caption())
                .and_then(|text| parse_go_command(text, me.username()))
        })
        .branch(dptree::case![Command::Start(arguments)].endpoint(SystemHandlers::handle_start))
        .branch(dptree::case![Command::Help].endpoint(SystemHandlers::handle_help))
        .branch(dptree::case![Command::Cancel].endpoint(SystemHandlers::handle_cancel))
        .branch(dptree::case![Command::Recap].endpoint(RecapHandlers::handle_recap))
        .branch(
            dptree::case![Command::ConfigureRecap].endpoint(RecapHandlers::handle_configure_recap),
        )
        .branch(
            dptree::case![Command::RecapForwardedStart]
                .endpoint(recap_forwarded::handle_recap_forwarded_start),
        )
        .branch(
            dptree::case![Command::RecapForwarded]
                .endpoint(recap_forwarded::handle_recap_forwarded),
        )
        .branch(
            dptree::case![Command::SubscribeRecap]
                .endpoint(recap_subscription::handle_subscribe_recap_command),
        )
        .branch(
            dptree::case![Command::UnsubscribeRecap]
                .endpoint(recap_subscription::handle_unsubscribe_recap_command),
        );

    // Go `OnChatMigrationFrom` (`chatmigrate.go:56-68`) fires on the **new
    // supergroup** side, when the service message carries
    // `migrate_from_chat_id`; the old group's own `migrate_to_chat_id`
    // message is never the trigger.
    let migration_filter = dptree::filter(|msg: Message| msg.migrate_from_chat_id().is_some())
        .endpoint(MigrationHandlers::handle_chat_migration);
    let left_member_filter =
        Message::filter_left_chat_member().endpoint(recap_subscription::handle_chat_member_left);

    // Message handler: record ALL messages first, then try commands.
    //
    // `inspect_async` is awaited in line, which is Go's synchronous middleware
    // pass, and it never branches, so a disabled chat or a storage failure
    // still reaches the migration and command branches below.
    let message_handler = Update::filter_message()
        .inspect_async(|ctx: Arc<AppContext>, msg: Message| async move {
            middleware::record_message(ctx, msg).await;
        })
        // Catch migration events before command parsing
        .branch(migration_filter)
        // A service message removes one physical subscriber row for the member.
        .branch(left_member_filter)
        // Then try to match commands
        .branch(commands);

    let edited_message_handler = Update::filter_edited_message().inspect_async(
        |ctx: Arc<AppContext>, msg: Message| async move {
            middleware::record_edited_message(ctx, msg).await;
        },
    );

    let callback_handler =
        Update::filter_callback_query().endpoint(RecapHandlers::handle_callback_query_with_me);

    // Go `welcome.go:57-184` OnMyChatMember: the bot's own `left` status
    // triggers the five-step cleanup, and the bot's own `member` status
    // triggers the first-join welcome (best-effort Telegram reply); the
    // handler filters the status itself.
    let my_chat_member_handler =
        Update::filter_my_chat_member().endpoint(chat_member::handle_my_chat_member);

    let handler = dptree::entry()
        .branch(message_handler)
        .branch(edited_message_handler)
        .branch(callback_handler)
        .branch(my_chat_member_handler);

    // Signal handling and reverse-order shutdown are driven explicitly by
    // `main.rs` via `Dispatcher::shutdown_token`, so teloxide's own built-in
    // Ctrl+C handler is left disabled to avoid a second, uncoordinated
    // shutdown path.
    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![ctx.clone()])
        .build()
}
