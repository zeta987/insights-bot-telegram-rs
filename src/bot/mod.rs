pub mod commands;
pub mod context;
pub mod handlers;
pub mod middleware;
pub mod router;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use teloxide::dispatching::ShutdownToken;
use teloxide::prelude::*;
use teloxide::types::BotCommand;
use teloxide::update_listeners::webhooks;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use context::AppContext;

/// A running dispatcher, returned once the Telegram bot has been authorized
/// and its update loop has been armed (spawned, not blocked on).
pub struct BotHandle {
    shutdown_token: ShutdownToken,
    join_handle: JoinHandle<()>,
}

impl BotHandle {
    /// Stop the dispatcher and wait for it to finish, mirroring Go's
    /// `bot.Stop(ctx)` (`pkg/bots/tgbot/tgbot.go`).
    pub async fn shutdown(self) {
        if let Ok(wait) = self.shutdown_token.shutdown() {
            wait.await;
        }
        let _ = self.join_handle.await;
    }
}

/// Authorize the bot, register its command menu, then arm the dispatcher.
///
/// Returns as soon as the dispatcher's update loop has been spawned rather
/// than blocking for the process lifetime, so callers can continue startup
/// (arming the automatic-recap poller) and later drive an orderly shutdown
/// through the returned [`BotHandle`].
pub async fn run(ctx: Arc<AppContext>) -> Result<BotHandle> {
    let bot = ctx.config.telegram.bot();

    // Go's `tgbotapi.NewBotAPI` resolves `bot.Self` via a synchronous GetMe
    // call during bot construction, and `telegram.NewBot` logs "Authorized as
    // bot @%s" immediately after (`internal/bots/telegram/telegram.go:64-75`).
    // Mirror that authorization step explicitly and flip the composite
    // `/health` readiness flag on success; a failure here fails startup, same
    // as Go's constructor-time error.
    let me = bot
        .get_me()
        .await
        .map_err(|e| anyhow::anyhow!("failed to authorize telegram bot: {e}"))?;
    ctx.lifecycle.mark_bot_authorized();
    info!("authorized as bot @{}", me.username());

    // Register bot commands with Telegram for menu display.
    if let Err(e) = register_commands(&bot).await {
        warn!(error = %e, "failed to register bot commands");
    }

    let webhook_url = ctx.config.telegram.webhook_url.clone();

    // Decide: webhook mode or long-polling mode.
    if let Some(url) = webhook_url {
        run_webhook(bot, ctx, &url).await
    } else {
        run_polling(bot, ctx).await
    }
}

/// Register bot commands with Telegram using setMyCommands API.
async fn register_commands(bot: &Bot) -> Result<()> {
    let commands = vec![
        BotCommand::new("start", "Show welcome message"),
        BotCommand::new("help", "Show help"),
        BotCommand::new("cancel", "Cancel current operation"),
        BotCommand::new("recap", "Generate chat recap"),
        BotCommand::new("configure_recap", "Configure recap settings"),
        BotCommand::new(
            "recap_forwarded_start",
            "使 Bot 接收在私聊中转发给 Bot 的消息，并在发送 /recap_forwarded 后开始总结",
        ),
        BotCommand::new(
            "recap_forwarded",
            "使 Bot 停止接收在私聊中转发给 Bot 的消息，对已经转发过的消息进行总结",
        ),
        BotCommand::new("subscribe_recap", "订阅当前群组的定时聊天回顾"),
        BotCommand::new("unsubscribe_recap", "取消订阅当前群组的定时聊天回顾"),
    ];

    bot.set_my_commands(commands)
        .await
        .map_err(|e| anyhow::anyhow!("setMyCommands failed: {}", e))?;

    info!("bot commands registered successfully");
    Ok(())
}

async fn run_polling(bot: Bot, ctx: Arc<AppContext>) -> Result<BotHandle> {
    info!("starting telegram dispatcher (long-polling mode)");
    let mut dispatcher = router::build_dispatcher(bot, ctx);
    let shutdown_token = dispatcher.shutdown_token();
    // teloxide 0.14's dispatch loop synchronously parks its calling thread
    // (`std::thread::scope` + `Thread::park` in dispatcher.rs:416) while the
    // real work runs on teloxide's own internal runtime. Parking a runtime
    // worker orphans this runtime's IO driver and starves every other task,
    // so the dispatcher gets a blocking-pool thread, which exists to be
    // blocked.
    let handle = tokio::runtime::Handle::current();
    let join_handle = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            dispatcher.dispatch().await;
        });
    });
    Ok(BotHandle {
        shutdown_token,
        join_handle,
    })
}

async fn run_webhook(bot: Bot, ctx: Arc<AppContext>, webhook_url: &str) -> Result<BotHandle> {
    let port = ctx.config.telegram.webhook_port.unwrap_or(8443);
    let addr: SocketAddr = ([0, 0, 0, 0], port).into();

    // Append bot token to webhook URL for security (matches Go implementation).
    let full_url = format!("{}/{}", webhook_url.trim_end_matches('/'), bot.token());
    let url = full_url
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid webhook URL: {}", e))?;

    info!(
        "starting telegram dispatcher (webhook mode) on {} -> {}",
        addr, webhook_url
    );

    let listener = webhooks::axum(bot.clone(), webhooks::Options::new(addr, url))
        .await
        .map_err(|e| anyhow::anyhow!("failed to setup webhook: {}", e))?;

    let mut dispatcher = router::build_dispatcher(bot, ctx);
    let shutdown_token = dispatcher.shutdown_token();
    // Same blocking-pool placement as `run_polling`: the dispatch loop parks
    // its thread for the process lifetime.
    let handle = tokio::runtime::Handle::current();
    let join_handle = tokio::task::spawn_blocking(move || {
        handle.block_on(async move {
            dispatcher
                .dispatch_with_listener(listener, LoggingErrorHandler::new())
                .await;
        });
    });

    Ok(BotHandle {
        shutdown_token,
        join_handle,
    })
}
