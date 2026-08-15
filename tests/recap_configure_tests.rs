//! `/configure_recap` keyboard parity against Go v1.0.0 `02aee8ce`.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

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
        recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
    services::{
        autorecap_queue::encode_auto_recap_member,
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
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
        CommandRateLimiter::new(1, Duration::from_secs(1)),
        None,
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
            configure_callback(wire.as_str().expect("callback wire")),
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
    for edit in edits {
        assert_eq!(
            request_body(edit)["text"],
            "好的。请在下面点击你想配置的选项进行操作吧。\n\n抱歉，此操作无法进行，抱歉，此操作无法进行，只有<b>群组创建者</b>角色可以配置聊天记录回顾的模式。"
        );
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

#[tokio::test]
async fn expired_toggle_edits_plain_error_and_preserves_the_existing_keyboard() {
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
    assert!(edit.get("parse_mode").is_none());
    let retained_markup: Value = match &edit["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(retained_markup, keyboard);
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
