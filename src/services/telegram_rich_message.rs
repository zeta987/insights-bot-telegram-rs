//! Raw Telegram transport used by Go v1.0.0 Rich recap delivery.

use std::fmt;

use serde::{Deserialize, Serialize};
use teloxide::types::{InlineKeyboardMarkup, Message};
use url::Url;

use crate::config::TelegramConfig;

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Serialize)]
pub struct RichMessageReplyParameters {
    pub message_id: i32,
    #[serde(skip_serializing_if = "is_zero_i64")]
    pub chat_id: i64,
    #[serde(skip_serializing_if = "is_false")]
    pub allow_sending_without_reply: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct RichMessageRequest<'a> {
    pub chat_id: i64,
    pub markdown: &'a str,
    pub reply_parameters: Option<RichMessageReplyParameters>,
    pub reply_markup: Option<&'a InlineKeyboardMarkup>,
    pub disable_notification: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct PlainMessageRequest<'a> {
    pub chat_id: i64,
    pub text: &'a str,
    pub parse_mode: Option<&'a str>,
    pub reply_to_message_id: i32,
    pub reply_markup: Option<&'a InlineKeyboardMarkup>,
    pub disable_notification: bool,
    pub allow_sending_without_reply: bool,
}

#[derive(Debug, Clone, Copy, Default, Eq, PartialEq, Deserialize)]
pub struct TelegramResponseParameters {
    #[serde(default)]
    pub migrate_to_chat_id: i64,
    #[serde(default)]
    pub retry_after: i64,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub enum TelegramRichMessageError {
    Api {
        code: i32,
        description: String,
        parameters: TelegramResponseParameters,
    },
    RequestEncoding,
    Transport,
    InvalidResponse,
}

impl fmt::Display for TelegramRichMessageError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Api { description, .. } => formatter.write_str(description),
            Self::RequestEncoding => {
                formatter.write_str("Telegram Rich Message request is invalid")
            }
            Self::Transport => formatter.write_str("Telegram Rich Message request failed"),
            Self::InvalidResponse => {
                formatter.write_str("Telegram Rich Message response is invalid")
            }
        }
    }
}

impl std::error::Error for TelegramRichMessageError {}

#[derive(Clone)]
pub struct TelegramRichMessageClient {
    http: reqwest::Client,
    rich_endpoint: Url,
    plain_endpoint: Url,
}

impl TelegramRichMessageClient {
    #[must_use]
    pub fn new(http: reqwest::Client, config: &TelegramConfig) -> Self {
        Self {
            http,
            rich_endpoint: telegram_method_endpoint(config, "sendRichMessage"),
            plain_endpoint: telegram_method_endpoint(config, "sendMessage"),
        }
    }

    pub async fn send(
        &self,
        request: RichMessageRequest<'_>,
    ) -> Result<Message, TelegramRichMessageError> {
        let mut form = Vec::with_capacity(5);
        if request.chat_id != 0 {
            form.push(("chat_id", request.chat_id.to_string()));
        }
        form.push((
            "rich_message",
            serde_json::to_string(&InputRichMessage {
                markdown: request.markdown,
            })
            .map_err(|_| TelegramRichMessageError::RequestEncoding)?,
        ));
        if let Some(reply_parameters) = request.reply_parameters {
            form.push((
                "reply_parameters",
                serde_json::to_string(&reply_parameters)
                    .map_err(|_| TelegramRichMessageError::RequestEncoding)?,
            ));
        }
        if let Some(reply_markup) = request.reply_markup {
            form.push((
                "reply_markup",
                serde_json::to_string(reply_markup)
                    .map_err(|_| TelegramRichMessageError::RequestEncoding)?,
            ));
        }
        if request.disable_notification {
            form.push(("disable_notification", "true".to_owned()));
        }

        self.send_form(&self.rich_endpoint, &form).await
    }

    pub async fn send_plain(
        &self,
        request: PlainMessageRequest<'_>,
    ) -> Result<Message, TelegramRichMessageError> {
        let mut form = Vec::with_capacity(7);
        if request.chat_id != 0 {
            form.push(("chat_id", request.chat_id.to_string()));
        }
        if !request.text.is_empty() {
            form.push(("text", request.text.to_owned()));
        }
        if let Some(parse_mode) = request.parse_mode {
            form.push(("parse_mode", parse_mode.to_owned()));
        }
        if request.reply_to_message_id != 0 {
            form.push((
                "reply_to_message_id",
                request.reply_to_message_id.to_string(),
            ));
        }
        if let Some(reply_markup) = request.reply_markup {
            form.push((
                "reply_markup",
                serde_json::to_string(reply_markup)
                    .map_err(|_| TelegramRichMessageError::RequestEncoding)?,
            ));
        }
        if request.disable_notification {
            form.push(("disable_notification", "true".to_owned()));
        }
        if request.allow_sending_without_reply {
            form.push(("allow_sending_without_reply", "true".to_owned()));
        }

        self.send_form(&self.plain_endpoint, &form).await
    }

    async fn send_form(
        &self,
        endpoint: &Url,
        form: &[(&str, String)],
    ) -> Result<Message, TelegramRichMessageError> {
        let response = self
            .http
            .post(endpoint.clone())
            .form(form)
            .send()
            .await
            .map_err(|_| TelegramRichMessageError::Transport)?;
        let status = response.status();
        let body = response
            .bytes()
            .await
            .map_err(|_| TelegramRichMessageError::Transport)?;
        let envelope = serde_json::from_slice::<TelegramResponse>(&body)
            .map_err(|_| TelegramRichMessageError::InvalidResponse)?;

        if !envelope.ok {
            return Err(TelegramRichMessageError::Api {
                code: envelope
                    .error_code
                    .unwrap_or_else(|| i32::from(status.as_u16())),
                description: envelope
                    .description
                    .unwrap_or_else(|| "Telegram API request failed".to_owned()),
                parameters: envelope.parameters.unwrap_or_default(),
            });
        }

        let result = envelope
            .result
            .ok_or(TelegramRichMessageError::InvalidResponse)?;
        serde_json::from_value(result).map_err(|_| TelegramRichMessageError::InvalidResponse)
    }
}

#[derive(Serialize)]
struct InputRichMessage<'a> {
    markdown: &'a str,
}

#[derive(Deserialize)]
struct TelegramResponse {
    ok: bool,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error_code: Option<i32>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    parameters: Option<TelegramResponseParameters>,
}

fn telegram_method_endpoint(config: &TelegramConfig, method: &str) -> Url {
    let mut endpoint = Url::parse(config.api_base())
        .expect("Telegram API endpoint was validated during configuration loading");
    endpoint
        .path_segments_mut()
        .expect("HTTP(S) Telegram endpoints can contain path segments")
        .pop_if_empty()
        .push(&format!("bot{}", config.bot_token))
        .push(method);
    endpoint
}

const fn is_zero_i64(value: &i64) -> bool {
    *value == 0
}

const fn is_false(value: &bool) -> bool {
    !*value
}
