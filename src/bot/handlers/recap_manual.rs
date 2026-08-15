//! Go v1.0.0 public manual-recap callback and presentation primitives.

use anyhow::{Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use teloxide::{
    prelude::*,
    types::{
        CallbackQuery, InlineKeyboardButton, InlineKeyboardMarkup, MessageId, ParseMode,
        ReplyParameters,
    },
};
use tracing::{error, info, warn};
use uuid::Uuid;

use crate::{
    bot::context::AppContext,
    db::{
        chat_history, feature_flags, feedback,
        models::{AutoRecapSendMode, ReactionCounts, ReactionType},
        recap_options,
    },
    redis::{keys, recap_state::RecapStateStore},
    services::{
        recap_delivery::{RecapDeliveryConfig, TelegramRecapSender, send_rich_recap_parts},
        recap_generation::RecapGenerationService,
        rich_recap::{
            RichRecapSummaryConfig, build_rich_recap_summary, compose_rich_recap_messages,
            fallback_condensed_summary,
        },
    },
};

pub const AUTO_RECAP_SEND_MODE_PUBLICLY: i64 = 0;
pub const AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS: i64 = 1;
pub const RECAP_SELECT_HOURS: [i64; 6] = [1, 2, 4, 6, 12, 24];

/// Go's `recap.SelectHourCallbackQueryData`, including field order and JSON
/// names because the compact bytes determine the callback action hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct SelectHourCallbackData {
    pub hour: i64,
    pub chat_id: i64,
    pub chat_title: String,
    pub recap_mode: i64,
}

impl SelectHourCallbackData {
    pub fn from_json(payload_json: &str) -> Result<Self> {
        let data: Self = serde_json::from_str(payload_json)?;
        if !RECAP_SELECT_HOURS.contains(&data.hour) {
            bail!("invalid hour: {}", data.hour);
        }
        Ok(data)
    }
}

#[derive(Debug, Serialize, Deserialize)]
struct FeedbackRecapReactionActionData {
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "logId")]
    log_id: String,
    #[serde(rename = "type")]
    reaction_type: String,
}

fn to_go_json<T>(value: &T) -> Result<String>
where
    T: Serialize,
{
    Ok(serde_json::to_string(value)?
        .replace('&', "\\u0026")
        .replace('<', "\\u003c")
        .replace('>', "\\u003e")
        .replace('\u{2028}', "\\u2028")
        .replace('\u{2029}', "\\u2029"))
}

/// Build Go's two-row six-hour selector and allocate all callback payloads.
pub async fn build_select_hour_keyboard(
    state: &dyn RecapStateStore,
    chat_id: i64,
    chat_title: &str,
    recap_mode: i64,
) -> Result<InlineKeyboardMarkup> {
    let mut rows = Vec::with_capacity(2);
    let mut row = Vec::with_capacity(3);
    for (index, hour) in RECAP_SELECT_HOURS.into_iter().enumerate() {
        let payload = to_go_json(&SelectHourCallbackData {
            hour,
            chat_id,
            chat_title: chat_title.to_owned(),
            recap_mode,
        })?;
        let wire = state
            .put_callback(keys::ROUTE_SELECT_HOUR, &payload)
            .await?;
        row.push(InlineKeyboardButton::callback(format!("{hour} 小时"), wire));
        if (index + 1) % 3 == 0 {
            rows.push(std::mem::take(&mut row));
        }
    }
    Ok(InlineKeyboardMarkup::new(rows))
}

/// Build the three initial recap vote buttons.
///
/// Go deliberately assigns the recap buttons to the summarization feedback
/// route, so the route below remains the `/smr` compatibility exception.
pub async fn build_vote_keyboard(
    state: &dyn RecapStateStore,
    chat_id: i64,
    log_id: &str,
    counts: ReactionCounts,
) -> Result<InlineKeyboardMarkup> {
    let specs = [
        ("👍", "up_vote", counts.up_votes),
        ("👎", "down_vote", counts.down_votes),
        ("🤣", "lmao", counts.lmao),
    ];
    let mut buttons = Vec::with_capacity(specs.len());
    for (emoji, reaction_type, count) in specs {
        let payload = to_go_json(&FeedbackRecapReactionActionData {
            chat_id,
            log_id: log_id.to_owned(),
            reaction_type: reaction_type.to_owned(),
        })?;
        let wire = state
            .put_callback(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT, &payload)
            .await?;
        let label = if count <= 0 {
            emoji.to_owned()
        } else {
            format!("{emoji} {count}")
        };
        buttons.push(InlineKeyboardButton::callback(label, wire));
    }
    Ok(InlineKeyboardMarkup::new(vec![buttons]))
}

#[must_use]
pub fn actor_display_name(first_name: &str, last_name: &str, username: &str) -> String {
    let full_name = format!("{first_name} {last_name}").trim().to_owned();
    if full_name.is_empty() {
        username.to_owned()
    } else {
        full_name
    }
}

#[must_use]
pub fn group_display_name(chat_title: &str) -> String {
    let title = chat_title.trim();
    if title.is_empty() {
        "當前聊天".to_owned()
    } else {
        title.to_owned()
    }
}

#[must_use]
pub fn insufficient_histories_message(hours: i64, recap_mode: i64) -> String {
    if recap_mode == AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS {
        format!(
            "最近 {hours} 小時內暫時沒有超過 5 條的聊天記錄可以生成聊天回顧哦，要再等待群內成員多聊點之後再試試嗎？"
        )
    } else {
        format!(
            "最近 {hours} 小時內暫時沒有超過 5 條的聊天記錄可以生成聊天回顧哦，要再多聊點之後再試試嗎？"
        )
    }
}

/// Public-group `/recap` command path through Go's feature flag, option and
/// Redis rate-limit repositories.
pub async fn handle_public_recap_command(
    bot: Bot,
    message: Message,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let chat_id = message.chat.id;
    if !message.chat.is_group() && !message.chat.is_supergroup() {
        send_reply(
            &bot,
            &message,
            "只有在群组和超级群组内才可以创建聊天记录回顾哦！",
        )
        .await?;
        return Ok(());
    }

    let chat_title = message.chat.title().unwrap_or_default();
    match feature_flags::has_recap_enabled(&context.db, chat_id.0, chat_title).await {
        Ok(true) => {}
        Ok(false) => {
            send_reply(
                &bot,
                &message,
                "聊天记录回顾功能在当前群组尚未启用，需要在群组管理员通过 /configure_recap 命令配置功能启用后才可以创建聊天回顾哦。",
            )
            .await?;
            return Ok(());
        }
        Err(source) => {
            error!(
                ?source,
                chat_id = chat_id.0,
                "failed to read manual recap feature flag"
            );
            send_reply(&bot, &message, "聊天记录回顾生成失败，请稍后再试！").await?;
            return Ok(());
        }
    }

    let options = match recap_options::find_one(&context.db, chat_id.0).await {
        Ok(options) => options,
        Err(source) => {
            error!(
                ?source,
                chat_id = chat_id.0,
                "failed to read manual recap options"
            );
            send_reply(&bot, &message, "聊天记录回顾生成失败，请稍后再试！").await?;
            return Ok(());
        }
    };

    // The private-subscription branch is wired by the following Task 12
    // section. Until then this function remains the public-mode implementation.
    let recap_mode = options
        .as_ref()
        .and_then(|option| option.send_mode())
        .unwrap_or(AutoRecapSendMode::Publicly)
        .as_stored();
    let per_seconds = recap_options::manual_rate_per_seconds(
        options.as_ref(),
        context.config.manual_recap_rate_per_seconds,
    );
    let Some(state) = context.recap_state.as_deref() else {
        error!(
            chat_id = chat_id.0,
            "manual recap state store is unavailable"
        );
        send_reply(&bot, &message, "聊天记录回顾生成失败，请稍后再试！").await?;
        return Ok(());
    };
    let rate = match state
        .check_manual_recap_rate(chat_id.0, 1, per_seconds)
        .await
    {
        Ok(rate) => rate,
        Err(source) => {
            error!(
                ?source,
                chat_id = chat_id.0,
                "failed to check manual recap rate"
            );
            crate::redis::recap_state::ManualRecapRateResult {
                counted_rate: 0,
                ttl_seconds: 0,
                allowed: false,
            }
        }
    };
    if !rate.allowed {
        let duration_value = per_seconds.wrapping_mul(1_000_000_000);
        let ttl_minutes = rate.ttl_seconds / 60;
        let wait_minutes = if ttl_minutes <= 1 { 1 } else { ttl_minutes };
        send_reply(
            &bot,
            &message,
            &format!(
                "很抱歉，您的操作触发了我们的限制机制，为了保证系统的可用性，本命令每最多 {duration_value} 分钟最多使用一次，请您耐心等待 {wait_minutes} 分钟后再试，感谢您的理解和支持。"
            ),
        )
        .await?;
        return Ok(());
    }

    let keyboard = match build_select_hour_keyboard(state, chat_id.0, chat_title, recap_mode).await
    {
        Ok(keyboard) => keyboard,
        Err(source) => {
            error!(
                ?source,
                chat_id = chat_id.0,
                "failed to build manual recap selector"
            );
            send_reply(&bot, &message, "聊天记录回顾生成失败，请稍后再试！").await?;
            return Ok(());
        }
    };

    bot.send_message(chat_id, "请问您要为过去几个小时内的聊天创建回顾呢？")
        .reply_parameters(ReplyParameters::new(message.id))
        .reply_markup(keyboard)
        .await?;
    Ok(())
}

async fn send_reply(bot: &Bot, message: &Message, text: &str) -> ResponseResult<Message> {
    bot.send_message(message.chat.id, text)
        .reply_parameters(ReplyParameters::new(message.id))
        .await
}

/// Resolve one stored select-hour payload and run Go's Rich manual-recap path.
pub async fn handle_select_hour_callback(
    bot: Bot,
    callback: CallbackQuery,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    // Telegram's progress spinner is acknowledged before the potentially long
    // OpenAI work. Go's dispatcher performs the equivalent callback handling.
    bot.answer_callback_query(callback.id.clone()).await?;
    let Some(waiting) = callback.message.as_ref() else {
        return Ok(());
    };
    let destination_chat_id = waiting.chat().id;
    let waiting_message_id = waiting.id();
    let reply_to_message_id = waiting
        .regular_message()
        .and_then(Message::reply_to_message)
        .map_or(0, |message| message.id.0);

    let data = match SelectHourCallbackData::from_json(&payload_json) {
        Ok(data) => data,
        Err(source) => {
            error!(?source, "failed to bind manual recap callback payload");
            send_callback_error(
                &bot,
                destination_chat_id,
                reply_to_message_id,
                "聊天記錄回顧生成失敗，請稍後再試！",
            )
            .await?;
            return Ok(());
        }
    };

    let result = execute_select_hour_callback(
        &bot,
        &callback,
        destination_chat_id,
        waiting_message_id,
        reply_to_message_id,
        &data,
        &context,
    )
    .await;
    if let Err(failure) = result {
        error!(
            error = ?failure.source,
            chat_id = data.chat_id,
            "manual recap callback failed"
        );
        send_callback_error(
            &bot,
            destination_chat_id,
            reply_to_message_id,
            &failure.user_message,
        )
        .await?;
    }
    Ok(())
}

/// Handle the narrow `/smr` feedback-route compatibility required by Rich
/// recap vote buttons. No `/smr` generation behavior is included here.
pub async fn handle_feedback_reaction_callback(
    bot: Bot,
    callback: CallbackQuery,
    payload_json: String,
    context: Arc<AppContext>,
) -> ResponseResult<()> {
    let Some(message) = callback.message.as_ref() else {
        return Ok(());
    };
    let message_id = message.id();
    let data: FeedbackRecapReactionActionData = match serde_json::from_str(&payload_json) {
        Ok(data) => data,
        Err(source) => {
            error!(?source, "failed to bind recap feedback callback payload");
            return Ok(());
        }
    };
    let log_id = match Uuid::parse_str(&data.log_id) {
        Ok(log_id) => log_id,
        Err(source) => {
            error!(?source, "failed to parse recap feedback log identifier");
            return Ok(());
        }
    };
    let Some(reaction) = ReactionType::from_stored(&data.reaction_type) else {
        return Ok(());
    };
    if reaction == ReactionType::None {
        return Ok(());
    }
    let user_id = match i64::try_from(callback.from.id.0) {
        Ok(user_id) => user_id,
        Err(source) => {
            error!(?source, "recap feedback actor identifier exceeds int64");
            return Ok(());
        }
    };
    if let Err(source) = feedback::react(
        &context.db,
        feedback::ReactionTable::Summarizations,
        data.chat_id,
        log_id,
        user_id,
        reaction,
    )
    .await
    {
        error!(?source, "failed to apply recap feedback reaction");
        return Ok(());
    }
    let counts = match feedback::counts(
        &context.db,
        feedback::ReactionTable::Summarizations,
        data.chat_id,
        log_id,
    )
    .await
    {
        Ok(counts) => counts,
        Err(source) => {
            error!(?source, "failed to count recap feedback reactions");
            return Ok(());
        }
    };
    let Some(state) = context.recap_state.as_deref() else {
        error!("recap feedback state store is unavailable");
        return Ok(());
    };
    let canonical_log_id = log_id.to_string();
    let markup = match build_vote_keyboard(state, data.chat_id, &canonical_log_id, counts).await {
        Ok(markup) => markup,
        Err(source) => {
            error!(?source, "failed to rebuild recap feedback markup");
            return Ok(());
        }
    };
    if let Err(source) = bot
        .edit_message_reply_markup(ChatId(data.chat_id), message_id)
        .reply_markup(markup)
        .await
    {
        error!(?source, "failed to edit recap feedback markup");
    }
    Ok(())
}

struct ManualCallbackFailure {
    source: anyhow::Error,
    user_message: String,
}

impl ManualCallbackFailure {
    fn generation(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            user_message: "聊天記錄回顧生成失敗，請稍後再試！".to_owned(),
        }
    }

    fn message(message: String) -> Self {
        Self {
            source: anyhow!(message.clone()),
            user_message: message,
        }
    }

    fn delivery(source: impl Into<anyhow::Error>) -> Self {
        Self {
            source: source.into(),
            user_message: "聊天記錄回顧發送失敗，請稍後再試！".to_owned(),
        }
    }
}

async fn execute_select_hour_callback(
    bot: &Bot,
    callback: &CallbackQuery,
    destination_chat_id: ChatId,
    waiting_message_id: MessageId,
    reply_to_message_id: i32,
    data: &SelectHourCallbackData,
    context: &AppContext,
) -> std::result::Result<(), ManualCallbackFailure> {
    let in_progress_text = if data.recap_mode == AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS {
        format!(
            "正在為 <b>{}</b> 過去 {} 個小時的聊天記錄生成回顧，請稍等...",
            escape_html(&data.chat_title),
            data.hour
        )
    } else {
        format!(
            "正在為過去 {} 個小時的聊天記錄生成回顧，請稍等...",
            data.hour
        )
    };
    let empty_keyboard = InlineKeyboardMarkup::new(Vec::<Vec<InlineKeyboardButton>>::new());
    if let Err(source) = bot
        .edit_message_text(destination_chat_id, waiting_message_id, in_progress_text)
        .parse_mode(ParseMode::Html)
        .reply_markup(empty_keyboard)
        .await
    {
        warn!(?source, "failed to edit manual recap waiting message");
    }

    let histories = chat_history::find_by_time_before(
        &context.db,
        data.chat_id,
        chrono::Duration::hours(data.hour),
    )
    .await
    .map_err(ManualCallbackFailure::generation)?;
    if histories.len() <= 5 {
        return Err(ManualCallbackFailure::message(
            insufficient_histories_message(data.hour, data.recap_mode),
        ));
    }
    let chat_type = histories
        .last()
        .map(|history| history.chat_type.as_str())
        .unwrap_or_default();
    let generation = RecapGenerationService::new(
        context.db.clone(),
        context.openai.clone(),
        &context.config.recap_openai,
    )
    .map_err(ManualCallbackFailure::generation)?;
    let mut detailed = generation
        .summarize_group_histories(data.chat_id, chat_type, &histories)
        .await
        .map_err(ManualCallbackFailure::generation)?;
    detailed.summaries.retain(|summary| !summary.is_empty());
    if detailed.summaries.is_empty() {
        return Err(ManualCallbackFailure::generation(anyhow!(
            "manual recap contains no detailed summaries"
        )));
    }

    let log_id = Uuid::parse_str(&detailed.log_id).map_err(ManualCallbackFailure::generation)?;
    let counts = feedback::counts(
        &context.db,
        feedback::ReactionTable::ChatHistoriesRecaps,
        data.chat_id,
        log_id,
    )
    .await
    .map_err(ManualCallbackFailure::generation)?;
    let state = context.recap_state.as_deref().ok_or_else(|| {
        ManualCallbackFailure::generation(anyhow!("manual recap state store is unavailable"))
    })?;
    let vote_keyboard = build_vote_keyboard(state, data.chat_id, &detailed.log_id, counts)
        .await
        .map_err(ManualCallbackFailure::generation)?;

    let actor_name = actor_display_name(
        &callback.from.first_name,
        callback.from.last_name.as_deref().unwrap_or_default(),
        callback.from.username.as_deref().unwrap_or_default(),
    );
    let group_name = group_display_name(&data.chat_title);
    let (condensed_summary, condensed_trace) = match generation
        .generate_condensed(data.chat_id, &histories)
        .await
    {
        Ok(result) => {
            let content = result.content.trim().to_owned();
            let content = if content.is_empty() {
                fallback_condensed_summary(
                    &detailed.summaries,
                    &format!("過去 {} 小時的群組聊天回顧", data.hour),
                )
            } else {
                content
            };
            (content, result.trace)
        }
        Err(source) => {
            warn!(error = ?source.source, "manual recap condensed generation failed");
            (
                fallback_condensed_summary(
                    &detailed.summaries,
                    &format!("過去 {} 小時的群組聊天回顧", data.hour),
                ),
                source.trace,
            )
        }
    };
    let initiator_user_id = i64::try_from(callback.from.id.0).unwrap_or_default();
    let visible_summary = build_rich_recap_summary(&RichRecapSummaryConfig {
        title: &group_name,
        hours: data.hour,
        automatic: false,
        initiator_name: &actor_name,
        initiator_user_id,
        condensed_summary: &condensed_summary,
        general_group_notice: chat_type == "group",
        subscription_chat_title: "",
        condensed_trace: Some(&condensed_trace),
        recap_trace: Some(&detailed.trace),
    });
    let parts = compose_rich_recap_messages(&visible_summary, &detailed.summaries);
    if parts.is_empty() {
        return Err(ManualCallbackFailure::generation(anyhow!(
            "manual recap composition returned no parts"
        )));
    }

    let sender =
        TelegramRecapSender::new(context.raw_telegram_http.clone(), &context.config.telegram);
    let delivery = send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            chat_id: destination_chat_id.0,
            parts,
            reply_to_message_id,
            reply_markup: Some(vote_keyboard),
            allow_sending_without_reply: true,
            ..Default::default()
        },
    )
    .await;

    match &delivery {
        Ok(messages) => info!(
            chat_id = destination_chat_id.0,
            message_count = messages.len(),
            "sent manual Rich recap"
        ),
        Err(failure) => error!(
            error = ?failure.error,
            chat_id = destination_chat_id.0,
            sent_message_count = failure.messages.len(),
            "failed to send manual Rich recap"
        ),
    }
    if let Err(source) = bot
        .delete_message(destination_chat_id, waiting_message_id)
        .await
    {
        warn!(?source, "failed to delete manual recap waiting message");
    }
    delivery
        .map(|_| ())
        .map_err(ManualCallbackFailure::delivery)
}

async fn send_callback_error(
    bot: &Bot,
    chat_id: ChatId,
    reply_to_message_id: i32,
    text: &str,
) -> ResponseResult<Message> {
    let request = bot.send_message(chat_id, text);
    if reply_to_message_id == 0 {
        request.await
    } else {
        request
            .reply_parameters(ReplyParameters::new(MessageId(reply_to_message_id)))
            .await
    }
}

fn escape_html(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
