//! Typed Telegram entity compatibility at the message-capture boundary.

use insights_bot_telegram_rs::services::message_capture::{
    CapturedEntityKind, captured_entity_from_teloxide,
};
use serde_json::json;
use teloxide::types::MessageEntity;

fn text_link(raw_url: &str) -> MessageEntity {
    serde_json::from_value(json!({
        "type": "text_link",
        "offset": 3,
        "length": 7,
        "url": raw_url,
    }))
    .expect("valid Telegram text_link entity")
}

#[test]
fn text_link_urls_use_teloxides_whatwg_serialization() {
    for (raw, normalized) in [
        ("https://example.com", "https://example.com/"),
        ("HTTPS://Example.COM/a", "https://example.com/a"),
        ("https://例え.jp/", "https://xn--r8jz45g.jp/"),
    ] {
        let captured = captured_entity_from_teloxide(&text_link(raw));
        assert_eq!(
            captured.kind,
            CapturedEntityKind::TextLink {
                url: normalized.to_string(),
            }
        );
        assert_eq!(captured.offset, 3);
        assert_eq!(captured.length, 7);
    }
}

#[test]
fn url_and_unhandled_entities_keep_their_go_categories() {
    let url: MessageEntity = serde_json::from_value(json!({
        "type": "url",
        "offset": 1,
        "length": 5,
    }))
    .expect("url entity");
    let bold: MessageEntity = serde_json::from_value(json!({
        "type": "bold",
        "offset": 2,
        "length": 4,
    }))
    .expect("bold entity");

    let captured_url = captured_entity_from_teloxide(&url);
    assert_eq!(captured_url.kind, CapturedEntityKind::Url);
    assert_eq!(captured_url.offset, 1);
    assert_eq!(captured_url.length, 5);

    let captured_bold = captured_entity_from_teloxide(&bold);
    assert_eq!(captured_bold.kind, CapturedEntityKind::Other);
    assert_eq!(captured_bold.offset, 2);
    assert_eq!(captured_bold.length, 4);
}
