use std::{
    collections::{HashMap, VecDeque},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use insights_bot_telegram_rs::{
    config::TelegramConfig,
    services::{
        recap_delivery::{
            BeforeSendHook, PlainRecapSendRequest, RecapDeliveryConfig, RecapDeliveryError,
            RecapDeliverySender, RichRecapSendRequest, TelegramRecapSender, send_rich_recap_parts,
        },
        telegram_rich_message::{TelegramResponseParameters, TelegramRichMessageError},
    },
};
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, Message};
use tokio::time::Instant;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::path};

#[derive(Clone, Default)]
struct FakeSender {
    state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
struct FakeState {
    rich_requests: Vec<RichRecapSendRequest>,
    plain_requests: Vec<PlainRecapSendRequest>,
    rich_outcomes: VecDeque<Result<Message, TelegramRichMessageError>>,
    plain_outcomes: VecDeque<Result<Message, TelegramRichMessageError>>,
    next_message_id: i32,
}

impl FakeSender {
    fn with_rich_outcomes(
        outcomes: impl IntoIterator<Item = Result<Message, TelegramRichMessageError>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                rich_outcomes: outcomes.into_iter().collect(),
                ..Default::default()
            })),
        }
    }

    fn with_errors(
        rich_errors: impl IntoIterator<Item = TelegramRichMessageError>,
        plain_errors: impl IntoIterator<Item = TelegramRichMessageError>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                rich_outcomes: rich_errors.into_iter().map(Err).collect(),
                plain_outcomes: plain_errors.into_iter().map(Err).collect(),
                ..Default::default()
            })),
        }
    }

    fn rich_requests(&self) -> Vec<RichRecapSendRequest> {
        self.state
            .lock()
            .expect("fake sender mutex")
            .rich_requests
            .clone()
    }

    fn plain_requests(&self) -> Vec<PlainRecapSendRequest> {
        self.state
            .lock()
            .expect("fake sender mutex")
            .plain_requests
            .clone()
    }
}

#[async_trait]
impl RecapDeliverySender for FakeSender {
    async fn send_rich(
        &self,
        request: RichRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError> {
        let mut state = self.state.lock().expect("fake sender mutex");
        state.rich_requests.push(request.clone());
        if let Some(outcome) = state.rich_outcomes.pop_front() {
            return outcome;
        }
        state.next_message_id += 1;
        Ok(message(state.next_message_id, request.chat_id))
    }

    async fn send_plain(
        &self,
        request: PlainRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError> {
        let mut state = self.state.lock().expect("fake sender mutex");
        state.plain_requests.push(request.clone());
        if let Some(outcome) = state.plain_outcomes.pop_front() {
            return outcome;
        }
        state.next_message_id += 1;
        Ok(message(state.next_message_id, request.chat_id))
    }
}

fn message(message_id: i32, chat_id: i64) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": message_id,
        "date": 1_710_000_000,
        "chat": {
            "id": chat_id,
            "type": if chat_id < 0 { "supergroup" } else { "private" }
        }
    }))
    .expect("valid Telegram message fixture")
}

fn api_error(code: i32, description: &str) -> TelegramRichMessageError {
    TelegramRichMessageError::Api {
        code,
        description: description.to_owned(),
        parameters: TelegramResponseParameters::default(),
    }
}

fn config(parts: &[&str]) -> RecapDeliveryConfig {
    RecapDeliveryConfig {
        chat_id: 123,
        parts: parts.iter().map(|part| (*part).to_owned()).collect(),
        ..Default::default()
    }
}

fn keyboard() -> InlineKeyboardMarkup {
    InlineKeyboardMarkup::new(vec![vec![InlineKeyboardButton::callback("vote", "vote")]])
}

fn decoded_form(body: &[u8]) -> HashMap<String, String> {
    url::form_urlencoded::parse(body)
        .into_owned()
        .collect::<HashMap<_, _>>()
}

#[test]
fn api_delivery_errors_render_the_same_wrapping_text_as_go() {
    let error = RecapDeliveryError::RichPart {
        part_number: 1,
        source: api_error(400, "Bad Request: chat not found"),
    };

    assert_eq!(
        error.to_string(),
        "send rich recap part 1: Bad Request: chat not found"
    );
}

#[tokio::test]
async fn rich_parts_reply_to_the_first_delivery_and_only_the_first_has_markup() {
    let sender = FakeSender::default();
    let markup = keyboard();
    let messages = send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            chat_id: -100123,
            parts: vec!["first".to_owned(), "second".to_owned()],
            reply_to_message_id: 77,
            reply_markup: Some(markup.clone()),
            disable_notification: true,
            allow_sending_without_reply: true,
            ..Default::default()
        },
    )
    .await
    .expect("both Rich parts should send");

    assert_eq!(messages.len(), 2);
    let requests = sender.rich_requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[0]
            .reply_parameters
            .expect("initial reply")
            .message_id,
        77
    );
    assert!(
        requests[0]
            .reply_parameters
            .unwrap()
            .allow_sending_without_reply
    );
    assert_eq!(requests[0].reply_markup, Some(markup));
    assert!(requests[0].disable_notification);
    assert_eq!(
        requests[1]
            .reply_parameters
            .expect("continuation reply")
            .message_id,
        messages[0].id.0
    );
    assert!(requests[1].reply_markup.is_none());
}

#[tokio::test]
async fn formatting_errors_fall_back_to_unformatted_plain_text() {
    let sender = FakeSender::with_errors(
        [api_error(
            400,
            "Bad Request: can't parse rich message: unsupported markdown block",
        )],
        [],
    );
    let markup = keyboard();
    let rich = "# Recap\n\n<details><summary>詳細總結</summary>\n\nSee [1](https://t.me/c/123/45).\n\n</details>";
    let messages = send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            parts: vec![rich.to_owned()],
            reply_to_message_id: 9,
            reply_markup: Some(markup.clone()),
            ..config(&[])
        },
    )
    .await
    .expect("formatting errors should fall back");

    assert_eq!(messages.len(), 1);
    assert_eq!(sender.rich_requests().len(), 1);
    let plain = sender.plain_requests();
    assert_eq!(plain.len(), 1);
    assert_eq!(plain[0].reply_to_message_id, 9);
    assert_eq!(plain[0].reply_markup, Some(markup));
    assert!(!plain[0].text.contains("<details>"));
    assert!(!plain[0].text.contains("</details>"));
    assert!(plain[0].text.contains("1 (https://t.me/c/123/45)"));
}

#[tokio::test]
async fn every_go_format_signature_triggers_plain_fallback() {
    let signatures = [
        "can't parse entities",
        "cannot parse entities",
        "can't find end of the entity",
        "unsupported start tag",
        "can't parse markdown",
        "cannot parse markdown",
        "failed to parse markdown",
        "invalid markdown",
        "can't parse rich message",
        "cannot parse rich message",
        "failed to parse rich message",
        "invalid rich message",
        "rich message is invalid",
        "rich message format",
        "rich message block",
        "rich message nesting",
        "rich message is too long",
    ];

    for signature in signatures {
        let sender =
            FakeSender::with_errors([api_error(400, &format!("Bad Request: {signature}"))], []);
        send_rich_recap_parts(&sender, config(&["**recap**"]))
            .await
            .unwrap_or_else(|failure| panic!("{signature}: {}", failure.error));
        assert_eq!(sender.plain_requests().len(), 1, "{signature}");
    }
}

#[tokio::test]
async fn unrelated_errors_do_not_fall_back_and_keep_partial_deliveries() {
    let errors = [
        api_error(500, "Internal Server Error"),
        api_error(400, "Bad Request: chat not found"),
        api_error(400, "Bad Request: invalid chat_id"),
        api_error(
            400,
            "Bad Request: invalid reply parameters for rich message",
        ),
        api_error(400, "Bad Request: failed to parse reply_markup JSON"),
        api_error(400, "Bad Request: parse error"),
        TelegramRichMessageError::Transport,
    ];

    for error in errors {
        let first = message(91, 123);
        let sender = FakeSender::with_rich_outcomes([Ok(first.clone()), Err(error.clone())]);
        let failure = send_rich_recap_parts(&sender, config(&["first", "second"]))
            .await
            .expect_err("unrelated failures should stop delivery");

        assert_eq!(failure.messages.len(), 1);
        assert_eq!(failure.messages[0].id, first.id);
        assert_eq!(
            failure.error,
            RecapDeliveryError::RichPart {
                part_number: 2,
                source: error,
            }
        );
        assert!(sender.plain_requests().is_empty());
    }
}

#[tokio::test]
async fn fallback_switches_every_remaining_part_to_plain_mode() {
    let sender = FakeSender::with_errors(
        [api_error(
            400,
            "Bad Request: can't parse entities: malformed markdown",
        )],
        [],
    );
    let messages = send_rich_recap_parts(&sender, config(&["**first**", "**second**"]))
        .await
        .expect("both plain parts should send");

    assert_eq!(messages.len(), 2);
    assert_eq!(sender.rich_requests().len(), 1);
    let plain = sender.plain_requests();
    assert_eq!(plain.len(), 2);
    assert_eq!(plain[0].text, "first");
    assert_eq!(plain[1].text, "second");
    assert_eq!(plain[1].reply_to_message_id, messages[0].id.0);
}

#[tokio::test]
async fn rich_message_too_long_uses_plain_fallback() {
    let sender = FakeSender::with_errors([api_error(400, "Bad Request: message is too long")], []);
    let messages = send_rich_recap_parts(&sender, config(&["**recap 🧾**"]))
        .await
        .expect("too-long Rich content should fall back");

    assert_eq!(messages.len(), 1);
    assert_eq!(sender.plain_requests()[0].text, "recap 🧾");
}

#[tokio::test]
async fn plain_too_long_retries_depth_first_with_a_halved_utf16_budget() {
    let sender = FakeSender::with_errors(
        [api_error(
            400,
            "Bad Request: can't parse entities: malformed markdown",
        )],
        [api_error(400, "Bad Request: message is too long")],
    );
    let text = format!("{} {}", "a".repeat(3000), "b".repeat(500));
    let messages = send_rich_recap_parts(&sender, config(&[&text]))
        .await
        .expect("halved pieces should send");

    let plain = sender.plain_requests();
    assert_eq!(plain.len(), 3);
    assert_eq!(plain[0].text, text);
    assert_eq!(messages.len(), 2);
    assert!(plain[1].text.encode_utf16().count() <= 2048);
    assert!(plain[2].text.encode_utf16().count() <= 2048);
    assert_eq!(format!("{}{}", plain[1].text, plain[2].text), text);
    assert_eq!(plain[2].reply_to_message_id, messages[0].id.0);
}

#[tokio::test]
async fn plain_non_length_errors_stop_without_resplitting() {
    let plain_error = api_error(400, "Bad Request: chat not found");
    let sender = FakeSender::with_errors(
        [api_error(
            400,
            "Bad Request: can't parse entities: malformed markdown",
        )],
        [plain_error.clone()],
    );
    let failure = send_rich_recap_parts(&sender, config(&["recap"]))
        .await
        .expect_err("plain chat errors should stop delivery");

    assert!(failure.messages.is_empty());
    assert_eq!(sender.plain_requests().len(), 1);
    assert_eq!(
        failure.error,
        RecapDeliveryError::PlainFallbackPart {
            part_number: 1,
            plain_part_number: 1,
            source: plain_error,
        }
    );
}

#[tokio::test]
async fn plain_fallback_splits_at_4096_utf16_units_and_replies_to_the_first() {
    let sender = FakeSender::with_errors(
        [api_error(400, "Bad Request: invalid rich message markdown")],
        [],
    );
    let rich = format!(
        "<details><summary>詳細總結</summary>\n\n{}\n\n</details>",
        "界".repeat(4106)
    );
    let messages = send_rich_recap_parts(&sender, config(&[&rich]))
        .await
        .expect("plain chunks should send");

    let plain = sender.plain_requests();
    assert_eq!(plain.len(), 2);
    assert_eq!(messages.len(), 2);
    assert!(
        plain
            .iter()
            .all(|request| request.text.encode_utf16().count() <= 4096)
    );
    assert_eq!(plain[0].reply_to_message_id, 0);
    assert_eq!(plain[1].reply_to_message_id, messages[0].id.0);
}

#[tokio::test]
async fn before_send_runs_for_every_rich_plain_and_retry_attempt() {
    let sender = FakeSender::with_errors(
        [api_error(
            400,
            "Bad Request: can't parse rich message markdown",
        )],
        [api_error(400, "Bad Request: message is too long")],
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let calls_for_hook = calls.clone();
    let before_send: BeforeSendHook = Arc::new(move || {
        let calls = calls_for_hook.clone();
        Box::pin(async move {
            calls.fetch_add(1, Ordering::SeqCst);
        })
    });
    let text = format!("{} {}", "a".repeat(3000), "b".repeat(500));
    send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            parts: vec![text],
            before_send: Some(before_send),
            ..config(&[])
        },
    )
    .await
    .expect("retry pieces should send");

    assert_eq!(calls.load(Ordering::SeqCst), 4);
}

#[tokio::test(start_paused = true)]
async fn before_send_is_awaited_before_every_attempt() {
    let sender = FakeSender::default();
    let before_send: BeforeSendHook = Arc::new(|| {
        Box::pin(async {
            tokio::time::sleep(Duration::from_secs(1)).await;
        })
    });
    let started = Instant::now();

    send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            parts: vec!["first".to_owned(), "second".to_owned()],
            before_send: Some(before_send),
            ..config(&[])
        },
    )
    .await
    .expect("both delayed attempts should send");

    assert_eq!(started.elapsed(), Duration::from_secs(2));
    assert_eq!(sender.rich_requests().len(), 2);
}

#[tokio::test]
async fn an_empty_plain_fallback_keeps_the_triggering_rich_error() {
    let rich_error = api_error(400, "Bad Request: invalid rich message markdown");
    let sender = FakeSender::with_errors([rich_error.clone()], []);
    let empty_rich = "<details><summary></summary>\n\n</details>";
    let failure = send_rich_recap_parts(&sender, config(&[empty_rich]))
        .await
        .expect_err("an empty fallback should fail");

    assert!(failure.messages.is_empty());
    assert_eq!(
        failure.error,
        RecapDeliveryError::NoPlainFallback {
            part_number: 1,
            source: Some(rich_error),
        }
    );
}

#[tokio::test]
async fn an_empty_later_plain_part_has_no_new_triggering_error() {
    let sender = FakeSender::with_errors(
        [api_error(400, "Bad Request: invalid rich message markdown")],
        [],
    );
    let empty_rich = "<details><summary></summary>\n\n</details>";
    let failure = send_rich_recap_parts(&sender, config(&["first", empty_rich]))
        .await
        .expect_err("plain mode cannot send an empty later part");

    assert_eq!(failure.messages.len(), 1);
    assert_eq!(
        failure.error,
        RecapDeliveryError::NoPlainFallback {
            part_number: 2,
            source: None,
        }
    );
}

#[tokio::test]
async fn production_sender_falls_back_through_the_pathful_telegram_endpoint() {
    let server = MockServer::start().await;
    Mock::given(path(
        "/telegram-proxy/bottest-token/sendRichMessage",
    ))
    .respond_with(ResponseTemplate::new(400).set_body_raw(
        r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse rich message"}"#,
        "application/json",
    ))
    .mount(&server)
    .await;
    Mock::given(path("/telegram-proxy/bottest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"{"ok":true,"result":{"message_id":42,"date":1710000000,"chat":{"id":-100123,"type":"supergroup"}}}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
    let telegram = TelegramConfig {
        bot_token: "test-token".to_owned(),
        api_endpoint: format!("{}/telegram-proxy", server.uri()),
        webhook_url: None,
        webhook_port: None,
    };
    let markup = keyboard();
    let sender = TelegramRecapSender::new(reqwest::Client::new(), &telegram);

    let delivery = send_rich_recap_parts(
        &sender,
        RecapDeliveryConfig {
            chat_id: -100123,
            parts: vec!["**recap**".to_owned()],
            reply_to_message_id: 9,
            reply_markup: Some(markup.clone()),
            disable_notification: true,
            allow_sending_without_reply: true,
            ..Default::default()
        },
    )
    .await;
    let requests = server
        .received_requests()
        .await
        .expect("wiremock should retain both attempts");
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/telegram-proxy/bottest-token/sendRichMessage",
            "/telegram-proxy/bottest-token/sendMessage"
        ]
    );
    let messages = delivery.expect("production sender should complete the plain fallback");
    assert_eq!(messages[0].id.0, 42);
    let plain_request = requests
        .iter()
        .find(|request| request.url.path().ends_with("/sendMessage"))
        .expect("plain fallback request");
    let form = decoded_form(&plain_request.body);
    assert_eq!(form.get("chat_id").map(String::as_str), Some("-100123"));
    assert_eq!(form.get("text").map(String::as_str), Some("recap"));
    assert_eq!(
        form.get("disable_notification").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        form.get("allow_sending_without_reply").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        form.get("reply_to_message_id").map(String::as_str),
        Some("9")
    );
    assert!(!form.contains_key("parse_mode"));
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&form["reply_markup"])
            .expect("reply markup JSON"),
        serde_json::to_value(markup).expect("expected markup JSON")
    );
}

#[tokio::test]
async fn production_plain_transport_preserves_go_error_envelopes() {
    let cases = [
        (
            401,
            r#"{"ok":false,"error_code":401,"description":"Unauthorized"}"#,
            "Unauthorized",
            TelegramResponseParameters::default(),
        ),
        (
            404,
            r#"{"ok":false,"error_code":404,"description":"Not Found"}"#,
            "Not Found",
            TelegramResponseParameters::default(),
        ),
        (
            429,
            r#"{"ok":false,"error_code":429,"description":"Too Many Requests: retry after 17","parameters":{"retry_after":17}}"#,
            "Too Many Requests: retry after 17",
            TelegramResponseParameters {
                retry_after: 17,
                ..Default::default()
            },
        ),
        (
            400,
            r#"{"ok":false,"error_code":400,"description":"Bad Request: group chat was upgraded to a supergroup chat","parameters":{"migrate_to_chat_id":-100999}}"#,
            "Bad Request: group chat was upgraded to a supergroup chat",
            TelegramResponseParameters {
                migrate_to_chat_id: -100999,
                ..Default::default()
            },
        ),
    ];

    for (status, body, description, parameters) in cases {
        let server = MockServer::start().await;
        Mock::given(path(
            "/telegram-proxy/bottest-token/sendRichMessage",
        ))
        .respond_with(ResponseTemplate::new(400).set_body_raw(
            r#"{"ok":false,"error_code":400,"description":"Bad Request: can't parse rich message"}"#,
            "application/json",
        ))
        .mount(&server)
        .await;
        Mock::given(path("/telegram-proxy/bottest-token/sendMessage"))
            .respond_with(ResponseTemplate::new(status).set_body_raw(body, "application/json"))
            .mount(&server)
            .await;
        let telegram = TelegramConfig {
            bot_token: "test-token".to_owned(),
            api_endpoint: format!("{}/telegram-proxy", server.uri()),
            webhook_url: None,
            webhook_port: None,
        };
        let sender = TelegramRecapSender::new(reqwest::Client::new(), &telegram);

        let failure = send_rich_recap_parts(&sender, config(&["**recap**"]))
            .await
            .expect_err("plain Telegram error should remain intact");

        assert_eq!(
            failure.error,
            RecapDeliveryError::PlainFallbackPart {
                part_number: 1,
                plain_part_number: 1,
                source: TelegramRichMessageError::Api {
                    code: i32::from(status),
                    description: description.to_owned(),
                    parameters,
                },
            }
        );
    }
}
