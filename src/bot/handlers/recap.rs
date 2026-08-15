use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, Me},
};
use tracing::error;

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
    pub async fn handle_recap(
        bot: Bot,
        msg: Message,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        crate::bot::handlers::recap_manual::handle_public_recap_command(bot, msg, me, ctx).await
    }

    pub async fn handle_configure_recap(
        bot: Bot,
        msg: Message,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        crate::bot::handlers::recap_configure::handle_configure_recap(bot, msg, me, ctx).await
    }

    /// Legacy callback handler that routes to appropriate handler.
    pub async fn handle_callback_query(
        bot: Bot,
        q: CallbackQuery,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        Self::handle_callback_query_inner(bot, q, None, ctx).await
    }

    pub async fn handle_callback_query_with_me(
        bot: Bot,
        q: CallbackQuery,
        me: Me,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        Self::handle_callback_query_inner(bot, q, Some(me), ctx).await
    }

    async fn handle_callback_query_inner(
        bot: Bot,
        q: CallbackQuery,
        me: Option<Me>,
        ctx: Arc<AppContext>,
    ) -> ResponseResult<()> {
        let data = q.data.clone().unwrap_or_default();

        if crate::redis::keys::decode_callback_wire(&data).is_some_and(|(route_hash, _)| {
            route_hash
                == crate::redis::keys::callback_route_hash(
                    crate::bot::handlers::recap_configure::ROUTE_NOP,
                )
        }) {
            return Ok(());
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
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_RECAP_FEEDBACK_REACT) {
            error!(
                ?source,
                "failed to bind legacy recap feedback callback route"
            );
            return Ok(());
        }
        if let Err(source) =
            registry.bind(crate::redis::keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT)
        {
            error!(?source, "failed to bind recap feedback callback route");
            return Ok(());
        }
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_UNSUBSCRIBE_RECAP) {
            error!(?source, "failed to bind recap unsubscribe callback route");
            return Ok(());
        }
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_CONFIGURE_TOGGLE) {
            error!(?source, "failed to bind recap toggle callback route");
            return Ok(());
        }
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_CONFIGURE_ASSIGN_MODE) {
            error!(?source, "failed to bind recap mode callback route");
            return Ok(());
        }
        if let Err(source) =
            registry.bind(crate::redis::keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY)
        {
            error!(?source, "failed to bind recap rate callback route");
            return Ok(());
        }
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_CONFIGURE_PIN) {
            error!(?source, "failed to bind recap pin callback route");
            return Ok(());
        }
        if let Err(source) = registry.bind(crate::redis::keys::ROUTE_CONFIGURE_COMPLETE) {
            error!(?source, "failed to bind recap complete callback route");
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
            } if route == crate::redis::keys::ROUTE_RECAP_FEEDBACK_REACT => {
                crate::bot::handlers::recap_manual::handle_recap_feedback_reaction_callback(
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
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_UNSUBSCRIBE_RECAP => {
                crate::bot::handlers::recap_subscription::handle_unsubscribe_callback(
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
            } if route == crate::redis::keys::ROUTE_CONFIGURE_TOGGLE => {
                let Some(me) = me else {
                    error!("bot identity is unavailable for recap toggle callback");
                    return Ok(());
                };
                crate::bot::handlers::recap_configure::handle_toggle_callback(
                    bot,
                    q,
                    me,
                    payload_json,
                    ctx,
                )
                .await
            }
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_CONFIGURE_ASSIGN_MODE => {
                let Some(me) = me else {
                    error!("bot identity is unavailable for recap mode callback");
                    return Ok(());
                };
                crate::bot::handlers::recap_configure::handle_assign_mode_callback(
                    bot,
                    q,
                    me,
                    payload_json,
                    ctx,
                )
                .await
            }
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY => {
                let Some(me) = me else {
                    error!("bot identity is unavailable for recap rate callback");
                    return Ok(());
                };
                crate::bot::handlers::recap_configure::handle_rates_callback(
                    bot,
                    q,
                    me,
                    payload_json,
                    ctx,
                )
                .await
            }
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_CONFIGURE_PIN => {
                let Some(me) = me else {
                    error!("bot identity is unavailable for recap pin callback");
                    return Ok(());
                };
                crate::bot::handlers::recap_configure::handle_pin_callback(
                    bot,
                    q,
                    me,
                    payload_json,
                    ctx,
                )
                .await
            }
            crate::redis::recap_state::CallbackResolution::Dispatch {
                route,
                payload_json,
                ..
            } if route == crate::redis::keys::ROUTE_CONFIGURE_COMPLETE => {
                crate::bot::handlers::recap_configure::handle_complete_callback(
                    bot,
                    q,
                    payload_json,
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
