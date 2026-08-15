use std::sync::Arc;

use teloxide::{RequestError, dispatching::DefaultKey, dptree, prelude::*};

use crate::bot::{
    commands::Command,
    context::AppContext,
    handlers::{migration::MigrationHandlers, recap::RecapHandlers, system::SystemHandlers},
    middleware,
};

pub fn build_dispatcher(
    bot: Bot,
    ctx: Arc<AppContext>,
) -> Dispatcher<Bot, RequestError, DefaultKey> {
    let commands = dptree::entry()
        .filter_command::<Command>()
        .branch(dptree::case![Command::Start].endpoint(SystemHandlers::handle_start))
        .branch(dptree::case![Command::Help].endpoint(SystemHandlers::handle_help))
        .branch(dptree::case![Command::Cancel].endpoint(SystemHandlers::handle_cancel))
        .branch(dptree::case![Command::Recap].endpoint(RecapHandlers::handle_recap))
        .branch(
            dptree::case![Command::ConfigureRecap].endpoint(RecapHandlers::handle_configure_recap),
        );

    let migration_filter = dptree::filter(|msg: Message| msg.migrate_to_chat_id().is_some())
        .endpoint(MigrationHandlers::handle_chat_migration);

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
        // Then try to match commands
        .branch(commands);

    let edited_message_handler = Update::filter_edited_message().inspect_async(
        |ctx: Arc<AppContext>, msg: Message| async move {
            middleware::record_edited_message(ctx, msg).await;
        },
    );

    let callback_handler =
        Update::filter_callback_query().endpoint(RecapHandlers::handle_callback_query);

    let handler = dptree::entry()
        .branch(message_handler)
        .branch(edited_message_handler)
        .branch(callback_handler);

    Dispatcher::builder(bot, handler)
        .dependencies(dptree::deps![ctx.clone()])
        .enable_ctrlc_handler()
        .build()
}
