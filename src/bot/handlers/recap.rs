use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{CallbackQuery, Me},
};
use tracing::error;

use crate::bot::context::AppContext;

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
