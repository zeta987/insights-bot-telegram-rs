//! Public manual-recap parity tests against Go v1.0.0 `02aee8ce`.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::{
            recap::RecapHandlers,
            recap_manual::{
                AUTO_RECAP_SEND_MODE_PUBLICLY, SelectHourCallbackData, actor_display_name,
                build_select_hour_keyboard, build_vote_keyboard, insufficient_histories_message,
            },
        },
    },
    config::AppConfig,
    db::models::{ReactionCounts, ReactionType},
    db::{Database, chat_history, feature_flags, models::NewTelegramChatHistory},
    i18n::I18n,
    redis::{
        keys,
        recap_state::{Clock, InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
    services::{
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
    },
};
use serde_json::Value;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{CallbackQuery, Me, Message};
use uuid::Uuid;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

const START_MS: i64 = 1_700_000_000_000;
const CHAT_ID: i64 = -1_001_234_567_890;
const LOG_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

fn store() -> (InMemoryRecapStateStore, Arc<TestClock>) {
    let clock = Arc::new(TestClock::new(START_MS));
    (InMemoryRecapStateStore::new(clock.clone()), clock)
}

fn command_message() -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 77,
        "date": 1_710_000_000,
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "text": "/recap"
    }))
    .expect("valid Telegram command fixture")
}

fn bot_me() -> Me {
    serde_json::from_value(serde_json::json!({
        "id": 9_999,
        "is_bot": true,
        "first_name": "Test Bot",
        "username": "TestBot",
        "can_join_groups": true,
        "can_read_all_group_messages": true,
        "supports_inline_queries": false
    }))
    .expect("valid Telegram bot identity")
}

async fn command_context(
    server: &MockServer,
    database: Database,
    state: Arc<dyn RecapStateStore>,
) -> Arc<AppContext> {
    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "test-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram", server.uri()),
        ),
        ("OPENAI_API_SECRET".to_owned(), "manual-test-key".to_owned()),
        ("OPENAI_API_HOST".to_owned(), format!("{}/v1", server.uri())),
        (
            "OPENAI_API_MODEL_NAME".to_owned(),
            "detail-model".to_owned(),
        ),
        (
            "SARCASTIC_CONDENSED_MODEL_NAME".to_owned(),
            "condensed-model".to_owned(),
        ),
        ("REDIS_PORT".to_owned(), "6379".to_owned()),
        (
            "HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS".to_owned(),
            "120".to_owned(),
        ),
        ("LOCALE".to_owned(), "zh-Hant".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("manual recap test config");
    let openai = OpenAiClient::new(
        &config.openai,
        &config.recap_openai,
        &config.condensed_prompts,
    )
    .expect("OpenAI test client")
    .with_rate_limiter(Arc::new(GoRateLimiter::per_second(1_000)));
    AppContext::new(
        config,
        database,
        I18n::load_from_dir("locales").expect("embedded locales"),
        openai,
        CommandRateLimiter::new(1, Duration::from_secs(1)),
        None,
        RecapRuntimeDependencies {
            recap_state: Some(state),
            raw_telegram_http: reqwest::Client::new(),
            message_preprocessor: None,
        },
    )
}

fn request_body(request: &wiremock::Request) -> Value {
    serde_json::from_slice(&request.body).unwrap_or_else(|_| {
        let map = url::form_urlencoded::parse(&request.body)
            .into_owned()
            .map(|(key, value)| (key, Value::String(value)))
            .collect::<serde_json::Map<_, _>>();
        Value::Object(map)
    })
}

fn callback_query(wire: &str) -> CallbackQuery {
    serde_json::from_value(serde_json::json!({
        "id": "manual-callback",
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": " Ada ",
            "last_name": " Lovelace ",
            "username": "ada"
        },
        "message": {
            "message_id": 101,
            "date": 1_710_000_001,
            "chat": {
                "id": CHAT_ID,
                "type": "supergroup",
                "title": "Parity Lab"
            },
            "text": "selector",
            "reply_to_message": {
                "message_id": 77,
                "date": 1_710_000_000,
                "chat": {
                    "id": CHAT_ID,
                    "type": "supergroup",
                    "title": "Parity Lab"
                },
                "text": "/recap"
            }
        },
        "chat_instance": "manual-chat-instance",
        "data": wire
    }))
    .expect("valid callback query fixture")
}

fn feedback_callback_query(wire: &str) -> CallbackQuery {
    serde_json::from_value(serde_json::json!({
        "id": "manual-feedback-callback",
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "message": {
            "message_id": 202,
            "date": 1_710_000_002,
            "chat": {
                "id": CHAT_ID,
                "type": "supergroup",
                "title": "Parity Lab"
            },
            "text": "Rich recap"
        },
        "chat_instance": "manual-chat-instance",
        "data": wire
    }))
    .expect("valid feedback callback fixture")
}

fn legacy_feedback_callback_query(wire: &str, markup: &Value) -> CallbackQuery {
    serde_json::from_value(serde_json::json!({
        "id": "legacy-recap-feedback-callback",
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "message": {
            "message_id": 204,
            "date": 1_710_000_004,
            "chat": {
                "id": CHAT_ID,
                "type": "supergroup",
                "title": "Parity Lab"
            },
            "text": "Legacy recap",
            "reply_markup": markup
        },
        "chat_instance": "legacy-recap-chat-instance",
        "data": wire
    }))
    .expect("valid legacy feedback callback fixture")
}

fn private_feedback_callback_query(wire: &str) -> CallbackQuery {
    serde_json::from_value(serde_json::json!({
        "id": "private-manual-feedback-callback",
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "message": {
            "message_id": 203,
            "date": 1_710_000_003,
            "chat": {"id": 42, "type": "private", "first_name": "Ada"},
            "text": "Private Rich recap"
        },
        "chat_instance": "private-manual-chat-instance",
        "data": wire
    }))
    .expect("valid private feedback callback fixture")
}

fn completion_response(model: &str, content: &str) -> Value {
    serde_json::json!({
        "id": "chatcmpl-manual",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": 3,
            "completion_tokens": 2,
            "total_tokens": 5
        }
    })
}

async fn mount_openai_response(
    server: &MockServer,
    requested_model: &str,
    resolved_model: &str,
    content: &str,
) {
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": requested_model}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion_response(resolved_model, content)),
        )
        .expect(1)
        .mount(server)
        .await;
}

async fn insert_histories(database: &Database, count: i64) {
    let now = chrono::Utc::now().timestamp_millis();
    for index in 1..=count {
        chat_history::save_one(
            database,
            &NewTelegramChatHistory {
                chat_id: CHAT_ID,
                chat_type: "group".to_owned(),
                chat_title: "Parity Lab".to_owned(),
                message_id: index,
                user_id: 1_000 + index,
                username: format!("user{index}"),
                full_name: format!("User {index}"),
                text: format!("message {index}"),
                replied_to_message_id: 0,
                replied_to_user_id: 0,
                replied_to_full_name: String::new(),
                replied_to_username: String::new(),
                replied_to_text: String::new(),
                replied_to_chat_type: String::new(),
                chatted_at: now - (count - index) * 1_000,
            },
        )
        .await
        .expect("insert history");
    }
    if count > 0 {
        chat_history::update_one_text(database, CHAT_ID, count, "")
            .await
            .expect("characterize a stored empty-text history");
    }
}

async fn mount_telegram_successes(server: &MockServer) {
    Mock::given(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 101,
                "date": 1_710_000_001,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"},
                "text": "processing"
            }
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(path("/telegram/bottest-token/sendRichMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 202,
                "date": 1_710_000_002,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(1)
        .mount(server)
        .await;
}

async fn mount_telegram_delivery_failure(server: &MockServer) {
    Mock::given(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 101,
                "date": 1_710_000_001,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"},
                "text": "processing"
            }
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(path("/telegram/bottest-token/sendRichMessage"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 500,
            "description": "Internal Server Error"
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(1)
        .mount(server)
        .await;
    Mock::given(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 304,
                "date": 1_710_000_004,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn hour_keyboard_uses_go_payload_order_labels_and_opaque_wires() {
    let (store, _) = store();
    let keyboard =
        build_select_hour_keyboard(&store, CHAT_ID, "Parity Lab", AUTO_RECAP_SEND_MODE_PUBLICLY)
            .await
            .expect("select-hour keyboard");
    let json = serde_json::to_value(&keyboard).expect("keyboard JSON");
    let rows = json["inline_keyboard"].as_array().expect("keyboard rows");

    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].as_array().expect("first row").len(), 3);
    assert_eq!(rows[1].as_array().expect("second row").len(), 3);
    let labels = rows
        .iter()
        .flat_map(|row| row.as_array().expect("row buttons"))
        .map(|button| button["text"].as_str().expect("button text"))
        .collect::<Vec<_>>();
    assert_eq!(
        labels,
        ["1 小时", "2 小时", "4 小时", "6 小时", "12 小时", "24 小时"]
    );

    let first_wire = rows[0][0]["callback_data"]
        .as_str()
        .expect("first callback data");
    assert!(!first_wire.contains("recap:"));
    let (route_hash, action_hash) =
        keys::decode_callback_wire(first_wire).expect("opaque callback wire");
    assert_eq!(
        route_hash,
        keys::callback_route_hash(keys::ROUTE_SELECT_HOUR)
    );
    assert_eq!(
        store
            .get_callback(keys::ROUTE_SELECT_HOUR, action_hash)
            .await
            .expect("stored callback")
            .as_deref(),
        Some(r#"{"hour":1,"chat_id":-1001234567890,"chat_title":"Parity Lab","recap_mode":0}"#)
    );
}

#[tokio::test]
async fn select_hour_payload_uses_go_html_safe_json_before_hashing() {
    let (store, _) = store();
    let keyboard = build_select_hour_keyboard(
        &store,
        CHAT_ID,
        "<&>\u{2028}\u{2029}",
        AUTO_RECAP_SEND_MODE_PUBLICLY,
    )
    .await
    .expect("select-hour keyboard");
    let json = serde_json::to_value(&keyboard).expect("keyboard JSON");
    let wire = json["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("first callback wire");
    let (_, action_hash) = keys::decode_callback_wire(wire).expect("opaque callback wire");
    let expected = r#"{"hour":1,"chat_id":-1001234567890,"chat_title":"\u003c\u0026\u003e\u2028\u2029","recap_mode":0}"#;

    assert_eq!(action_hash, keys::callback_action_hash(expected));
    assert_eq!(
        store
            .get_callback(keys::ROUTE_SELECT_HOUR, action_hash)
            .await
            .expect("stored callback")
            .as_deref(),
        Some(expected)
    );
}

#[test]
fn select_hour_payload_rejects_every_value_outside_the_go_set() {
    for hour in [i64::MIN, -1, 0, 3, 5, 8, 23, 25, i64::MAX] {
        let payload = format!(
            r#"{{"hour":{hour},"chat_id":{CHAT_ID},"chat_title":"Parity Lab","recap_mode":0}}"#
        );
        assert!(
            SelectHourCallbackData::from_json(&payload).is_err(),
            "hour {hour} must not fall back to six"
        );
    }
    for hour in [1, 2, 4, 6, 12, 24] {
        let payload = format!(
            r#"{{"hour":{hour},"chat_id":{CHAT_ID},"chat_title":"Parity Lab","recap_mode":0}}"#
        );
        assert_eq!(
            SelectHourCallbackData::from_json(&payload)
                .expect("accepted hour")
                .hour,
            hour
        );
    }
}

#[tokio::test]
async fn manual_rate_limit_matches_go_get_ttl_set_lifecycle() {
    let (store, clock) = store();
    let first = store
        .check_manual_recap_rate(CHAT_ID, 1, 120)
        .await
        .expect("first rate check");
    assert_eq!(first.counted_rate, 1);
    assert_eq!(first.ttl_seconds, -2);
    assert!(first.allowed);
    assert_eq!(
        store.raw_string(&keys::manual_recap_rate_key(CHAT_ID)),
        Some("1".to_owned())
    );

    let denied = store
        .check_manual_recap_rate(CHAT_ID, 1, 120)
        .await
        .expect("second rate check");
    assert_eq!(denied.counted_rate, 1);
    assert_eq!(denied.ttl_seconds, 120);
    assert!(!denied.allowed);

    clock.advance_ms(119_000);
    assert_eq!(
        store
            .check_manual_recap_rate(CHAT_ID, 1, 120)
            .await
            .expect("nearly expired rate check")
            .ttl_seconds,
        1
    );
    clock.advance_ms(1_000);
    assert!(
        store
            .check_manual_recap_rate(CHAT_ID, 1, 120)
            .await
            .expect("expired rate check")
            .allowed
    );
}

#[tokio::test]
async fn nonpositive_manual_interval_never_touches_redis() {
    let (store, _) = store();
    for seconds in [i64::MIN, -1, 0] {
        let result = store
            .check_manual_recap_rate(CHAT_ID, 1, seconds)
            .await
            .expect("disabled rate limit");
        assert_eq!(result.counted_rate, 0);
        assert_eq!(result.ttl_seconds, 0);
        assert!(result.allowed);
    }
    assert!(store.keys().is_empty());
}

#[tokio::test]
async fn vote_keyboard_preserves_the_go_smr_compatibility_route() {
    let (store, _) = store();
    let keyboard = build_vote_keyboard(
        &store,
        CHAT_ID,
        LOG_ID,
        ReactionCounts {
            up_votes: 0,
            down_votes: 2,
            lmao: 0,
        },
    )
    .await
    .expect("vote keyboard");
    let json = serde_json::to_value(&keyboard).expect("keyboard JSON");
    let buttons = json["inline_keyboard"][0].as_array().expect("vote row");
    assert_eq!(
        buttons
            .iter()
            .map(|button| button["text"].as_str().expect("button text"))
            .collect::<Vec<_>>(),
        ["👍", "👎 2", "🤣"]
    );

    let expected_types = ["up_vote", "down_vote", "lmao"];
    for (button, reaction_type) in buttons.iter().zip(expected_types) {
        let wire = button["callback_data"].as_str().expect("callback wire");
        let (route_hash, action_hash) =
            keys::decode_callback_wire(wire).expect("opaque callback wire");
        assert_eq!(
            route_hash,
            keys::callback_route_hash(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT)
        );
        let payload = store
            .get_callback(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT, action_hash)
            .await
            .expect("stored vote callback")
            .expect("live vote callback");
        let expected_payload =
            format!(r#"{{"chatId":{CHAT_ID},"logId":"{LOG_ID}","type":"{reaction_type}"}}"#);
        assert_eq!(payload, expected_payload);
        assert_eq!(action_hash, keys::callback_action_hash(&expected_payload));
        assert_eq!(
            serde_json::from_str::<Value>(&payload).expect("vote payload JSON"),
            serde_json::json!({
                "chatId": CHAT_ID,
                "logId": LOG_ID,
                "type": reaction_type,
            })
        );
    }
}

#[tokio::test]
async fn vote_callback_toggles_the_summarization_table_and_only_edits_markup() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/EditMessageReplyMarkup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 202,
                "date": 1_710_000_002,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"},
                "text": "Rich recap"
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let keyboard = build_vote_keyboard(state.as_ref(), CHAT_ID, LOG_ID, ReactionCounts::default())
        .await
        .expect("vote keyboard");
    let json = serde_json::to_value(&keyboard).expect("keyboard JSON");
    let up_vote_wire = json["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("up-vote callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state).await;

    for expected_count in [1, 0] {
        RecapHandlers::handle_callback_query(
            context.config.telegram.bot(),
            feedback_callback_query(&up_vote_wire),
            context.clone(),
        )
        .await
        .expect("feedback callback");
        let counts = insights_bot_telegram_rs::db::feedback::counts(
            &database,
            insights_bot_telegram_rs::db::feedback::ReactionTable::Summarizations,
            CHAT_ID,
            Uuid::parse_str(LOG_ID).expect("log UUID"),
        )
        .await
        .expect("summarization counts");
        assert_eq!(counts.up_votes, expected_count);
    }

    assert!(
        !insights_bot_telegram_rs::db::feedback::has_summarization_reacted(
            &database,
            CHAT_ID,
            Uuid::parse_str(LOG_ID).expect("log UUID"),
            42,
            ReactionType::UpVote,
        )
        .await
        .expect("reaction lookup")
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests.len(),
        2,
        "Go sends no callback answer or text edit"
    );
    assert!(
        requests.iter().all(|request| {
            request.url.path() == "/telegram/bottest-token/EditMessageReplyMarkup"
        })
    );
    let first_markup = request_body(&requests[0]);
    let first_markup: Value = match &first_markup["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup string"),
        value => value.clone(),
    };
    assert_eq!(first_markup["inline_keyboard"][0][0]["text"], "👍 1");
    let second_markup = request_body(&requests[1]);
    let second_markup: Value = match &second_markup["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup string"),
        value => value.clone(),
    };
    assert_eq!(second_markup["inline_keyboard"][0][0]["text"], "👍");
}

#[tokio::test]
async fn legacy_recap_feedback_route_updates_recap_table_and_only_clicked_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageReplyMarkup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 204,
                "date": 1_710_000_004,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"},
                "text": "Legacy recap"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let payload = format!(r#"{{"chatId":{CHAT_ID},"logId":"{LOG_ID}","type":"up_vote"}}"#);
    let wire = state
        .put_callback(keys::ROUTE_RECAP_FEEDBACK_REACT, &payload)
        .await
        .expect("legacy recap callback");
    let markup = serde_json::json!({
        "inline_keyboard": [
            [{"text": "👍", "callback_data": wire}],
            [{"text": "keep", "callback_data": "keep-this-row"}]
        ]
    });
    let context = command_context(&server, database.clone(), state.clone()).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        legacy_feedback_callback_query(&wire, &markup),
        context,
    )
    .await
    .expect("legacy recap feedback callback");

    let counts = insights_bot_telegram_rs::db::feedback::counts(
        &database,
        insights_bot_telegram_rs::db::feedback::ReactionTable::ChatHistoriesRecaps,
        CHAT_ID,
        Uuid::parse_str(LOG_ID).expect("log UUID"),
    )
    .await
    .expect("recap reaction counts");
    assert_eq!(counts.up_votes, 1);

    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(
        requests.len(),
        1,
        "Go sends no callback answer or text edit"
    );
    let edit = request_body(&requests[0]);
    let edited_markup: Value = match &edit["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup string"),
        value => value.clone(),
    };
    assert_eq!(edited_markup["inline_keyboard"][0][0]["text"], "👍 1");
    assert_eq!(
        edited_markup["inline_keyboard"][1],
        markup["inline_keyboard"][1]
    );
    for button in edited_markup["inline_keyboard"][0]
        .as_array()
        .expect("rebuilt vote row")
    {
        let rebuilt_wire = button["callback_data"].as_str().expect("callback wire");
        let (route_hash, _) =
            keys::decode_callback_wire(rebuilt_wire).expect("opaque callback wire");
        assert_eq!(
            route_hash,
            keys::callback_route_hash(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT),
            "Go rebuilds legacy recap buttons onto the smr compatibility route"
        );
    }
}

#[tokio::test]
async fn private_vote_callback_preserves_go_source_group_edit_destination() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/EditMessageReplyMarkup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 203,
                "date": 1_710_000_003,
                "chat": {"id": 42, "type": "private", "first_name": "Ada"},
                "text": "Private Rich recap"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let keyboard = build_vote_keyboard(state.as_ref(), CHAT_ID, LOG_ID, ReactionCounts::default())
        .await
        .expect("vote keyboard");
    let json = serde_json::to_value(&keyboard).expect("keyboard JSON");
    let wire = json["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("up-vote callback");
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        private_feedback_callback_query(wire),
        context,
    )
    .await
    .expect("private feedback callback");

    let counts = insights_bot_telegram_rs::db::feedback::counts(
        &database,
        insights_bot_telegram_rs::db::feedback::ReactionTable::Summarizations,
        CHAT_ID,
        Uuid::parse_str(LOG_ID).expect("log UUID"),
    )
    .await
    .expect("source-group reaction counts");
    assert_eq!(counts.up_votes, 1);
    let requests = server.received_requests().await.expect("Telegram request");
    let edit = request_body(&requests[0]);
    assert_eq!(edit["chat_id"], CHAT_ID);
    assert_eq!(edit["message_id"], 203);
}

#[tokio::test]
async fn vote_callback_canonicalizes_a_parseable_uuid_before_rebuilding_buttons() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/EditMessageReplyMarkup"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 202,
                "date": 1_710_000_002,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"},
                "text": "Rich recap"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let noncanonical_log_id = LOG_ID.to_ascii_uppercase();
    let payload =
        format!(r#"{{"chatId":{CHAT_ID},"logId":"{noncanonical_log_id}","type":"up_vote"}}"#);
    let wire = state
        .put_callback(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT, &payload)
        .await
        .expect("store noncanonical feedback callback");
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        feedback_callback_query(&wire),
        context,
    )
    .await
    .expect("feedback callback");

    let requests = server.received_requests().await.expect("Telegram request");
    let body = request_body(&requests[0]);
    let markup: Value = match &body["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup string"),
        value => value.clone(),
    };
    let rebuilt_wire = markup["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("rebuilt callback wire");
    let (_, action_hash) = keys::decode_callback_wire(rebuilt_wire).expect("opaque callback wire");
    let rebuilt_payload = state
        .get_callback(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT, action_hash)
        .await
        .expect("stored callback")
        .expect("live callback");
    assert_eq!(
        rebuilt_payload,
        format!(r#"{{"chatId":{CHAT_ID},"logId":"{LOG_ID}","type":"up_vote"}}"#)
    );
}

#[test]
fn manual_presentation_matches_go_name_and_history_messages() {
    assert_eq!(
        actor_display_name(" Ada ", " Lovelace ", "ada"),
        "Ada   Lovelace",
        "Go trims the combined string but preserves interior whitespace"
    );
    assert_eq!(actor_display_name(" ", "", "ada"), "ada");
    assert_eq!(
        insufficient_histories_message(6, AUTO_RECAP_SEND_MODE_PUBLICLY),
        "最近 6 小時內暫時沒有超過 5 條的聊天記錄可以生成聊天回顧哦，要再多聊點之後再試試嗎？"
    );
}

#[tokio::test]
async fn public_recap_command_checks_go_flags_and_replies_with_opaque_selector() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 101,
                "date": 1_710_000_001,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity Lab")
        .await
        .expect("enable recap");
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        command_message(),
        bot_me(),
        context,
    )
    .await
    .expect("public /recap command");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(requests.len(), 1);
    let body = request_body(&requests[0]);
    assert_eq!(
        body["chat_id"].to_string().trim_matches('"'),
        CHAT_ID.to_string()
    );
    assert_eq!(body["text"], "请问您要为过去几个小时内的聊天创建回顾呢？");
    assert_eq!(body["reply_parameters"]["message_id"], 77);
    let markup: Value = match &body["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup string"),
        value => value.clone(),
    };
    let buttons = markup["inline_keyboard"]
        .as_array()
        .expect("selector rows")
        .iter()
        .flat_map(|row| row.as_array().expect("selector row"))
        .collect::<Vec<_>>();
    assert_eq!(buttons.len(), 6);
    assert!(buttons.iter().all(|button| {
        button["callback_data"]
            .as_str()
            .is_some_and(|wire| keys::decode_callback_wire(wire).is_some())
    }));
    assert_eq!(state.keys().len(), 7, "six callbacks plus the rate counter");
}

#[tokio::test]
async fn denied_public_recap_stops_before_allocating_any_callback() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 102,
                "date": 1_710_000_002,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity Lab")
        .await
        .expect("enable recap");
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    state
        .check_manual_recap_rate(CHAT_ID, 1, 120)
        .await
        .expect("occupy rate counter");
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        command_message(),
        bot_me(),
        context,
    )
    .await
    .expect("rate-denied /recap command");

    assert_eq!(
        state.keys(),
        vec![keys::manual_recap_rate_key(CHAT_ID)],
        "no selector payload may be allocated after denial"
    );
    let requests = server.received_requests().await.expect("Telegram request");
    let body = request_body(&requests[0]);
    assert_eq!(
        body["text"],
        "很抱歉，您的操作触发了我们的限制机制，为了保证系统的可用性，本命令每最多 120000000000 分钟最多使用一次，请您耐心等待 2 分钟后再试，感谢您的理解和支持。"
    );
    assert!(body.get("reply_markup").is_none());
    assert_eq!(body["reply_parameters"]["message_id"], 77);
}

#[tokio::test]
async fn select_hour_callback_generates_votes_sends_rich_and_then_deletes_waiting() {
    let server = MockServer::start().await;
    mount_telegram_successes(&server).await;
    mount_openai_response(
        &server,
        "detail-model",
        "detail-model-resolved",
        "## 討論主題\n- 六筆歷史也包含空文字 {{tg-ref:1}}",
    )
    .await;
    mount_openai_response(
        &server,
        "condensed-model",
        "condensed-model-resolved",
        "**一句濃縮**——群組完成六筆聊天。",
    )
    .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    insert_histories(&database, 6).await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let payload = serde_json::to_string(&SelectHourCallbackData {
        hour: 6,
        chat_id: CHAT_ID,
        chat_title: "Parity Lab".to_owned(),
        recap_mode: AUTO_RECAP_SEND_MODE_PUBLICLY,
    })
    .expect("select-hour payload");
    let wire = state
        .put_callback(keys::ROUTE_SELECT_HOUR, &payload)
        .await
        .expect("store select-hour callback");
    let context = command_context(&server, database, state).await;

    let callback_result = RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        callback_query(&wire),
        context,
    )
    .await;

    let requests = server.received_requests().await.expect("all requests");
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert!(
        callback_result.is_ok(),
        "select-hour callback failed: {callback_result:?}; paths: {paths:?}"
    );
    assert_eq!(
        paths,
        [
            "/telegram/bottest-token/EditMessageText",
            "/v1/chat/completions",
            "/v1/chat/completions",
            "/telegram/bottest-token/sendRichMessage",
            "/telegram/bottest-token/DeleteMessage",
        ]
    );
    assert_eq!(
        paths
            .iter()
            .filter(|request_path| request_path.contains("AnswerCallbackQuery"))
            .count(),
        0,
        "Go never calls answerCallbackQuery for the select-hour route"
    );
    let rich = requests
        .iter()
        .find(|request| request.url.path().ends_with("/sendRichMessage"))
        .expect("Rich recap request");
    let form = url::form_urlencoded::parse(&rich.body)
        .into_owned()
        .collect::<std::collections::HashMap<_, _>>();
    assert_eq!(
        form.get("chat_id").map(String::as_str),
        Some("-1001234567890")
    );
    let rich_message =
        serde_json::from_str::<Value>(&form["rich_message"]).expect("Rich message JSON");
    let markdown = rich_message["markdown"].as_str().expect("Rich Markdown");
    assert!(markdown.contains("# 【Parity Lab】聊天回顧"));
    assert!(markdown.contains("Ada   Lovelace"));
    assert!(markdown.contains("**一句濃縮**"));
    assert!(markdown.contains("<details><summary>詳細總結</summary>"));
    assert!(
        markdown.contains("detail\\-model\\-resolved"),
        "resolved detail trace missing from:\n{markdown}"
    );
    assert!(
        markdown.contains("condensed\\-model\\-resolved"),
        "resolved condensed trace missing from:\n{markdown}"
    );
    assert_eq!(
        serde_json::from_str::<Value>(&form["reply_parameters"]).expect("Rich reply parameters")["message_id"],
        77
    );
    let markup = serde_json::from_str::<Value>(&form["reply_markup"]).expect("Rich vote keyboard");
    assert_eq!(markup["inline_keyboard"][0][0]["text"], "👍");
    assert_eq!(markup["inline_keyboard"][0][1]["text"], "👎");
    assert_eq!(markup["inline_keyboard"][0][2]["text"], "🤣");
}

#[tokio::test]
async fn five_histories_reply_with_error_and_keep_the_waiting_message() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 101,
                "date": 1_710_000_001,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"},
                "text": "processing"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 303,
                "date": 1_710_000_003,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    insert_histories(&database, 5).await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let payload = serde_json::to_string(&SelectHourCallbackData {
        hour: 6,
        chat_id: CHAT_ID,
        chat_title: "Parity Lab".to_owned(),
        recap_mode: AUTO_RECAP_SEND_MODE_PUBLICLY,
    })
    .expect("select-hour payload");
    let wire = state
        .put_callback(keys::ROUTE_SELECT_HOUR, &payload)
        .await
        .expect("store callback");
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        callback_query(&wire),
        context,
    )
    .await
    .expect("insufficient-history callback");

    let requests = server.received_requests().await.expect("Telegram requests");
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/telegram/bottest-token/EditMessageText",
            "/telegram/bottest-token/SendMessage",
        ],
        "Go leaves the edited waiting message in place before delivery begins"
    );
    let error_body = request_body(requests.last().expect("error reply"));
    assert_eq!(
        error_body["text"],
        "最近 6 小時內暫時沒有超過 5 條的聊天記錄可以生成聊天回顧哦，要再多聊點之後再試試嗎？"
    );
    assert_eq!(error_body["reply_parameters"]["message_id"], 77);
}

#[tokio::test]
async fn delivery_failure_deletes_waiting_before_replying_with_send_error() {
    let server = MockServer::start().await;
    mount_telegram_delivery_failure(&server).await;
    mount_openai_response(
        &server,
        "detail-model",
        "detail-model-resolved",
        "## 討論主題\n- detailed",
    )
    .await;
    mount_openai_response(
        &server,
        "condensed-model",
        "condensed-model-resolved",
        "condensed line",
    )
    .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    insert_histories(&database, 6).await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let payload = serde_json::to_string(&SelectHourCallbackData {
        hour: 6,
        chat_id: CHAT_ID,
        chat_title: "Parity Lab".to_owned(),
        recap_mode: AUTO_RECAP_SEND_MODE_PUBLICLY,
    })
    .expect("select-hour payload");
    let wire = state
        .put_callback(keys::ROUTE_SELECT_HOUR, &payload)
        .await
        .expect("store callback");
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        callback_query(&wire),
        context,
    )
    .await
    .expect("delivery error is rendered to Telegram");

    let requests = server.received_requests().await.expect("all requests");
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/telegram/bottest-token/EditMessageText",
            "/v1/chat/completions",
            "/v1/chat/completions",
            "/telegram/bottest-token/sendRichMessage",
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/SendMessage",
        ]
    );
    let error_body = request_body(requests.last().expect("send error reply"));
    assert_eq!(error_body["text"], "聊天記錄回顧發送失敗，請稍後再試！");
    assert_eq!(error_body["reply_parameters"]["message_id"], 77);
}

/// Go's `callback_query.go:653-654` rejects an hour outside the fixed set
/// with a bare `tgbot.NewExceptionError(...)` and no `WithMessage`, so
/// `processExceptionError` (`handler.go:117-156`) falls back to its default
/// text with no edit target, no parse mode, and no keyboard.
#[tokio::test]
async fn select_hour_callback_with_out_of_range_hour_sends_the_go_default_exception_text() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 305,
                "date": 1_710_000_005,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    // Hand-crafted payload: valid JSON, hour outside {1,2,4,6,12,24}. This
    // reaches the handler's range check rather than its JSON bind step.
    let payload =
        format!(r#"{{"hour":3,"chat_id":{CHAT_ID},"chat_title":"Parity Lab","recap_mode":0}}"#);
    let wire = state
        .put_callback(keys::ROUTE_SELECT_HOUR, &payload)
        .await
        .expect("store out-of-range select-hour callback");
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        callback_query(&wire),
        context,
    )
    .await
    .expect("out-of-range hour callback");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests.len(),
        1,
        "no waiting-message edit or generation call precedes the range check"
    );
    let body = request_body(&requests[0]);
    assert_eq!(body["text"], "发生了一些错误，请稍后再试");
    assert!(
        body.get("parse_mode").is_none(),
        "Go's default exception text carries no parse mode"
    );
    assert!(
        body.get("reply_markup").is_none(),
        "Go's default exception text carries no keyboard"
    );
    assert_eq!(
        body["reply_parameters"]["message_id"], 77,
        "Go replies to the callback message's reply_to_message"
    );
}

/// Go's `callback_query.go:649-651` reports a JSON bind failure with the
/// generation-failure text, distinct from the range-check's default text
/// above.
#[tokio::test]
async fn select_hour_callback_with_malformed_payload_sends_the_go_generation_failure_text() {
    let server = MockServer::start().await;
    Mock::given(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 306,
                "date": 1_710_000_006,
                "chat": {"id": CHAT_ID, "type": "supergroup", "title": "Parity Lab"}
            }
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(
        Arc::new(TestClock::new(START_MS)) as Arc<dyn Clock>,
    ));
    let wire = state
        .put_callback(keys::ROUTE_SELECT_HOUR, "{ this is not json")
        .await
        .expect("store malformed select-hour callback");
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        callback_query(&wire),
        context,
    )
    .await
    .expect("malformed payload callback");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests.len(),
        1,
        "no waiting-message edit or generation call precedes the bind check"
    );
    let body = request_body(&requests[0]);
    assert_eq!(body["text"], "聊天記錄回顧生成失敗，請稍後再試！");
    assert_eq!(body["reply_parameters"]["message_id"], 77);
}

#[test]
fn production_manual_callback_has_no_legacy_telegraph_path() {
    let source = include_str!("../src/bot/handlers/recap.rs");

    for legacy_fragment in [
        "handle_recap_callback",
        "generate_dual_recap",
        "create_page_auto_nodes",
        "messages_since_hours",
        "recap:hours:",
    ] {
        assert!(
            !source.contains(legacy_fragment),
            "legacy manual recap fragment remains reachable: {legacy_fragment}"
        );
    }
}
