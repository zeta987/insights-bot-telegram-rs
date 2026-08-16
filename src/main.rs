use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use insights_bot_telegram_rs::{
    bot, config, db, http, i18n, redis::recap_state as recap_redis_state, services, telemetry,
};
use tracing::{info, warn};

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("Error: {e:?}");
        // On Windows, wait for user input before closing so they can see the error.
        #[cfg(target_os = "windows")]
        {
            eprintln!("\nPress Enter to exit...");
            let _ = std::io::stdin().read_line(&mut String::new());
        }
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    load_env()?;

    // Ensure required directories exist (locales are embedded at compile time).
    ensure_directories(&["data", "logs"])?;

    let config = config::AppConfig::from_env()?;
    // Hold the guard to keep file logging active for the program's duration
    let _log_guard = telemetry::init_tracing(&config)?;

    let database = db::Database::connect_from_env(&config.db).await?;
    let i18n = i18n::I18n::load_from_dir(&config.locales_dir)?;
    let openai = services::openai::OpenAiClient::new(
        &config.openai,
        &config.recap_openai,
        &config.condensed_prompts,
    )?
    .with_token_usage_recorder(Arc::new(
        services::recap_generation::DatabaseTokenUsageRecorder::new(database.clone()),
    ));
    let recap_state = connect_recap_state(&config.redis).await?;
    // Built before `openai` is moved into the context, so the summarizer and
    // the handlers share one configured client.
    let message_preprocessor = build_message_preprocessor(&openai)?;
    let ctx = bot::context::AppContext::new(
        config,
        database.clone(),
        i18n,
        openai,
        bot::context::RecapRuntimeDependencies {
            recap_state: Some(recap_state),
            message_preprocessor: Some(message_preprocessor),
            ..Default::default()
        },
    );

    info!(
        "bootstrap completed (backend: {:?}, locale: {})",
        database.backend,
        ctx.config.locale.code()
    );

    // Startup order mirrors Go (`docs/adr/0001-go-parity-adjudication.md`,
    // decision 6): bind the health listener, hold Go's one-second pause,
    // start the Telegram dispatcher, then arm the automatic-recap poller.
    let health_addr = format!("0.0.0.0:{}", ctx.config.health_http_port)
        .parse()
        .context("invalid health listener address")?;
    let health = http::health::serve(ctx.lifecycle.clone(), health_addr).await?;

    tokio::time::sleep(Duration::from_secs(1)).await;

    let bot_handle = bot::run(ctx.clone()).await?;

    services::autorecap::spawn_autorecap(ctx.clone()).await;

    wait_for_shutdown_signal().await;
    info!("shutdown signal received; stopping in Go's reverse startup order");

    // Reverse order (decision 8): dispatcher, database pool, HTTP server
    // (ten-second timeout), poller.
    bot_handle.shutdown().await;
    database.pool.close().await;
    health.shutdown().await;
    let _ = ctx.shutdown_tx.send(true);

    Ok(())
}

/// Wait for SIGINT (all platforms) or SIGTERM (Unix only), matching Go's
/// `fx` app, which listens for both to begin graceful shutdown.
async fn wait_for_shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};

        let mut sigterm =
            signal(SignalKind::terminate()).expect("failed to install SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => info!("received SIGINT"),
            _ = sigterm.recv() => info!("received SIGTERM"),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        info!("received Ctrl+C");
    }
}

/// Build the one production message preprocessor the middleware runs.
///
/// The previewer is Go's `linkprev.Client` over Go's `req.C()` defaults, and
/// the summarizer is the configured OpenAI client, shared rather than rebuilt
/// so both halves of the process talk to the same endpoint.
fn build_message_preprocessor(
    openai: &services::openai::OpenAiClient,
) -> Result<Arc<services::message_capture::DynMessagePreprocessor>> {
    let previewer: Arc<dyn services::message_capture::LinkPreviewer> =
        Arc::new(services::link_preview::HttpLinkPreviewer::new(
            services::link_preview::go_parity_http_client()?,
        ));
    let summarizer: Arc<dyn services::message_capture::Summarizer> = Arc::new(
        services::message_capture::OpenAiSummarizer::new(Arc::new(openai.clone())),
    );

    Ok(Arc::new(
        services::message_capture::MessagePreprocessor::new(previewer, summarizer),
    ))
}

/// Connect the recap state store described by the Redis configuration.
///
/// Redis is required infrastructure. Go's provider returns any client-creation
/// or thirty-second `PING` failure to the application container, which aborts
/// startup, so a failure here stops the bot instead of disabling recap.
///
/// The error arrives already reduced to an operation name and an error kind, so
/// the address, the credentials, and any stored payload stay out of the log.
async fn connect_recap_state(
    redis: &config::RedisConfig,
) -> Result<Arc<dyn recap_redis_state::RecapStateStore>> {
    let store = recap_redis_state::RedisRecapStateStore::connect(redis).await?;
    info!("recap Redis state store connected");
    Ok(Arc::new(store))
}

/// Load environment variables, falling back to lenient parsing for .env files
/// that contain exotic values (long prompts with `"""`, `#`, backticks, etc.)
/// which `dotenvy` cannot parse strictly.
fn load_env() -> Result<()> {
    // First try the current working directory with strict parser.
    match dotenvy::dotenv() {
        Ok(_) => return Ok(()),
        Err(_) => {
            // dotenvy sets env vars as it iterates, so some vars from early
            // lines may already be set even though it returned Err.
            // Re-parse with lenient parser to pick up ALL remaining vars.
            let cwd_env = Path::new(".env");
            if cwd_env.exists() {
                load_env_lenient(cwd_env)?;
                return Ok(());
            }
        }
    }

    // Fallback: directory containing the executable (double-click scenarios).
    if let Ok(exe_path) = std::env::current_exe()
        && let Some(dir) = exe_path.parent()
    {
        let env_path = dir.join(".env");
        if env_path.exists() && dotenvy::from_path(&env_path).is_err() {
            load_env_lenient(&env_path)?;
        }
    }

    Ok(())
}

/// Lenient parser for .env files: allows unquoted values containing spaces.
fn load_env_lenient(path: &Path) -> Result<()> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read .env from {}", path.display()))?;

    for (idx, line) in content.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        if let Some(eq) = trimmed.find('=') {
            let (key, value) = trimmed.split_at(eq);
            let key = key.trim();
            let value = value[1..].trim(); // skip '='

            if key.is_empty() {
                warn!(line = idx + 1, "skipping .env line with empty key");
                continue;
            }

            // dotenv semantics: a variable already present in the process
            // environment wins over the .env file. The strict `dotenvy` path
            // never overrides, so the lenient fallback must not either.
            if std::env::var_os(key).is_some() {
                continue;
            }

            // Setting env vars is inherently process-global; mark explicit unsafe block
            // to satisfy targets that treat `set_var` as unsafe.
            unsafe {
                std::env::set_var(key, value);
            }
        } else {
            warn!(line = idx + 1, "skipping .env line without '='");
        }
    }

    Ok(())
}

/// Ensure required directories exist, creating them if necessary.
fn ensure_directories(dirs: &[&str]) -> Result<()> {
    for dir in dirs {
        let path = Path::new(dir);
        if !path.exists() {
            fs::create_dir_all(path)?;
            info!("created directory: {}", dir);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::load_env_lenient;

    #[test]
    fn the_lenient_loader_keeps_existing_process_env_precedence() {
        let path = std::env::temp_dir().join(format!(
            "insights_lenient_env_precedence_{}.env",
            std::process::id()
        ));
        let mut file = std::fs::File::create(&path).expect("temp .env");
        writeln!(file, "INSIGHTS_TEST_LENIENT_PRESET=from-file").expect("write preset line");
        writeln!(file, "INSIGHTS_TEST_LENIENT_FRESH=fresh value with spaces")
            .expect("write fresh line");
        drop(file);

        unsafe { std::env::set_var("INSIGHTS_TEST_LENIENT_PRESET", "from-process") };
        load_env_lenient(&path).expect("lenient load");

        assert_eq!(
            std::env::var("INSIGHTS_TEST_LENIENT_PRESET").expect("preset var"),
            "from-process",
            "a variable already present in the process environment wins"
        );
        assert_eq!(
            std::env::var("INSIGHTS_TEST_LENIENT_FRESH").expect("fresh var"),
            "fresh value with spaces",
            "variables absent from the process environment come from the file"
        );
        let _ = std::fs::remove_file(&path);
    }
}
