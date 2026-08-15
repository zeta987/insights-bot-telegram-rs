use std::collections::HashMap;

use insights_bot_telegram_rs::{
    config::TelegramConfig,
    services::telegram_rich_message::{
        PlainMessageRequest, RichMessageReplyParameters, RichMessageRequest,
        TelegramResponseParameters, TelegramRichMessageClient, TelegramRichMessageError,
    },
};
use serde_json::Value;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::any};

fn telegram_config(server: &MockServer) -> TelegramConfig {
    TelegramConfig {
        bot_token: "test-token".to_owned(),
        api_endpoint: format!("{}/telegram-proxy", server.uri()),
        webhook_url: None,
        webhook_port: None,
    }
}

fn decoded_form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body)
        .into_owned()
        .collect::<HashMap<_, _>>()
}

#[tokio::test]
async fn raw_rich_message_preserves_endpoint_prefix_and_decodes_the_message() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"message_id":42,"date":1710000000,"chat":{"id":-100123,"type":"supergroup"}}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));

    let message = client
        .send(RichMessageRequest {
            chat_id: -100123,
            markdown: "# Recap\n\nRich details",
            ..Default::default()
        })
        .await
        .expect("Telegram success response should decode");

    assert_eq!(message.id.0, 42);
    assert_eq!(message.chat.id.0, -100123);
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain the request");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.path(),
        "/telegram-proxy/bottest-token/sendRichMessage"
    );
    let form = decoded_form(&requests[0].body);
    assert_eq!(form.len(), 2);
    assert_eq!(form.get("chat_id").map(String::as_str), Some("-100123"));
    assert_eq!(
        serde_json::from_str::<Value>(&form["rich_message"]).expect("rich message JSON"),
        serde_json::json!({"markdown": "# Recap\n\nRich details"})
    );
    assert!(!form.contains_key("reply_parameters"));
    assert!(!form.contains_key("reply_markup"));
    assert!(!form.contains_key("disable_notification"));
}

#[tokio::test]
async fn raw_rich_message_encodes_reply_markup_and_notification_options() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"message_id":43,"date":1710000000,"chat":{"id":-100123,"type":"supergroup"}}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));
    let reply_markup = InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback(
        "Vote", "vote:up",
    )]]);

    client
        .send(RichMessageRequest {
            chat_id: -100123,
            markdown: "# Recap",
            reply_parameters: Some(RichMessageReplyParameters {
                message_id: 7,
                chat_id: -100456,
                allow_sending_without_reply: true,
            }),
            reply_markup: Some(&reply_markup),
            disable_notification: true,
        })
        .await
        .expect("Telegram success response should decode");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain the request");
    let form = decoded_form(&requests[0].body);
    assert_eq!(
        form.get("disable_notification").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        serde_json::from_str::<Value>(&form["reply_parameters"]).expect("reply JSON"),
        serde_json::json!({
            "message_id": 7,
            "chat_id": -100456,
            "allow_sending_without_reply": true
        })
    );
    assert_eq!(
        serde_json::from_str::<Value>(&form["reply_markup"]).expect("markup JSON"),
        serde_json::json!({
            "inline_keyboard": [[{"text": "Vote", "callback_data": "vote:up"}]]
        })
    );
}

#[tokio::test]
async fn raw_rich_message_preserves_telegram_bad_request_details() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_raw(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse rich message"}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));

    let error = client
        .send(RichMessageRequest {
            chat_id: -100123,
            markdown: "invalid markdown",
            ..Default::default()
        })
        .await
        .expect_err("Telegram bad request should remain an error");

    assert_eq!(
        error,
        TelegramRichMessageError::Api {
            code: 400,
            description: "Bad Request: can't parse rich message".to_owned(),
            parameters: TelegramResponseParameters::default(),
        }
    );
    assert!(!error.to_string().contains("test-token"));
}

#[tokio::test]
async fn raw_rich_message_preserves_telegram_response_parameters() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(429).set_body_raw(
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests","parameters":{"retry_after":17,"migrate_to_chat_id":-100999}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));

    let error = client
        .send(RichMessageRequest {
            chat_id: -100123,
            markdown: "# Recap",
            ..Default::default()
        })
        .await
        .expect_err("Telegram response parameters should remain available");

    assert_eq!(
        error,
        TelegramRichMessageError::Api {
            code: 429,
            description: "Too Many Requests".to_owned(),
            parameters: TelegramResponseParameters {
                migrate_to_chat_id: -100999,
                retry_after: 17,
            },
        }
    );
}

#[tokio::test]
async fn raw_rich_message_checks_api_errors_before_decoding_result() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(400).set_body_raw(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse rich message","result":false}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));

    let error = client
        .send(RichMessageRequest {
            chat_id: -100123,
            markdown: "invalid markdown",
            ..Default::default()
        })
        .await
        .expect_err("Telegram API errors should win over result decoding");

    assert_eq!(
        error,
        TelegramRichMessageError::Api {
            code: 400,
            description: "Bad Request: can't parse rich message".to_owned(),
            parameters: TelegramResponseParameters::default(),
        }
    );
}

#[tokio::test]
async fn raw_plain_message_encodes_parse_mode_when_requested() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"message_id":44,"date":1710000000,"chat":{"id":123,"type":"private"}}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));

    client
        .send_plain(PlainMessageRequest {
            chat_id: 123,
            text: "<b>Recap</b>",
            parse_mode: Some("HTML"),
            ..Default::default()
        })
        .await
        .expect("Telegram success response should decode");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain the request");
    let form = decoded_form(&requests[0].body);
    assert_eq!(form.get("parse_mode").map(String::as_str), Some("HTML"));
}

#[tokio::test]
async fn raw_plain_message_omits_parse_mode_by_default() {
    let server = MockServer::start().await;
    Mock::given(any())
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"message_id":45,"date":1710000000,"chat":{"id":123,"type":"private"}}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let client = TelegramRichMessageClient::new(reqwest::Client::new(), &telegram_config(&server));

    client
        .send_plain(PlainMessageRequest {
            chat_id: 123,
            text: "Plain recap",
            ..Default::default()
        })
        .await
        .expect("Telegram success response should decode");

    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain the request");
    let form = decoded_form(&requests[0].body);
    assert!(!form.contains_key("parse_mode"));
}
