use std::{sync::Arc, time::Duration};

use crate::db::models::RecapConfig;
use chrono::Utc;
use teloxide::prelude::*;
use teloxide::types::MessageId;
use tokio::time::interval;
use tracing::{error, info, warn};

use crate::bot::handlers::recap::build_recap_nodes;
use crate::{bot::context::AppContext, db::chat_history, services::recap::RecapService};

/// Schedule slots for each frequency (UTC hours).
const SCHEDULE_2X: &[u32] = &[8, 20];
const SCHEDULE_3X: &[u32] = &[0, 8, 16];
const SCHEDULE_4X: &[u32] = &[2, 8, 14, 20];

/// Message window in hours for each frequency.
fn message_window_hours(rates_per_day: i32) -> i64 {
    match rates_per_day {
        2 => 12,
        3 => 8,
        _ => 6, // 4x default
    }
}

/// Get the schedule slots for a given frequency.
fn schedule_slots(rates_per_day: i32) -> &'static [u32] {
    match rates_per_day {
        2 => SCHEDULE_2X,
        3 => SCHEDULE_3X,
        _ => SCHEDULE_4X,
    }
}

/// Check if a config is due for recap based on schedule slots.
/// Returns true if the current UTC hour matches a slot AND last_recap_at
/// is before the current slot's start time.
fn is_due_for_recap(cfg: &RecapConfig, now: i64) -> bool {
    let current_hour = ((now % 86400) / 3600) as u32;
    let slots = schedule_slots(cfg.auto_recap_rates_per_day);

    // Find if current hour is within ±30 minutes of any slot
    let matching_slot = slots.iter().find(|&&slot_hour| {
        // Allow a 59-minute window: exact hour match only
        // (the 60s ticker ensures we check each hour)
        current_hour == slot_hour
    });

    let Some(&slot_hour) = matching_slot else {
        return false;
    };

    // Calculate the start timestamp of this slot today
    let today_start = now - (now % 86400); // midnight UTC today
    let slot_start = today_start + (slot_hour as i64 * 3600);

    // Due if never recapped or last recap was before this slot started
    match cfg.last_recap_at {
        None => true,
        Some(last) => last < slot_start,
    }
}

pub async fn spawn_autorecap(ctx: Arc<AppContext>) {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(60));
        loop {
            ticker.tick().await;
            if let Err(err) = run_once(ctx.clone()).await {
                error!("auto_recap tick failed: {err:?}");
            }
        }
    });
}

async fn run_once(ctx: Arc<AppContext>) -> anyhow::Result<()> {
    let now = Utc::now().timestamp();
    let configs = crate::db::recap_config::list_auto_recap_enabled(&ctx.db.pool).await?;
    if configs.is_empty() {
        return Ok(());
    }

    // Filter to only configs that are due based on schedule slots
    let due_configs: Vec<_> = configs
        .into_iter()
        .filter(|c| is_due_for_recap(c, now))
        .collect();
    if due_configs.is_empty() {
        return Ok(());
    }

    let bot = ctx.config.telegram.bot();
    let service = RecapService::new(&ctx.db, &ctx.openai);

    for cfg in due_configs {
        let hours = message_window_hours(cfg.auto_recap_rates_per_day);

        // Fetch messages using the time-based window instead of a fixed limit
        let messages =
            match chat_history::messages_since_hours(&ctx.db.pool, cfg.chat_id, hours).await {
                Ok(msgs) => msgs,
                Err(err) => {
                    warn!(
                        "auto_recap fetch messages for chat {} failed: {err:?}",
                        cfg.chat_id
                    );
                    continue;
                }
            };

        // Need at least 5 text messages
        let text_count = messages.iter().filter(|m| !m.text.is_empty()).count();
        if text_count < 5 {
            // Update last_recap_at to avoid re-triggering this slot
            update_last_recap_at(&ctx, cfg.chat_id, now).await;
            continue;
        }

        match service
            .generate_dual_recap(&messages, &ctx.config.locale, cfg.chat_id, &ctx.i18n)
            .await
        {
            Ok(output) => {
                let chat_title = extract_chat_title(&cfg);
                let page_title = ctx.i18n.t(
                    ctx.config.locale,
                    "recap.auto_page_title",
                    &[("chat", &chat_title)],
                );

                let nodes = build_recap_nodes(
                    &output.condensed_summary,
                    &output.segmented_summary,
                    &output.trace,
                    cfg.chat_id,
                    &ctx.config.locale,
                    &ctx.i18n,
                );

                let telegraph_url = if let Some(ref telegraph) = ctx.telegraph {
                    match telegraph.create_page_auto_nodes(&page_title, &nodes).await {
                        Ok(urls) => urls.first().cloned(),
                        Err(err) => {
                            warn!("telegraph page creation failed (auto_recap): {err:?}");
                            None
                        }
                    }
                } else {
                    None
                };

                fn esc(text: &str) -> String {
                    text.replace('&', "&amp;")
                        .replace('<', "&lt;")
                        .replace('>', "&gt;")
                }

                let footer = output
                    .trace
                    .build_status_lines(&ctx.config.locale, &ctx.i18n);

                let final_text = if let Some(url) = telegraph_url {
                    ctx.i18n.t(
                        ctx.config.locale,
                        "recap.auto_published",
                        &[
                            ("url", &url),
                            ("title", &esc(&page_title)),
                            ("condensed", &esc(&output.condensed_summary)),
                            ("group", &chat_title),
                            ("footer", &footer),
                        ],
                    )
                } else {
                    ctx.i18n.t(
                        ctx.config.locale,
                        "recap.auto_no_telegraph",
                        &[
                            ("condensed", &esc(&output.condensed_summary)),
                            ("segmented", &output.segmented_summary_html),
                            ("group", &chat_title),
                            ("footer", &footer),
                        ],
                    )
                };

                let send_result = bot
                    .send_message(ChatId(cfg.chat_id), final_text.clone())
                    .parse_mode(teloxide::types::ParseMode::Html)
                    .await;

                // Only advance last_recap_at on successful delivery
                if let Ok(sent_msg) = &send_result {
                    if cfg.pin_auto_recap_message {
                        handle_pin(&bot, &ctx, &cfg, sent_msg.id).await;
                    }
                    update_last_recap_at(&ctx, cfg.chat_id, now).await;
                } else if let Err(err) = &send_result {
                    warn!("auto_recap send to chat {} failed: {err:?}", cfg.chat_id);
                }
            }
            Err(err) => warn!("auto_recap for chat {} failed: {err:?}", cfg.chat_id),
        }
    }

    info!("auto_recap completed");
    Ok(())
}

/// Handle pin/unpin logic for auto-recap messages.
async fn handle_pin(bot: &Bot, ctx: &AppContext, cfg: &RecapConfig, new_msg_id: MessageId) {
    let chat_id = ChatId(cfg.chat_id);

    // Unpin previous recap message if tracked
    if let Some(prev_id) = cfg.last_pinned_message_id
        && let Err(err) = bot
            .unpin_chat_message(chat_id)
            .message_id(MessageId(prev_id as i32))
            .await
    {
        warn!(
            "failed to unpin previous recap message {} in chat {}: {err:?}",
            prev_id, cfg.chat_id
        );
    }

    // Pin the new message
    match bot.pin_chat_message(chat_id, new_msg_id).await {
        Ok(_) => {
            // Store the pinned message ID
            if let Err(err) = crate::db::recap_config::set_last_pinned_message_id(
                &ctx.db.pool,
                cfg.chat_id,
                Some(new_msg_id.0 as i64),
            )
            .await
            {
                warn!("failed to store pinned message id: {err:?}");
            }
        }
        Err(err) => {
            warn!(
                "failed to pin recap message in chat {}: {err:?}",
                cfg.chat_id
            );
            // Clear tracked pin since we couldn't pin
            let _ = crate::db::recap_config::set_last_pinned_message_id(
                &ctx.db.pool,
                cfg.chat_id,
                None,
            )
            .await;
        }
    }
}

async fn update_last_recap_at(ctx: &AppContext, chat_id: i64, now: i64) {
    if let Err(err) = crate::db::recap_config::set_last_recap_at(&ctx.db.pool, chat_id, now).await {
        warn!("update last_recap_at failed: {err:?}");
    }
}

fn extract_chat_title(cfg: &RecapConfig) -> String {
    cfg.chat_id.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::models::RecapConfig;

    fn make_config(rates: i32, last_recap_at: Option<i64>) -> RecapConfig {
        RecapConfig {
            chat_id: -100123,
            enabled: true,
            auto_recap_enabled: true,
            last_recap_at,
            updated_at: None,
            auto_recap_rates_per_day: rates,
            pin_auto_recap_message: false,
            last_pinned_message_id: None,
        }
    }

    #[test]
    fn schedule_slots_2x() {
        assert_eq!(schedule_slots(2), &[8, 20]);
    }

    #[test]
    fn schedule_slots_3x() {
        assert_eq!(schedule_slots(3), &[0, 8, 16]);
    }

    #[test]
    fn schedule_slots_4x() {
        assert_eq!(schedule_slots(4), &[2, 8, 14, 20]);
    }

    #[test]
    fn message_windows() {
        assert_eq!(message_window_hours(2), 12);
        assert_eq!(message_window_hours(3), 8);
        assert_eq!(message_window_hours(4), 6);
    }

    #[test]
    fn due_when_never_recapped_and_slot_matches() {
        // 08:30 UTC => hour 8
        let now = 86400 + 8 * 3600 + 1800; // day 1, 08:30
        let cfg = make_config(2, None); // 2x: slots [8, 20]
        assert!(is_due_for_recap(&cfg, now));
    }

    #[test]
    fn not_due_when_wrong_hour() {
        // 10:00 UTC => hour 10 (not in 2x slots [8, 20])
        let now = 86400 + 10 * 3600;
        let cfg = make_config(2, None);
        assert!(!is_due_for_recap(&cfg, now));
    }

    #[test]
    fn not_due_when_already_recapped_this_slot() {
        // 08:30 UTC, last recapped at 08:15 today
        let today_start = 86400;
        let now = today_start + 8 * 3600 + 1800;
        let last = today_start + 8 * 3600 + 900; // 08:15 > slot start 08:00
        let cfg = make_config(4, Some(last)); // 4x: slots [2, 8, 14, 20]
        assert!(!is_due_for_recap(&cfg, now));
    }

    #[test]
    fn due_when_last_recap_before_slot_start() {
        // 08:30 UTC, last recapped yesterday at 20:30
        let today_start = 86400;
        let now = today_start + 8 * 3600 + 1800;
        let last = 20 * 3600 + 1800; // yesterday 20:30
        let cfg = make_config(4, Some(last));
        assert!(is_due_for_recap(&cfg, now));
    }

    #[test]
    fn due_for_3x_at_midnight() {
        // 00:15 UTC => hour 0 (in 3x slots [0, 8, 16])
        let now = 86400 + 900;
        let cfg = make_config(3, None);
        assert!(is_due_for_recap(&cfg, now));
    }
}
