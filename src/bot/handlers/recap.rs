use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::{error, info, warn};

use crate::{
    bot::context::AppContext,
    config::Locale,
    db::chat_history,
    i18n::I18n,
    services::{
        openai::RecapTrace,
        rate_limit::RateKey,
        recap::RecapService,
        telegraph::{Node, NodeChild},
    },
};
use regex::Regex;

/// Escape HTML special characters for Telegram HTML parse mode.
fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Build Telegraph nodes from structured markdown summaries.
/// Parses markdown format with ## headers, Participants, Discussion, Conclusion sections.
pub fn build_recap_nodes(
    _condensed: &str,
    segmented: &str,
    trace: &RecapTrace,
    _chat_id: i64,
    locale: &Locale,
    i18n: &I18n,
) -> Vec<Node> {
    let mut nodes = Vec::new();

    // Get labels from all locales for matching (supports any AI output language)
    let participants_labels: Vec<String> = [Locale::En, Locale::ZhHans, Locale::ZhHant]
        .iter()
        .map(|l| {
            let label = i18n.t(*l, "labels.participants", &[]);
            let colon = i18n.t(*l, "labels.colon", &[]);
            format!("{}{}", label, colon)
        })
        .collect();

    let discussion_labels: Vec<String> = [Locale::En, Locale::ZhHans, Locale::ZhHant]
        .iter()
        .map(|l| {
            let label = i18n.t(*l, "labels.discussion", &[]);
            let colon = i18n.t(*l, "labels.colon", &[]);
            format!("{}{}", label, colon)
        })
        .collect();

    let conclusion_labels: Vec<String> = [Locale::En, Locale::ZhHans, Locale::ZhHant]
        .iter()
        .map(|l| {
            let label = i18n.t(*l, "labels.conclusion", &[]);
            let colon = i18n.t(*l, "labels.colon", &[]);
            format!("{}{}", label, colon)
        })
        .collect();

    // Regex to extract links already in HTML format: <a href="...">text</a>
    let re_link = Regex::new(r#"<a href="([^"]+)">([^<]+)</a>"#).expect("invalid regex");

    // Process each line of segmented summary
    for line in segmented.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        // Handle ## headers (topic titles)
        if let Some(header_content) = trimmed.strip_prefix("## ") {
            // Check if header contains a link
            if let Some(cap) = re_link.captures(header_content) {
                let href = cap.get(1).unwrap().as_str();
                let text = cap.get(2).unwrap().as_str();

                nodes.push(Node {
                    tag: "h3".into(),
                    attrs: None,
                    children: vec![NodeChild::Node(Box::new(Node {
                        tag: "a".into(),
                        attrs: Some({
                            let mut hm = std::collections::HashMap::new();
                            hm.insert("href".into(), href.to_string());
                            hm
                        }),
                        children: vec![NodeChild::Text(text.to_string())],
                    }))],
                });
            } else {
                // Plain text header
                nodes.push(Node {
                    tag: "h3".into(),
                    attrs: None,
                    children: vec![NodeChild::Text(header_content.to_string())],
                });
            }
            continue;
        }

        // Handle Participants line -> blockquote
        if participants_labels
            .iter()
            .any(|label| trimmed.starts_with(label))
        {
            nodes.push(Node {
                tag: "blockquote".into(),
                attrs: None,
                children: vec![NodeChild::Text(trimmed.to_string())],
            });
            continue;
        }

        // Handle Discussion label
        if discussion_labels.iter().any(|label| trimmed == label) {
            nodes.push(Node {
                tag: "p".into(),
                attrs: None,
                children: vec![NodeChild::Text(trimmed.to_string())],
            });
            continue;
        }

        // Handle discussion points (lines starting with " - ")
        if trimmed.starts_with(" - ") || trimmed.starts_with("- ") {
            let point_text = if let Some(point_text) = trimmed.strip_prefix(" - ") {
                point_text
            } else {
                trimmed
                    .strip_prefix("- ")
                    .expect("discussion points must start with a supported prefix")
            };

            // Parse links in the point text
            let children = parse_html_links(point_text);

            // Wrap in paragraph with " - " prefix
            let mut para_children = vec![NodeChild::Text(" - ".to_string())];
            para_children.extend(children);

            nodes.push(Node {
                tag: "p".into(),
                attrs: None,
                children: para_children,
            });
            continue;
        }

        // Handle Conclusion
        if conclusion_labels
            .iter()
            .any(|label| trimmed.starts_with(label))
        {
            nodes.push(Node {
                tag: "p".into(),
                attrs: None,
                children: vec![NodeChild::Text(trimmed.to_string())],
            });
            continue;
        }

        // Other lines as regular paragraphs
        let children = parse_html_links(trimmed);
        if !children.is_empty() {
            nodes.push(Node {
                tag: "p".into(),
                attrs: None,
                children,
            });
        }
    }

    // Add horizontal rule before footer
    nodes.push(Node {
        tag: "hr".into(),
        attrs: None,
        children: vec![],
    });

    // Footer: three lines for model info (condensed, segmented, check)
    let footer_text = trace.build_status_lines(locale, i18n);
    for line in footer_text.lines() {
        nodes.push(Node {
            tag: "p".into(),
            attrs: None,
            children: vec![NodeChild::Node(Box::new(Node {
                tag: "em".into(),
                attrs: None,
                children: vec![NodeChild::Text(line.to_string())],
            }))],
        });
    }

    nodes
}

/// Parse HTML anchor tags in text and convert to NodeChild elements.
fn parse_html_links(text: &str) -> Vec<NodeChild> {
    let re_link = Regex::new(r#"<a href="([^"]+)">([^<]+)</a>"#).expect("invalid regex");
    let mut children = Vec::new();
    let mut last = 0;

    for cap in re_link.captures_iter(text) {
        let m = cap.get(0).unwrap();
        let href = cap.get(1).unwrap().as_str();
        let link_text = cap.get(2).unwrap().as_str();

        // Add text before link
        if m.start() > last {
            children.push(NodeChild::Text(text[last..m.start()].to_string()));
        }

        // Add link node
        children.push(NodeChild::Node(Box::new(Node {
            tag: "a".into(),
            attrs: Some({
                let mut hm = std::collections::HashMap::new();
                hm.insert("href".into(), href.to_string());
                hm
            }),
            children: vec![NodeChild::Text(link_text.to_string())],
        })));

        last = m.end();
    }

    // Add remaining text after last link
    if last < text.len() {
        children.push(NodeChild::Text(text[last..].to_string()));
    }

    children
}

/// Available hour options for recap time selection (matching Go version).
const RECAP_HOURS: &[i64] = &[1, 2, 4, 6, 12, 24];

pub struct RecapHandlers;

impl RecapHandlers {
    /// Handle /recap command - shows time selection buttons.
    pub async fn handle_recap(bot: Bot, msg: Message, ctx: Arc<AppContext>) -> ResponseResult<()> {
        let chat_id = msg.chat.id;
        if !msg.chat.is_group() && !msg.chat.is_supergroup() {
            let text = ctx.i18n.t(ctx.config.locale, "recap.group_only", &[]);
            bot.send_message(chat_id, text).await?;
            return Ok(());
        }

        match chat_history::is_recap_enabled(&ctx.db.pool, chat_id.0).await {
            Ok(false) => {
                let text = ctx.i18n.t(ctx.config.locale, "recap.disabled", &[]);
                bot.send_message(chat_id, text).await?;
                return Ok(());
            }
            Ok(true) => {}
            Err(err) => {
                error!("failed to load recap enablement: {err:?}");
                let text = ctx.i18n.t(ctx.config.locale, "config.load_failed", &[]);
                bot.send_message(chat_id, text).await?;
                return Ok(());
            }
        }

        let chat_title = msg
            .chat
            .title()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Chat".to_string());

        // Build time selection keyboard.
        let keyboard = Self::build_time_selection_keyboard(
            chat_id.0,
            &chat_title,
            &ctx.config.locale,
            &ctx.i18n,
        );

        let prompt_text = ctx
            .i18n
            .t(ctx.config.locale, "recap.select_time_range", &[]);

        bot.send_message(chat_id, prompt_text)
            .reply_markup(keyboard)
            .await?;
        Ok(())
    }

    /// Build the inline keyboard for time selection.
    fn build_time_selection_keyboard(
        chat_id: i64,
        _chat_title: &str,
        locale: &Locale,
        i18n: &I18n,
    ) -> InlineKeyboardMarkup {
        let mut rows: Vec<Vec<InlineKeyboardButton>> = Vec::new();
        let mut current_row: Vec<InlineKeyboardButton> = Vec::new();

        for (i, &hours) in RECAP_HOURS.iter().enumerate() {
            let label = if hours == 1 {
                i18n.t(*locale, "recap.hour_label", &[])
            } else {
                i18n.t(
                    *locale,
                    "recap.hours_label",
                    &[("hours", &hours.to_string())],
                )
            };

            // Callback data format: recap:hours:<chat_id>:<hours>
            let callback_data = format!("recap:hours:{}:{}", chat_id, hours);
            current_row.push(InlineKeyboardButton::callback(label, callback_data));

            // 3 buttons per row
            if (i + 1) % 3 == 0 || i == RECAP_HOURS.len() - 1 {
                rows.push(current_row);
                current_row = Vec::new();
            }
        }

        InlineKeyboardMarkup::new(rows)
    }

    /// Handle callback query for recap time selection.
    /// Flow (matching Go version):
    /// 1. Answer callback query (toast)
    /// 2. Edit original message to "generating..." (clear buttons)
    /// 3. Check rate limit
    /// 4. Query messages
    /// 5. If not enough messages -> send error, delete "generating" message
    /// 6. Generate dual recap -> publish to Telegraph, send condensed summary
    pub async fn handle_recap_callback(
        bot: Bot,
        q: CallbackQuery,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let callback_id = q.id.clone();
        let data = q.data.clone().unwrap_or_default();

        // Parse callback data: recap:hours:<chat_id>:<hours>
        let parts: Vec<&str> = data.split(':').collect();
        if parts.len() < 4 || parts[0] != "recap" || parts[1] != "hours" {
            bot.answer_callback_query(callback_id).await?;
            return Ok(());
        }

        let chat_id: i64 = parts[2].parse().unwrap_or(0);
        let hours: i64 = parts[3].parse().unwrap_or(6);

        match chat_history::is_recap_enabled(&ctx.db.pool, chat_id).await {
            Ok(false) => {
                let text = ctx.i18n.t(ctx.config.locale, "recap.disabled", &[]);
                bot.answer_callback_query(callback_id).text(text).await?;
                return Ok(());
            }
            Ok(true) => {}
            Err(err) => {
                error!("failed to load recap enablement for callback: {err:?}");
                bot.answer_callback_query(callback_id).await?;
                return Ok(());
            }
        }

        // Get original message info (the one with time selection buttons).
        let Some(original_msg) = q.message.as_ref() else {
            bot.answer_callback_query(callback_id).await?;
            return Ok(());
        };
        let tg_chat_id = original_msg.chat().id;
        let original_msg_id = original_msg.id();
        let chat_title = original_msg
            .chat()
            .title()
            .map(|s| s.to_string())
            .unwrap_or_else(|| "Chat".to_string());

        // Get actor name for page title.
        let actor_name = q.from.full_name();

        // Step 1: Acknowledge callback immediately (toast notification).
        let toast_text = ctx.i18n.t(ctx.config.locale, "recap.processing", &[]);
        bot.answer_callback_query(callback_id)
            .text(toast_text)
            .await?;

        // Step 2: Edit original message to "generating..." and clear buttons.
        let processing_text = ctx.i18n.t(
            ctx.config.locale,
            "recap.processing_hours",
            &[("hours", &hours.to_string())],
        );
        // Edit message text and remove inline keyboard.
        let empty_keyboard = InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
        if let Err(err) = bot
            .edit_message_text(tg_chat_id, original_msg_id, &processing_text)
            .reply_markup(empty_keyboard)
            .await
        {
            warn!("failed to edit message to processing state: {err:?}");
        }

        // Step 3: Rate limit check.
        if let Err(err) = ctx.limiter.check(RateKey(chat_id, "recap")) {
            let rate_limit_text = ctx.i18n.t(ctx.config.locale, "recap.rate_limited", &[]);
            bot.send_message(tg_chat_id, rate_limit_text).await?;
            // Delete the "generating" message.
            let _ = bot.delete_message(tg_chat_id, original_msg_id).await;
            error!("rate limited recap: {err:?}");
            return Ok(());
        }

        // Step 4: Get messages within time range.
        let messages = match chat_history::messages_since_hours(&ctx.db.pool, chat_id, hours).await
        {
            Ok(msgs) => msgs,
            Err(err) => {
                error!("failed to fetch messages: {err:?}");
                let fail_text = ctx.i18n.t(ctx.config.locale, "recap.fetch_failed", &[]);
                bot.send_message(tg_chat_id, fail_text).await?;
                // Delete the "generating" message.
                let _ = bot.delete_message(tg_chat_id, original_msg_id).await;
                return Ok(());
            }
        };

        // Filter only text messages.
        let text_messages: Vec<_> = messages.iter().filter(|m| !m.text.is_empty()).collect();

        // Step 5: Check if enough messages.
        if text_messages.len() < 5 {
            let not_enough_text = ctx.i18n.t(
                ctx.config.locale,
                "recap.min_messages",
                &[
                    ("hours", &hours.to_string()),
                    ("count", &text_messages.len().to_string()),
                ],
            );
            bot.send_message(tg_chat_id, not_enough_text).await?;
            // Delete the "generating" message.
            let _ = bot.delete_message(tg_chat_id, original_msg_id).await;
            return Ok(());
        }

        info!(
            "generating recap for chat {} with {} messages from last {} hours",
            chat_id,
            text_messages.len(),
            hours
        );

        // Step 6: Generate dual recap using RecapService.
        let svc = RecapService::new(&ctx.db, &ctx.openai);
        let recap_result = svc
            .generate_dual_recap(&messages, &ctx.config.locale, chat_id, &ctx.i18n)
            .await;

        match recap_result {
            Ok(output) => {
                // Build Telegraph page title.
                let page_title = ctx.i18n.t(
                    ctx.config.locale,
                    "recap.page_title",
                    &[
                        ("group", &chat_title),
                        ("user", &actor_name),
                        ("hours", &hours.to_string()),
                    ],
                );

                // Telegraph nodes: condensed + segmented + footer.
                let nodes = build_recap_nodes(
                    &output.condensed_summary,
                    &output.segmented_summary,
                    &output.trace,
                    chat_id,
                    &ctx.config.locale,
                    &ctx.i18n,
                );

                // Try to create Telegraph page (auto split).
                let telegraph_url = if let Some(ref telegraph) = ctx.telegraph {
                    match telegraph.create_page_auto_nodes(&page_title, &nodes).await {
                        Ok(urls) => urls.first().cloned(),
                        Err(err) => {
                            warn!("telegraph page creation failed: {err:?}");
                            None
                        }
                    }
                } else {
                    None
                };

                // Build footer from execution trace.
                let footer = output
                    .trace
                    .build_status_lines(&ctx.config.locale, &ctx.i18n);

                // Format final message with condensed summary and Telegraph link.
                let final_text = if let Some(url) = telegraph_url {
                    ctx.i18n.t(
                        ctx.config.locale,
                        "recap.published",
                        &[
                            ("url", &url),
                            ("title", &escape_html(&page_title)),
                            ("condensed", &escape_html(&output.condensed_summary)),
                            ("footer", &footer),
                        ],
                    )
                } else {
                    // Fallback: send condensed summary + segmented summary directly.
                    // Use segmented_summary_html (already Telegram-HTML-safe) instead of
                    // escape_html on the markdown version, which would destroy <a> links.
                    ctx.i18n.t(
                        ctx.config.locale,
                        "recap.no_telegraph",
                        &[
                            ("hours", &hours.to_string()),
                            ("condensed", &escape_html(&output.condensed_summary)),
                            ("segmented", &output.segmented_summary_html),
                            ("footer", &footer),
                        ],
                    )
                };

                // Send with HTML parse mode.
                bot.send_message(tg_chat_id, final_text)
                    .parse_mode(teloxide::types::ParseMode::Html)
                    .await?;
            }
            Err(err) => {
                error!("recap generation failed: {err:?}");
                let fail_text = ctx.i18n.t(ctx.config.locale, "recap.failed", &[]);
                bot.send_message(tg_chat_id, fail_text).await?;
            }
        }

        // Delete the "generating" message (original time selection message).
        let _ = bot.delete_message(tg_chat_id, original_msg_id).await;

        Ok(())
    }

    pub async fn handle_configure_recap(
        bot: Bot,
        msg: Message,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let chat_id = msg.chat.id;
        if !msg.chat.is_group() && !msg.chat.is_supergroup() {
            let text = ctx.i18n.t(ctx.config.locale, "config.group_only", &[]);
            bot.send_message(chat_id, text).await?;
            return Ok(());
        }

        // Best-effort admin check.
        if let Some(from) = msg.from.as_ref()
            && let Err(err) = bot.get_chat_member(chat_id, from.id).await
        {
            warn!("admin check skipped: {err:?}");
        }

        let cfg = match crate::db::recap_config::get_or_create_recap_config(&ctx.db.pool, chat_id.0)
            .await
        {
            Ok(c) => c,
            Err(err) => {
                error!("load recap config failed: {err:?}");
                let text = ctx.i18n.t(ctx.config.locale, "config.load_failed", &[]);
                bot.send_message(chat_id, text).await?;
                return Ok(());
            }
        };

        let header = ctx.i18n.t(ctx.config.locale, "config.header", &[]);
        let kb = build_configure_keyboard(&cfg, &ctx.i18n, ctx.config.locale);
        bot.send_message(chat_id, header).reply_markup(kb).await?;
        Ok(())
    }

    pub async fn handle_config_callback(
        bot: Bot,
        q: CallbackQuery,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let id = q.id.clone();
        bot.answer_callback_query(id).await?;
        let Some(msg) = q.message else {
            return Ok(());
        };
        let chat_id = msg.chat().id;
        let message_id = msg.id();
        let data = q.data.clone().unwrap_or_default();
        let parts: Vec<&str> = data.split(':').collect();
        if parts.len() < 2 || parts[0] != "cfg" {
            return Ok(());
        }

        // Section header — no-op
        if parts[1] == "noop" {
            return Ok(());
        }

        // Done — remove keyboard and show confirmation
        if parts[1] == "done" {
            let done_text = ctx.i18n.t(ctx.config.locale, "config.updated", &[]);
            bot.edit_message_text(chat_id, message_id, done_text)
                .await
                .ok();
            return Ok(());
        }

        // Setting change — need 3 parts: cfg:{setting}:{value}
        if parts.len() < 3 {
            return Ok(());
        }

        match (parts[1], parts[2]) {
            ("enable", "on") => {
                crate::db::recap_config::set_enabled(&ctx.db.pool, chat_id.0, true)
                    .await
                    .map_err(|e| error!("set enable failed: {e:?}"))
                    .ok();
            }
            ("enable", "off") => {
                crate::db::recap_config::set_enabled(&ctx.db.pool, chat_id.0, false)
                    .await
                    .map_err(|e| error!("set enable failed: {e:?}"))
                    .ok();
            }
            ("auto", "on") => {
                crate::db::recap_config::set_auto_recap(&ctx.db.pool, chat_id.0, true)
                    .await
                    .map_err(|e| error!("set auto failed: {e:?}"))
                    .ok();
            }
            ("auto", "off") => {
                crate::db::recap_config::set_auto_recap(&ctx.db.pool, chat_id.0, false)
                    .await
                    .map_err(|e| error!("set auto failed: {e:?}"))
                    .ok();
            }
            ("freq", rate_str) => {
                if let Ok(rate) = rate_str.parse::<i32>()
                    && [2, 3, 4].contains(&rate)
                {
                    crate::db::recap_config::set_auto_recap_rates_per_day(
                        &ctx.db.pool,
                        chat_id.0,
                        rate,
                    )
                    .await
                    .map_err(|e| error!("set freq failed: {e:?}"))
                    .ok();
                }
            }
            ("pin", "on") => {
                crate::db::recap_config::set_pin_auto_recap_message(
                    &ctx.db.pool,
                    chat_id.0,
                    true,
                )
                .await
                .map_err(|e| error!("set pin failed: {e:?}"))
                .ok();
            }
            ("pin", "off") => {
                crate::db::recap_config::set_pin_auto_recap_message(
                    &ctx.db.pool,
                    chat_id.0,
                    false,
                )
                .await
                .map_err(|e| error!("set pin failed: {e:?}"))
                .ok();
            }
            _ => return Ok(()),
        }

        // Reload config and refresh keyboard in-place
        if let Ok(new_cfg) =
            crate::db::recap_config::get_or_create_recap_config(&ctx.db.pool, chat_id.0).await
        {
            let kb = build_configure_keyboard(&new_cfg, &ctx.i18n, ctx.config.locale);
            bot.edit_message_reply_markup(chat_id, message_id)
                .reply_markup(kb)
                .await
                .ok();
        }
        Ok(())
    }

    /// Legacy callback handler that routes to appropriate handler.
    pub async fn handle_callback_query(
        bot: Bot,
        q: CallbackQuery,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let data = q.data.clone().unwrap_or_default();

        if data.starts_with("recap:") {
            Self::handle_recap_callback(bot, q, ctx).await
        } else if data.starts_with("cfg:") {
            Self::handle_config_callback(bot, q, ctx).await
        } else {
            // Unknown callback, just acknowledge.
            bot.answer_callback_query(q.id).await?;
            Ok(())
        }
    }
}

/// Build the inline keyboard for /configure_recap in Go-style grouped layout.
fn build_configure_keyboard(
    cfg: &crate::db::models::RecapConfig,
    i18n: &I18n,
    locale: Locale,
) -> InlineKeyboardMarkup {
    let on = i18n.t(locale, "config.on", &[]);
    let off = i18n.t(locale, "config.off", &[]);

    let selected = |label: &str| format!("● {label}");

    let (enable_on, enable_off) = if cfg.enabled {
        (selected(&on), off.clone())
    } else {
        (on.clone(), selected(&off))
    };

    let (auto_on, auto_off) = if cfg.auto_recap_enabled {
        (selected(&on), off.clone())
    } else {
        (on.clone(), selected(&off))
    };

    let rates = cfg.auto_recap_rates_per_day;
    let freq_2x = i18n.t(locale, "config.freq_2x", &[]);
    let freq_3x = i18n.t(locale, "config.freq_3x", &[]);
    let freq_4x = i18n.t(locale, "config.freq_4x", &[]);
    let f2 = if rates == 2 { selected(&freq_2x) } else { freq_2x };
    let f3 = if rates == 3 { selected(&freq_3x) } else { freq_3x };
    let f4 = if rates == 4 { selected(&freq_4x) } else { freq_4x };

    let (pin_on, pin_off) = if cfg.pin_auto_recap_message {
        (selected(&on), off)
    } else {
        (on, selected(&off))
    };

    let done = i18n.t(locale, "config.done", &[]);

    InlineKeyboardMarkup::new(vec![
        // Section: Enabled
        vec![InlineKeyboardButton::callback(
            i18n.t(locale, "config.section_enabled", &[]),
            "cfg:noop",
        )],
        vec![
            InlineKeyboardButton::callback(enable_on, "cfg:enable:on"),
            InlineKeyboardButton::callback(enable_off, "cfg:enable:off"),
        ],
        // Section: Auto-recap
        vec![InlineKeyboardButton::callback(
            i18n.t(locale, "config.section_auto", &[]),
            "cfg:noop",
        )],
        vec![
            InlineKeyboardButton::callback(auto_on, "cfg:auto:on"),
            InlineKeyboardButton::callback(auto_off, "cfg:auto:off"),
        ],
        // Section: Frequency
        vec![InlineKeyboardButton::callback(
            i18n.t(locale, "config.section_freq", &[]),
            "cfg:noop",
        )],
        vec![
            InlineKeyboardButton::callback(f2, "cfg:freq:2"),
            InlineKeyboardButton::callback(f3, "cfg:freq:3"),
            InlineKeyboardButton::callback(f4, "cfg:freq:4"),
        ],
        // Section: Pin
        vec![InlineKeyboardButton::callback(
            i18n.t(locale, "config.section_pin", &[]),
            "cfg:noop",
        )],
        vec![
            InlineKeyboardButton::callback(pin_on, "cfg:pin:on"),
            InlineKeyboardButton::callback(pin_off, "cfg:pin:off"),
        ],
        // Done
        vec![InlineKeyboardButton::callback(
            format!("✅ {done}"),
            "cfg:done",
        )],
    ])
}
