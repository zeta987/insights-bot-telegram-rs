//! Go v1.0.0 automatic Rich recap orchestration.

use std::{future::Future, sync::Arc, time::Duration};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use chrono::{Duration as ChronoDuration, Utc};
use teloxide::{prelude::*, types::ChatFullInfo};
use tokio::time::{Instant, MissedTickBehavior, interval_at, timeout};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    bot::{
        context::AppContext,
        handlers::{
            recap_manual::build_vote_keyboard, recap_subscription::build_subscriber_vote_keyboard,
        },
    },
    db::{
        Database, chat_history, feature_flags,
        feedback::{self, ReactionTable},
        models::{
            CHAT_TYPE_GROUP, CHAT_TYPE_SUPERGROUP, TelegramChatAutoRecapsSubscriber,
            TelegramChatRecapsOptions,
        },
        recap_options, subscribers,
    },
    redis::recap_state::RecapStateStore,
    services::{
        autorecap_delivery::{AutoRecapDeliveryTarget, deliver_auto_recap_targets},
        autorecap_queue::{
            AUTO_RECAP_POLL_INTERVAL, auto_recap_window_hours, effective_auto_recap_rate,
            enqueue_auto_recap, next_auto_recap_at_ms, pop_due_auto_recap,
        },
        rate_limit::GoRateLimiter,
        recap_delivery::{BeforeSendHook, TelegramRecapSender},
        recap_generation::RecapGenerationService,
        rich_recap::{
            RichRecapSummaryConfig, build_rich_recap_summary, compose_rich_recap_messages,
            fallback_condensed_summary,
        },
    },
};

const STATE_READ_ATTEMPTS: usize = 10;
const AUTO_RECAP_QUEUE_TIMEOUT: Duration = Duration::from_secs(60);

/// The three database reads a popped Go TimeCapsule performs before requeueing.
#[async_trait]
pub trait AutoRecapStateReader: Send + Sync {
    async fn recap_enabled(&self, chat_id: i64) -> Result<bool>;

    async fn recap_options(&self, chat_id: i64) -> Result<Option<TelegramChatRecapsOptions>>;

    async fn recap_subscribers(
        &self,
        chat_id: i64,
    ) -> Result<Vec<TelegramChatAutoRecapsSubscriber>>;
}

#[async_trait]
impl AutoRecapStateReader for Database {
    async fn recap_enabled(&self, chat_id: i64) -> Result<bool> {
        feature_flags::has_recap_enabled(self, chat_id, "").await
    }

    async fn recap_options(&self, chat_id: i64) -> Result<Option<TelegramChatRecapsOptions>> {
        recap_options::find_one(self, chat_id).await
    }

    async fn recap_subscribers(
        &self,
        chat_id: i64,
    ) -> Result<Vec<TelegramChatAutoRecapsSubscriber>> {
        subscribers::list(self, chat_id).await
    }
}

/// Values retained after Go's three independent ten-attempt reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoRecapReadState {
    pub enabled: bool,
    pub options: Option<TelegramChatRecapsOptions>,
    pub subscribers: Vec<TelegramChatAutoRecapsSubscriber>,
    pub read_error_count: usize,
}

/// One delivery destination in Go's observable iteration order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AutoRecapTarget {
    pub chat_id: i64,
    pub is_private_subscriber: bool,
}

/// Decision made after state reads and the required next-slot queue writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoRecapPreparation {
    Disabled,
    PrivateWithoutSubscribers {
        options: TelegramChatRecapsOptions,
    },
    Generate {
        options: TelegramChatRecapsOptions,
        subscribers: Vec<TelegramChatAutoRecapsSubscriber>,
    },
}

/// Retry feature, option and subscriber reads independently, without delay.
pub async fn read_auto_recap_state<R>(reader: &R, chat_id: i64) -> AutoRecapReadState
where
    R: AutoRecapStateReader + ?Sized,
{
    let (enabled, enabled_failed) =
        retry_state_read("feature enablement", || reader.recap_enabled(chat_id)).await;
    let (options, options_failed) =
        retry_state_read("recap options", || reader.recap_options(chat_id)).await;
    let (subscribers, subscribers_failed) =
        retry_state_read("recap subscribers", || reader.recap_subscribers(chat_id)).await;

    AutoRecapReadState {
        enabled: enabled.unwrap_or(false),
        options: options.flatten(),
        subscribers: subscribers.unwrap_or_default(),
        read_error_count: [enabled_failed, options_failed, subscribers_failed]
            .into_iter()
            .filter(|failed| *failed)
            .count(),
    }
}

async fn retry_state_read<T, F, Fut>(label: &'static str, mut read: F) -> (Option<T>, bool)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    for attempt in 1..=STATE_READ_ATTEMPTS {
        match read().await {
            Ok(value) => return (Some(value), false),
            Err(source) => error!(
                operation = label,
                attempt,
                error = %source,
                "automatic recap state read failed"
            ),
        }
    }
    (None, true)
}

/// Public-group delivery comes first; subscriber rows keep DB order and duplicates.
#[must_use]
pub fn build_auto_recap_targets(
    source_chat_id: i64,
    send_mode: i64,
    subscriber_ids: &[i64],
) -> Vec<AutoRecapTarget> {
    let mut targets = Vec::with_capacity(subscriber_ids.len() + usize::from(send_mode == 0));
    if send_mode == 0 {
        targets.push(AutoRecapTarget {
            chat_id: source_chat_id,
            is_private_subscriber: false,
        });
    }
    targets.extend(
        subscriber_ids
            .iter()
            .copied()
            .map(|chat_id| AutoRecapTarget {
                chat_id,
                is_private_subscriber: true,
            }),
    );
    targets
}

/// Go generates an automatic recap only when more than five rows were loaded.
#[must_use]
pub const fn has_enough_auto_recap_histories(history_count: usize) -> bool {
    history_count > 5
}

/// Calculate and enqueue the next slot while swallowing the queue failure.
///
/// Go logs `BuryUtil` failures but returns nil to every caller, so the due time
/// remains the only observable return value here.
pub async fn queue_next_auto_recap(
    state: &(impl RecapStateStore + ?Sized),
    chat_id: i64,
    rates_per_day: i64,
    timezone_shift_seconds: i64,
    now_utc_ms: i64,
) -> i64 {
    let configured_rate = i32::try_from(rates_per_day).unwrap_or(4);
    let rate = effective_auto_recap_rate(configured_rate);
    let due_ms = next_auto_recap_at_ms(now_utc_ms, timezone_shift_seconds, rate);
    let queued = timeout(
        AUTO_RECAP_QUEUE_TIMEOUT,
        enqueue_auto_recap(state, chat_id, due_ms),
    )
    .await;
    match queued {
        Ok(Ok(_)) => info!(chat_id, due_ms, "automatic recap scheduled"),
        Ok(Err(source)) => error!(
            chat_id,
            due_ms,
            error = %source,
            "failed to enqueue automatic recap"
        ),
        Err(_) => error!(
            chat_id,
            due_ms, "automatic recap enqueue timed out after sixty seconds"
        ),
    }
    due_ms
}

/// Read state, reproduce Go's error requeue, then perform the normal requeue.
pub async fn prepare_auto_recap<R>(
    reader: &R,
    state: &(impl RecapStateStore + ?Sized),
    chat_id: i64,
    timezone_shift_seconds: i64,
    now_utc_ms: i64,
) -> Result<AutoRecapPreparation>
where
    R: AutoRecapStateReader + ?Sized,
{
    let mut read = read_auto_recap_state(reader, chat_id).await;

    if read.read_error_count > 0 {
        let options = read.options.as_mut().ok_or_else(|| {
            anyhow!("automatic recap state reads were exhausted and no usable options remained")
        })?;
        normalize_options_rate(options);
        queue_next_auto_recap(
            state,
            chat_id,
            options.auto_recap_rates_per_day,
            timezone_shift_seconds,
            now_utc_ms,
        )
        .await;
    }

    if !read.enabled {
        return Ok(AutoRecapPreparation::Disabled);
    }

    let mut options = read
        .options
        .ok_or_else(|| anyhow!("automatic recap is enabled but its options row is unavailable"))?;
    normalize_options_rate(&mut options);
    queue_next_auto_recap(
        state,
        chat_id,
        options.auto_recap_rates_per_day,
        timezone_shift_seconds,
        now_utc_ms,
    )
    .await;

    if options.auto_recap_send_mode == 1 && read.subscribers.is_empty() {
        return Ok(AutoRecapPreparation::PrivateWithoutSubscribers { options });
    }

    Ok(AutoRecapPreparation::Generate {
        options,
        subscribers: read.subscribers,
    })
}

fn normalize_options_rate(options: &mut TelegramChatRecapsOptions) {
    let configured = i32::try_from(options.auto_recap_rates_per_day).unwrap_or(4);
    options.auto_recap_rates_per_day = i64::from(effective_auto_recap_rate(configured));
}

/// Start the one-second TimeCapsule digger after seeding enabled chats.
pub async fn spawn_autorecap(ctx: Arc<AppContext>) {
    let Some(state) = ctx.recap_state.clone() else {
        warn!("automatic recap Redis state store is unavailable");
        return;
    };

    queue_all_enabled_chats(&ctx, state.as_ref()).await;

    if ctx.config.auto_recap_test.enabled && ctx.config.auto_recap_test.chat_id != 0 {
        let test_ctx = ctx.clone();
        let test_state = state.clone();
        let test_chat_id = ctx.config.auto_recap_test.chat_id;
        tokio::spawn(async move {
            if let Err(source) = handle_auto_recap_capsule(test_ctx, test_state, test_chat_id).await
            {
                error!(
                    chat_id = test_chat_id,
                    error = %source,
                    "automatic recap test capsule failed"
                );
            }
        });
    }

    tokio::spawn(async move {
        let first_tick = Instant::now() + AUTO_RECAP_POLL_INTERVAL;
        let mut ticker = interval_at(first_tick, AUTO_RECAP_POLL_INTERVAL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            ticker.tick().await;
            let now_ms = Utc::now().timestamp_millis();
            match pop_due_auto_recap(state.as_ref(), now_ms).await {
                Ok(Some(capsule)) => {
                    if let Err(source) =
                        handle_auto_recap_capsule(ctx.clone(), state.clone(), capsule.chat_id).await
                    {
                        error!(
                            chat_id = capsule.chat_id,
                            error = %source,
                            "automatic recap capsule failed"
                        );
                    }
                }
                Ok(None) => {}
                Err(source) => error!(error = %source, "automatic recap queue poll failed"),
            }
        }
    });
}

async fn queue_all_enabled_chats(ctx: &AppContext, state: &dyn RecapStateStore) {
    let chats = match feature_flags::list_recap_enabled_groups(&ctx.db).await {
        Ok(chats) => chats,
        Err(source) => {
            error!(error = %source, "failed to list enabled automatic recap chats");
            return;
        }
    };

    for chat in chats {
        match recap_options::find_one_or_create(&ctx.db, chat.chat_id).await {
            Ok(options) => {
                queue_next_auto_recap(
                    state,
                    chat.chat_id,
                    options.auto_recap_rates_per_day,
                    ctx.config.timezone_shift_seconds,
                    Utc::now().timestamp_millis(),
                )
                .await;
            }
            Err(source) => error!(
                chat_id = chat.chat_id,
                error = %source,
                "failed to find or create automatic recap options"
            ),
        }
    }
}

async fn handle_auto_recap_capsule(
    ctx: Arc<AppContext>,
    state: Arc<dyn RecapStateStore>,
    chat_id: i64,
) -> Result<()> {
    let preparation = prepare_auto_recap(
        &ctx.db,
        state.as_ref(),
        chat_id,
        ctx.config.timezone_shift_seconds,
        Utc::now().timestamp_millis(),
    )
    .await?;

    match preparation {
        AutoRecapPreparation::Disabled => {
            info!(chat_id, "automatic recap is disabled; capsule discarded");
        }
        AutoRecapPreparation::PrivateWithoutSubscribers { .. } => {
            info!(
                chat_id,
                "private-only automatic recap has no subscribers; generation skipped"
            );
        }
        AutoRecapPreparation::Generate {
            options,
            subscribers,
        } => {
            tokio::spawn(async move {
                if let Err(source) =
                    generate_and_deliver_auto_recap(ctx, state, chat_id, options, subscribers).await
                {
                    error!(
                        chat_id,
                        error = %source,
                        "automatic recap generation failed"
                    );
                }
            });
        }
    }
    Ok(())
}

async fn generate_and_deliver_auto_recap(
    ctx: Arc<AppContext>,
    state: Arc<dyn RecapStateStore>,
    chat_id: i64,
    options: TelegramChatRecapsOptions,
    subscribers: Vec<TelegramChatAutoRecapsSubscriber>,
) -> Result<()> {
    let bot = ctx.config.telegram.bot();
    let chat = bot.get_chat(ChatId(chat_id)).await?;
    let chat_type = telegram_chat_type(&chat);
    let hours =
        auto_recap_window_hours(i32::try_from(options.auto_recap_rates_per_day).unwrap_or(4));
    let histories =
        chat_history::find_by_time_before(&ctx.db, chat_id, ChronoDuration::hours(hours)).await?;
    if !has_enough_auto_recap_histories(histories.len()) {
        warn!(
            chat_id,
            history_count = histories.len(),
            "not enough chat histories"
        );
        return Ok(());
    }
    let chat_title = histories
        .last()
        .map(|history| history.chat_title.clone())
        .unwrap_or_default();

    let generation =
        RecapGenerationService::new(ctx.db.clone(), ctx.openai.clone(), &ctx.config.recap_openai)?;
    let detailed = generation
        .summarize_group_histories(chat_id, chat_type, &histories)
        .await?;
    let log_id = Uuid::parse_str(&detailed.log_id)?;
    let counts =
        feedback::counts(&ctx.db, ReactionTable::ChatHistoriesRecaps, chat_id, log_id).await?;
    let public_keyboard =
        build_vote_keyboard(state.as_ref(), chat_id, &detailed.log_id, counts).await?;
    if detailed.summaries.is_empty() {
        warn!(chat_id, "automatic recap detailed summaries are empty");
        return Ok(());
    }

    let (condensed_summary, condensed_trace) =
        match generation.generate_condensed(chat_id, &histories).await {
            Ok(result) if !result.content.trim().is_empty() => {
                (result.content.trim().to_owned(), result.trace)
            }
            Ok(result) => (
                fallback_condensed_summary(
                    &detailed.summaries,
                    &format!("過去 {hours} 小時的群組聊天回顧"),
                ),
                result.trace,
            ),
            Err(source) => {
                warn!(
                    chat_id,
                    error = %source,
                    "using automatic recap condensed fallback"
                );
                (
                    fallback_condensed_summary(
                        &detailed.summaries,
                        &format!("過去 {hours} 小時的群組聊天回顧"),
                    ),
                    source.trace,
                )
            }
        };

    let build_parts = |subscription_chat_title: &str| {
        let visible = build_rich_recap_summary(&RichRecapSummaryConfig {
            title: &chat_title,
            hours,
            automatic: true,
            initiator_name: "",
            initiator_user_id: 0,
            condensed_summary: &condensed_summary,
            general_group_notice: chat.is_group(),
            subscription_chat_title,
            condensed_trace: Some(&condensed_trace),
            recap_trace: Some(&detailed.trace),
        });
        compose_rich_recap_messages(&visible, &detailed.summaries)
    };

    let public_parts = if options.auto_recap_send_mode == 0 {
        build_parts("")
    } else {
        Vec::new()
    };
    let subscriber_parts = if subscribers.is_empty() {
        Vec::new()
    } else {
        build_parts(&chat_title)
    };
    let subscriber_ids = subscribers
        .iter()
        .map(|subscriber| subscriber.user_id)
        .collect::<Vec<_>>();
    let targets = build_auto_recap_targets(chat_id, options.auto_recap_send_mode, &subscriber_ids);

    let mut delivery_targets = Vec::with_capacity(targets.len());
    for target in targets {
        let (parts, keyboard) = if target.is_private_subscriber {
            let keyboard = match build_subscriber_vote_keyboard(
                state.as_ref(),
                chat_id,
                &chat_title,
                target.chat_id,
                &detailed.log_id,
                counts,
            )
            .await
            {
                Ok(keyboard) => keyboard,
                Err(source) => {
                    error!(
                        chat_id,
                        target_chat_id = target.chat_id,
                        error = %source,
                        "failed to build subscriber automatic recap keyboard"
                    );
                    continue;
                }
            };
            (subscriber_parts.clone(), keyboard)
        } else {
            (public_parts.clone(), public_keyboard.clone())
        };
        if parts.is_empty() {
            error!(
                chat_id,
                target_chat_id = target.chat_id,
                "automatic Rich recap composer returned no messages"
            );
            continue;
        }
        delivery_targets.push(AutoRecapDeliveryTarget {
            chat_id: target.chat_id,
            parts,
            keyboard: Some(keyboard),
            pin_first: options.pin_auto_recap_message && !target.is_private_subscriber,
        });
    }

    let limiter = Arc::new(GoRateLimiter::per_second(5));
    let before_send: BeforeSendHook = Arc::new(move || {
        let limiter = limiter.clone();
        Box::pin(async move { limiter.take().await })
    });
    let sender = TelegramRecapSender::new(ctx.raw_telegram_http.clone(), &ctx.config.telegram);
    deliver_auto_recap_targets(&ctx.db, &sender, &bot, delivery_targets, Some(before_send)).await;
    Ok(())
}

fn telegram_chat_type(chat: &ChatFullInfo) -> &'static str {
    if chat.is_group() {
        CHAT_TYPE_GROUP
    } else if chat.is_supergroup() {
        CHAT_TYPE_SUPERGROUP
    } else if chat.is_channel() {
        "channel"
    } else {
        "private"
    }
}
