//! Group-to-supergroup migration parity against Go v1.0.0 (pinned `02aee8ce`)
//! `internal/bots/telegram/handlers/chatmigrate/chatmigrate.go`.
//!
//! Go's `OnChatMigrationFrom` fires on the **new supergroup** side, when the
//! service message carries `migrate_from_chat_id` (`chatmigrate.go:56-68`).
//! The old group's own `migrate_to_chat_id` service message is never the
//! trigger. After the five-step migration, Go looks up the new chat's
//! language and sends a best-effort HTML notification into the new
//! supergroup (`chatmigrate.go:148-166`); a send failure is only logged.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::migration::MigrationHandlers,
    },
    config::AppConfig,
    db::{
        Database, chat_history, feature_flags,
        models::{CHAT_TYPE_GROUP, NewTelegramChatHistory, TokenUsage},
        recap_logs, recap_options, subscribers,
    },
    i18n::I18n,
    redis::recap_state::{InMemoryRecapStateStore, TestClock},
    services::{
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
    },
};
use serde_json::Value;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{Me, Message};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

const OLD_CHAT_ID: i64 = -234_500_000;
const NEW_CHAT_ID: i64 = -1_001_234_500_000;
const MEMBER_USER_ID: i64 = 7_654_321_098;
const START_MS: i64 = 1_700_000_000_000;

const EXPECTED_NOTIFICATION_ZH_HANS: &str = "Test Bot @TestBot 监测到您的群组已从 <b>群组（group）</b> 升级为了 <b>超级群组（supergroup）</b>，届时，群组的 ID 将会发生变更，<b>现已自动将过去的历史记录和数据留存自动迁移到了新的群组 ID 名下</b>，之前的设置将会保留并继续沿用，不过需要注意的是，由于 Telegram 官方的限制，迁移事件前的消息 ID 将无法与今后发送的消息 ID 相兼容，所以当下一次总结消息时将不会包含在迁移事件发生前所发送的消息，由此带来的不便敬请谅解。";

async fn test_context(server: &MockServer, database: Database) -> Arc<AppContext> {
    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "test-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram", server.uri()),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "chat-migration-test-key".to_owned(),
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
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("chat migration test config");
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
            recap_state: Some(Arc::new(InMemoryRecapStateStore::new(Arc::new(
                TestClock::new(START_MS),
            )))),
            raw_telegram_http: reqwest::Client::new(),
            message_preprocessor: None,
        },
    )
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

/// The service message Telegram delivers to the **new supergroup**, carrying
/// `migrate_from_chat_id`. This is Go's actual trigger (`chatmigrate.go:56-68`).
fn migrate_from_message() -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 1,
        "date": 1_710_000_000,
        "chat": {
            "id": NEW_CHAT_ID,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "from": {
            "id": 1_087_968_824_i64,
            "is_bot": true,
            "first_name": "Group",
            "username": "GroupAnonymousBot"
        },
        "sender_chat": {
            "id": NEW_CHAT_ID,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "migrate_from_chat_id": OLD_CHAT_ID
    }))
    .expect("valid migrate_from service message fixture")
}

/// The service message Telegram delivers to the **old group**, carrying only
/// `migrate_to_chat_id`. Go never reacts to this side.
fn migrate_to_only_message() -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": 1,
        "date": 1_710_000_000,
        "chat": {
            "id": OLD_CHAT_ID,
            "type": "group",
            "title": "Parity Lab"
        },
        "from": {
            "id": 1_087_968_824_i64,
            "is_bot": true,
            "first_name": "Group",
            "username": "GroupAnonymousBot"
        },
        "migrate_to_chat_id": NEW_CHAT_ID
    }))
    .expect("valid migrate_to-only service message fixture")
}

fn sample_message(chat_id: i64, message_id: i64) -> NewTelegramChatHistory {
    NewTelegramChatHistory {
        chat_id,
        chat_type: CHAT_TYPE_GROUP.to_owned(),
        chat_title: "Parity Lab".to_owned(),
        message_id,
        user_id: MEMBER_USER_ID,
        username: "sender_one".to_owned(),
        full_name: "Sender One".to_owned(),
        text: "captured".to_owned(),
        replied_to_message_id: 0,
        replied_to_user_id: 0,
        replied_to_full_name: String::new(),
        replied_to_username: String::new(),
        replied_to_text: String::new(),
        replied_to_chat_type: String::new(),
        chatted_at: START_MS,
    }
}

/// Seed the five tables the migration coordinator reaches, on `chat_id`, with
/// the stored language explicitly set to `zh-Hans` so the notification test
/// proves the handler actually reads it rather than always defaulting.
async fn seed_migratable_chat(db: &Database, chat_id: i64) {
    feature_flags::enable_recap(db, chat_id, CHAT_TYPE_GROUP, "Parity Lab")
        .await
        .expect("seed the recap feature flag");
    feature_flags::set_language(db, chat_id, CHAT_TYPE_GROUP, "Parity Lab", "zh-Hans")
        .await
        .expect("seed the group language");
    recap_options::find_one_or_create(db, chat_id)
        .await
        .expect("seed the recap options");
    subscribers::insert_unchecked(db, chat_id, MEMBER_USER_ID)
        .await
        .expect("seed one subscriber");
    chat_history::save_one(db, &sample_message(chat_id, 1))
        .await
        .expect("seed one chat history row");
    recap_logs::create_group_recap(
        db,
        chat_id,
        "the recap inputs",
        "the recap outputs",
        TokenUsage {
            prompt_tokens: 11,
            completion_tokens: 22,
            total_tokens: 33,
        },
        "gpt-parity",
    )
    .await
    .expect("seed one recap log row");
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

#[tokio::test]
async fn a_migrate_from_message_in_the_new_supergroup_migrates_data_and_notifies() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/telegram/bottest-token/SendMessage"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": 500,
                "date": 1_710_000_001,
                "chat": {"id": NEW_CHAT_ID, "type": "supergroup"},
                "text": "notified"
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    seed_migratable_chat(&database, OLD_CHAT_ID).await;
    let context = test_context(&server, database.clone()).await;

    MigrationHandlers::handle_chat_migration(
        context.config.telegram.bot(),
        migrate_from_message(),
        bot_me(),
        context,
    )
    .await
    .expect("migrate_from message is handled");

    // The migration coordinator moved the old group's rows onto the new
    // supergroup id, and left nothing behind under the old id.
    assert!(
        feature_flags::find_one_for_groups(&database, NEW_CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_some(),
        "feature flags moved to the new supergroup id"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, OLD_CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_none(),
        "no feature flag row remains under the old group id"
    );
    assert_eq!(
        subscribers::list(&database, NEW_CHAT_ID)
            .await
            .expect("subscribers are readable")
            .len(),
        1,
        "the subscriber moved to the new supergroup id"
    );
    assert!(
        subscribers::list(&database, OLD_CHAT_ID)
            .await
            .expect("subscribers are readable")
            .is_empty(),
        "no subscriber remains under the old group id"
    );

    let requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(requests.len(), 1, "exactly one Telegram request was sent");
    let body = request_body(&requests[0]);
    assert_eq!(body["chat_id"].as_i64(), Some(NEW_CHAT_ID));
    assert_eq!(body["parse_mode"], "HTML");
    assert_eq!(body["text"], EXPECTED_NOTIFICATION_ZH_HANS);
}

#[tokio::test]
async fn a_migrate_to_only_message_in_the_old_group_is_not_the_trigger() {
    let server = MockServer::start().await;
    // No mock is mounted for SendMessage: the assertion below on
    // `received_requests()` is what proves the control group sends nothing.

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    seed_migratable_chat(&database, OLD_CHAT_ID).await;
    let context = test_context(&server, database.clone()).await;

    MigrationHandlers::handle_chat_migration(
        context.config.telegram.bot(),
        migrate_to_only_message(),
        bot_me(),
        context,
    )
    .await
    .expect("migrate_to-only message is a no-op");

    assert!(
        feature_flags::find_one_for_groups(&database, OLD_CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_some(),
        "the old group's feature flag row is untouched"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, NEW_CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_none(),
        "nothing was created under the new supergroup id"
    );
    assert_eq!(
        subscribers::list(&database, OLD_CHAT_ID)
            .await
            .expect("subscribers are readable")
            .len(),
        1,
        "the old group's subscriber is untouched"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "the migrate_to-only side sends no Telegram notification"
    );
}

#[tokio::test]
async fn a_failed_notification_send_does_not_undo_the_migration_or_error_the_handler() {
    let server = MockServer::start().await;
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
    seed_migratable_chat(&database, OLD_CHAT_ID).await;
    let context = test_context(&server, database.clone()).await;

    let outcome = MigrationHandlers::handle_chat_migration(
        context.config.telegram.bot(),
        migrate_from_message(),
        bot_me(),
        context,
    )
    .await;

    assert!(
        outcome.is_ok(),
        "a best-effort notification failure must not surface as a handler error"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, NEW_CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_some(),
        "migration results survive a notification failure"
    );
    assert_eq!(
        subscribers::list(&database, NEW_CHAT_ID)
            .await
            .expect("subscribers are readable")
            .len(),
        1,
        "the subscriber migration survives a notification failure"
    );
}
