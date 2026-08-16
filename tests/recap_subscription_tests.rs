//! Private recap and subscription parity tests against Go v1.0.0 `02aee8ce`.

mod support;

use std::{collections::BTreeMap, sync::Arc};

use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::{
            recap::RecapHandlers,
            recap_manual::AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS,
            recap_subscription::{
                build_subscriber_vote_keyboard, encode_start_context, handle_chat_member_left,
                handle_start_continuation, handle_subscribe_recap_command,
                handle_unsubscribe_recap_command, is_group_anonymous_bot,
            },
            system::SystemHandlers,
        },
    },
    config::AppConfig,
    db::{
        Database, feature_flags,
        models::{AutoRecapSendMode, ReactionCounts},
        recap_options, subscribers,
    },
    i18n::I18n,
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
    services::{openai::OpenAiClient, rate_limit::GoRateLimiter},
};
use serde_json::Value;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{CallbackQuery, Me, Message, User};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const START_MS: i64 = 1_700_000_000_000;
const CHAT_ID: i64 = -1_001_234_567_890;
const LOG_ID: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

fn user(value: serde_json::Value) -> User {
    serde_json::from_value(value).expect("valid Telegram user fixture")
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

fn command_message() -> Message {
    group_command("/recap")
}

fn group_command(text: &str) -> Message {
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
            "title": "Parity <Lab> & Friends"
        },
        "text": text
    }))
    .expect("valid Telegram command fixture")
}

fn anonymous_group_command(text: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 78,
        "date": 1_710_000_001,
        "from": {
            "id": 1_087_968_824_i64,
            "is_bot": true,
            "first_name": "Group",
            "username": "GroupAnonymousBot"
        },
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity <Lab> & Friends"
        },
        "text": text
    }))
    .expect("valid anonymous Telegram command fixture")
}

fn private_command(text: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 95,
        "date": 1_710_000_013,
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "chat": {"id": 42, "type": "private", "first_name": "Ada"},
        "text": text
    }))
    .expect("valid Telegram private command fixture")
}

fn start_message(token: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 91,
        "date": 1_710_000_010,
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "chat": {"id": 42, "type": "private", "first_name": "Ada"},
        "text": format!("/start {token}")
    }))
    .expect("valid Telegram start fixture")
}

/// A private-chat `/help` message from a sender whose Telegram
/// `language_code` is `language_code` (omitted entirely when `None`,
/// matching a client that reports nothing).
fn help_message(language_code: Option<&str>) -> Message {
    let mut from = serde_json::json!({
        "id": 42,
        "is_bot": false,
        "first_name": "Ada",
        "username": "ada"
    });
    if let Some(code) = language_code {
        from["language_code"] = Value::String(code.to_owned());
    }
    serde_json::from_value(serde_json::json!({
        "message_id": 91,
        "date": 1_710_000_010,
        "from": from,
        "chat": {"id": 42, "type": "private", "first_name": "Ada"},
        "text": "/help"
    }))
    .expect("valid Telegram help fixture")
}

fn group_start_message(text: &str) -> Message {
    let command_length = text
        .split_whitespace()
        .next()
        .expect("start command token")
        .encode_utf16()
        .count();
    serde_json::from_value(serde_json::json!({
        "message_id": 93,
        "date": 1_710_000_011,
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity <Lab> & Friends"
        },
        "text": text,
        "entities": [{
            "type": "bot_command",
            "offset": 0,
            "length": command_length
        }]
    }))
    .expect("valid Telegram group start fixture")
}

fn left_member_message(left_user: Value) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 92,
        "date": 1_710_000_020,
        "from": {
            "id": 7,
            "is_bot": false,
            "first_name": "Owner"
        },
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity <Lab> & Friends"
        },
        "left_chat_member": left_user
    }))
    .expect("valid Telegram left-member fixture")
}

fn unsubscribe_callback(wire: &str, markup: &Value) -> CallbackQuery {
    unsubscribe_callback_from(wire, markup, 42)
}

fn unsubscribe_callback_from(wire: &str, markup: &Value, from_id: i64) -> CallbackQuery {
    serde_json::from_value(serde_json::json!({
        "id": "unsubscribe-callback",
        "from": {
            "id": from_id,
            "is_bot": false,
            "first_name": "Ada",
            "username": "ada"
        },
        "message": {
            "message_id": 401,
            "date": 1_710_000_030,
            "chat": {"id": 42, "type": "private", "first_name": "Ada"},
            "text": "Rich recap",
            "reply_markup": markup
        },
        "chat_instance": "subscription-chat-instance",
        "data": wire
    }))
    .expect("valid unsubscribe callback fixture")
}

fn telegram_message_result(message_id: i32, chat_id: i64, chat_type: &str, text: &str) -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "message_id": message_id,
            "date": 1_710_000_001,
            "chat": {"id": chat_id, "type": chat_type},
            "text": text
        }
    })
}

fn telegram_administrator_result() -> Value {
    serde_json::json!({
        "ok": true,
        "result": {
            "user": {
                "id": 9_999,
                "is_bot": true,
                "first_name": "Test Bot",
                "username": "TestBot"
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

fn telegram_member_result() -> Value {
    serde_json::json!({
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

async fn command_context(
    server: &MockServer,
    database: Database,
    state: Arc<dyn RecapStateStore>,
) -> Arc<AppContext> {
    command_context_with_insights_lang(server, database, state, None).await
}

/// Like [`command_context`], but sets `INSIGHTS_LANG` to `insights_lang`
/// when given, pinning `AppConfig::locale` away from the default `en`. Used
/// to prove that a message's own Telegram `language_code` overrides the
/// configured fallback locale rather than merely matching it by
/// coincidence.
async fn command_context_with_insights_lang(
    server: &MockServer,
    database: Database,
    state: Arc<dyn RecapStateStore>,
    insights_lang: Option<&str>,
) -> Arc<AppContext> {
    let mut values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "test-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram", server.uri()),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "subscription-test-key".to_owned(),
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
    if let Some(insights_lang) = insights_lang {
        values.insert("INSIGHTS_LANG".to_owned(), insights_lang.to_owned());
    }
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("subscription test config");
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

async fn private_mode_database() -> Database {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity <Lab> & Friends")
        .await
        .expect("enable recap");
    recap_options::set_send_mode(
        &database,
        CHAT_ID,
        AutoRecapSendMode::OnlyPrivateSubscriptions,
    )
    .await
    .expect("private recap mode");
    database
}

/// Force every future subscriber insert to fail, so the DM-succeeds-then-DB-fails
/// branch can be exercised without a mock database layer.
async fn block_subscriber_inserts(database: &Database) {
    sqlx::query(
        "CREATE TRIGGER block_subscriber_insert
         BEFORE INSERT ON telegram_chat_auto_recaps_subscribers
         BEGIN
             SELECT RAISE(ABORT, 'forced insert failure for parity test');
         END;",
    )
    .execute(&database.pool)
    .await
    .expect("install insert-blocking trigger");
}

/// Force every future subscriber delete to fail, so the unsubscribe DB-failure
/// branch can be exercised without a mock database layer.
async fn block_subscriber_deletes(database: &Database) {
    sqlx::query(
        "CREATE TRIGGER block_subscriber_delete
         BEFORE DELETE ON telegram_chat_auto_recaps_subscribers
         BEGIN
             SELECT RAISE(ABORT, 'forced delete failure for parity test');
         END;",
    )
    .execute(&database.pool)
    .await
    .expect("install delete-blocking trigger");
}

#[test]
fn start_context_json_matches_go_field_order_and_html_escaping() {
    assert_eq!(
        encode_start_context(CHAT_ID, "A <B> & C\u{2028}D").expect("start context JSON"),
        r#"{"chat_id":-1001234567890,"chat_title":"A \u003cB\u003e \u0026 C\u2028D"}"#
    );
}

#[test]
fn group_anonymous_bot_requires_go_exact_identity_tuple() {
    let anonymous = user(serde_json::json!({
        "id": 1_087_968_824,
        "is_bot": true,
        "first_name": "Group",
        "username": "GroupAnonymousBot"
    }));
    assert!(is_group_anonymous_bot(&anonymous));

    for changed in [
        serde_json::json!({
            "id": 1_087_968_825,
            "is_bot": true,
            "first_name": "Group",
            "username": "GroupAnonymousBot"
        }),
        serde_json::json!({
            "id": 1_087_968_824,
            "is_bot": false,
            "first_name": "Group",
            "username": "GroupAnonymousBot"
        }),
        serde_json::json!({
            "id": 1_087_968_824,
            "is_bot": true,
            "first_name": "Not Group",
            "username": "GroupAnonymousBot"
        }),
        serde_json::json!({
            "id": 1_087_968_824,
            "is_bot": true,
            "first_name": "Group",
            "username": "OtherBot"
        }),
    ] {
        assert!(!is_group_anonymous_bot(&user(changed)));
    }
}

#[tokio::test]
async fn administrator_group_bare_start_is_silent_before_context_dispatch() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_administrator_result()))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let token = "group-bare-start";
    state
        .put_start_context(
            keys::StartContextDomain::PrivateSubscription,
            token,
            &encode_start_context(CHAT_ID, "Should Not Dispatch").expect("start context"),
        )
        .await
        .expect("store start context");
    let context = command_context(&server, database, state).await;

    SystemHandlers::handle_start(
        context.config.telegram.bot(),
        group_start_message(&format!("/start {token}")),
        token.to_owned(),
        bot_me(),
        context,
    )
    .await
    .expect("bare administrator-group start");

    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1, "Go stops after the administrator lookup");
    assert_eq!(
        requests[0].url.path(),
        "/telegram/bottest-token/GetChatMember"
    );
    let lookup = request_body(&requests[0]);
    assert_eq!(lookup["chat_id"], CHAT_ID);
    assert_eq!(lookup["user_id"], 9_999);
}

#[tokio::test]
async fn administrator_group_addressed_start_dispatches_the_private_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_administrator_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                603,
                CHAT_ID,
                "supergroup",
                "selector",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let token = "group-addressed-start";
    state
        .put_start_context(
            keys::StartContextDomain::PrivateSubscription,
            token,
            &encode_start_context(CHAT_ID, "Addressed <Source>").expect("start context"),
        )
        .await
        .expect("store start context");
    let context = command_context(&server, database, state).await;

    SystemHandlers::handle_start(
        context.config.telegram.bot(),
        group_start_message(&format!("/start@TestBot {token}")),
        token.to_owned(),
        bot_me(),
        context,
    )
    .await
    .expect("addressed administrator-group start");

    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/SendMessage"
        ]
    );
    let selector = request_body(&requests[1]);
    assert_eq!(selector["reply_parameters"]["message_id"], 93);
    assert_eq!(
        selector["text"],
        "您正在请求为群组 <b>Addressed &amp;lt;Source&amp;gt;</b> 创建聊天回顾。\n请问您要为过去几个小时内的聊天创建回顾呢？"
    );
}

#[tokio::test]
async fn missing_start_context_falls_back_to_the_localized_help_reply() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_member_result()))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(604, 42, "private", "help")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state).await;

    SystemHandlers::handle_start(
        context.config.telegram.bot(),
        start_message("missing-context"),
        "missing-context".to_owned(),
        bot_me(),
        context,
    )
    .await
    .expect("missing start context fallback");

    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/GetChatMember",
            "/telegram/bottest-token/SendMessage"
        ],
        "Go checks bot-admin status in /start and again in the help fallback"
    );
    let reply = request_body(&requests[2]);
    let reply_text = reply["text"].as_str().expect("reply text is a string");
    // `start_message` carries no `from.language_code`, so this exercises the
    // same config-locale fallback as `help_command_with_missing_language_code`
    // below, proving Go's `/start` fallthrough (`start_command.go:44-53`)
    // renders the identical composed `/help` body.
    assert!(
        reply_text.starts_with("Greetings! 👋 Welcome to using Insights Bot!"),
        "a missing language_code falls back to the config locale (English), \
         matching Go's i18n default"
    );
    assert!(
        reply_text.contains("<b>Basic Commands</b>"),
        "the basic group's own name and help text follow the resolved locale"
    );
    assert!(
        reply_text.contains("/recap@TestBot - 总结过去的聊天记录并生成回顾快报"),
        "the recap group's name and help text are Go string literals \
         (recap.go:41-86), always Simplified Chinese regardless of locale"
    );
    assert_eq!(reply["parse_mode"], "HTML");
    assert_eq!(reply["reply_parameters"]["message_id"], 91);
}

/// Go `pkg/bots/tgbot/context.go:141-156`: the locale is resolved from the
/// *sender's* Telegram `language_code`, evaluated per message, not pinned
/// at startup. `zh-hans` (Telegram's own lowercase tag) must resolve to the
/// Simplified Chinese basic group.
#[tokio::test]
async fn help_command_with_zh_hans_language_code_renders_the_localized_basic_group() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_member_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(605, 42, "private", "help")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state).await;

    SystemHandlers::handle_help(
        context.config.telegram.bot(),
        help_message(Some("zh-hans")),
        bot_me(),
        context,
    )
    .await
    .expect("zh-Hans /help");

    let requests = server.received_requests().await.expect("Telegram requests");
    let reply = request_body(&requests[1]);
    let reply_text = reply["text"].as_str().expect("reply text is a string");
    assert!(
        reply_text.starts_with("你好！👋 欢迎使用 Insights Bot！"),
        "the sender's zh-hans language_code selects the Simplified Chinese \
         basic-group text"
    );
    assert!(reply_text.contains("<b>基础命令</b>"));
    assert!(reply_text.contains("/help@TestBot - 获取帮助"));
    assert!(
        reply_text.contains("/recap@TestBot - 总结过去的聊天记录并生成回顾快报"),
        "the recap group is always Simplified Chinese regardless of locale \
         (recap.go:41-86), so it renders the same text here as elsewhere"
    );
    assert_eq!(reply["parse_mode"], "HTML");
    assert_eq!(reply["reply_parameters"]["message_id"], 91);
}

/// An explicit `en` `language_code` renders the basic group in English, but
/// the recap group's Go-quirk Simplified Chinese text is unaffected --
/// locking in that the recap group's help text is a fixed literal, not an
/// i18n lookup (`recap.go:41-86`).
#[tokio::test]
async fn help_command_with_english_language_code_keeps_the_recap_group_in_chinese() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_member_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(606, 42, "private", "help")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state).await;

    SystemHandlers::handle_help(
        context.config.telegram.bot(),
        help_message(Some("en")),
        bot_me(),
        context,
    )
    .await
    .expect("en /help");

    let requests = server.received_requests().await.expect("Telegram requests");
    let reply = request_body(&requests[1]);
    let reply_text = reply["text"].as_str().expect("reply text is a string");
    assert!(reply_text.starts_with("Greetings! 👋 Welcome to using Insights Bot!"));
    assert!(reply_text.contains("<b>Basic Commands</b>"));
    assert!(reply_text.contains("/help@TestBot - Obtain assistance"));
    assert!(
        reply_text.contains("/recap@TestBot - 总结过去的聊天记录并生成回顾快报"),
        "Go quirk: the recap group's name and help text are Simplified \
         Chinese literals even when the rest of /help renders in English"
    );
    assert_eq!(reply["parse_mode"], "HTML");
    assert_eq!(reply["reply_parameters"]["message_id"], 91);
}

/// `zh-tw` is not one of Go's own locale tags, but this port ships a
/// zh-Hant bundle Go lacks, so `Locale::from_language_code` routes it there
/// -- a documented divergence. The zh-Hant bundle's basic-group keys carry
/// the Simplified text Go's matcher would actually serve a zh-TW sender
/// (every zh-* tag resolves to zh-CN there). `AppConfig::locale` stays the
/// default English here so the Chinese reply can only be explained by the
/// sender's `language_code` overriding the configured fallback.
#[tokio::test]
async fn help_command_with_zh_tw_language_code_overrides_the_configured_zh_hans_locale() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_member_result()))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(607, 42, "private", "help")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context_with_insights_lang(&server, database, state, None).await;

    SystemHandlers::handle_help(
        context.config.telegram.bot(),
        // Mixed case, matching Go's case-insensitive-in-practice Telegram
        // tags and exercising `Locale::from_language_code`'s
        // case-insensitive match.
        help_message(Some("zh-TW")),
        bot_me(),
        context,
    )
    .await
    .expect("zh-TW /help");

    let requests = server.received_requests().await.expect("Telegram requests");
    let reply = request_body(&requests[1]);
    let reply_text = reply["text"].as_str().expect("reply text is a string");
    assert!(
        reply_text.starts_with("你好！👋 欢迎使用 Insights Bot！"),
        "zh-TW resolves to the zh-Hant locale, whose basic-group keys carry \
         the Simplified text Go's matcher serves every zh-* sender -- \
         overriding the configured English default rather than \
         coincidentally matching it"
    );
    assert!(reply_text.contains("<b>基础命令</b>"));
    assert!(reply_text.contains("/recap@TestBot - 总结过去的聊天记录并生成回顾快报"));
    assert_eq!(reply["parse_mode"], "HTML");
    assert_eq!(reply["reply_parameters"]["message_id"], 91);
}

/// Go `help_command.go:44-58`: a bare (non-`@bot`) `/help` in a group where
/// the bot is an administrator matches neither of the two allowed exact
/// command forms, so the handler returns before sending anything.
#[tokio::test]
async fn administrator_group_bare_help_is_silent_before_reply() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/GetChatMember"))
        .respond_with(ResponseTemplate::new(200).set_body_json(telegram_administrator_result()))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state).await;

    SystemHandlers::handle_help(
        context.config.telegram.bot(),
        group_start_message("/help"),
        bot_me(),
        context,
    )
    .await
    .expect("bare administrator-group help");

    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1, "Go stops after the administrator lookup");
    assert_eq!(
        requests[0].url.path(),
        "/telegram/bottest-token/GetChatMember"
    );
}

#[tokio::test]
async fn subscriber_keyboard_uses_go_unsubscribe_payload_and_second_row() {
    let state = InMemoryRecapStateStore::new(Arc::new(TestClock::new(START_MS)));
    let markup = build_subscriber_vote_keyboard(
        &state,
        CHAT_ID,
        "A <B> & C",
        42,
        LOG_ID,
        ReactionCounts::default(),
    )
    .await
    .expect("subscriber keyboard");

    assert_eq!(markup.inline_keyboard.len(), 2);
    assert_eq!(markup.inline_keyboard[0].len(), 3);
    assert_eq!(markup.inline_keyboard[1].len(), 1);
    assert_eq!(markup.inline_keyboard[1][0].text, "取消订阅");
    let serialized = serde_json::to_value(&markup).expect("serialize subscriber keyboard");
    let wire = serialized["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("unsubscribe callback data");
    let (_, action_hash) = keys::decode_callback_wire(wire).expect("opaque callback wire");
    assert_eq!(
        state
            .get_callback(keys::ROUTE_UNSUBSCRIBE_RECAP, action_hash)
            .await
            .expect("stored unsubscribe payload")
            .as_deref(),
        Some(r#"{"chatId":-1001234567890,"chatTitle":"A \u003cB\u003e \u0026 C","fromId":42}"#)
    );

    let (_, select_hash) = keys::decode_callback_wire(
        serialized["inline_keyboard"][0][0]["callback_data"]
            .as_str()
            .expect("vote callback data"),
    )
    .expect("opaque vote callback wire");
    assert!(
        state
            .get_callback(keys::ROUTE_SMR_SUMMARIZATION_FEEDBACK_REACT, select_hash,)
            .await
            .expect("stored vote payload")
            .is_some()
    );
    assert_eq!(AUTO_RECAP_SEND_MODE_ONLY_PRIVATE_SUBSCRIPTIONS, 1);
}

#[tokio::test]
async fn private_mode_dms_selector_before_deleting_command_and_draining_messages() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(301, 42, "private", "selector")),
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

    let database = private_mode_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    state
        .push_delete_later(42, -999, 66)
        .await
        .expect("plant delete-later message");
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        command_message(),
        bot_me(),
        context,
    )
    .await
    .expect("private /recap command");

    assert!(
        state
            .raw_string(&keys::manual_recap_rate_key(CHAT_ID))
            .is_none(),
        "private mode bypasses the public manual-recap limiter"
    );
    assert!(state.raw_list(&keys::delete_later_key(42)).is_none());
    let requests = server.received_requests().await.expect("Telegram requests");
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            "/telegram/bottest-token/sendMessage",
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/DeleteMessage",
        ]
    );
    let body = request_body(&requests[0]);
    assert_eq!(body["chat_id"], "42");
    assert_eq!(body["parse_mode"], "HTML");
    assert!(body.get("reply_to_message_id").is_none());
    assert_eq!(
        body["text"],
        "您正在请求为群组 <b>Parity &amp;lt;Lab&amp;gt; &amp; Friends</b> 创建聊天回顾。\n请问您要为过去几个小时内的聊天创建回顾呢？"
    );
    let markup: Value = serde_json::from_str(
        body["reply_markup"]
            .as_str()
            .expect("serialized selector markup"),
    )
    .expect("selector JSON");
    assert_eq!(markup["inline_keyboard"].as_array().map(Vec::len), Some(2));
    let wire = markup["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("selector callback");
    let (_, action_hash) = keys::decode_callback_wire(wire).expect("opaque callback");
    let payload = state
        .get_callback(keys::ROUTE_SELECT_HOUR, action_hash)
        .await
        .expect("selector payload")
        .expect("live selector payload");
    assert!(payload.contains(r#""recap_mode":1"#));
}

#[tokio::test]
async fn private_mode_403_stores_context_then_tracks_exact_group_guidance() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 403,
            "description": "Forbidden: bot can't initiate conversation with a user"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                88,
                CHAT_ID,
                "supergroup",
                "guidance",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let database = private_mode_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        command_message(),
        bot_me(),
        context,
    )
    .await
    .expect("private /recap 403 path");

    let token = keys::StartContextDomain::PrivateSubscription.token(CHAT_ID);
    let context_key = keys::StartContextDomain::PrivateSubscription.key(&token);
    assert_eq!(
        state.raw_string(&context_key).as_deref(),
        Some(r#"{"chat_id":-1001234567890,"chat_title":"Parity \u003cLab\u003e \u0026 Friends"}"#)
    );
    assert_eq!(state.ttl_ms(&context_key), Some(86_400_000));
    assert_eq!(
        state.raw_list(&keys::delete_later_key(42)),
        Some(vec![format!("{CHAT_ID};88"), format!("{CHAT_ID};77")])
    );
    assert!(
        state
            .raw_string(&keys::manual_recap_rate_key(CHAT_ID))
            .is_none()
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(requests.len(), 2);
    let guidance = request_body(&requests[1]);
    assert_eq!(guidance["parse_mode"], "HTML");
    assert_eq!(guidance["reply_parameters"]["message_id"], 77);
    assert_eq!(
        guidance["text"],
        format!(
            "抱歉，在给您发送引导您创建聊天回顾的消息时出现了问题，这似乎是因为您<b>从未</b>和本 Bot（@TestBot） <b>发起过对话</b>导致的。\n\n由于当前群组的聊天回顾功能已经被<b>群组创建者</b>设定为<b>私聊订阅模式</b>，Bot 需要通过私聊的方式向您发送引导您创建聊天回顾的消息，届时，您需要完成以下任一一个操作后方可继续创建聊天回顾：\n1. <b>点击链接</b> https://t.me/TestBot?start={token} 与 Bot 开始对话就能继续原先的 /recap 命令操作；\n2. 点击 Bot 头像并且开始对话，然后在群组内重新发送 /recap 命令来创建聊天回顾。"
        )
    );
}

#[tokio::test]
async fn subscribe_command_dms_before_insert_then_deletes_and_drains() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                501,
                42,
                "private",
                "subscribed",
            )),
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
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity <Lab> & Friends")
        .await
        .expect("enable recap");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    state
        .push_delete_later(42, -999, 66)
        .await
        .expect("plant delete-later message");
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_subscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/subscribe_recap"),
        bot_me(),
        context,
    )
    .await
    .expect("subscribe command");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_some()
    );
    assert!(state.raw_list(&keys::delete_later_key(42)).is_none());
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/sendMessage",
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/DeleteMessage",
        ]
    );
    let dm = request_body(&requests[0]);
    assert_eq!(dm["parse_mode"], "HTML");
    assert_eq!(
        dm["text"],
        "您已成功订阅群组 <b>Parity &amp;lt;Lab&amp;gt; &amp; Friends</b> 的定时聊天回顾！"
    );
}

#[tokio::test]
async fn subscribe_403_stores_its_namespace_before_group_guidance() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 403,
            "description": "Forbidden: bot was blocked by the user"
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                502,
                CHAT_ID,
                "supergroup",
                "guidance",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity <Lab> & Friends")
        .await
        .expect("enable recap");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_subscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/subscribe_recap"),
        bot_me(),
        context,
    )
    .await
    .expect("subscribe blocked path");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none()
    );
    let token = keys::StartContextDomain::SubscribeRecap.token(CHAT_ID);
    let context_key = keys::StartContextDomain::SubscribeRecap.key(&token);
    assert_eq!(
        state.raw_string(&context_key).as_deref(),
        Some(r#"{"chat_id":-1001234567890,"chat_title":"Parity \u003cLab\u003e \u0026 Friends"}"#)
    );
    assert_eq!(state.ttl_ms(&context_key), Some(86_400_000));
    assert_eq!(
        state.raw_list(&keys::delete_later_key(42)),
        Some(vec![format!("{CHAT_ID};502"), format!("{CHAT_ID};77")])
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    let guidance = request_body(&requests[1]);
    assert_eq!(guidance["parse_mode"], "HTML");
    assert_eq!(
        guidance["text"],
        format!(
            "抱歉，在为您订阅本群组定时聊天回顾时出现了问题，这似乎是因为您已将本 Bot（@TestBot）<b>停用</b>或是添加到了<b>黑名单</b>中导致的。\n\n订阅群组的聊天回顾需要 Bot 需要有权限通过私聊的方式向您定期发送聊天回顾，届时，您需要根据下面的提示进行操作：\n1. 将 Bot 从<b>黑名单中移除</b>；\n2. <b>点击链接</b> https://t.me/TestBot?start={token} 继续订阅本群组的定时聊天回顾操作，或是在群组内重新发送 /subscribe_recap 命令来订阅本群组的定时聊天回顾。"
        )
    );
}

#[tokio::test]
async fn private_start_context_wins_collision_is_reusable_and_skips_feature_checks() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(601, 42, "private", "selector")),
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
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let token = "collision";
    let private_payload = encode_start_context(CHAT_ID, "Private Source").expect("private context");
    let subscribe_payload =
        encode_start_context(CHAT_ID, "Subscribe Source").expect("subscribe context");
    state
        .put_start_context(
            keys::StartContextDomain::PrivateSubscription,
            token,
            &private_payload,
        )
        .await
        .expect("private start context");
    state
        .put_start_context(
            keys::StartContextDomain::SubscribeRecap,
            token,
            &subscribe_payload,
        )
        .await
        .expect("subscription start context");
    state
        .push_delete_later(42, -999, 66)
        .await
        .expect("plant delete-later message");
    let context = command_context(&server, database.clone(), state.clone()).await;

    assert!(
        handle_start_continuation(
            &context.config.telegram.bot(),
            &start_message(token),
            token,
            &context,
        )
        .await
        .expect("start continuation")
    );

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none(),
        "private continuation aborts before the colliding subscription namespace"
    );
    assert_eq!(
        state
            .get_start_context(keys::StartContextDomain::PrivateSubscription, token)
            .await
            .expect("private context reread")
            .as_deref(),
        Some(private_payload.as_str())
    );
    assert!(state.raw_list(&keys::delete_later_key(42)).is_none());
    let requests = server.received_requests().await.expect("Telegram requests");
    let start_reply = request_body(&requests[1]);
    assert_eq!(start_reply["parse_mode"], "HTML");
    assert_eq!(start_reply["reply_parameters"]["message_id"], 91);
    assert_eq!(
        start_reply["text"],
        "您正在请求为群组 <b>Private Source</b> 创建聊天回顾。\n请问您要为过去几个小时内的聊天创建回顾呢？"
    );
}

#[tokio::test]
async fn subscription_start_inserts_before_non_reply_confirmation_and_keeps_context() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                602,
                42,
                "private",
                "subscribed",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let token = "subscribe-only";
    let payload =
        encode_start_context(CHAT_ID, "Subscription <Source>").expect("subscription start context");
    state
        .put_start_context(keys::StartContextDomain::SubscribeRecap, token, &payload)
        .await
        .expect("store subscription context");
    let context = command_context(&server, database.clone(), state.clone()).await;

    assert!(
        handle_start_continuation(
            &context.config.telegram.bot(),
            &start_message(token),
            token,
            &context,
        )
        .await
        .expect("subscription start continuation")
    );

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_some()
    );
    assert_eq!(
        state
            .get_start_context(keys::StartContextDomain::SubscribeRecap, token)
            .await
            .expect("subscription context reread")
            .as_deref(),
        Some(payload.as_str())
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    let confirmation = request_body(&requests[0]);
    assert!(confirmation.get("reply_parameters").is_none());
    assert_eq!(confirmation["parse_mode"], "HTML");
    assert_eq!(
        confirmation["text"],
        "您已成功订阅群组 <b>Subscription &amp;lt;Source&amp;gt;</b> 的定时聊天回顾！"
    );
}

#[tokio::test]
async fn unsubscribe_command_deletes_row_and_command_before_private_confirmation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                701,
                42,
                "private",
                "unsubscribed",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state).await;

    handle_unsubscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/unsubscribe_recap"),
        context,
    )
    .await
    .expect("unsubscribe command");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none()
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/sendMessage",
        ]
    );
    let dm = request_body(&requests[1]);
    assert_eq!(dm["parse_mode"], "HTML");
    assert_eq!(
        dm["text"],
        "您已成功取消订阅群组 <b>Parity &amp;lt;Lab&amp;gt; &amp; Friends</b> 的定时聊天回顾！"
    );
}

#[tokio::test]
async fn inline_unsubscribe_removes_only_clicked_button_and_sends_private_confirmation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageReplyMarkup"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                401,
                42,
                "private",
                "Rich recap",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                702,
                42,
                "private",
                "unsubscribed",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let markup = build_subscriber_vote_keyboard(
        state.as_ref(),
        CHAT_ID,
        "Parity <Lab> & Friends",
        42,
        LOG_ID,
        ReactionCounts::default(),
    )
    .await
    .expect("subscriber keyboard");
    let markup_value = serde_json::to_value(&markup).expect("serialize subscriber keyboard");
    let wire = markup_value["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("unsubscribe callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        unsubscribe_callback(&wire, &markup_value),
        context,
    )
    .await
    .expect("inline unsubscribe callback");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none()
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(requests.len(), 2, "Go sends no callback answer");
    let edit = request_body(&requests[0]);
    let edited_markup: Value = match &edit["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    assert_eq!(
        edited_markup["inline_keyboard"].as_array().map(Vec::len),
        Some(1)
    );
    assert_eq!(
        edited_markup["inline_keyboard"][0].as_array().map(Vec::len),
        Some(3)
    );
    let confirmation = request_body(&requests[1]);
    assert_eq!(confirmation["parse_mode"], "HTML");
    assert_eq!(
        confirmation["text"],
        "已成功取消订阅群组 <b>Parity &amp;lt;Lab&amp;gt; &amp; Friends</b> 的定时聊天回顾。"
    );
}

#[tokio::test]
async fn inline_unsubscribe_keeps_database_change_when_confirmation_fails() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageReplyMarkup"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                401,
                42,
                "private",
                "Rich recap",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 500,
            "description": "Internal Server Error"
        })))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let markup = build_subscriber_vote_keyboard(
        state.as_ref(),
        CHAT_ID,
        "Parity <Lab> & Friends",
        42,
        LOG_ID,
        ReactionCounts::default(),
    )
    .await
    .expect("subscriber keyboard");
    let markup_value = serde_json::to_value(&markup).expect("serialize subscriber keyboard");
    let wire = markup_value["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("unsubscribe callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        unsubscribe_callback(&wire, &markup_value),
        context,
    )
    .await
    .expect("Go logs and swallows confirmation delivery failure");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none()
    );
}

#[tokio::test]
async fn ordinary_left_member_removes_one_row_while_bot_self_is_skipped() {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::insert_unchecked(&database, CHAT_ID, 42)
        .await
        .expect("first subscriber row");
    subscribers::insert_unchecked(&database, CHAT_ID, 42)
        .await
        .expect("duplicate subscriber row");
    let context_server = MockServer::start().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&context_server, database.clone(), state).await;
    let left_user = user(serde_json::json!({
        "id": 42,
        "is_bot": false,
        "first_name": "Ada"
    }));

    handle_chat_member_left(
        left_member_message(serde_json::to_value(&left_user).expect("left user JSON")),
        left_user,
        bot_me(),
        context.clone(),
    )
    .await
    .expect("ordinary member-left event");
    assert_eq!(
        subscribers::list(&database, CHAT_ID)
            .await
            .expect("rows")
            .len(),
        1
    );

    let me = bot_me();
    handle_chat_member_left(
        left_member_message(serde_json::to_value(&me.user).expect("bot user JSON")),
        me.user.clone(),
        me,
        context,
    )
    .await
    .expect("bot member-left event");
    assert_eq!(
        subscribers::list(&database, CHAT_ID)
            .await
            .expect("rows")
            .len(),
        1
    );
}

#[tokio::test]
async fn private_mode_anonymous_tracks_only_the_error_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                801,
                CHAT_ID,
                "supergroup",
                "anonymous error",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    let database = private_mode_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        anonymous_group_command("/recap"),
        bot_me(),
        context,
    )
    .await
    .expect("anonymous private recap command");

    assert_eq!(
        state.raw_list(&keys::delete_later_key(1_087_968_824)),
        Some(vec![format!("{CHAT_ID};801")])
    );
    assert!(
        state
            .keys()
            .iter()
            .all(|key| !key.contains(keys::ROUTE_SELECT_HOUR))
    );
    let requests = server.received_requests().await.expect("Telegram request");
    let response = request_body(&requests[0]);
    assert_eq!(
        response["text"],
        "匿名管理员无法在设定为私聊回顾模式的群组内请求创建聊天记录回顾哦！如果需要创建聊天记录回顾，必须先将发送角色切换为普通用户然后再试哦。"
    );
    assert!(
        response.get("parse_mode").is_none(),
        "Go sends this plain error without parse_mode"
    );
}

#[tokio::test]
async fn matching_description_without_403_stores_context_but_sends_no_guidance() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 400,
            "description": "Forbidden: bot can't initiate conversation with a user"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let database = private_mode_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        command_message(),
        bot_me(),
        context,
    )
    .await
    .expect("non-403 private recap failure");

    let token = keys::StartContextDomain::PrivateSubscription.token(CHAT_ID);
    assert!(
        state
            .raw_string(&keys::StartContextDomain::PrivateSubscription.key(&token))
            .is_some()
    );
    assert!(state.raw_list(&keys::delete_later_key(42)).is_none());
    assert_eq!(
        server
            .received_requests()
            .await
            .expect("Telegram request")
            .len(),
        1
    );
}

#[tokio::test]
async fn disabled_subscription_tracks_only_the_group_error_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                802,
                CHAT_ID,
                "supergroup",
                "disabled",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    handle_subscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/subscribe_recap"),
        bot_me(),
        context,
    )
    .await
    .expect("disabled subscription command");

    assert_eq!(
        state.raw_list(&keys::delete_later_key(42)),
        Some(vec![format!("{CHAT_ID};802")])
    );
    let requests = server.received_requests().await.expect("Telegram request");
    let response = request_body(&requests[0]);
    assert_eq!(
        response["text"],
        "聊天记录回顾功能在当前群组尚未启用，需要在群组管理员通过 /configure_recap 命令配置功能启用后才可以订阅聊天回顾哦。"
    );
}

/// Go `processExceptionError` (`pkg/bots/tgbot/handler.go:117-156`) builds the
/// `ExceptionError` edit branch as a bare `NewEditMessageText(chatID,
/// editMessage.MessageID, message)`: it never reads `ExceptionError.replyMarkup`,
/// even though `callback_query.go:381-400`'s `handleCallbackQueryUnsubscribe`
/// calls `WithReplyMarkup(...)`. This locks in that the wire edit drops the
/// existing subscriber keyboard rather than preserving it.
#[tokio::test]
async fn expired_inline_unsubscribe_edits_a_bare_wire_error_dropping_all_buttons() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(401, 42, "private", "error")),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let markup = build_subscriber_vote_keyboard(
        state.as_ref(),
        CHAT_ID,
        "Parity <Lab> & Friends",
        42,
        LOG_ID,
        ReactionCounts::default(),
    )
    .await
    .expect("subscriber keyboard");
    let markup_value = serde_json::to_value(&markup).expect("serialize subscriber keyboard");
    let wire = markup_value["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("unsubscribe callback")
        .to_owned();
    let (_, action_hash) = keys::decode_callback_wire(&wire).expect("opaque callback");
    state.expire_key_now(&keys::callback_payload_key(
        keys::ROUTE_UNSUBSCRIBE_RECAP,
        action_hash,
    ));
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        unsubscribe_callback(&wire, &markup_value),
        context,
    )
    .await
    .expect("expired unsubscribe callback");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_some()
    );
    let requests = server.received_requests().await.expect("Telegram request");
    let edit = request_body(&requests[0]);
    assert_eq!(edit["text"], "取消订阅时出现了问题，请稍后再试！");
    assert!(
        edit.get("parse_mode").is_none(),
        "Go's ExceptionError edit has no parse mode"
    );
    assert!(
        edit.get("reply_markup").is_none(),
        "Go's ExceptionError edit never reads replyMarkup and drops the existing keyboard"
    );
}

#[tokio::test]
async fn expired_inline_unsubscribe_swallows_failed_error_edit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/EditMessageText"))
        .respond_with(ResponseTemplate::new(500).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 500,
            "description": "Internal Server Error"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let markup = build_subscriber_vote_keyboard(
        state.as_ref(),
        CHAT_ID,
        "Parity <Lab> & Friends",
        42,
        LOG_ID,
        ReactionCounts::default(),
    )
    .await
    .expect("subscriber keyboard");
    let markup_value = serde_json::to_value(&markup).expect("serialize subscriber keyboard");
    let wire = markup_value["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("unsubscribe callback")
        .to_owned();
    let (_, action_hash) = keys::decode_callback_wire(&wire).expect("opaque callback");
    state.expire_key_now(&keys::callback_payload_key(
        keys::ROUTE_UNSUBSCRIBE_RECAP,
        action_hash,
    ));
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        unsubscribe_callback(&wire, &markup_value),
        context,
    )
    .await
    .expect("Go logs and swallows callback error edit failure");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_some()
    );
}

#[tokio::test]
async fn inline_unsubscribe_from_another_actor_is_silent_and_keeps_the_row() {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let markup = build_subscriber_vote_keyboard(
        state.as_ref(),
        CHAT_ID,
        "Parity <Lab> & Friends",
        42,
        LOG_ID,
        ReactionCounts::default(),
    )
    .await
    .expect("subscriber keyboard");
    let markup_value = serde_json::to_value(&markup).expect("serialize subscriber keyboard");
    let wire = markup_value["inline_keyboard"][1][0]["callback_data"]
        .as_str()
        .expect("unsubscribe callback")
        .to_owned();
    let context = command_context(&server, database.clone(), state).await;

    RecapHandlers::handle_callback_query(
        context.config.telegram.bot(),
        unsubscribe_callback_from(&wire, &markup_value, 43),
        context,
    )
    .await
    .expect("foreign unsubscribe callback");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_some()
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("Telegram requests")
            .is_empty()
    );
}

#[tokio::test]
async fn unsubscribe_403_keeps_database_change_and_repeats_command_deletion() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(2)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({
            "ok": false,
            "error_code": 403,
            "description": "Forbidden: bot was blocked by the user"
        })))
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_unsubscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/unsubscribe_recap"),
        context,
    )
    .await
    .expect("blocked unsubscribe command");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none()
    );
    assert!(state.keys().iter().all(|key| {
        !key.starts_with(keys::StartContextDomain::PrivateSubscription.key_prefix())
            && !key.starts_with(keys::StartContextDomain::SubscribeRecap.key_prefix())
    }));
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/DeleteMessage",
            "/telegram/bottest-token/sendMessage",
            "/telegram/bottest-token/DeleteMessage",
        ]
    );
}

#[tokio::test]
async fn unknown_persisted_send_mode_uses_public_selector_and_rate_limit() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                803,
                CHAT_ID,
                "supergroup",
                "public selector",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity <Lab> & Friends")
        .await
        .expect("enable recap");
    recap_options::find_one_or_create(&database, CHAT_ID)
        .await
        .expect("create recap options");
    sqlx::query(
        "UPDATE telegram_chat_recaps_options SET auto_recap_send_mode = 99 WHERE chat_id = $1",
    )
    .bind(CHAT_ID)
    .execute(&database.pool)
    .await
    .expect("store unknown mode");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    RecapHandlers::handle_recap(
        context.config.telegram.bot(),
        command_message(),
        bot_me(),
        context,
    )
    .await
    .expect("unknown-mode recap command");

    assert_eq!(
        state
            .raw_string(&keys::manual_recap_rate_key(CHAT_ID))
            .as_deref(),
        Some("1")
    );
    let requests = server.received_requests().await.expect("Telegram request");
    let body = request_body(&requests[0]);
    let markup: Value = match &body["reply_markup"] {
        Value::String(raw) => serde_json::from_str(raw).expect("reply markup JSON"),
        value => value.clone(),
    };
    let wire = markup["inline_keyboard"][0][0]["callback_data"]
        .as_str()
        .expect("selector callback");
    let (_, action_hash) = keys::decode_callback_wire(wire).expect("opaque callback");
    let payload = state
        .get_callback(keys::ROUTE_SELECT_HOUR, action_hash)
        .await
        .expect("selector payload")
        .expect("live selector payload");
    assert!(payload.contains(r#""recap_mode":0"#));
}

#[tokio::test]
async fn subscribe_command_private_chat_rejects_without_tracking() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(905, 42, "private", "gate")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    handle_subscribe_recap_command(
        context.config.telegram.bot(),
        private_command("/subscribe_recap"),
        bot_me(),
        context,
    )
    .await
    .expect("private-chat subscribe gate");

    assert!(
        state.raw_list(&keys::delete_later_key(42)).is_none(),
        "the chat-type gate returns before any delete-later tracking"
    );
    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1);
    let reply = request_body(&requests[0]);
    assert_eq!(
        reply["text"],
        "只有在群组和超级群组内才可以订阅定时的聊天记录回顾哦！"
    );
    assert_eq!(reply["reply_parameters"]["message_id"], 95);
    assert!(reply.get("parse_mode").is_none());
}

#[tokio::test]
async fn subscribe_command_anonymous_bot_rejects_and_tracks_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                902,
                CHAT_ID,
                "supergroup",
                "anonymous subscribe error",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_subscribe_recap_command(
        context.config.telegram.bot(),
        anonymous_group_command("/subscribe_recap"),
        bot_me(),
        context,
    )
    .await
    .expect("anonymous subscribe command");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 1_087_968_824)
            .await
            .expect("subscriber lookup")
            .is_none()
    );
    assert_eq!(
        state.raw_list(&keys::delete_later_key(1_087_968_824)),
        Some(vec![format!("{CHAT_ID};902")])
    );
    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1);
    let response = request_body(&requests[0]);
    assert_eq!(
        response["text"],
        "匿名管理员无法订阅定时的聊天记录回顾哦！如果需要订阅定时的聊天记录回顾，必须先将发送角色切换为普通用户然后再试哦。"
    );
    assert!(response.get("parse_mode").is_none());
    assert_eq!(response["reply_parameters"]["message_id"], 78);
}

#[tokio::test]
async fn subscribe_command_db_failure_after_dm_sends_error_and_leaves_no_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/sendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                903,
                42,
                "private",
                "subscribed",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                904,
                CHAT_ID,
                "supergroup",
                "subscribe db error",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(&database, CHAT_ID, "supergroup", "Parity <Lab> & Friends")
        .await
        .expect("enable recap");
    block_subscriber_inserts(&database).await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_subscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/subscribe_recap"),
        bot_me(),
        context,
    )
    .await
    .expect("subscribe DB failure path");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none(),
        "the blocked insert must leave no subscriber row"
    );
    assert_eq!(
        state.raw_list(&keys::delete_later_key(42)),
        Some(vec![format!("{CHAT_ID};904")])
    );
    let requests = server.received_requests().await.expect("Telegram requests");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path())
            .collect::<Vec<_>>(),
        [
            "/telegram/bottest-token/sendMessage",
            "/telegram/bottest-token/SendMessage",
        ]
    );
    let error_reply = request_body(&requests[1]);
    assert_eq!(
        error_reply["text"],
        "订阅群组定时聊天回顾时出现问题，请稍后再试！"
    );
    assert_eq!(error_reply["reply_parameters"]["message_id"], 77);
    assert!(error_reply.get("parse_mode").is_none());
}

#[tokio::test]
async fn unsubscribe_command_private_chat_rejects_without_tracking() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(telegram_message_result(906, 42, "private", "gate")),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state.clone()).await;

    handle_unsubscribe_recap_command(
        context.config.telegram.bot(),
        private_command("/unsubscribe_recap"),
        context,
    )
    .await
    .expect("private-chat unsubscribe gate");

    assert!(
        state.raw_list(&keys::delete_later_key(42)).is_none(),
        "the chat-type gate returns before any delete-later tracking"
    );
    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1);
    let reply = request_body(&requests[0]);
    assert_eq!(
        reply["text"],
        "只有在群组和超级群组内才可以取消订阅定时的聊天记录回顾哦！"
    );
    assert_eq!(reply["reply_parameters"]["message_id"], 95);
    assert!(reply.get("parse_mode").is_none());
}

#[tokio::test]
async fn unsubscribe_command_anonymous_bot_silently_deletes_command() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/DeleteMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": true
        })))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database, state).await;

    handle_unsubscribe_recap_command(
        context.config.telegram.bot(),
        anonymous_group_command("/unsubscribe_recap"),
        context,
    )
    .await
    .expect("anonymous unsubscribe command");

    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1, "Go sends no text, only the deletion");
    assert_eq!(
        requests[0].url.path(),
        "/telegram/bottest-token/DeleteMessage"
    );
}

#[tokio::test]
async fn unsubscribe_command_db_failure_sends_error_and_keeps_row() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                907,
                CHAT_ID,
                "supergroup",
                "unsubscribe db error",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    subscribers::subscribe(&database, CHAT_ID, 42)
        .await
        .expect("plant subscriber");
    block_subscriber_deletes(&database).await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let context = command_context(&server, database.clone(), state.clone()).await;

    handle_unsubscribe_recap_command(
        context.config.telegram.bot(),
        group_command("/unsubscribe_recap"),
        context,
    )
    .await
    .expect("unsubscribe DB failure path");

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_some(),
        "the blocked delete must preserve the original row"
    );
    assert_eq!(
        state.raw_list(&keys::delete_later_key(42)),
        Some(vec![format!("{CHAT_ID};907")])
    );
    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url.path(),
        "/telegram/bottest-token/SendMessage"
    );
    let error_reply = request_body(&requests[0]);
    assert_eq!(
        error_reply["text"],
        "订阅群组定时聊天回顾时出现问题，请稍后再试！"
    );
    assert_eq!(error_reply["reply_parameters"]["message_id"], 77);
    assert!(error_reply.get("parse_mode").is_none());
}

#[tokio::test]
async fn start_continuation_subscribe_db_failure_sends_error_and_keeps_context_reusable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(telegram_message_result(
                908,
                42,
                "private",
                "start subscribe db error",
            )),
        )
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    block_subscriber_inserts(&database).await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let token = "subscribe-db-failure";
    let payload =
        encode_start_context(CHAT_ID, "Subscribe DB Failure").expect("subscription start context");
    state
        .put_start_context(keys::StartContextDomain::SubscribeRecap, token, &payload)
        .await
        .expect("store subscription context");
    let context = command_context(&server, database.clone(), state.clone()).await;

    assert!(
        handle_start_continuation(
            &context.config.telegram.bot(),
            &start_message(token),
            token,
            &context,
        )
        .await
        .expect("start continuation subscribe DB failure")
    );

    assert!(
        subscribers::find_one(&database, CHAT_ID, 42)
            .await
            .expect("subscriber lookup")
            .is_none(),
        "the blocked insert must leave no subscriber row"
    );
    assert_eq!(
        state
            .get_start_context(keys::StartContextDomain::SubscribeRecap, token)
            .await
            .expect("subscription context reread")
            .as_deref(),
        Some(payload.as_str()),
        "the context must stay reusable after a failed subscribe"
    );
    let requests = server.received_requests().await.expect("Telegram request");
    assert_eq!(requests.len(), 1);
    let error_reply = request_body(&requests[0]);
    assert_eq!(
        error_reply["text"],
        "订阅群组定时聊天回顾时出现问题，请稍后再试！"
    );
    assert_eq!(error_reply["reply_parameters"]["message_id"], 91);
    assert!(error_reply.get("parse_mode").is_none());
}
