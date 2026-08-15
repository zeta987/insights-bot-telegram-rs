//! `/configure_recap` keyboard parity against Go v1.0.0 `02aee8ce`.

use std::sync::Arc;

use insights_bot_telegram_rs::{
    bot::handlers::recap_configure::{ConfigureRecapView, build_configure_keyboard},
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
};
use serde_json::Value;

const START_MS: i64 = 1_700_000_000_000;
const CHAT_ID: i64 = -1_001_234_567_890;
const FROM_ID: i64 = 42;

async fn stored_payload(state: &InMemoryRecapStateStore, route: &str, button: &Value) -> String {
    let wire = button["callback_data"]
        .as_str()
        .expect("callback wire value");
    let (route_hash, action_hash) = keys::decode_callback_wire(wire).expect("hashed callback");
    assert_eq!(route_hash, keys::callback_route_hash(route));
    state
        .get_callback(route, action_hash)
        .await
        .expect("callback lookup")
        .expect("stored callback payload")
}

#[tokio::test]
async fn configure_keyboard_preserves_go_rows_labels_and_compact_callback_json() {
    let state = InMemoryRecapStateStore::new(Arc::new(TestClock::new(START_MS)));

    let disabled = build_configure_keyboard(
        &state,
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: false,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("disabled keyboard");
    let disabled = serde_json::to_value(disabled).expect("serialize disabled keyboard");
    let disabled_rows = disabled["inline_keyboard"]
        .as_array()
        .expect("disabled rows");
    assert_eq!(disabled_rows.len(), 5);
    assert_eq!(
        disabled_rows
            .iter()
            .map(|row| {
                row.as_array()
                    .expect("button row")
                    .iter()
                    .map(|button| button["text"].as_str().expect("button text"))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
        vec![
            vec!["🔈 聊天记录回顾"],
            vec!["开启", "🔘 关闭"],
            vec!["📩 聊天记录回顾投递方式"],
            vec!["🔘 公开", "私聊"],
            vec!["✅ 完成"],
        ]
    );
    assert_eq!(
        stored_payload(&state, keys::ROUTE_CONFIGURE_TOGGLE, &disabled_rows[1][0]).await,
        r#"{"status":true,"chatId":-1001234567890,"fromId":42}"#
    );
    assert_eq!(
        stored_payload(
            &state,
            keys::ROUTE_CONFIGURE_ASSIGN_MODE,
            &disabled_rows[3][1]
        )
        .await,
        r#"{"mode":1,"chatId":-1001234567890,"fromId":42}"#
    );
    assert_eq!(
        stored_payload(&state, keys::ROUTE_CONFIGURE_COMPLETE, &disabled_rows[4][0]).await,
        r#"{"chatId":-1001234567890,"fromId":42}"#
    );
    assert_eq!(
        state.raw_string(&keys::callback_payload_key(
            "nop",
            &keys::callback_action_hash(r#""""#),
        )),
        Some(r#""""#.to_owned())
    );

    let enabled = build_configure_keyboard(
        &state,
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 1,
            rates_per_day: 3,
            pin_enabled: true,
        },
    )
    .await
    .expect("enabled keyboard");
    let enabled = serde_json::to_value(enabled).expect("serialize enabled keyboard");
    assert_eq!(
        enabled["inline_keyboard"]
            .as_array()
            .expect("enabled rows")
            .iter()
            .map(|row| {
                row.as_array()
                    .expect("button row")
                    .iter()
                    .map(|button| button["text"].as_str().expect("button text"))
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>(),
        vec![
            vec!["🔈 聊天记录回顾"],
            vec!["🔘 开启", "关闭"],
            vec!["📩 聊天记录回顾投递方式"],
            vec!["公开", "🔘 私聊"],
            vec!["🛎️ 每天自动创建回顾次数"],
            vec!["2 次", "🔘 3 次", "4 次"],
            vec!["🪧 置顶聊天记录回顾"],
            vec!["🔘 开启", "关闭"],
            vec!["✅ 完成"],
        ]
    );
    let enabled_rows = enabled["inline_keyboard"].as_array().expect("enabled rows");
    assert_eq!(
        stored_payload(
            &state,
            keys::ROUTE_CONFIGURE_AUTO_RECAP_RATES_PER_DAY,
            &enabled_rows[5][2],
        )
        .await,
        r#"{"rates":4,"chatId":-1001234567890,"fromId":42}"#
    );
    assert_eq!(
        stored_payload(&state, keys::ROUTE_CONFIGURE_PIN, &enabled_rows[7][0]).await,
        r#"{"status":true,"chatId":-1001234567890}"#
    );
}
