//! Shared process lifecycle state.
//!
//! [`LifecycleFlags`] backs the composite `/health` readiness surface. It
//! mirrors Go's `internal/services/health/health.go`, where each named check
//! reads a `started bool` field (`BotService.webhookStarted`,
//! `AutoRecapTimeCapsuleDigger.started`, `AutoRecapService.started`) that
//! starts `false` and is flipped to `true` exactly once by the startup stage
//! it represents. Flags never revert to `false`.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Readiness flags surfaced on `/health`. Each flag is set at most once.
#[derive(Debug, Default)]
pub struct LifecycleFlags {
    bot_authorized: AtomicBool,
    poller_started: AtomicBool,
    auto_recap_started: AtomicBool,
}

impl LifecycleFlags {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Telegram bot authorization succeeded (Go's `telegram_bot` check).
    pub fn mark_bot_authorized(&self) {
        self.bot_authorized.store(true, Ordering::SeqCst);
    }

    /// The automatic-recap queue poller loop has been armed (Go's
    /// `"auto recap timecapsule digger"` check).
    pub fn mark_poller_started(&self) {
        self.poller_started.store(true, Ordering::SeqCst);
    }

    /// The automatic-recap subsystem has been armed (Go's `auto_recap`
    /// check).
    pub fn mark_auto_recap_started(&self) {
        self.auto_recap_started.store(true, Ordering::SeqCst);
    }

    #[must_use]
    pub fn bot_authorized(&self) -> bool {
        self.bot_authorized.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn poller_started(&self) -> bool {
        self.poller_started.load(Ordering::SeqCst)
    }

    #[must_use]
    pub fn auto_recap_started(&self) -> bool {
        self.auto_recap_started.load(Ordering::SeqCst)
    }
}
