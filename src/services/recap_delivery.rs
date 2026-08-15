//! Go v1.0.0 Rich recap delivery and deterministic plain-text fallback.

use std::{fmt, future::Future, pin::Pin, sync::Arc};

use async_trait::async_trait;
use teloxide::types::{InlineKeyboardMarkup, Message};

use crate::{
    config::TelegramConfig,
    services::{
        rich_recap::{rich_markdown_to_plain_text, split_plain_text},
        telegram_rich_message::{
            PlainMessageRequest as TelegramPlainMessageRequest, RichMessageReplyParameters,
            RichMessageRequest, TelegramRichMessageClient, TelegramRichMessageError,
        },
    },
};

/// Telegram's plain `sendMessage` limit measured in UTF-16 code units.
pub const PLAIN_MESSAGE_UTF16_UNIT_LIMIT: usize = 4_096;

pub type BeforeSendFuture = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub type BeforeSendHook = Arc<dyn Fn() -> BeforeSendFuture + Send + Sync>;

#[derive(Clone, Default)]
pub struct RecapDeliveryConfig {
    pub chat_id: i64,
    pub parts: Vec<String>,
    pub reply_to_message_id: i32,
    pub reply_markup: Option<InlineKeyboardMarkup>,
    pub disable_notification: bool,
    pub allow_sending_without_reply: bool,
    pub before_send: Option<BeforeSendHook>,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct RichRecapSendRequest {
    pub chat_id: i64,
    pub markdown: String,
    pub reply_parameters: Option<RichMessageReplyParameters>,
    pub reply_markup: Option<InlineKeyboardMarkup>,
    pub disable_notification: bool,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct PlainRecapSendRequest {
    pub chat_id: i64,
    pub text: String,
    pub reply_to_message_id: i32,
    pub reply_markup: Option<InlineKeyboardMarkup>,
    pub disable_notification: bool,
    pub allow_sending_without_reply: bool,
}

#[async_trait]
pub trait RecapDeliverySender: Send + Sync {
    async fn send_rich(
        &self,
        request: RichRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError>;

    async fn send_plain(
        &self,
        request: PlainRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError>;
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum RecapDeliveryError {
    RichPart {
        part_number: usize,
        source: TelegramRichMessageError,
    },
    NoPlainFallback {
        part_number: usize,
        source: Option<TelegramRichMessageError>,
    },
    PlainFallbackPart {
        part_number: usize,
        plain_part_number: usize,
        source: TelegramRichMessageError,
    },
}

impl fmt::Display for RecapDeliveryError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::RichPart {
                part_number,
                source,
            } => write!(formatter, "send rich recap part {part_number}: {source}"),
            Self::NoPlainFallback {
                part_number,
                source: Some(source),
            } => write!(
                formatter,
                "rich recap part {part_number} has no plain-text fallback: {source}"
            ),
            Self::NoPlainFallback {
                part_number,
                source: None,
            } => write!(
                formatter,
                "rich recap part {part_number} has no plain-text fallback"
            ),
            Self::PlainFallbackPart {
                part_number,
                plain_part_number,
                source,
            } => write!(
                formatter,
                "send plain recap fallback part {part_number}.{plain_part_number}: {source}"
            ),
        }
    }
}

impl std::error::Error for RecapDeliveryError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::RichPart { source, .. } | Self::PlainFallbackPart { source, .. } => Some(source),
            Self::NoPlainFallback {
                source: Some(source),
                ..
            } => Some(source),
            Self::NoPlainFallback { source: None, .. } => None,
        }
    }
}

#[derive(Debug)]
pub struct RecapDeliveryFailure {
    pub messages: Vec<Message>,
    pub error: RecapDeliveryError,
}

impl fmt::Display for RecapDeliveryFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for RecapDeliveryFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

/// Send every Rich Markdown part and permanently switch to plain text after
/// the first formatting-specific or message-too-long Telegram 400 response.
pub async fn send_rich_recap_parts<S>(
    sender: &S,
    config: RecapDeliveryConfig,
) -> Result<Vec<Message>, RecapDeliveryFailure>
where
    S: RecapDeliverySender + ?Sized,
{
    let mut messages = Vec::with_capacity(config.parts.len());
    let mut plain_mode = false;

    for (part_index, part) in config.parts.iter().enumerate() {
        let part_number = part_index + 1;
        let mut fallback_error = None;

        if !plain_mode {
            let reply_to_message_id = continuation_reply_id(&messages, config.reply_to_message_id);
            let reply_parameters =
                (reply_to_message_id != 0).then_some(RichMessageReplyParameters {
                    message_id: reply_to_message_id,
                    allow_sending_without_reply: config.allow_sending_without_reply,
                    ..Default::default()
                });
            let request = RichRecapSendRequest {
                chat_id: config.chat_id,
                markdown: part.clone(),
                reply_parameters,
                reply_markup: (part_index == 0)
                    .then(|| config.reply_markup.clone())
                    .flatten(),
                disable_notification: config.disable_notification,
            };

            invoke_before_send(&config).await;
            match sender.send_rich(request).await {
                Ok(message) => {
                    messages.push(message);
                    continue;
                }
                Err(error)
                    if is_rich_message_formatting_error(&error)
                        || is_message_too_long_error(&error) =>
                {
                    fallback_error = Some(error);
                    plain_mode = true;
                }
                Err(source) => {
                    return Err(RecapDeliveryFailure {
                        messages,
                        error: RecapDeliveryError::RichPart {
                            part_number,
                            source,
                        },
                    });
                }
            }
        }

        let plain = rich_markdown_to_plain_text(part);
        let plain_parts = split_plain_text(&plain, PLAIN_MESSAGE_UTF16_UNIT_LIMIT);
        if plain_parts.is_empty() {
            return Err(RecapDeliveryFailure {
                messages,
                error: RecapDeliveryError::NoPlainFallback {
                    part_number,
                    source: fallback_error,
                },
            });
        }

        for (plain_index, plain_part) in plain_parts.into_iter().enumerate() {
            let with_markup = part_index == 0 && plain_index == 0;
            if let Err(error) = send_plain_recap_part(
                sender,
                &config,
                &mut messages,
                plain_part,
                PLAIN_MESSAGE_UTF16_UNIT_LIMIT,
                with_markup,
                part_number,
                plain_index + 1,
            )
            .await
            {
                return Err(RecapDeliveryFailure { messages, error });
            }
        }
    }

    Ok(messages)
}

#[derive(Debug)]
struct PendingPlainPart {
    text: String,
    limit: usize,
    with_markup: bool,
}

#[allow(clippy::too_many_arguments)]
async fn send_plain_recap_part<S>(
    sender: &S,
    config: &RecapDeliveryConfig,
    messages: &mut Vec<Message>,
    text: String,
    limit: usize,
    with_markup: bool,
    part_number: usize,
    plain_part_number: usize,
) -> Result<(), RecapDeliveryError>
where
    S: RecapDeliverySender + ?Sized,
{
    let mut pending = vec![PendingPlainPart {
        text,
        limit,
        with_markup,
    }];

    while let Some(part) = pending.pop() {
        let request = PlainRecapSendRequest {
            chat_id: config.chat_id,
            text: part.text.clone(),
            reply_to_message_id: continuation_reply_id(messages, config.reply_to_message_id),
            reply_markup: part
                .with_markup
                .then(|| config.reply_markup.clone())
                .flatten(),
            disable_notification: config.disable_notification,
            allow_sending_without_reply: config.allow_sending_without_reply,
        };

        invoke_before_send(config).await;
        match sender.send_plain(request).await {
            Ok(message) => messages.push(message),
            Err(source) => {
                let halved_limit = part.limit / 2;
                if !is_message_too_long_error(&source) || halved_limit < 1 {
                    return Err(RecapDeliveryError::PlainFallbackPart {
                        part_number,
                        plain_part_number,
                        source,
                    });
                }

                let pieces = split_plain_text(&part.text, halved_limit);
                if pieces.len() < 2 {
                    return Err(RecapDeliveryError::PlainFallbackPart {
                        part_number,
                        plain_part_number,
                        source,
                    });
                }

                for (piece_index, piece) in pieces.into_iter().enumerate().rev() {
                    pending.push(PendingPlainPart {
                        text: piece,
                        limit: halved_limit,
                        with_markup: part.with_markup && piece_index == 0,
                    });
                }
            }
        }
    }

    Ok(())
}

async fn invoke_before_send(config: &RecapDeliveryConfig) {
    if let Some(before_send) = &config.before_send {
        before_send().await;
    }
}

fn continuation_reply_id(messages: &[Message], initial_reply_id: i32) -> i32 {
    messages
        .first()
        .map_or(initial_reply_id, |message| message.id.0)
}

fn is_message_too_long_error(error: &TelegramRichMessageError) -> bool {
    api_error_description(error).is_some_and(|(code, description)| {
        code == 400
            && description
                .to_ascii_lowercase()
                .contains("message is too long")
    })
}

fn is_rich_message_formatting_error(error: &TelegramRichMessageError) -> bool {
    let Some((400, description)) = api_error_description(error) else {
        return false;
    };
    let message = description.to_ascii_lowercase();

    for payload_signature in [
        "reply markup",
        "reply_markup",
        "reply parameters",
        "reply_parameters",
    ] {
        if message.contains(payload_signature) {
            return false;
        }
    }

    for signature in [
        "can't parse entities",
        "cannot parse entities",
        "can't find end of the entity",
        "unsupported start tag",
        "can't parse markdown",
        "cannot parse markdown",
        "failed to parse markdown",
        "invalid markdown",
    ] {
        if message.contains(signature) {
            return true;
        }
    }

    let rich_context = message.contains("rich message")
        || message.contains("rich_message")
        || message.contains("inputrichmessage");
    if !rich_context {
        return false;
    }

    [
        "can't parse rich message",
        "cannot parse rich message",
        "failed to parse rich message",
        "invalid rich message",
        "rich message is invalid",
        "rich message format",
        "rich message block",
        "rich message nesting",
        "rich message is too long",
    ]
    .iter()
    .any(|signature| message.contains(signature))
}

fn api_error_description(error: &TelegramRichMessageError) -> Option<(i32, &str)> {
    match error {
        TelegramRichMessageError::Api {
            code, description, ..
        } => Some((*code, description)),
        TelegramRichMessageError::RequestEncoding
        | TelegramRichMessageError::Transport
        | TelegramRichMessageError::InvalidResponse => None,
    }
}

/// Production adapter that keeps Rich and plain recap requests on one token,
/// endpoint, proxy configuration, and HTTP connection pool.
#[derive(Clone)]
pub struct TelegramRecapSender {
    telegram: TelegramRichMessageClient,
}

impl TelegramRecapSender {
    #[must_use]
    pub fn new(http: reqwest::Client, config: &TelegramConfig) -> Self {
        Self {
            telegram: TelegramRichMessageClient::new(http, config),
        }
    }
}

#[async_trait]
impl RecapDeliverySender for TelegramRecapSender {
    async fn send_rich(
        &self,
        request: RichRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError> {
        self.telegram
            .send(RichMessageRequest {
                chat_id: request.chat_id,
                markdown: &request.markdown,
                reply_parameters: request.reply_parameters,
                reply_markup: request.reply_markup.as_ref(),
                disable_notification: request.disable_notification,
            })
            .await
    }

    async fn send_plain(
        &self,
        request: PlainRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError> {
        self.telegram
            .send_plain(TelegramPlainMessageRequest {
                chat_id: request.chat_id,
                text: &request.text,
                parse_mode: None,
                reply_to_message_id: request.reply_to_message_id,
                reply_markup: request.reply_markup.as_ref(),
                disable_notification: request.disable_notification,
                allow_sending_without_reply: request.allow_sending_without_reply,
            })
            .await
    }
}
