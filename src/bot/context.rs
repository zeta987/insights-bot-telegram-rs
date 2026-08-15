use std::sync::Arc;

use tokio::sync::watch;

use crate::{
    config::AppConfig,
    db::Database,
    i18n::I18n,
    lifecycle::LifecycleFlags,
    redis::recap_state::RecapStateStore,
    services::{
        message_capture::DynMessagePreprocessor, openai::OpenAiClient,
        rate_limit::CommandRateLimiter, telegraph::TelegraphService,
    },
};

#[derive(Clone)]
pub struct AppContext {
    pub config: AppConfig,
    pub db: Database,
    pub i18n: I18n,
    pub openai: OpenAiClient,
    pub limiter: CommandRateLimiter,
    pub telegraph: Option<TelegraphService>,
    /// Recap state store. Production always installs one; `None` exists for
    /// test injection and for handlers that are still being staged.
    pub recap_state: Option<Arc<dyn RecapStateStore>>,
    /// Shared HTTP transport for Task 7's raw Telegram Rich Message client.
    pub raw_telegram_http: reqwest::Client,
    /// Go's `chathistories.Model` preprocessing. Production always installs
    /// one; `None` exists for legacy and unit-test construction, and makes the
    /// message middleware skip persistence rather than guess at a substitute.
    pub message_preprocessor: Option<Arc<DynMessagePreprocessor>>,
    /// Composite `/health` readiness flags; see [`LifecycleFlags`].
    pub lifecycle: Arc<LifecycleFlags>,
    /// Trigger side of the process-wide shutdown signal. Sending `true`
    /// tells cancellable background loops (the automatic-recap poller) to
    /// stop, mirroring Go's reverse-order `fx.Hook` teardown.
    pub shutdown_tx: watch::Sender<bool>,
    /// Receive side of the shutdown signal, cloned by each cancellable loop.
    pub shutdown_rx: watch::Receiver<bool>,
}

#[derive(Clone)]
pub struct RecapRuntimeDependencies {
    /// Recap state store. Production always installs one; `None` exists for
    /// test injection and for handlers that are still being staged.
    pub recap_state: Option<Arc<dyn RecapStateStore>>,
    /// Shared HTTP transport for Task 7's raw Telegram Rich Message client.
    pub raw_telegram_http: reqwest::Client,
    /// Go's `chathistories.Model` preprocessing; see [`AppContext`].
    pub message_preprocessor: Option<Arc<DynMessagePreprocessor>>,
}

impl Default for RecapRuntimeDependencies {
    fn default() -> Self {
        Self {
            recap_state: None,
            raw_telegram_http: reqwest::Client::new(),
            message_preprocessor: None,
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
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        Arc::new(Self {
            config,
            db,
            i18n,
            openai,
            limiter,
            telegraph,
            recap_state: recap_runtime.recap_state,
            raw_telegram_http: recap_runtime.raw_telegram_http,
            message_preprocessor: recap_runtime.message_preprocessor,
            lifecycle: LifecycleFlags::new(),
            shutdown_tx,
            shutdown_rx,
        })
    }
}
