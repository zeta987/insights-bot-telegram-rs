//! Automatic recap target delivery, pinning, and sent-message persistence.

use anyhow::{Context, Result};
use async_trait::async_trait;
use teloxide::{
    Bot,
    payloads::UnpinChatMessageSetters,
    prelude::Requester,
    types::{ChatId, InlineKeyboardMarkup, Message, MessageId},
};

use crate::{
    db::{Database, sent_messages},
    services::recap_delivery::{
        BeforeSendHook, RecapDeliveryConfig, RecapDeliveryError, RecapDeliverySender,
        send_rich_recap_parts,
    },
};

/// One destination for an automatic recap.
///
/// Callers set `pin_first` only for the public source chat. Subscriber targets
/// keep it false, even when public recap pinning is enabled.
#[derive(Clone, Default)]
pub struct AutoRecapDeliveryTarget {
    pub chat_id: i64,
    pub parts: Vec<String>,
    pub keyboard: Option<InlineKeyboardMarkup>,
    pub pin_first: bool,
}

/// Observable outcome for one target. Delivery errors stay local to their
/// target, allowing callers and tests to inspect partial progress.
#[derive(Debug)]
pub struct AutoRecapTargetReport {
    pub chat_id: i64,
    pub messages: Vec<Message>,
    pub delivery_error: Option<RecapDeliveryError>,
    pub pin_succeeded: bool,
}

/// Outcomes remain in the exact order supplied by the caller.
#[derive(Debug, Default)]
pub struct AutoRecapDeliveryReport {
    pub targets: Vec<AutoRecapTargetReport>,
}

/// Telegram pin operations used by automatic recap delivery.
#[async_trait]
pub trait AutoRecapPinClient: Send + Sync {
    async fn pin_message(&self, chat_id: i64, message_id: i32) -> Result<()>;
    async fn unpin_message(&self, chat_id: i64, message_id: i32) -> Result<()>;
}

/// Production pin adapter using the same Teloxide bot configured by the
/// automatic recap caller.
#[async_trait]
impl AutoRecapPinClient for Bot {
    async fn pin_message(&self, chat_id: i64, message_id: i32) -> Result<()> {
        self.pin_chat_message(ChatId(chat_id), MessageId(message_id))
            .await
            .context("pin Telegram message")?;
        Ok(())
    }

    async fn unpin_message(&self, chat_id: i64, message_id: i32) -> Result<()> {
        self.unpin_chat_message(ChatId(chat_id))
            .message_id(MessageId(message_id))
            .await
            .context("unpin Telegram message")?;
        Ok(())
    }
}

/// Deliver automatic recap parts to every caller-provided target.
///
/// The shared hook is cloned into every target configuration, so the existing
/// rich delivery primitive invokes it before each rich, plain, and retry send.
pub async fn deliver_auto_recap_targets<S, P, I>(
    db: &Database,
    sender: &S,
    pinner: &P,
    targets: I,
    before_send: Option<BeforeSendHook>,
) -> AutoRecapDeliveryReport
where
    S: RecapDeliverySender + ?Sized,
    P: AutoRecapPinClient + ?Sized,
    I: IntoIterator<Item = AutoRecapDeliveryTarget>,
{
    let mut report = AutoRecapDeliveryReport::default();

    for target in targets {
        let delivery = send_rich_recap_parts(
            sender,
            RecapDeliveryConfig {
                chat_id: target.chat_id,
                parts: target.parts,
                reply_markup: target.keyboard,
                before_send: before_send.clone(),
                ..Default::default()
            },
        )
        .await;

        match delivery {
            Ok(messages) => {
                if messages.is_empty() {
                    tracing::error!(
                        chat_id = target.chat_id,
                        "automatic recap delivered no messages"
                    );
                    report.targets.push(AutoRecapTargetReport {
                        chat_id: target.chat_id,
                        messages,
                        delivery_error: None,
                        pin_succeeded: false,
                    });
                    continue;
                }

                let pin_succeeded = if target.pin_first {
                    replace_pinned_message(db, pinner, target.chat_id, messages[0].id.0).await
                } else {
                    false
                };
                persist_messages(db, &messages, pin_succeeded).await;
                report.targets.push(AutoRecapTargetReport {
                    chat_id: target.chat_id,
                    messages,
                    delivery_error: None,
                    pin_succeeded,
                });
            }
            Err(failure) => {
                tracing::error!(
                    chat_id = target.chat_id,
                    sent_message_count = failure.messages.len(),
                    error = %failure.error,
                    "automatic recap target delivery failed"
                );
                persist_messages(db, &failure.messages, false).await;
                report.targets.push(AutoRecapTargetReport {
                    chat_id: target.chat_id,
                    messages: failure.messages,
                    delivery_error: Some(failure.error),
                    pin_succeeded: false,
                });
            }
        }
    }

    report
}

async fn replace_pinned_message<P>(
    db: &Database,
    pinner: &P,
    chat_id: i64,
    new_message_id: i32,
) -> bool
where
    P: AutoRecapPinClient + ?Sized,
{
    match sent_messages::find_latest_pinned(db, chat_id).await {
        Ok(previous) => {
            match i32::try_from(previous.message_id) {
                Ok(previous_message_id) => {
                    if let Err(error) = pinner.unpin_message(chat_id, previous_message_id).await {
                        tracing::error!(
                            chat_id,
                            message_id = previous.message_id,
                            error = %error,
                            "failed to unpin previous automatic recap message"
                        );
                    }
                }
                Err(error) => {
                    tracing::error!(
                        chat_id,
                        message_id = previous.message_id,
                        error = %error,
                        "previous automatic recap message id does not fit Telegram MessageId"
                    );
                }
            }

            if let Err(error) =
                sent_messages::set_pinned(db, previous.chat_id, previous.message_id, false).await
            {
                tracing::error!(
                    chat_id = previous.chat_id,
                    message_id = previous.message_id,
                    error = %error,
                    "failed to clear previous automatic recap pinned flag"
                );
            }
        }
        Err(error) => {
            tracing::error!(
                chat_id,
                error = %error,
                "failed to find previous automatic recap pinned message"
            );
        }
    }

    match pinner.pin_message(chat_id, new_message_id).await {
        Ok(()) => true,
        Err(error) => {
            tracing::error!(
                chat_id,
                message_id = new_message_id,
                error = %error,
                "failed to pin first automatic recap message"
            );
            false
        }
    }
}

async fn persist_messages(db: &Database, messages: &[Message], pin_first: bool) {
    for (index, message) in messages.iter().enumerate() {
        let is_pinned = pin_first && index == 0;
        let chat_id = message.chat.id.0;
        if let Err(error) = sent_messages::create_auto_recap_message(
            db,
            chat_id,
            i64::from(message.id.0),
            message.text().unwrap_or_default(),
            is_pinned,
        )
        .await
        {
            tracing::error!(
                chat_id,
                message_id = message.id.0,
                error = %error,
                "failed to persist delivered automatic recap message"
            );
        }
    }
}
