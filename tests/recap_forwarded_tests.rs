//! Private-forwarded recap parity tests against Go v1.0.0 `02aee8ce`.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::{
            recap_forwarded::{handle_recap_forwarded, handle_recap_forwarded_start},
            system::SystemHandlers,
        },
    },
    config::AppConfig,
    db::Database,
    i18n::I18n,
    redis::recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    services::{
        message_capture::PrivateForwardedReplayChatHistory,
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
    },
};
use serde_json::Value;
use sqlx::Row;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{Me, Message};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

const START_MS: i64 = 1_700_000_000_000;
const USER_ID: i64 = 42;
const COMMAND_MESSAGE_ID: i32 = 77;

fn private_command(text: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": COMMAND_MESSAGE_ID,
        "date": 1_710_000_000,
        "from": {
            "id": USER_ID,
            "is_bot": false,
            "first_name": " Ada ",
            "last_name": " Lovelace ",
            "username": "ada"
        },
        "chat": {
            "id": USER_ID,
            "type": "private",
            "first_name": "Ada"
        },
        "text": text,
        "entities": [{"type": "bot_command", "offset": 0, "length": text.len()}]
    }))
    .expect("valid private command fixture")
}

fn group_command(text: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": COMMAND_MESSAGE_ID,
        "date": 1_710_000_000,
        "from": {
            "id": USER_ID,
            "is_bot": false,
            "first_name": "Ada"
        },
        "chat": {
            "id": -100123,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "text": text,
        "entities": [{"type": "bot_command", "offset": 0, "length": text.len()}]
    }))
    .expect("valid group command fixture")
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

fn telegram_message_result(message_id: i32, text: &str) -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "message_id": message_id,
            "date": 1_710_000_001,
            "chat": {"id": USER_ID, "type": "private", "first_name": "Ada"},
            "text": text
        }
    })
}

fn completion_response(model: &str, content: &str, prompt: i64, completion: i64) -> Value {
    serde_json::json!({
        "id": "chatcmpl-forwarded",
        "object": "chat.completion",
        "created": 0,
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": content},
            "finish_reason": "stop"
        }],
        "usage": {
            "prompt_tokens": prompt,
            "completion_tokens": completion,
            "total_tokens": prompt + completion
        }
    })
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

#[test]
fn forwarded_json_decode_matches_go_zero_values_and_last_duplicate_key() {
    let missing = serde_json::from_str::<PrivateForwardedReplayChatHistory>(r#"{"message_id":7}"#)
        .expect("Go fills missing fields with zero values");
    assert_eq!(missing.message_id, 7);
    assert_eq!(missing.chat_id, 0);
    assert!(missing.text.is_empty());

    let duplicate = serde_json::from_str::<PrivateForwardedReplayChatHistory>(
        r#"{"text":"first","text":"last"}"#,
    )
    .expect("Go lets the last duplicate key win");
    assert_eq!(duplicate.text, "last");

    let null = serde_json::from_str::<PrivateForwardedReplayChatHistory>("null")
        .expect("Go accepts null for a struct pointer target");
    assert_eq!(null, PrivateForwardedReplayChatHistory::default());

    let folded = serde_json::from_str::<PrivateForwardedReplayChatHistory>(
        r#"{"chat_id":7,"CHAT_ID":null,"TEXT":"folded","text":"last"}"#,
    )
    .expect("Go matches struct field names without ASCII case sensitivity");
    assert_eq!(folded.chat_id, 7, "null leaves the existing Go value alone");
    assert_eq!(folded.text, "last");
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
        (
            "OPENAI_API_SECRET".to_owned(),
            "forwarded-test-key".to_owned(),
        ),
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
        ("INSIGHTS_LANG".to_owned(), "zh-Hans".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("forwarded test config");
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

fn forwarded_history(index: i64) -> PrivateForwardedReplayChatHistory {
    PrivateForwardedReplayChatHistory {
        chat_id: USER_ID,
        chat_type: "private".to_owned(),
        chat_title: "Ada Lovelace".to_owned(),
        message_id: 100 + index,
        actor_id: 1_000 + index,
        actor_username: format!("user{index}"),
        actor_display_name: format!("User {index}"),
        text: format!("forwarded message {index}"),
        chatted_at: START_MS + index * 1_000,
    }
}

async fn plant_forwarded_batch(state: &dyn RecapStateStore, count: i64) {
    state.start_forwarded(USER_ID).await.expect("start session");
    for index in 1..=count {
        let history = forwarded_history(index);
        state
            .append_forwarded(
                USER_ID,
                history.chatted_at,
                &serde_json::to_string(&history).expect("history JSON"),
            )
            .await
            .expect("append history");
    }
}

#[tokio::test]
async fn forwarded_start_replaces_an_active_batch_before_replying() {
    let server = MockServer::start().await;
    let text = "没问题，请将你需要总结的消息在 2 小时内发给我吧。发送完毕后可以通过发送 /recap_forwarded 给我来开始总结哦！";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": text})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result(501, text)))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 1).await;
    let context = command_context(&server, database, state.clone()).await;

    handle_recap_forwarded_start(
        context.config.telegram.bot(),
        private_command("/recap_forwarded_start"),
        context,
    )
    .await
    .expect("forwarded start command");

    assert!(
        state
            .forwarded_active(USER_ID)
            .await
            .expect("active session")
    );
    assert!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("forwarded batch")
            .is_empty()
    );
    let requests = server.received_requests().await.expect("Telegram request");
    let reply = request_body(&requests[0]);
    assert_eq!(reply["reply_parameters"]["message_id"], COMMAND_MESSAGE_ID);
}

#[tokio::test]
async fn forwarded_commands_are_private_only_without_reply_parameters() {
    let server = MockServer::start().await;
    let text = "该命令当前只能在私聊中使用哦！";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": text})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result(502, text)))
        .expect(2)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    handle_recap_forwarded_start(
        context.config.telegram.bot(),
        group_command("/recap_forwarded_start"),
        context.clone(),
    )
    .await
    .expect("group forwarded start");
    handle_recap_forwarded(
        context.config.telegram.bot(),
        group_command("/recap_forwarded"),
        context,
    )
    .await
    .expect("group forwarded recap");

    assert!(
        !state
            .forwarded_active(USER_ID)
            .await
            .expect("inactive session")
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(requests.len(), 2);
    assert!(
        requests
            .iter()
            .all(|request| request_body(request).get("reply_parameters").is_none())
    );
}

#[tokio::test]
async fn four_forwarded_histories_keep_the_waiting_message_and_session() {
    let server = MockServer::start().await;
    let waiting = "正在为已经接收到的聊天记录生成回顾，请稍等...";
    let insufficient = "目前收到的聊天记录不足 5 条哦，要再多发送给我一些之后之后再试试吗？";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": waiting})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(601, waiting)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": insufficient})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(602, insufficient)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 4).await;
    let context = command_context(&server, database, state.clone()).await;

    handle_recap_forwarded(
        context.config.telegram.bot(),
        private_command("/recap_forwarded"),
        context,
    )
    .await
    .expect("insufficient forwarded recap");

    assert!(
        state
            .forwarded_active(USER_ID)
            .await
            .expect("active session")
    );
    assert_eq!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("retained batch")
            .len(),
        4
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/SendMessage",
            "/telegram/bottest-token/SendMessage",
        ]
    );
    let waiting_request = request_body(&requests[0]);
    assert!(waiting_request.get("reply_parameters").is_none());
    let error_request = request_body(&requests[1]);
    assert_eq!(
        error_request["reply_parameters"]["message_id"],
        COMMAND_MESSAGE_ID
    );
}

#[tokio::test]
async fn malformed_forwarded_json_keeps_the_waiting_message_and_session() {
    let server = MockServer::start().await;
    let waiting = "正在为已经接收到的聊天记录生成回顾，请稍等...";
    let failure = "聊天记录回顾生成失败，请稍后再试！";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": waiting})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(610, waiting)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": failure})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(611, failure)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    state.start_forwarded(USER_ID).await.expect("start session");
    state
        .append_forwarded(USER_ID, START_MS, "{malformed")
        .await
        .expect("append malformed member");
    let context = command_context(&server, database, state.clone()).await;

    handle_recap_forwarded(
        context.config.telegram.bot(),
        private_command("/recap_forwarded"),
        context,
    )
    .await
    .expect("malformed forwarded recap");

    assert!(
        state
            .forwarded_active(USER_ID)
            .await
            .expect("active session")
    );
    assert_eq!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("retained batch"),
        ["{malformed"]
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/SendMessage",
            "/telegram/bottest-token/SendMessage",
        ]
    );
    assert_eq!(
        request_body(&requests[1])["reply_parameters"]["message_id"],
        COMMAND_MESSAGE_ID
    );
}

#[tokio::test]
async fn five_forwarded_histories_generate_rich_recap_and_retain_the_session() {
    let server = MockServer::start().await;
    let waiting = "正在为已经接收到的聊天记录生成回顾，请稍等...";
    let completion = "总结完成，如果你觉得不满意，可以再次发送 /recap_forwarded 重新生成哦！如果觉得满意，并且希望进行其他的总结操作，可以在开始前发送 /cancel 来清空当前已经接收到的消息，如果不进行操作，缓存的消息将会在 2 小时后被自动清理。";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": waiting})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(701, waiting)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": "detail-model"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion_response(
                "resolved-detail",
                "**Forwarded detail** {{tg-ref:101}}",
                8,
                3,
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": "condensed-model"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion_response(
                "resolved-condensed",
                "**濃縮總結** ✨",
                4,
                2,
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendRichMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result(702, "")))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": true})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": completion})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(703, completion)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 5).await;
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_recap_forwarded(
        context.config.telegram.bot(),
        private_command("/recap_forwarded"),
        context,
    )
    .await
    .expect("successful forwarded recap");

    assert!(
        state
            .forwarded_active(USER_ID)
            .await
            .expect("active session")
    );
    assert_eq!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("retained batch")
            .len(),
        5
    );
    let requests = server.received_requests().await.expect("all requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/SendMessage",
            "/v1/chat/completions",
            "/v1/chat/completions",
            "/telegram/bottest-token/sendRichMessage",
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/SendMessage",
        ]
    );
    let rich = request_body(&requests[3]);
    assert_eq!(rich["chat_id"], USER_ID.to_string());
    let rich_message =
        serde_json::from_str::<Value>(rich["rich_message"].as_str().expect("Rich Message JSON"))
            .expect("valid Rich Message JSON");
    assert!(
        rich_message["markdown"]
            .as_str()
            .expect("Rich Markdown")
            .contains("# 【轉發訊息】聊天回顧")
    );
    assert!(
        rich_message["markdown"]
            .as_str()
            .expect("Rich Markdown")
            .contains("[Ada   Lovelace](tg://user?id=42)")
    );
    let reply_parameters = serde_json::from_str::<Value>(
        rich["reply_parameters"]
            .as_str()
            .expect("reply parameters JSON"),
    )
    .expect("valid reply parameters JSON");
    assert_eq!(reply_parameters["message_id"], COMMAND_MESSAGE_ID);
    assert_eq!(reply_parameters["allow_sending_without_reply"], true);
    assert!(rich.get("reply_markup").is_none());
    let completion_request = request_body(&requests[5]);
    assert_eq!(
        completion_request["reply_parameters"]["message_id"],
        COMMAND_MESSAGE_ID
    );

    let log = sqlx::query(
        "SELECT recap_inputs, recap_outputs, recap_type, model_name, \
         prompt_token_usage, completion_token_usage, total_token_usage \
         FROM log_chat_histories_recaps",
    )
    .fetch_one(&database.pool)
    .await
    .expect("forwarded recap log");
    assert!(
        log.try_get::<String, _>(0)
            .expect("recap inputs")
            .contains("msgId:101: User 1 sent: forwarded message 1")
    );
    assert_eq!(
        log.try_get::<String, _>(1).expect("recap outputs"),
        "**Forwarded detail**"
    );
    assert_eq!(log.try_get::<i64, _>(2).expect("recap type"), 1);
    assert_eq!(log.try_get::<String, _>(3).expect("model name"), "");
    assert_eq!(log.try_get::<i64, _>(4).expect("prompt usage"), 8);
    assert_eq!(log.try_get::<i64, _>(5).expect("completion usage"), 3);
    assert_eq!(log.try_get::<i64, _>(6).expect("total usage"), 11);
    let sent_count = sqlx::query("SELECT COUNT(*) FROM sent_messages")
        .fetch_one(&database.pool)
        .await
        .expect("sent-message count")
        .try_get::<i64, _>(0)
        .expect("count");
    assert_eq!(
        sent_count, 0,
        "forwarded delivery creates no sent-message rows"
    );
}

#[tokio::test]
async fn rich_delivery_failure_deletes_waiting_and_keeps_the_session() {
    let server = MockServer::start().await;
    let waiting = "正在为已经接收到的聊天记录生成回顾，请稍等...";
    let failure = "聊天记录回顾发送失败，请稍后再试！";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": waiting})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(720, waiting)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": "detail-model"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion_response(
                "resolved-detail",
                "Forwarded detail",
                8,
                3,
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(body_partial_json(
            serde_json::json!({"model": "condensed-model"}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(completion_response(
                "resolved-condensed",
                "Condensed",
                4,
                2,
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendRichMessage"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 500,
            "description": "Internal Server Error"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(serde_json::json!({"ok": true, "result": true})),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": failure})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(721, failure)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 5).await;
    let context = command_context(&server, database, state.clone()).await;

    handle_recap_forwarded(
        context.config.telegram.bot(),
        private_command("/recap_forwarded"),
        context,
    )
    .await
    .expect("delivery errors are reported through Telegram");

    assert!(
        state
            .forwarded_active(USER_ID)
            .await
            .expect("active session")
    );
    assert_eq!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("retained batch")
            .len(),
        5
    );
    let requests = server.received_requests().await.expect("all requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/SendMessage",
            "/v1/chat/completions",
            "/v1/chat/completions",
            "/telegram/bottest-token/sendRichMessage",
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/SendMessage",
        ]
    );
    assert_eq!(
        request_body(&requests[5])["reply_parameters"]["message_id"],
        COMMAND_MESSAGE_ID
    );
}

#[tokio::test]
async fn cancel_clears_only_an_active_forwarded_session() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "user": {
                    "id": 9_999,
                    "is_bot": true,
                    "first_name": "Test Bot",
                    "username": "TestBot"
                },
                "status": "member"
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    let cancelled = "好的，已经帮你把消息清理掉了，如果需要总结转发的消息，可以再次发送 /recap_forwarded_start 开始操作。";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": cancelled})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(801, cancelled)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let none_active = "已经没有正在进行的操作了";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": none_active})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(802, none_active)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 1).await;
    let context = command_context(&server, database, state.clone()).await;

    SystemHandlers::handle_cancel(
        context.config.telegram.bot(),
        private_command("/cancel"),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("active cancel");
    assert!(!state.forwarded_active(USER_ID).await.expect("cancelled"));
    assert!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("cleared batch")
            .is_empty()
    );

    SystemHandlers::handle_cancel(
        context.config.telegram.bot(),
        private_command("/cancel"),
        bot_me(),
        context,
    )
    .await
    .expect("already cancelled");

    let requests = server.received_requests().await.expect("Telegram requests");
    let replies = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/SendMessage"))
        .map(request_body)
        .collect::<Vec<_>>();
    assert_eq!(replies.len(), 2);
    assert!(
        replies
            .iter()
            .all(|reply| { reply["reply_parameters"]["message_id"] == COMMAND_MESSAGE_ID })
    );
}

#[tokio::test]
async fn active_cancel_still_clears_state_when_the_admin_check_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 500,
            "description": "Internal Server Error"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let cancelled = "好的，已经帮你把消息清理掉了，如果需要总结转发的消息，可以再次发送 /recap_forwarded_start 开始操作。";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": cancelled})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(810, cancelled)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 1).await;
    let context = command_context(&server, database, state.clone()).await;

    SystemHandlers::handle_cancel(
        context.config.telegram.bot(),
        private_command("/cancel"),
        bot_me(),
        context,
    )
    .await
    .expect("active cancellation survives the failed administrator lookup");

    assert!(!state.forwarded_active(USER_ID).await.expect("cancelled"));
    assert!(
        state
            .forwarded_batch(USER_ID)
            .await
            .expect("cleared batch")
            .is_empty()
    );
}

#[tokio::test]
async fn administrator_group_cancel_requires_the_own_bot_mention() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "user": {
                    "id": 9_999,
                    "is_bot": true,
                    "first_name": "Test Bot",
                    "username": "TestBot"
                },
                "status": "administrator",
                "can_be_edited": false,
                "can_manage_chat": true,
                "can_delete_messages": true,
                "can_manage_video_chats": true,
                "can_restrict_members": true,
                "can_promote_members": true,
                "can_change_info": true,
                "can_invite_users": true,
                "can_post_stories": true,
                "can_edit_stories": true,
                "can_delete_stories": true,
                "is_anonymous": false
            }
        })))
        .expect(2)
        .mount(&server)
        .await;
    let cancelled = "好的，已经帮你把消息清理掉了，如果需要总结转发的消息，可以再次发送 /recap_forwarded_start 开始操作。";
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .and(body_partial_json(serde_json::json!({"text": cancelled})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(820, cancelled)),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    plant_forwarded_batch(state.as_ref(), 1).await;
    let context = command_context(&server, database, state.clone()).await;

    SystemHandlers::handle_cancel(
        context.config.telegram.bot(),
        group_command("/cancel"),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("bare administrator-group cancel is suppressed");
    assert!(state.forwarded_active(USER_ID).await.expect("still active"));

    SystemHandlers::handle_cancel(
        context.config.telegram.bot(),
        group_command("/cancel@TestBot"),
        bot_me(),
        context,
    )
    .await
    .expect("mentioned administrator-group cancel executes");
    assert!(!state.forwarded_active(USER_ID).await.expect("cancelled"));

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/SendMessage",
        ]
    );
}
