//! `/configure_recap` keyboard parity against Go v1.0.0 `02aee8ce`.

mod support;

use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::{
            recap::RecapHandlers,
            recap_configure::{
                ConfigureRecapView, build_configure_keyboard, handle_configure_recap,
            },
        },
    },
    config::AppConfig,
    db::{Database, feature_flags, recap_options},
    i18n::I18n,
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, ManualRecapRateResult, RecapStateStore, TestClock},
    },
    services::{
        autorecap_queue::encode_auto_recap_member, openai::OpenAiClient, rate_limit::GoRateLimiter,
    },
};
use serde_json::Value;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{CallbackQuery, Me, Message};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

const START_MS: i64 = 1_700_000_000_000;
const CHAT_ID: i64 = -1_001_234_567_890;
const FROM_ID: i64 = 42;
const GROUP_ANONYMOUS_BOT_ID: i64 = 1_087_968_824;
const PRIVATE_CHAT_ID: i64 = 4_242;

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

fn configure_command() -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 77,
        "date": 1_710_000_000,
        "from": {
            "id": FROM_ID,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "text": "/configure_recap"
    }))
    .expect("valid Telegram configure command")
}

fn telegram_administrator_result(user_id: i64, is_bot: bool) -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "user": {
                "id": user_id,
                "is_bot": is_bot,
                "first_name": if is_bot { "Test Bot" } else { "Ada" }
            },
            "status": "administrator",
            "is_anonymous": false,
            "can_be_edited": false,
            "can_manage_chat": true,
            "can_change_info": true,
            "can_delete_messages": true,
            "can_manage_video_chats": true,
            "can_invite_users": true,
            "can_restrict_members": true,
            "can_pin_messages": true,
            "can_promote_members": true
        }
    })
}

fn telegram_member_result(user_id: i64) -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "user": {
                "id": user_id,
                "is_bot": false,
                "first_name": "Ada"
            },
            "status": "member"
        }
    })
}

fn group_anonymous_bot_json() -> Value {
    serde_json::json!({
        "id": GROUP_ANONYMOUS_BOT_ID,
        "is_bot": true,
        "first_name": "Group",
        "username": "GroupAnonymousBot"
    })
}

fn telegram_group_anonymous_member_result() -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "user": group_anonymous_bot_json(),
            "status": "member"
        }
    })
}

fn telegram_owner_result() -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "user": {
                "id": FROM_ID,
                "is_bot": false,
                "first_name": "Ada"
            },
            "status": "creator",
            "is_anonymous": false,
            "custom_title": null
        }
    })
}

fn telegram_message_result() -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "message_id": 88,
            "date": 1_710_000_001,
            "chat": {"id": CHAT_ID, "type": "supergroup"},
            "text": "configured"
        }
    })
}

fn configure_callback(wire: &str) -> CallbackQuery {
    serde_json::from_value(serde_json::json!({
        "id": "configure-callback",
        "from": {
            "id": FROM_ID,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "message": {
            "message_id": 88,
            "date": 1_710_000_001,
            "from": {
                "id": 9_999,
                "is_bot": true,
                "first_name": "Test Bot",
                "username": "TestBot"
            },
            "chat": {
                "id": CHAT_ID,
                "type": "supergroup",
                "title": "Parity Lab"
            },
            "reply_to_message": serde_json::to_value(configure_command())
                .expect("serialize original command"),
            "text": "好的。请在下面点击你想配置的选项进行操作吧。"
        },
        "chat_instance": "configure-chat-instance",
        "data": wire
    }))
    .expect("valid configure callback")
}

fn anonymous_configure_command() -> Message {
    let mut value = serde_json::to_value(configure_command()).expect("serialize command");
    value["from"] = group_anonymous_bot_json();
    serde_json::from_value(value).expect("anonymous configure command")
}

fn anonymous_configure_callback(wire: &str) -> CallbackQuery {
    let mut value = serde_json::to_value(configure_callback(wire)).expect("serialize callback");
    value["from"] = group_anonymous_bot_json();
    value["message"]["reply_to_message"]["from"] = group_anonymous_bot_json();
    serde_json::from_value(value).expect("anonymous configure callback")
}

fn anonymous_origin_configure_callback(wire: &str) -> CallbackQuery {
    let mut value = serde_json::to_value(configure_callback(wire)).expect("serialize callback");
    value["message"]["reply_to_message"]["from"] = group_anonymous_bot_json();
    serde_json::from_value(value).expect("anonymous-origin configure callback")
}

fn configure_callback_with_markup(wire: &str, markup: &Value) -> CallbackQuery {
    let mut value = serde_json::to_value(configure_callback(wire)).expect("serialize callback");
    value["message"]["reply_markup"] = markup.clone();
    serde_json::from_value(value).expect("configure callback with markup")
}

fn private_configure_callback(wire: &str, markup: &Value) -> CallbackQuery {
    let mut value = serde_json::to_value(configure_callback_with_markup(wire, markup))
        .expect("serialize callback");
    let private_chat = serde_json::json!({
        "id": PRIVATE_CHAT_ID,
        "type": "private",
        "first_name": "Ada"
    });
    value["message"]["chat"] = private_chat.clone();
    value["message"]["reply_to_message"]["chat"] = private_chat;
    serde_json::from_value(value).expect("private configure callback")
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
            "configure-test-key".to_owned(),
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
        (
            "HARD_LIMIT_MANUAL_RECAP_RATE_PER_SECONDS".to_owned(),
            "120".to_owned(),
        ),
        ("LOCALE".to_owned(), "zh-Hant".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("configure test config");
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
        RecapRuntimeDependencies {
            recap_state: Some(state),
            raw_telegram_http: reqwest::Client::new(),
            message_preprocessor: None,
        },
    )
}

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

#[tokio::test]
async fn configure_command_checks_bot_and_actor_and_keeps_missing_options_ephemeral() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(FROM_ID, false)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state).await;

    handle_configure_recap(
        context.config.telegram.bot(),
        configure_command(),
        bot_me(),
        context,
    )
    .await
    .expect("configure command");

    assert!(
        recap_options::find_one(&database, CHAT_ID)
            .await
            .expect("recap options lookup")
            .is_none(),
        "Go renders an in-memory default without inserting options"
    );
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
    assert_eq!(request_body(&requests[0])["user_id"], 9_999);
    assert_eq!(request_body(&requests[1])["user_id"], FROM_ID);
    let response = request_body(&requests[2]);
    assert!(
        response.get("parse_mode").is_none(),
        "Go sends the configure command response without parse_mode"
    );
    assert_eq!(
        response["text"],
        "好的。请在下面点击你想配置的选项进行操作吧。"
    );
    assert_eq!(response["reply_parameters"]["message_id"], 77);
    let markup: Value = match &response["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(
        markup["inline_keyboard"]
            .as_array()
            .expect("configure keyboard")
            .len(),
        5
    );
}

#[tokio::test]
async fn toggle_on_queues_and_toggle_off_retains_the_existing_member() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(FROM_ID, false)),
        )
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .expect(2)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
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
    .expect("disabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let toggle_on_wire = keyboard["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("toggle callback")
        .to_owned();
    let toggle_off_wire = keyboard["inline_keyboard"][1][1]["callback_data"]
        .as_str()
        .expect("toggle off callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state.clone()).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&toggle_on_wire),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("enable recap callback");

    assert!(
        feature_flags::has_recap_enabled(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("feature flag")
    );
    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("recap options")
        .expect("first enable options");
    assert_eq!(options.auto_recap_rates_per_day, 4);
    assert_eq!(options.auto_recap_send_mode, 0);
    assert!(!options.pin_auto_recap_message);
    let queue = state
        .raw_zset(keys::AUTO_RECAP_QUEUE_KEY)
        .expect("automatic recap queue");
    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].1, encode_auto_recap_member(CHAT_ID));
    let queued_before_disable = queue.clone();

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/EditMessageText",
        ]
    );
    let response = request_body(&requests[2]);
    assert!(
        response.get("parse_mode").is_none(),
        "Go sends toggle success without parse_mode"
    );
    assert_eq!(
        response["text"],
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾功能已开启，开启后将会自动收集群组中的聊天记录并定时发送聊天回顾快报。"
    );
    let markup: Value = match &response["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(
        markup["inline_keyboard"]
            .as_array()
            .expect("enabled keyboard")
            .len(),
        9
    );

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&toggle_off_wire),
        bot_me(),
        context,
    )
    .await
    .expect("disable recap callback");
    assert!(
        !feature_flags::has_recap_enabled(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("disabled feature flag")
    );
    assert_eq!(
        state.raw_zset(keys::AUTO_RECAP_QUEUE_KEY),
        Some(queued_before_disable),
        "Go leaves the old member for the worker to consume after disable"
    );
}

#[tokio::test]
async fn assign_private_mode_is_creator_only_and_does_not_queue() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_owner_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity Lab")
        .await
        .expect("enable recap");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("disabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wire = keyboard["inline_keyboard"][3][1]["callback_data"]
        .as_str()
        .expect("private-mode callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state.clone()).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("assign private mode callback");

    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("recap options")
        .expect("mode-only options row");
    assert_eq!(options.auto_recap_send_mode, 1);
    assert_eq!(
        options.auto_recap_rates_per_day, 0,
        "Go's mode-only create leaves the schema-default rate"
    );
    assert!(state.raw_zset(keys::AUTO_RECAP_QUEUE_KEY).is_none());
    let requests = server.received_requests().await.expect("Telegram requests");
    let response = request_body(&requests[2]);
    assert_eq!(
        response["text"],
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾模式已切换为<b>私聊</b>，将会自动收集群组中的聊天记录并定时发送聊天回顾快报给通过 /subscribe_recap 命令订阅了本群组聊天回顾用户。"
    );
    assert_eq!(response["parse_mode"], "HTML");
    let markup: Value = match &response["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(markup["inline_keyboard"][5][2]["text"], "🔘 4 次");
}

#[tokio::test]
async fn rate_change_rescores_one_member_even_when_recap_is_disabled() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_owner_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("create recap options");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let member = encode_auto_recap_member(CHAT_ID);
    state
        .auto_recap_zadd(&member, START_MS)
        .await
        .expect("plant old schedule");
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("enabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wire = keyboard["inline_keyboard"][5][0]["callback_data"]
        .as_str()
        .expect("two-per-day callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state.clone()).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("rate callback");

    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("recap options")
        .expect("recap options row");
    assert_eq!(options.auto_recap_rates_per_day, 2);
    let queue = state
        .raw_zset(keys::AUTO_RECAP_QUEUE_KEY)
        .expect("automatic recap queue");
    assert_eq!(queue.len(), 1, "ZADD rescores the deterministic member");
    assert_eq!(queue[0].1, member);
    assert_ne!(queue[0].0, START_MS);
    assert!(
        !feature_flags::has_recap_enabled(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("disabled feature flag"),
        "rate changes do not enable recap"
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    let response = request_body(&requests[2]);
    let markup: Value = match &response["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(
        markup["inline_keyboard"]
            .as_array()
            .expect("disabled keyboard")
            .len(),
        5
    );
    assert_eq!(
        response["text"],
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n每天自动创建聊天回顾的频率次数已设定为 <b>2</b>，将会自动收集群组中的聊天记录并在 <b>08:00</b>，<b>20:00</b> 发送聊天回顾快报。"
    );
}

#[tokio::test]
async fn pin_off_preserves_go_old_options_and_recap_status_wiring_bug() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_owner_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity Lab")
        .await
        .expect("enable recap");
    recap_options::set_pin_enabled(&database, CHAT_ID)
        .await
        .expect("enable recap pin");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: true,
        },
    )
    .await
    .expect("enabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wire = keyboard["inline_keyboard"][7][1]["callback_data"]
        .as_str()
        .expect("pin-off callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("pin-off callback");

    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("recap options")
        .expect("recap options row");
    assert!(!options.pin_auto_recap_message);
    assert!(
        feature_flags::has_recap_enabled(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("feature flag"),
        "pin mutation does not disable recap in storage"
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    let response = request_body(&requests[2]);
    assert!(
        response.get("parse_mode").is_none(),
        "Go sends pin success without parse_mode"
    );
    let markup: Value = match &response["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(
        markup["inline_keyboard"]
            .as_array()
            .expect("bug-compatible keyboard")
            .len(),
        5,
        "Go mistakenly passes pin status as recap status when rebuilding"
    );
    assert_eq!(
        response["text"],
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾消息置顶功能已关闭，关闭后将不会再收集群组中的聊天记录了。"
    );
}

#[tokio::test]
async fn complete_checks_only_actor_then_best_effort_deletes_settings_and_command() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(FROM_ID, false)),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(2)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
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
    .expect("disabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wire = keyboard["inline_keyboard"][4][0]["callback_data"]
        .as_str()
        .expect("complete callback")
        .to_owned();
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("complete callback");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/DeleteMessage",
        ],
        "complete does not recheck bot-admin status"
    );
    assert_eq!(request_body(&requests[1])["message_id"], 88);
    assert_eq!(request_body(&requests[2])["message_id"], 77);
}

#[tokio::test]
async fn ordinary_members_are_silently_ignored_by_every_config_mutation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_member_result(FROM_ID)))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("enabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wires = [
        &keyboard["inline_keyboard"][1][0]["callback_data"],
        &keyboard["inline_keyboard"][3][1]["callback_data"],
        &keyboard["inline_keyboard"][5][0]["callback_data"],
        &keyboard["inline_keyboard"][7][0]["callback_data"],
    ];
    let context = command_context(&server, database, state).await;

    for wire in wires {
        RecapHandlers::handle_callback_query_with_me(
            context.config.telegram.bot(),
            configure_callback(wire.as_str().expect("callback wire")),
            bot_me(),
            context.clone(),
        )
        .await
        .expect("ordinary member callback is silently ignored");
    }

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| request.url.path().ends_with("/EditMessageText"))
            .count(),
        0
    );
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.url.path().ends_with("/GetChatMember")
                    && request_body(request)["user_id"] == FROM_ID
            })
            .count(),
        7,
        "Go checks creator and then administrator separately for mode, rate, and pin"
    );
}

#[tokio::test]
async fn administrators_receive_go_creator_only_error_for_mode_rate_and_pin() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(FROM_ID, false)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("enabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wires = [
        &keyboard["inline_keyboard"][3][1]["callback_data"],
        &keyboard["inline_keyboard"][5][0]["callback_data"],
        &keyboard["inline_keyboard"][7][0]["callback_data"],
    ];
    let context = command_context(&server, database, state).await;

    for wire in wires {
        RecapHandlers::handle_callback_query_with_me(
            context.config.telegram.bot(),
            configure_callback_with_markup(wire.as_str().expect("callback wire"), &keyboard),
            bot_me(),
            context.clone(),
        )
        .await
        .expect("administrator receives creator-only edit");
    }

    let requests = server.received_requests().await.expect("Telegram requests");
    let actor_checks = requests
        .iter()
        .filter(|request| {
            request.url.path().ends_with("/GetChatMember")
                && request_body(request)["user_id"] == FROM_ID
        })
        .count();
    assert_eq!(actor_checks, 6);
    let edits = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/EditMessageText"))
        .collect::<Vec<_>>();
    assert_eq!(edits.len(), 3);
    // Go `processMessageError` (`pkg/bots/tgbot/handler.go:43-115`) reads
    // `MessageError.replyMarkup`/`parseMode` and re-applies them on the edit,
    // unlike the bare `ExceptionError` edits above. Lock in that this
    // creator-only edit — an HTML `MessageError` — keeps the caller's
    // keyboard, marking the boundary between the two error classes.
    for edit in edits {
        let body = request_body(edit);
        assert_eq!(
            body["text"],
            "好的。请在下面点击你想配置的选项进行操作吧。\n\n抱歉，此操作无法进行，抱歉，此操作无法进行，只有<b>群组创建者</b>角色可以配置聊天记录回顾的模式。"
        );
        assert_eq!(body["parse_mode"], "HTML");
        let retained_markup: Value = match &body["reply_markup"] {
            Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
            value => value.clone(),
        };
        assert_eq!(retained_markup, keyboard);
    }
}

#[tokio::test]
async fn group_anonymous_bot_is_looked_up_before_every_admin_exception() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(
            serde_json::json!({"user_id": GROUP_ANONYMOUS_BOT_ID}),
        ))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_group_anonymous_member_result()),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    handle_configure_recap(
        context.config.telegram.bot(),
        anonymous_configure_command(),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("anonymous configure command");

    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: GROUP_ANONYMOUS_BOT_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("anonymous enabled keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let toggle_off = keyboard["inline_keyboard"][1][1]["callback_data"]
        .as_str()
        .expect("toggle off wire");
    let complete = keyboard["inline_keyboard"][8][0]["callback_data"]
        .as_str()
        .expect("complete wire");

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        anonymous_configure_callback(toggle_off),
        bot_me(),
        context.clone(),
    )
    .await
    .expect("anonymous toggle callback");
    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        anonymous_configure_callback(complete),
        bot_me(),
        context,
    )
    .await
    .expect("anonymous complete callback");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.url.path().ends_with("/GetChatMember")
                    && request_body(request)["user_id"] == GROUP_ANONYMOUS_BOT_ID
            })
            .count(),
        3,
        "Go queries membership before applying each GroupAnonymousBot exception"
    );
}

#[tokio::test]
async fn anonymous_original_command_bypasses_complete_payload_actor_binding() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_owner_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(2)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: GROUP_ANONYMOUS_BOT_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("anonymous-origin keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let complete = keyboard["inline_keyboard"][8][0]["callback_data"]
        .as_str()
        .expect("complete wire");
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        anonymous_origin_configure_callback(complete),
        bot_me(),
        context,
    )
    .await
    .expect("anonymous-origin complete callback");
}

/// Go `processExceptionError` (`pkg/bots/tgbot/handler.go:117-156`) builds the
/// `ExceptionError` edit branch as a bare `NewEditMessageText(chatID,
/// editMessage.MessageID, message)`: it never reads `ExceptionError.replyMarkup`
/// or applies a parse mode, even though the callback_query.go call sites pass
/// `WithReplyMarkup(...)`. The incoming message here still carries a keyboard,
/// so this locks in that the wire edit drops it rather than falling back to
/// reusing it.
#[tokio::test]
async fn expired_toggle_edits_a_bare_wire_error_dropping_the_existing_keyboard() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let clock = Arc::new(TestClock::new(START_MS));
    let state = Arc::new(InMemoryRecapStateStore::new(clock.clone()));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("enabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wire = keyboard["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("toggle callback")
        .to_owned();
    clock.advance_ms(86_400_000);
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback_with_markup(&wire, &keyboard),
        bot_me(),
        context,
    )
    .await
    .expect("expired toggle callback");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(requests.len(), 1);
    let edit = request_body(&requests[0]);
    assert_eq!(
        edit["text"],
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n应用聊天记录回顾功能的配置时出现了问题，请稍后再试！"
    );
    assert!(
        edit.get("parse_mode").is_none(),
        "Go's ExceptionError edit has no parse mode"
    );
    assert!(
        edit.get("reply_markup").is_none(),
        "Go's ExceptionError edit never reads replyMarkup and drops the existing keyboard"
    );
}

// ---------------------------------------------------------------------------
// Stage-specific configuration failures, Go `callback_query.go:95-641`.
//
// Go routes every post-permission failure through `ExceptionError`. Six of
// those branches call `WithEdit(c.Update.Message)`, and in a callback update
// that message is `nil`: `WithEdit(nil)` is a silent no-op, so
// `processExceptionError` falls through to a brand-new plain `SendMessage`
// without keyboard, reply target, or parse mode. Every other branch passes the
// callback message and stays an `EditMessageText` with its stage text.
// ---------------------------------------------------------------------------

/// Delegates every read to the wrapped in-memory store but fails each
/// `put_callback`, so keyboard reconstruction breaks while the route lookup
/// for the already-built configuration keyboard still resolves.
struct FailingPutRecapStateStore {
    inner: Arc<InMemoryRecapStateStore>,
}

#[async_trait]
impl RecapStateStore for FailingPutRecapStateStore {
    async fn put_callback(&self, _route: &str, _payload_json: &str) -> anyhow::Result<String> {
        anyhow::bail!("simulated Redis failure while storing a callback payload")
    }

    async fn get_callback(&self, route: &str, action_hash: &str) -> anyhow::Result<Option<String>> {
        self.inner.get_callback(route, action_hash).await
    }

    async fn check_manual_recap_rate(
        &self,
        chat_id: i64,
        rate: i64,
        per_seconds: i64,
    ) -> anyhow::Result<ManualRecapRateResult> {
        self.inner
            .check_manual_recap_rate(chat_id, rate, per_seconds)
            .await
    }

    async fn put_start_context(
        &self,
        domain: keys::StartContextDomain,
        token: &str,
        json: &str,
    ) -> anyhow::Result<()> {
        self.inner.put_start_context(domain, token, json).await
    }

    async fn get_start_context(
        &self,
        domain: keys::StartContextDomain,
        token: &str,
    ) -> anyhow::Result<Option<String>> {
        self.inner.get_start_context(domain, token).await
    }

    async fn forwarded_active(&self, user_id: i64) -> anyhow::Result<bool> {
        self.inner.forwarded_active(user_id).await
    }

    async fn start_forwarded(&self, user_id: i64) -> anyhow::Result<()> {
        self.inner.start_forwarded(user_id).await
    }

    async fn append_forwarded(
        &self,
        user_id: i64,
        score_ms: i64,
        json: &str,
    ) -> anyhow::Result<()> {
        self.inner.append_forwarded(user_id, score_ms, json).await
    }

    async fn forwarded_batch(&self, user_id: i64) -> anyhow::Result<Vec<String>> {
        self.inner.forwarded_batch(user_id).await
    }

    async fn cancel_forwarded(&self, user_id: i64) -> anyhow::Result<bool> {
        self.inner.cancel_forwarded(user_id).await
    }

    async fn push_delete_later(
        &self,
        user_id: i64,
        chat_id: i64,
        message_id: i32,
    ) -> anyhow::Result<()> {
        self.inner
            .push_delete_later(user_id, chat_id, message_id)
            .await
    }

    async fn drain_delete_later(&self, user_id: i64) -> anyhow::Result<Vec<(i64, i32)>> {
        self.inner.drain_delete_later(user_id).await
    }

    async fn auto_recap_zadd(&self, member: &str, score_ms: i64) -> anyhow::Result<()> {
        self.inner.auto_recap_zadd(member, score_ms).await
    }

    async fn auto_recap_zpop_due(&self, now_ms: i64) -> anyhow::Result<Option<String>> {
        self.inner.auto_recap_zpop_due(now_ms).await
    }

    async fn auto_recap_zrem(&self, member: &str) -> anyhow::Result<()> {
        self.inner.auto_recap_zrem(member).await
    }
}

/// Delegates every call to the wrapped in-memory store but fails
/// `auto_recap_zadd`, so the automatic-recap queue write breaks while
/// callback routing, DB mutations, and keyboard rebuilds stay usable.
struct FailingZaddRecapStateStore {
    inner: Arc<InMemoryRecapStateStore>,
}

#[async_trait]
impl RecapStateStore for FailingZaddRecapStateStore {
    async fn put_callback(&self, route: &str, payload_json: &str) -> anyhow::Result<String> {
        self.inner.put_callback(route, payload_json).await
    }

    async fn get_callback(&self, route: &str, action_hash: &str) -> anyhow::Result<Option<String>> {
        self.inner.get_callback(route, action_hash).await
    }

    async fn check_manual_recap_rate(
        &self,
        chat_id: i64,
        rate: i64,
        per_seconds: i64,
    ) -> anyhow::Result<ManualRecapRateResult> {
        self.inner
            .check_manual_recap_rate(chat_id, rate, per_seconds)
            .await
    }

    async fn put_start_context(
        &self,
        domain: keys::StartContextDomain,
        token: &str,
        json: &str,
    ) -> anyhow::Result<()> {
        self.inner.put_start_context(domain, token, json).await
    }

    async fn get_start_context(
        &self,
        domain: keys::StartContextDomain,
        token: &str,
    ) -> anyhow::Result<Option<String>> {
        self.inner.get_start_context(domain, token).await
    }

    async fn forwarded_active(&self, user_id: i64) -> anyhow::Result<bool> {
        self.inner.forwarded_active(user_id).await
    }

    async fn start_forwarded(&self, user_id: i64) -> anyhow::Result<()> {
        self.inner.start_forwarded(user_id).await
    }

    async fn append_forwarded(
        &self,
        user_id: i64,
        score_ms: i64,
        json: &str,
    ) -> anyhow::Result<()> {
        self.inner.append_forwarded(user_id, score_ms, json).await
    }

    async fn forwarded_batch(&self, user_id: i64) -> anyhow::Result<Vec<String>> {
        self.inner.forwarded_batch(user_id).await
    }

    async fn cancel_forwarded(&self, user_id: i64) -> anyhow::Result<bool> {
        self.inner.cancel_forwarded(user_id).await
    }

    async fn push_delete_later(
        &self,
        user_id: i64,
        chat_id: i64,
        message_id: i32,
    ) -> anyhow::Result<()> {
        self.inner
            .push_delete_later(user_id, chat_id, message_id)
            .await
    }

    async fn drain_delete_later(&self, user_id: i64) -> anyhow::Result<Vec<(i64, i32)>> {
        self.inner.drain_delete_later(user_id).await
    }

    async fn auto_recap_zadd(&self, _member: &str, _score_ms: i64) -> anyhow::Result<()> {
        anyhow::bail!("simulated Redis failure while scheduling automatic recap")
    }

    async fn auto_recap_zpop_due(&self, now_ms: i64) -> anyhow::Result<Option<String>> {
        self.inner.auto_recap_zpop_due(now_ms).await
    }

    async fn auto_recap_zrem(&self, member: &str) -> anyhow::Result<()> {
        self.inner.auto_recap_zrem(member).await
    }
}

/// Mount the bot-admin check, one actor membership result, and permissive
/// `SendMessage`/`EditMessageText` sinks so wrong-branch responses are
/// recorded instead of erroring out of the handler.
async fn mount_callback_stage_mocks(server: &MockServer, actor_result: Value) {
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(actor_result))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(server)
        .await;
}

/// Build the configuration keyboard against `state` and return one wire value.
async fn built_wire(
    state: &InMemoryRecapStateStore,
    recap_enabled: bool,
    row: usize,
    column: usize,
) -> String {
    let keyboard = build_configure_keyboard(
        state,
        ConfigureRecapView {
            chat_id: CHAT_ID,
            from_id: FROM_ID,
            recap_enabled,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    keyboard["inline_keyboard"][row][column]["callback_data"]
        .as_str()
        .expect("callback wire")
        .to_owned()
}

fn suffix_count(requests: &[wiremock::Request], suffix: &str) -> usize {
    requests
        .iter()
        .filter(|request| request.url.path().ends_with(suffix))
        .count()
}

fn single_request<'a>(requests: &'a [wiremock::Request], suffix: &str) -> &'a wiremock::Request {
    let matching = requests
        .iter()
        .filter(|request| request.url.path().ends_with(suffix))
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "exactly one {suffix} request expected");
    matching[0]
}

/// Go `processExceptionError` fallback: a brand-new plain message.
fn assert_plain_new_message(request: &wiremock::Request, text: &str) {
    let body = request_body(request);
    assert_eq!(body["text"], text);
    assert_eq!(body["chat_id"], CHAT_ID);
    assert!(
        body.get("parse_mode").is_none(),
        "Go's fallback message has no parse mode"
    );
    assert!(
        body.get("reply_markup").is_none(),
        "Go's fallback message has no keyboard"
    );
    assert!(
        body.get("reply_parameters").is_none(),
        "Go's fallback message is not a reply"
    );
}

/// A stage-text `ExceptionError` edit of the configuration message.
fn assert_stage_edit(request: &wiremock::Request, text: &str) {
    let body = request_body(request);
    assert_eq!(body["text"], text);
    assert_eq!(body["message_id"], 88);
    assert!(
        body.get("parse_mode").is_none(),
        "stage errors are plain-text edits"
    );
}

#[tokio::test]
async fn toggle_options_lookup_failure_sends_a_plain_new_message() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_administrator_result(FROM_ID, false)).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), false, 1, 0).await;
    let context = command_context(&server, database.clone(), state).await;
    database.pool.close().await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("toggle callback with a failing options lookup");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        suffix_count(&requests, "/EditMessageText"),
        0,
        "Go WithEdit(nil) never edits the callback message"
    );
    assert_plain_new_message(
        single_request(&requests, "/SendMessage"),
        "暂时无法配置聊天记录回顾功能，请稍后再试！",
    );
}

#[tokio::test]
async fn toggle_keyboard_failure_sends_a_plain_new_message_after_the_mutation() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_administrator_result(FROM_ID, false)).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let inner = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(inner.as_ref(), false, 1, 0).await;
    let state = Arc::new(FailingPutRecapStateStore {
        inner: inner.clone(),
    });
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("toggle callback with a failing keyboard rebuild");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/EditMessageText"), 0);
    assert_plain_new_message(
        single_request(&requests, "/SendMessage"),
        "暂时无法配置聊天记录回顾功能，请稍后再试！",
    );
    assert!(
        feature_flags::has_recap_enabled(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("feature flag read"),
        "the enable mutation lands before the keyboard rebuild fails"
    );
}

#[tokio::test]
async fn mode_options_lookup_failure_sends_a_plain_new_message() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    // The mode update trips this trigger, which corrupts the row so only the
    // handler's follow-up options reload fails while the mutation succeeds.
    sqlx::query(
        "CREATE TRIGGER poison_recap_options
             AFTER UPDATE OF auto_recap_send_mode ON telegram_chat_recaps_options
         BEGIN
             UPDATE telegram_chat_recaps_options
                 SET pin_auto_recap_message = 'poisoned'
                 WHERE chat_id = NEW.chat_id;
         END",
    )
    .execute(&database.pool)
    .await
    .expect("options poison trigger");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), false, 3, 1).await;
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("mode callback with a failing options reload");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/EditMessageText"), 0);
    assert_plain_new_message(
        single_request(&requests, "/SendMessage"),
        "暂时无法配置聊天记录回顾功能，请稍后再试！",
    );
}

#[tokio::test]
async fn rates_options_lookup_failure_sends_the_rate_stage_text_as_a_new_message() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    sqlx::query(
        "CREATE TRIGGER poison_recap_options
             AFTER UPDATE OF auto_recap_rates_per_day ON telegram_chat_recaps_options
         BEGIN
             UPDATE telegram_chat_recaps_options
                 SET pin_auto_recap_message = 'poisoned'
                 WHERE chat_id = NEW.chat_id;
         END",
    )
    .execute(&database.pool)
    .await
    .expect("options poison trigger");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), true, 5, 0).await;
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("rates callback with a failing options reload");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/EditMessageText"), 0);
    // Unlike the other nil-edit branches, Go keeps the configuration header
    // and the rate stage text on this new message.
    assert_plain_new_message(
        single_request(&requests, "/SendMessage"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n每天自动创建回顾频率次数设定失败，请稍后再试！",
    );
}

#[tokio::test]
async fn pin_options_lookup_failure_sends_a_plain_new_message() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), true, 7, 0).await;
    let context = command_context(&server, database.clone(), state).await;
    database.pool.close().await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("pin callback with a failing options lookup");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/EditMessageText"), 0);
    assert_plain_new_message(
        single_request(&requests, "/SendMessage"),
        "暂时无法配置聊天记录回顾消息置顶功能，请稍后再试！",
    );
}

#[tokio::test]
async fn pin_keyboard_failure_sends_a_plain_new_message_after_the_mutation() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    let inner = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(inner.as_ref(), true, 7, 0).await;
    let state = Arc::new(FailingPutRecapStateStore {
        inner: inner.clone(),
    });
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("pin callback with a failing keyboard rebuild");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/EditMessageText"), 0);
    assert_plain_new_message(
        single_request(&requests, "/SendMessage"),
        "暂时无法配置聊天记录回顾消息置顶功能，请稍后再试！",
    );
    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("options read")
        .expect("options row");
    assert!(
        options.pin_auto_recap_message,
        "the pin mutation lands before the keyboard rebuild fails"
    );
}

#[tokio::test]
async fn mode_keyboard_failure_edits_with_the_bare_unavailable_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    let inner = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(inner.as_ref(), false, 3, 1).await;
    let state = Arc::new(FailingPutRecapStateStore {
        inner: inner.clone(),
    });
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("mode callback with a failing keyboard rebuild");

    let requests = server.received_requests().await.expect("Telegram requests");
    // Go passes the callback message to WithEdit here, so unlike the toggle
    // and pin keyboard failures this stays an edit.
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "暂时无法配置聊天记录回顾功能，请稍后再试！",
    );
}

#[tokio::test]
async fn toggle_enable_failure_edits_the_stage_specific_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_administrator_result(FROM_ID, false)).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    sqlx::query("DROP TABLE telegram_chat_feature_flags")
        .execute(&database.pool)
        .await
        .expect("drop the feature flag table");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), false, 1, 0).await;
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("toggle callback with a failing enable mutation");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾功能开启失败，请稍后再试！",
    );
    assert!(
        recap_options::find_one(&database, CHAT_ID)
            .await
            .expect("options read")
            .is_some(),
        "the options row is materialised before the enable mutation fails"
    );
}

#[tokio::test]
async fn toggle_disable_failure_edits_the_stage_specific_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_administrator_result(FROM_ID, false)).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    sqlx::query("DROP TABLE telegram_chat_feature_flags")
        .execute(&database.pool)
        .await
        .expect("drop the feature flag table");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), false, 1, 1).await;
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("toggle callback with a failing disable mutation");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾功能关闭失败，请稍后再试！",
    );
}

#[tokio::test]
async fn mode_feature_lookup_failure_edits_the_stage_specific_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    sqlx::query("DROP TABLE telegram_chat_feature_flags")
        .execute(&database.pool)
        .await
        .expect("drop the feature flag table");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), false, 3, 1).await;
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("mode callback with a failing feature lookup");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾模式设定失败，请稍后再试！",
    );
    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("options read")
        .expect("options row");
    assert_eq!(
        options.auto_recap_send_mode, 1,
        "the mode mutation lands before the feature lookup fails"
    );
}

#[tokio::test]
async fn rate_mutation_failure_edits_the_rate_stage_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), true, 5, 1).await;
    let context = command_context(&server, database.clone(), state).await;
    database.pool.close().await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("rates callback with a failing rate mutation");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n每天自动创建回顾频率次数设定失败，请稍后再试！",
    );
}

// ---------------------------------------------------------------------------
// Automatic-recap queue write failures, ADR 0001 decision 2.
//
// Go surfaces a failed `QueueOneSendChatHistoriesRecapTaskForChatID` write
// during toggle-enable or rate-change as that stage's `ExceptionError` edit
// (`callback_query.go:170-177`, `callback_query.go:487-494`), matching every
// other post-permission failure that passes the callback message to
// `WithEdit`. The DB mutation always lands first; the queue write failing
// afterward must not rebuild the keyboard or emit the success text.
// ---------------------------------------------------------------------------

#[tokio::test]
async fn toggle_enable_queue_failure_edits_the_stage_specific_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_administrator_result(FROM_ID, false)).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let inner = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(inner.as_ref(), false, 1, 0).await;
    let state = Arc::new(FailingZaddRecapStateStore {
        inner: inner.clone(),
    });
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("toggle callback with a failing automatic recap queue write");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    let edit = single_request(&requests, "/EditMessageText");
    assert_stage_edit(
        edit,
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾功能开启失败，请稍后再试！",
    );
    assert!(
        request_body(edit).get("reply_markup").is_none(),
        "the queue-write stage error is a bare edit with no keyboard"
    );
    assert!(
        feature_flags::has_recap_enabled(&database, CHAT_ID, "Parity Lab")
            .await
            .expect("feature flag read"),
        "the enable mutation lands before the automatic recap queue write fails"
    );
}

#[tokio::test]
async fn rates_queue_failure_edits_the_rate_stage_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let inner = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(inner.as_ref(), true, 5, 1).await;
    let state = Arc::new(FailingZaddRecapStateStore {
        inner: inner.clone(),
    });
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("rates callback with a failing automatic recap queue write");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    let edit = single_request(&requests, "/EditMessageText");
    assert_stage_edit(
        edit,
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n每天自动创建回顾频率次数设定失败，请稍后再试！",
    );
    assert!(
        request_body(edit).get("reply_markup").is_none(),
        "the queue-write stage error is a bare edit with no keyboard"
    );
    let options = recap_options::find_one(&database, CHAT_ID)
        .await
        .expect("options read")
        .expect("options row");
    assert_eq!(
        options.auto_recap_rates_per_day, 3,
        "the rate mutation lands before the automatic recap queue write fails"
    );
}

#[tokio::test]
async fn pin_enable_failure_edits_the_pin_stage_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    sqlx::query(
        "CREATE TRIGGER block_pin_update
             BEFORE UPDATE OF pin_auto_recap_message ON telegram_chat_recaps_options
         BEGIN
             SELECT RAISE(ABORT, 'pin update blocked');
         END",
    )
    .execute(&database.pool)
    .await
    .expect("pin update trigger");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), true, 7, 0).await;
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("pin callback with a failing enable mutation");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾消息置顶功能开启失败，请稍后再试！",
    );
}

#[tokio::test]
async fn pin_disable_failure_edits_the_pin_stage_text() {
    let server = MockServer::start().await;
    mount_callback_stage_mocks(&server, telegram_owner_result()).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("seed recap options");
    recap_options::set_pin_enabled(&database, CHAT_ID)
        .await
        .expect("seed pin enabled");
    sqlx::query(
        "CREATE TRIGGER block_pin_update
             BEFORE UPDATE OF pin_auto_recap_message ON telegram_chat_recaps_options
         BEGIN
             SELECT RAISE(ABORT, 'pin update blocked');
         END",
    )
    .execute(&database.pool)
    .await
    .expect("pin update trigger");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let wire = built_wire(state.as_ref(), true, 7, 1).await;
    let context = command_context(&server, database, state).await;

    RecapHandlers::handle_callback_query_with_me(
        context.config.telegram.bot(),
        configure_callback(&wire),
        bot_me(),
        context,
    )
    .await
    .expect("pin callback with a failing disable mutation");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(suffix_count(&requests, "/SendMessage"), 0);
    assert_stage_edit(
        single_request(&requests, "/EditMessageText"),
        "好的。请在下面点击你想配置的选项进行操作吧。\n\n聊天记录回顾消息置顶功能关闭失败，请稍后再试！",
    );
}

#[tokio::test]
async fn private_config_callbacks_stop_after_bot_admin_check_without_mutation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": 9_999})))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_administrator_result(9_999, true)),
        )
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .and(body_partial_json(serde_json::json!({"user_id": FROM_ID})))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_owner_result()))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_message_result()))
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let keyboard = build_configure_keyboard(
        state.as_ref(),
        ConfigureRecapView {
            chat_id: PRIVATE_CHAT_ID,
            from_id: FROM_ID,
            recap_enabled: true,
            send_mode: 0,
            rates_per_day: 4,
            pin_enabled: false,
        },
    )
    .await
    .expect("enabled configure keyboard");
    let keyboard = serde_json::to_value(keyboard).expect("serialize configure keyboard");
    let wires = [
        &keyboard["inline_keyboard"][1][0]["callback_data"],
        &keyboard["inline_keyboard"][3][1]["callback_data"],
        &keyboard["inline_keyboard"][5][0]["callback_data"],
        &keyboard["inline_keyboard"][7][0]["callback_data"],
    ];
    let context = command_context(&server, database.clone(), state).await;

    for wire in wires {
        RecapHandlers::handle_callback_query_with_me(
            context.config.telegram.bot(),
            private_configure_callback(wire.as_str().expect("callback wire"), &keyboard),
            bot_me(),
            context.clone(),
        )
        .await
        .expect("private callback denial");
    }

    assert!(
        recap_options::find_one(&database, PRIVATE_CHAT_ID)
            .await
            .expect("recap options lookup")
            .is_none(),
        "Go rejects the chat before any configuration write"
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .filter(|request| {
                request.url.path().ends_with("/GetChatMember")
                    && request_body(request)["user_id"] == FROM_ID
            })
            .count(),
        0,
        "Go checks chat type before actor membership"
    );
    let edits = requests
        .iter()
        .filter(|request| request.url.path().ends_with("/EditMessageText"))
        .collect::<Vec<_>>();
    assert_eq!(edits.len(), 4);
    for edit in edits {
        let body = request_body(edit);
        assert_eq!(body["parse_mode"], "HTML");
        assert_eq!(
            body["text"],
            "好的。请在下面点击你想配置的选项进行操作吧。\n\n抱歉，此操作无法进行，聊天记录回顾功能只有<b>群组</b>和<b>超级群组</b>的管理员可以配置哦！\n请将 Bot 添加到群组中，并配置 Bot 为管理员后使用管理员权限的用户账户为 Bot 进行配置吧。"
        );
    }
}
