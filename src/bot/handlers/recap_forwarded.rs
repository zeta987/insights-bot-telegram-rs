//! Go v1.0.0 private-forwarded recap command orchestration.

use std::sync::Arc;

use teloxide::{
    prelude::*,
    types::{ChatId, MessageId, ReplyParameters},
};
use tracing::{error, info, warn};

use crate::{
    bot::{context::AppContext, handlers::recap_manual::actor_display_name},
    services::{
        message_capture::PrivateForwardedReplayChatHistory,
        recap_delivery::{RecapDeliveryConfig, TelegramRecapSender, send_rich_recap_parts},
        recap_generation::RecapGenerationService,
        rich_recap::{
            RichRecapSummaryConfig, build_rich_recap_summary, compose_rich_recap_messages,
            fallback_condensed_summary,
        },
    },
};

const PRIVATE_ONLY_MESSAGE: &str = "该命令当前只能在私聊中使用哦！";
const START_MESSAGE: &str = "没问题，请将你需要总结的消息在 2 小时内发给我吧。发送完毕后可以通过发送 /recap_forwarded 给我来开始总结哦！";
const WAITING_MESSAGE: &str = "正在为已经接收到的聊天记录生成回顾，请稍等...";
const INSUFFICIENT_MESSAGE: &str =
    "目前收到的聊天记录不足 5 条哦，要再多发送给我一些之后之后再试试吗？";
const GENERATION_FAILURE_MESSAGE: &str = "聊天记录回顾生成失败，请稍后再试！";
const DELIVERY_FAILURE_MESSAGE: &str = "聊天记录回顾发送失败，请稍后再试！";
const COMPLETION_MESSAGE: &str = "总结完成，如果你觉得不满意，可以再次发送 /recap_forwarded 重新生成哦！如果觉得满意，并且希望进行其他的总结操作，可以在开始前发送 /cancel 来清空当前已经接收到的消息，如果不进行操作，缓存的消息将会在 2 小时后被自动清理。";

/// Open or restart the caller's two-hour forwarded-message session.
pub async fn handle_recap_forwarded_start(
    bot: Bot,
    message: Message,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if !message.chat.is_private() {
        send_best_effort(&bot, message.chat.id, PRIVATE_ONLY_MESSAGE, None).await;
        return Ok(());
    }
    let Some(user_id) = message.from.as_ref().and_then(telegram_user_id) else {
        send_best_effort(&bot, message.chat.id, "发生了一些错误，请稍后再试", None).await;
        return Ok(());
    };
    let Some(state) = context.recap_state.as_deref() else {
        error!("forwarded recap state store is unavailable");
        send_best_effort(&bot, message.chat.id, "发生了一些错误，请稍后再试", None).await;
        return Ok(());
    };
    if let Err(source) = state.start_forwarded(user_id).await {
        error!(?source, "failed to start forwarded recap session");
        send_best_effort(&bot, message.chat.id, "发生了一些错误，请稍后再试", None).await;
        return Ok(());
    }

    send_best_effort(&bot, message.chat.id, START_MESSAGE, Some(message.id)).await;
    Ok(())
}

/// Generate a Rich recap from the caller's retained forwarded-message batch.
pub async fn handle_recap_forwarded(
    bot: Bot,
    message: Message,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    if !message.chat.is_private() {
        send_best_effort(&bot, message.chat.id, PRIVATE_ONLY_MESSAGE, None).await;
        return Ok(());
    }
    let Some(user_id) = message.from.as_ref().and_then(telegram_user_id) else {
        send_best_effort(
            &bot,
            message.chat.id,
            GENERATION_FAILURE_MESSAGE,
            Some(message.id),
        )
        .await;
        return Ok(());
    };

    let waiting = send_best_effort(&bot, ChatId(user_id), WAITING_MESSAGE, None).await;
    let Some(state) = context.recap_state.as_deref() else {
        error!("forwarded recap state store is unavailable");
        send_best_effort(
            &bot,
            message.chat.id,
            GENERATION_FAILURE_MESSAGE,
            Some(message.id),
        )
        .await;
        return Ok(());
    };
    let serialized = match state.forwarded_batch(user_id).await {
        Ok(serialized) => serialized,
        Err(source) => {
            error!(?source, "failed to read forwarded recap batch");
            send_best_effort(
                &bot,
                message.chat.id,
                GENERATION_FAILURE_MESSAGE,
                Some(message.id),
            )
            .await;
            return Ok(());
        }
    };
    let mut histories = Vec::with_capacity(serialized.len());
    for json in serialized {
        match serde_json::from_str::<PrivateForwardedReplayChatHistory>(&json) {
            Ok(history) => histories.push(history),
            Err(source) => {
                error!(?source, "failed to decode forwarded recap history");
                send_best_effort(
                    &bot,
                    message.chat.id,
                    GENERATION_FAILURE_MESSAGE,
                    Some(message.id),
                )
                .await;
                return Ok(());
            }
        }
    }
    if histories.len() < 5 {
        send_best_effort(
            &bot,
            message.chat.id,
            INSUFFICIENT_MESSAGE,
            Some(message.id),
        )
        .await;
        return Ok(());
    }

    let generation = match RecapGenerationService::new(
        context.db.clone(),
        context.openai.clone(),
        &context.config.recap_openai,
    ) {
        Ok(generation) => generation,
        Err(source) => {
            error!(
                ?source,
                "failed to construct forwarded recap generation service"
            );
            send_best_effort(
                &bot,
                message.chat.id,
                GENERATION_FAILURE_MESSAGE,
                Some(message.id),
            )
            .await;
            return Ok(());
        }
    };
    let detailed = match generation
        .summarize_private_forwarded_histories(user_id, &histories)
        .await
    {
        Ok(detailed) => detailed,
        Err(source) => {
            error!(error = ?source.source, "forwarded recap detailed generation failed");
            send_best_effort(
                &bot,
                message.chat.id,
                GENERATION_FAILURE_MESSAGE,
                Some(message.id),
            )
            .await;
            return Ok(());
        }
    };
    let summaries = detailed
        .summaries
        .into_iter()
        .filter(|summary| !summary.is_empty())
        .collect::<Vec<_>>();
    if summaries.is_empty() {
        send_best_effort(
            &bot,
            message.chat.id,
            GENERATION_FAILURE_MESSAGE,
            Some(message.id),
        )
        .await;
        return Ok(());
    }

    let source_history = histories
        .iter()
        .map(|history| {
            let actor = if history.actor_display_name.is_empty() {
                history.actor_username.as_str()
            } else {
                history.actor_display_name.as_str()
            };
            format!("{actor}: {}\n", history.text)
        })
        .collect::<String>();
    let (condensed_summary, condensed_trace) = match generation
        .generate_condensed_from_text(user_id, &source_history)
        .await
    {
        Ok(result) => {
            let content = result.content.trim().to_owned();
            let content = if content.is_empty() {
                fallback_condensed_summary(&summaries, "轉發訊息的聊天回顧")
            } else {
                content
            };
            (content, result.trace)
        }
        Err(source) => {
            warn!(error = ?source.source, "forwarded recap condensed generation failed");
            (
                fallback_condensed_summary(&summaries, "轉發訊息的聊天回顧"),
                source.trace,
            )
        }
    };
    let from = message
        .from
        .as_ref()
        .expect("the sender was validated above");
    let initiator_name = actor_display_name(
        &from.first_name,
        from.last_name.as_deref().unwrap_or_default(),
        from.username.as_deref().unwrap_or_default(),
    );
    let visible_summary = build_rich_recap_summary(&RichRecapSummaryConfig {
        title: "轉發訊息",
        initiator_name: &initiator_name,
        initiator_user_id: user_id,
        condensed_summary: &condensed_summary,
        condensed_trace: Some(&condensed_trace),
        recap_trace: Some(&detailed.trace),
        ..Default::default()
    });
    let parts = compose_rich_recap_messages(&visible_summary, &summaries);
    let sender =
        TelegramRecapSender::new(context.raw_telegram_http.clone(), &context.config.telegram);
    let delivery = send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            chat_id: message.chat.id.0,
            parts,
            reply_to_message_id: message.id.0,
            allow_sending_without_reply: true,
            ..Default::default()
        },
    )
    .await;

    match &delivery {
        Ok(messages) => info!(
            message_count = messages.len(),
            "sent forwarded histories Rich recap"
        ),
        Err(failure) => error!(
            error = ?failure.error,
            sent_message_count = failure.messages.len(),
            "failed to send forwarded histories Rich recap"
        ),
    }
    if let Some(waiting) = waiting
        && let Err(source) = bot.delete_message(ChatId(user_id), waiting.id).await
    {
        warn!(?source, "failed to delete forwarded recap waiting message");
    }
    if delivery.is_err() {
        send_best_effort(
            &bot,
            message.chat.id,
            DELIVERY_FAILURE_MESSAGE,
            Some(message.id),
        )
        .await;
        return Ok(());
    }

    send_best_effort(&bot, message.chat.id, COMPLETION_MESSAGE, Some(message.id)).await;
    Ok(())
}

fn telegram_user_id(user: &teloxide::types::User) -> Option<i64> {
    i64::try_from(user.id.0).ok()
}

async fn send_best_effort(
    bot: &Bot,
    chat_id: ChatId,
    text: &str,
    reply_to: Option<MessageId>,
) -> Option<Message> {
    let request = bot.send_message(chat_id, text);
    let result = if let Some(reply_to) = reply_to {
        request
            .reply_parameters(ReplyParameters::new(reply_to))
            .await
    } else {
        request.await
    };
    match result {
        Ok(message) => Some(message),
        Err(source) => {
            warn!(?source, "failed to send forwarded recap response");
            None
        }
    }
}
