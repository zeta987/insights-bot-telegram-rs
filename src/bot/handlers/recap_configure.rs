//! Go v1.0.0 `/configure_recap` command and callback presentation primitives.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};

use crate::{
    bot::handlers::recap_manual::to_go_json,
    redis::{keys, recap_state::RecapStateStore},
};

const ROUTE_NOP: &str = "nop";

/// Current values rendered by Go's `/configure_recap` keyboard.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConfigureRecapView {
    pub chat_id: i64,
    pub from_id: i64,
    pub recap_enabled: bool,
    pub send_mode: i64,
    pub rates_per_day: i64,
    pub pin_enabled: bool,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct ToggleActionData {
    status: bool,
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct AssignModeActionData {
    mode: i64,
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct CompleteActionData {
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct RatesActionData {
    rates: i64,
    #[serde(rename = "chatId")]
    chat_id: i64,
    #[serde(rename = "fromId")]
    from_id: i64,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
struct PinActionData {
    status: bool,
    #[serde(rename = "chatId")]
    chat_id: i64,
}

async fn callback<T>(
    state: &(impl RecapStateStore + ?Sized),
    route: &str,
    data: &T,
) -> Result<String>
where
    T: Serialize,
{
    state.put_callback(route, &to_go_json(data)?).await
}

/// Build Go's five-row disabled or nine-row enabled configuration keyboard.
///
/// Callback payload field order and compact camel-case JSON are wire format:
/// changing either changes the SHA-256 action hash shown to Telegram.
pub async fn build_configure_keyboard(
    state: &(impl RecapStateStore + ?Sized),
    view: ConfigureRecapView,
) -> Result<InlineKeyboardMarkup> {
    let nop = state.put_callback(ROUTE_NOP, r#""""#).await?;
    let toggle_on = callback(
        state,
        keys::ROUTE_CONFIGURE_TOGGLE,
        &ToggleActionData {
            status: true,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let toggle_off = callback(
        state,
        keys::ROUTE_CONFIGURE_TOGGLE,
        &ToggleActionData {
            status: false,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let public = callback(
        state,
        keys::ROUTE_CONFIGURE_ASSIGN_MODE,
        &AssignModeActionData {
            mode: 0,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let private = callback(
        state,
        keys::ROUTE_CONFIGURE_ASSIGN_MODE,
        &AssignModeActionData {
            mode: 1,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let complete = callback(
        state,
        keys::ROUTE_CONFIGURE_COMPLETE,
        &CompleteActionData {
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    // Go allocates pin callbacks before returning its disabled keyboard even
    // though those buttons are not visible in that state.
    let pin_on = callback(
        state,
        keys::ROUTE_CONFIGURE_PIN,
        &PinActionData {
            status: true,
            chat_id: view.chat_id,
        },
    )
    .await?;
    let pin_off = callback(
        state,
        keys::ROUTE_CONFIGURE_PIN,
        &PinActionData {
            status: false,
            chat_id: view.chat_id,
        },
    )
    .await?;

    let selected = |selected: bool, label: &str| {
        if selected {
            format!("🔘 {label}")
        } else {
            label.to_owned()
        }
    };
    let mut rows = vec![
        vec![InlineKeyboardButton::callback(
            "🔈 聊天记录回顾",
            nop.clone(),
        )],
        vec![
            InlineKeyboardButton::callback(selected(view.recap_enabled, "开启"), toggle_on),
            InlineKeyboardButton::callback(selected(!view.recap_enabled, "关闭"), toggle_off),
        ],
        vec![InlineKeyboardButton::callback(
            "📩 聊天记录回顾投递方式",
            nop.clone(),
        )],
        vec![
            InlineKeyboardButton::callback(selected(view.send_mode == 0, "公开"), public),
            InlineKeyboardButton::callback(selected(view.send_mode == 1, "私聊"), private),
        ],
    ];
    if !view.recap_enabled {
        rows.push(vec![InlineKeyboardButton::callback("✅ 完成", complete)]);
        return Ok(InlineKeyboardMarkup::new(rows));
    }

    let rate_two = callback(
        state,
        keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
        &RatesActionData {
            rates: 2,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let rate_three = callback(
        state,
        keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
        &RatesActionData {
            rates: 3,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    let rate_four = callback(
        state,
        keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
        &RatesActionData {
            rates: 4,
            chat_id: view.chat_id,
            from_id: view.from_id,
        },
    )
    .await?;
    rows.extend([
        vec![InlineKeyboardButton::callback(
            "🛎️ 每天自动创建回顾次数",
            nop.clone(),
        )],
        vec![
            InlineKeyboardButton::callback(selected(view.rates_per_day == 2, "2 次"), rate_two),
            InlineKeyboardButton::callback(selected(view.rates_per_day == 3, "3 次"), rate_three),
            InlineKeyboardButton::callback(selected(view.rates_per_day == 4, "4 次"), rate_four),
        ],
        vec![InlineKeyboardButton::callback("🪧 置顶聊天记录回顾", nop)],
        vec![
            InlineKeyboardButton::callback(selected(view.pin_enabled, "开启"), pin_on),
            InlineKeyboardButton::callback(selected(!view.pin_enabled, "关闭"), pin_off),
        ],
        vec![InlineKeyboardButton::callback("✅ 完成", complete)],
    ]);
    Ok(InlineKeyboardMarkup::new(rows))
}
