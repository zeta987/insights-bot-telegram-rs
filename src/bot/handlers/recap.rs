use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup},
};
use tracing::{error, warn};

use crate::{
    bot::context::AppContext,
    config::Locale,
    i18n::I18n,
    services::{
        openai::RecapTrace,
        telegraph::{Node, NodeChild},
    },
};
use regex::Regex;

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
pub struct RecapHandlers;

impl RecapHandlers {
    /// Handle /recap command - shows time selection buttons.
    pub async fn handle_recap(bot: Bot, msg: Message, ctx: Arc<AppContext>) -> ResponseResult<()> {
        crate::bot::handlers::recap_manual::handle_public_recap_command(bot, msg, ctx).await
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
                crate::db::recap_config::set_pin_auto_recap_message(&ctx.db.pool, chat_id.0, true)
                    .await
                    .map_err(|e| error!("set pin failed: {e:?}"))
                    .ok();
            }
            ("pin", "off") => {
                crate::db::recap_config::set_pin_auto_recap_message(&ctx.db.pool, chat_id.0, false)
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

        if data.starts_with("cfg:") {
            return Self::handle_config_callback(bot, q, ctx).await;
        }

        let Some(state) = ctx.recap_state.as_deref() else {
            error!("recap callback state store is unavailable");
            return Ok(());
        };
        let mut registry = crate::redis::recap_state::CallbackRouteRegistry::new();
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_SELECT_HOUR) {
            error!(?source, "failed to bind select-hour callback route");
            return Ok(());
        }
        if let Err(source) =
            registry.bind(crate::redis::keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT)
        {
            error!(?source, "failed to bind recap feedback callback route");
            return Ok(());
        }
        let resolution = match registry.resolve(state, &data).await {
            Ok(resolution) => resolution,
            Err(source) => {
                error!(?source, "failed to resolve recap callback route");
                return Ok(());
            }
        };
        match resolution {
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_SELECT_HOUR => {
                crate::bot::handlers::recap_manual::handle_select_hour_callback(
                    bot,
                    q,
                    payload_json,
                    ctx,
                )
                .await
            }
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT => {
                crate::bot::handlers::recap_manual::handle_feedback_reaction_callback(
                    bot,
                    q,
                    payload_json,
                    ctx,
                )
                .await
            }
            crate::redis::recap_state::CallbackResolution::UnknownRoute => Ok(()),
            crate::redis::recap_state::CallbackResolution::Malformed
            | crate::redis::recap_state::CallbackResolution::MissingHandler { .. }
            | crate::redis::recap_state::CallbackResolution::Dispatch { .. } => {
                if let Some(message) = q.message.as_ref() {
                    bot.edit_message_text(
                        message.chat().id,
                        message.id(),
                        "抱歉，因为操作无效，此操作无法进行，请重新发起操作后再试。",
                    )
                    .await
                    .ok();
                }
                Ok(())
            }
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
    let f2 = if rates == 2 {
        selected(&freq_2x)
    } else {
        freq_2x
    };
    let f3 = if rates == 3 {
        selected(&freq_3x)
    } else {
        freq_3x
    };
    let f4 = if rates == 4 {
        selected(&freq_4x)
    } else {
        freq_4x
    };

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
