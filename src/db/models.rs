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

/// Go's `FromPlatformTelegram`, the first value of an `iota` block.
pub const FROM_PLATFORM_TELEGRAM: i64 = 0;
/// Go's `RecapTypeForGroup`.
pub const RECAP_TYPE_FOR_GROUP: i64 = 0;
/// Go's `RecapTypeForPrivateForwarded`.
pub const RECAP_TYPE_FOR_PRIVATE_FORWARDED: i64 = 1;
/// Go's `autoRecapMessage`, the only sent-message type in this port.
pub const MESSAGE_TYPE_AUTO_RECAP: i64 = 0;

/// The three token counters an OpenAI response reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
}

/// One row of `log_chat_histories_recaps`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogChatHistoriesRecap {
    pub id: String,
    pub chat_id: i64,
    pub recap_inputs: String,
    pub recap_outputs: String,
    pub from_platform: i64,
    pub prompt_token_usage: i64,
    pub completion_token_usage: i64,
    pub total_token_usage: i64,
    pub recap_type: i64,
    pub model_name: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// The reaction vocabulary both feedback tables share.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReactionType {
    None,
    UpVote,
    DownVote,
    Lmao,
}

impl ReactionType {
    /// The string Go persists, which the schema `CHECK` also enumerates.
    pub fn as_stored(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::UpVote => "up_vote",
            Self::DownVote => "down_vote",
            Self::Lmao => "lmao",
        }
    }

    /// The reaction a stored string denotes, or `None` for anything else.
    pub fn from_stored(stored: &str) -> Option<Self> {
        match stored {
            "none" => Some(Self::None),
            "up_vote" => Some(Self::UpVote),
            "down_vote" => Some(Self::DownVote),
            "lmao" => Some(Self::Lmao),
            _ => None,
        }
    }
}

/// One physical row of either feedback reaction table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedbackReaction {
    pub id: String,
    pub chat_id: i64,
    pub log_id: String,
    pub user_id: i64,
    /// The raw stored string; see [`FeedbackReaction::reaction`].
    pub reaction_type: String,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

impl FeedbackReaction {
    /// The recognised reaction, or `None` when the stored string is unknown.
    pub fn reaction(&self) -> Option<ReactionType> {
        ReactionType::from_stored(&self.reaction_type)
    }
}

/// The three buttons Go renders under a recap.
///
/// `none` belongs to no bucket, exactly as Go's three filters skip it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReactionCounts {
    pub up_votes: i64,
    pub down_votes: i64,
    pub lmao: i64,
}

/// One row of `sent_messages`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SentMessage {
    pub id: String,
    pub chat_id: i64,
    pub message_id: i64,
    pub text: String,
    pub is_pinned: bool,
    pub from_platform: i64,
    pub message_type: i64,
    /// Unix milliseconds.
    pub created_at: i64,
    /// Unix milliseconds.
    pub updated_at: i64,
}

/// One row of `metric_open_ai_chat_completion_token_usages`.
///
/// The table is append-only and carries no `updated_at`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MetricOpenAiChatCompletionTokenUsage {
    pub id: String,
    pub prompt_operation: String,
    pub prompt_character_length: i64,
    pub prompt_token_usage: i64,
    pub completion_character_length: i64,
    pub completion_token_usage: i64,
    pub total_token_usage: i64,
    pub model_name: String,
    /// Unix milliseconds.
    pub created_at: i64,
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
