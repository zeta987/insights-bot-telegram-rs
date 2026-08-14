use serde::{Deserialize, Serialize};
use sqlx::FromRow;

#[allow(dead_code)]
#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct Chat {
    pub id: i64,
    pub title: Option<String>,
    pub username: Option<String>,
    pub kind: Option<String>,
    pub created_at: Option<i64>,
    pub updated_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum MessageKind {
    Text,
    Photo,
    Video,
    Audio,
    Voice,
    Document,
    Sticker,
    Other,
}

impl MessageKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            MessageKind::Text => "text",
            MessageKind::Photo => "photo",
            MessageKind::Video => "video",
            MessageKind::Audio => "audio",
            MessageKind::Voice => "voice",
            MessageKind::Document => "document",
            MessageKind::Sticker => "sticker",
            MessageKind::Other => "other",
        }
    }
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct ChatHistory {
    pub id: i64,
    pub chat_id: i64,
    pub message_id: i64,
    /// Zero when NULL (due to SQLx Any driver limitation).
    pub from_id: i64,
    /// Empty string when NULL (due to SQLx Any driver limitation).
    pub from_full_name: String,
    /// Empty string when NULL (due to SQLx Any driver limitation).
    pub from_username: String,
    pub kind: String,
    /// Empty string when NULL (due to SQLx Any driver limitation).
    pub text: String,
    /// Empty string when NULL (due to SQLx Any driver limitation).
    pub media_url: String,
    pub created_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RecapConfig {
    pub chat_id: i64,
    pub enabled: bool,
    pub auto_recap_enabled: bool,
    pub last_recap_at: Option<i64>,
    pub updated_at: Option<i64>,
    /// Auto-recap frequency: 2, 3, or 4 times per day.
    pub auto_recap_rates_per_day: i32,
    /// Whether to pin auto-recap messages in the group.
    pub pin_auto_recap_message: bool,
    /// Message ID of the last pinned recap (for unpin-before-pin).
    pub last_pinned_message_id: Option<i64>,
}

/// The two Telegram chat types the recap feature is offered in.
pub const RECAP_ELIGIBLE_CHAT_TYPES: [&str; 2] = [CHAT_TYPE_GROUP, CHAT_TYPE_SUPERGROUP];

/// Telegram's wire spelling for a basic group.
pub const CHAT_TYPE_GROUP: &str = "group";
/// Telegram's wire spelling for a supergroup.
pub const CHAT_TYPE_SUPERGROUP: &str = "supergroup";

/// The language a chat falls back to when none was ever stored.
pub const DEFAULT_FEATURE_LANGUAGE: &str = "en";

/// One row of `telegram_chat_feature_flags`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramChatFeatureFlags {
    pub id: String,
    pub chat_id: i64,
    pub chat_type: String,
    pub chat_title: String,
    pub feature_chat_histories_recap: bool,
    pub feature_language: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// Where an automatic recap is delivered.
///
/// The stored column is a plain integer, so an unrecognised value stays
/// readable rather than being coerced into a neighbouring variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AutoRecapSendMode {
    /// Sent into the group itself.
    Publicly,
    /// Sent only to users who subscribed privately.
    OnlyPrivateSubscriptions,
}

impl AutoRecapSendMode {
    /// The integer Go persists for this mode.
    ///
    /// Go's `int` is 64-bit on the pinned production target and ent maps
    /// `field.Int` onto PostgreSQL `bigint`, so the stored width is 64 bits.
    pub fn as_stored(self) -> i64 {
        match self {
            Self::Publicly => 0,
            Self::OnlyPrivateSubscriptions => 1,
        }
    }

    /// The mode a stored integer denotes, or `None` for an unknown value.
    ///
    /// An unrecognised value is reported as `None` rather than coerced, so the
    /// raw column stays readable through
    /// [`TelegramChatRecapsOptions::auto_recap_send_mode`].
    pub fn from_stored(stored: i64) -> Option<Self> {
        match stored {
            0 => Some(Self::Publicly),
            1 => Some(Self::OnlyPrivateSubscriptions),
            _ => None,
        }
    }
}

/// One row of `telegram_chat_recaps_options`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramChatRecapsOptions {
    pub id: String,
    pub chat_id: i64,
    /// Raw stored integer; see [`TelegramChatRecapsOptions::send_mode`].
    pub auto_recap_send_mode: i64,
    pub manual_recap_rate_per_seconds: i64,
    pub auto_recap_rates_per_day: i64,
    pub pin_auto_recap_message: bool,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

impl TelegramChatRecapsOptions {
    /// The recognised send mode, or `None` when the stored integer is unknown.
    pub fn send_mode(&self) -> Option<AutoRecapSendMode> {
        AutoRecapSendMode::from_stored(self.auto_recap_send_mode)
    }
}

/// One row of `telegram_chat_auto_recaps_subscribers`.
///
/// Rows are physical: the schema carries no uniqueness, so two rows may hold the
/// same `(chat_id, user_id)` pair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelegramChatAutoRecapsSubscriber {
    pub id: String,
    pub chat_id: i64,
    pub user_id: i64,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

#[derive(Debug, Clone, FromRow, Serialize, Deserialize)]
pub struct RecapLog {
    pub id: String,
    pub chat_id: i64,
    pub prompt: Option<String>,
    pub recap_text: Option<String>,
    pub model: Option<String>,
    pub prompt_tokens: Option<i32>,
    pub completion_tokens: Option<i32>,
    pub created_at: Option<i64>,
}
