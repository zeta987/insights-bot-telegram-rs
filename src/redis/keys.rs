//! Exact Redis key literals and hash codecs for the recap domain.
//!
//! Every literal here is pinned to Go v1.0.0 commit
//! `02aee8ce260165592e2152eb5a024a602e4eced1`. Changing any string in this file
//! silently invalidates callback buttons and deep links that are already live in
//! Telegram, so treat them as a wire format rather than as implementation
//! detail.

use sha2::{Digest, Sha256};

/// Lifetime of a stored callback payload.
pub const CALLBACK_PAYLOAD_TTL_SECONDS: i64 = 86_400;
/// Lifetime of a stored `/start` deep-link context.
pub const START_CONTEXT_TTL_SECONDS: i64 = 86_400;
/// Lifetime of both forwarded-session keys.
pub const FORWARDED_SESSION_TTL_SECONDS: i64 = 7_200;
/// Lifetime of the per-actor delete-later list.
pub const DELETE_LATER_TTL_SECONDS: i64 = 86_400;

/// The only value Go ever writes into the forwarded control key.
pub const FORWARDED_CONTROL_ACTIVE_VALUE: &str = "1";

/// Number of lowercase hex characters kept from the route/action digests.
pub const CALLBACK_ROUTE_HASH_HEX_LEN: usize = 16;
/// Number of lowercase hex characters kept from a start-context digest.
pub const START_CONTEXT_TOKEN_HEX_LEN: usize = 8;

/// Separator between the route hash and the action hash on the Telegram wire.
const CALLBACK_WIRE_SEPARATOR: char = ';';
/// Separator inside a delete-later list member.
const DELETE_LATER_SEPARATOR: char = ';';

const CALLBACK_PAYLOAD_KEY_PREFIX: &str = "callback_query/button_data";
const FORWARDED_KEY_PREFIX: &str = "recap/replay_from_private_message";
const DELETE_LATER_KEY_PREFIX: &str = "session/delete_later_messages_for_actor";
const MANUAL_RECAP_RATE_KEY_PREFIX: &str = "rate_limit/manual_recap/command:/recap/group/Telegram";

pub const ROUTE_SELECT_HOUR: &str = "recap/select-hour";
pub const ROUTE_CONFIGURE_TOGGLE: &str = "recap/configure/toggle";
pub const ROUTE_CONFIGURE_ASSIGN_MODE: &str = "recap/configure/assign_mode";
pub const ROUTE_CONFIGURE_COMPLETE: &str = "recap/configure/complete";
pub const ROUTE_UNSUBSCRIBE_RECAP: &str = "recap/unsubscribe_recap";
pub const ROUTE_RECAP_FEEDBACK_REACT: &str = "recap/recap/feedback/react";
pub const ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY: &str =
    "recap/configure/auto_recap_rates_per_day";
pub const ROUTE_CONFIGURE_PIN: &str = "recap/configure/pin";
/// Compatibility route for recap buttons that were minted by `/smr`.
///
/// `/smr` generation itself is deliberately out of scope; only the feedback
/// callback survives so existing keyboards keep working.
pub const ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT: &str = "smr/summarization/feedback/react";

/// Every callback route literal Go registers, in registration order.
pub const REGISTERED_CALLBACK_ROUTES: [&str; 9] = [
    ROUTE_SELECT_HOUR,
    ROUTE_CONFIGURE_TOGGLE,
    ROUTE_CONFIGURE_ASSIGN_MODE,
    ROUTE_CONFIGURE_COMPLETE,
    ROUTE_UNSUBSCRIBE_RECAP,
    ROUTE_RECAP_FEEDBACK_REACT,
    ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
    ROUTE_CONFIGURE_PIN,
    ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT,
];

const HEX_DIGITS: [u8; 16] = *b"0123456789abcdef";

/// Lowercase hex of the first `hex_len` nibbles of `SHA-256(input)`.
fn sha256_hex_prefix(input: &str, hex_len: usize) -> String {
    let digest = Sha256::digest(input.as_bytes());
    let bytes: &[u8] = digest.as_ref();
    let mut hex = String::with_capacity(hex_len);
    for byte in bytes.iter().take(hex_len.div_ceil(2)) {
        hex.push(char::from(HEX_DIGITS[usize::from(byte >> 4)]));
        hex.push(char::from(HEX_DIGITS[usize::from(byte & 0x0f)]));
    }
    hex.truncate(hex_len);
    hex
}

/// Hash of a callback route literal as it appears on the Telegram wire.
pub fn callback_route_hash(route: &str) -> String {
    sha256_hex_prefix(route, CALLBACK_ROUTE_HASH_HEX_LEN)
}

/// Hash of a callback action payload as it appears on the Telegram wire.
pub fn callback_action_hash(payload_json: &str) -> String {
    sha256_hex_prefix(payload_json, CALLBACK_ROUTE_HASH_HEX_LEN)
}

/// The `<route-hash>;<action-hash>` value carried by an inline button.
pub fn callback_wire_value(route: &str, payload_json: &str) -> String {
    format!(
        "{}{CALLBACK_WIRE_SEPARATOR}{}",
        callback_route_hash(route),
        callback_action_hash(payload_json)
    )
}

/// Redis key of a stored callback payload.
///
/// The key embeds the *literal* route while the wire carries the route hash.
pub fn callback_payload_key(route: &str, action_hash: &str) -> String {
    format!("{CALLBACK_PAYLOAD_KEY_PREFIX}/{route}/{action_hash}")
}

/// Split a wire value into its route and action hashes.
///
/// Anything other than exactly two separator-delimited segments is malformed.
pub fn decode_callback_wire(wire: &str) -> Option<(&str, &str)> {
    let mut segments = wire.split(CALLBACK_WIRE_SEPARATOR);
    let route_hash = segments.next()?;
    let action_hash = segments.next()?;
    if segments.next().is_some() {
        return None;
    }
    Some((route_hash, action_hash))
}

/// Map a wire route hash back to its registered literal.
pub fn resolve_callback_route(route_hash: &str) -> Option<&'static str> {
    REGISTERED_CALLBACK_ROUTES
        .into_iter()
        .find(|route| callback_route_hash(route) == route_hash)
}

/// The two `/start` deep-link context families.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum StartContextDomain {
    /// `/recap` in private-subscription mode.
    PrivateSubscription,
    /// `/subscribe_recap`.
    SubscribeRecap,
}

impl StartContextDomain {
    /// Prefix Go hashes to derive the deep-link token.
    ///
    /// The private domain hashes `private_subscription_mode` while storing under
    /// `private_subscription`. That mismatch is part of the released wire format
    /// and is reproduced deliberately.
    pub fn token_source_prefix(self) -> &'static str {
        match self {
            Self::PrivateSubscription => "recap/private_subscription_mode/start_command_context",
            Self::SubscribeRecap => "recap/subscribe_recap/start_command_context",
        }
    }

    /// Prefix of the Redis key that holds the serialized context.
    pub fn key_prefix(self) -> &'static str {
        match self {
            Self::PrivateSubscription => "recap/private_subscription/start_command_context",
            Self::SubscribeRecap => "recap/subscribe_recap/start_command_context",
        }
    }

    /// The exact string Go feeds into SHA-256 for this chat.
    pub fn token_source(self, chat_id: i64) -> String {
        format!("{}/{chat_id}", self.token_source_prefix())
    }

    /// The deep-link token for this chat.
    pub fn token(self, chat_id: i64) -> String {
        sha256_hex_prefix(&self.token_source(chat_id), START_CONTEXT_TOKEN_HEX_LEN)
    }

    /// Redis key holding the context for `token`.
    pub fn key(self, token: &str) -> String {
        format!("{}/{token}", self.key_prefix())
    }
}

/// Redis key of the forwarded-session control flag for `user_id`.
pub fn forwarded_control_key(user_id: i64) -> String {
    format!("{FORWARDED_KEY_PREFIX}/{user_id}")
}

/// Redis key of the forwarded-session replay batch for `user_id`.
pub fn forwarded_batch_key(user_id: i64) -> String {
    format!("{FORWARDED_KEY_PREFIX}/{user_id}/batch")
}

/// Redis key of the delete-later list for `user_id`.
pub fn delete_later_key(user_id: i64) -> String {
    format!("{DELETE_LATER_KEY_PREFIX}/{user_id}")
}

/// Go's per-group `/recap` command counter key.
pub fn manual_recap_rate_key(chat_id: i64) -> String {
    format!("{MANUAL_RECAP_RATE_KEY_PREFIX}/{chat_id}")
}

/// A `<chat-id>;<message-id>` delete-later list member.
pub fn delete_later_member(chat_id: i64, message_id: i32) -> String {
    format!("{chat_id}{DELETE_LATER_SEPARATOR}{message_id}")
}

/// Parse a delete-later member, rejecting anything Go would skip.
///
/// Go skips a member that does not split into exactly two segments, that fails
/// to parse, or whose chat or message identifier is zero.
pub fn parse_delete_later_member(raw: &str) -> Option<(i64, i32)> {
    let mut segments = raw.split(DELETE_LATER_SEPARATOR);
    let chat_id = segments.next()?.parse::<i64>().ok()?;
    let message_id = segments.next()?.parse::<i32>().ok()?;
    if segments.next().is_some() {
        return None;
    }
    if chat_id == 0 || message_id == 0 {
        return None;
    }
    Some((chat_id, message_id))
}
