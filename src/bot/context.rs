use std::sync::Arc;

use crate::{
    config::AppConfig,
    db::Database,
    i18n::I18n,
    services::{openai::OpenAiClient, rate_limit::CommandRateLimiter, telegraph::TelegraphService},
};

#[derive(Clone)]
pub struct AppContext {
    pub config: AppConfig,
    pub db: Database,
    pub i18n: I18n,
    pub openai: OpenAiClient,
    pub limiter: CommandRateLimiter,
    pub telegraph: Option<TelegraphService>,
    /// Task 3 installs the concrete Redis-backed recap state client here.
    pub recap_redis_client: Option<redis::Client>,
    /// Shared HTTP transport for Task 7's raw Telegram Rich Message client.
    pub raw_telegram_http: reqwest::Client,
}

#[derive(Clone)]
pub struct RecapRuntimeDependencies {
    /// Task 3 installs the concrete Redis-backed recap state client here.
    pub recap_redis_client: Option<redis::Client>,
    /// Shared HTTP transport for Task 7's raw Telegram Rich Message client.
    pub raw_telegram_http: reqwest::Client,
}

impl Default for RecapRuntimeDependencies {
    fn default() -> Self {
        Self {
            recap_redis_client: None,
            raw_telegram_http: reqwest::Client::new(),
        }
    }
}

impl AppContext {
    pub fn new(
        config: AppConfig,
        db: Database,
        i18n: I18n,
        openai: OpenAiClient,
        limiter: CommandRateLimiter,
        telegraph: Option<TelegraphService>,
        recap_runtime: RecapRuntimeDependencies,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            db,
            i18n,
            openai,
            limiter,
            telegraph,
            recap_redis_client: recap_runtime.recap_redis_client,
            raw_telegram_http: recap_runtime.raw_telegram_http,
        })
    }
}
