pub mod commands;
pub mod context;
pub mod handlers;
pub mod middleware;
pub mod router;

use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use teloxide::prelude::*;
use teloxide::types::BotCommand;
use teloxide::update_listeners::webhooks;
use tracing::{info, warn};

use context::AppContext;

pub async fn run(ctx: Arc<AppContext>) -> Result<()> {
    let bot = ctx.config.telegram.bot();
    let webhook_url = ctx.config.telegram.webhook_url.clone();

    // Register bot commands with Telegram for menu display.
    if let Err(e) = register_commands(&bot).await {
        warn!(error = %e, "failed to register bot commands");
    }

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
        BotCommand::new("subscribe_recap", "订阅当前群组的定时聊天回顾"),
        BotCommand::new("unsubscribe_recap", "取消订阅当前群组的定时聊天回顾"),
    ];

    bot.set_my_commands(commands)
        .await
        .map_err(|e| anyhow::anyhow!("setMyCommands failed: {}", e))?;

    info!("bot commands registered successfully");
    Ok(())
}

async fn run_polling(bot: Bot, ctx: Arc<AppContext>) -> Result<()> {
    info!("starting telegram dispatcher (long-polling mode)");
    let mut dispatcher = router::build_dispatcher(bot, ctx);
    dispatcher.dispatch().await;
    Ok(())
}

async fn run_webhook(bot: Bot, ctx: Arc<AppContext>, webhook_url: &str) -> Result<()> {
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
    dispatcher
        .dispatch_with_listener(listener, LoggingErrorHandler::new())
        .await;

    Ok(())
}
