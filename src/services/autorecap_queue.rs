//! Automatic recap time-capsule wire format, queue lifecycle, and scheduling.

use anyhow::{Context, Result};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use chrono::{DateTime, Days, NaiveDateTime, Timelike, Utc};
use serde::{Deserialize, Serialize};

use crate::redis::recap_state::RecapStateStore;

pub use crate::redis::keys::AUTO_RECAP_QUEUE_KEY;

/// Polling cadence used by the Go timecapsule/v2 digger.
pub const AUTO_RECAP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(1);

const TWO_PER_DAY: &[u32] = &[8, 20];
const THREE_PER_DAY: &[u32] = &[0, 8, 16];
const FOUR_PER_DAY: &[u32] = &[2, 8, 14, 20];

/// Payload stored inside the generic timecapsule/v2 envelope.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AutoRecapCapsule {
    pub chat_id: i64,
}

#[derive(Deserialize, Serialize)]
struct AutoRecapEnvelope {
    payload: AutoRecapCapsule,
}

/// Encode the exact compact, padded standard-Base64 Go queue member.
pub fn encode_auto_recap_member(chat_id: i64) -> String {
    let json = serde_json::to_vec(&AutoRecapEnvelope {
        payload: AutoRecapCapsule { chat_id },
    })
    .expect("serializing an i64-only queue envelope cannot fail");
    STANDARD.encode(json)
}

/// Decode one queue member into its automatic recap payload.
pub fn decode_auto_recap_member(member: &str) -> Result<AutoRecapCapsule> {
    let json = STANDARD
        .decode(member)
        .context("invalid automatic recap member Base64")?;
    let envelope: AutoRecapEnvelope =
        serde_json::from_slice(&json).context("invalid automatic recap member JSON")?;
    Ok(envelope.payload)
}

/// Add or rescore a chat's deterministic queue member.
pub async fn enqueue_auto_recap(
    store: &(impl RecapStateStore + ?Sized),
    chat_id: i64,
    score_ms: i64,
) -> Result<String> {
    let member = encode_auto_recap_member(chat_id);
    store.auto_recap_zadd(&member, score_ms).await?;
    Ok(member)
}

/// Pop at most one due member, remove it before decoding, and return its payload.
pub async fn pop_due_auto_recap(
    store: &(impl RecapStateStore + ?Sized),
    now_ms: i64,
) -> Result<Option<AutoRecapCapsule>> {
    let Some(member) = store.auto_recap_zpop_due(now_ms).await? else {
        return Ok(None);
    };
    store.auto_recap_zrem(&member).await?;
    decode_auto_recap_member(&member).map(Some)
}

/// Go accepts only two, three, or four runs per day and otherwise uses four.
pub const fn effective_auto_recap_rate(rates_per_day: i32) -> i32 {
    match rates_per_day {
        2..=4 => rates_per_day,
        _ => 4,
    }
}

fn schedule_slots(rates_per_day: i32) -> &'static [u32] {
    match effective_auto_recap_rate(rates_per_day) {
        2 => TWO_PER_DAY,
        3 => THREE_PER_DAY,
        _ => FOUR_PER_DAY,
    }
}

/// Next automatic recap slot expressed as Unix milliseconds.
///
/// The configured offset is fixed and therefore has no daylight-saving rules.
/// Only the local hour participates in slot selection; reaching any minute in a
/// target hour means that slot has already passed.
pub fn next_auto_recap_at_ms(
    now_utc_ms: i64,
    timezone_shift_seconds: i64,
    rates_per_day: i32,
) -> i64 {
    let offset_ms = timezone_shift_seconds.saturating_mul(1_000);
    let local_ms = now_utc_ms.saturating_add(offset_ms);
    let local_now = DateTime::<Utc>::from_timestamp_millis(local_ms)
        .expect("automatic recap timestamp must be in chrono's supported range");
    let slots = schedule_slots(rates_per_day);
    let (date, hour) = match slots
        .iter()
        .copied()
        .find(|target| local_now.hour() < *target)
    {
        Some(hour) => (local_now.date_naive(), hour),
        None => (
            local_now
                .date_naive()
                .checked_add_days(Days::new(1))
                .expect("next automatic recap date must be representable"),
            slots[0],
        ),
    };
    let local_due: NaiveDateTime = date
        .and_hms_opt(hour, 0, 0)
        .expect("automatic recap schedule hours are valid");
    local_due
        .and_utc()
        .timestamp_millis()
        .saturating_sub(offset_ms)
}

/// Message-history window paired with the effective daily rate.
pub const fn auto_recap_window_hours(rates_per_day: i32) -> i64 {
    match effective_auto_recap_rate(rates_per_day) {
        2 => 12,
        3 => 8,
        _ => 6,
    }
}
