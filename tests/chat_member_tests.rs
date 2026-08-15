//! Bot-left production wiring against Go v1.0.0 `welcome/welcome.go:57-135`.
//!
//! Only a `my_chat_member` transition of the bot itself to exactly `left`
//! triggers the five-step cleanup: subscribers, feature flags, recap options,
//! and chat histories are deleted, while the recap log keeps its row and only
//! blanks its input and output text. A ban (Telegram status `kicked`) matches
//! no Go branch and must leave every row alone. Neither path sends any
//! Telegram request.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::{
        context::{AppContext, RecapRuntimeDependencies},
        handlers::chat_member::handle_my_chat_member,
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
use sqlx::AnyPool;
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::ChatMemberUpdated;
use wiremock::MockServer;

const CHAT_ID: i64 = -1_001_234_567_890;
const MEMBER_USER_ID: i64 = 7_654_321_098;
const START_MS: i64 = 1_700_000_000_000;

async fn test_context(server: &MockServer, database: Database) -> Arc<AppContext> {
    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "test-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram", server.uri()),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "chat-member-test-key".to_owned(),
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
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("chat member test config");
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

fn bot_user() -> Value {
    serde_json::json!({
        "id": 9_999,
        "is_bot": true,
        "first_name": "Test Bot",
        "username": "TestBot"
    })
}

fn chat_member_update(new_chat_member: Value) -> ChatMemberUpdated {
    serde_json::from_value(serde_json::json!({
        "chat": {
            "id": CHAT_ID,
            "type": "supergroup",
            "title": "Parity Lab"
        },
        "from": {
            "id": 42,
            "is_bot": false,
            "first_name": "Ada"
        },
        "date": 1_710_000_000,
        "old_chat_member": {
            "status": "member",
            "user": bot_user()
        },
        "new_chat_member": new_chat_member
    }))
    .expect("valid chat member update")
}

/// The bot's own membership becoming exactly `left`.
fn left_update() -> ChatMemberUpdated {
    chat_member_update(serde_json::json!({
        "status": "left",
        "user": bot_user()
    }))
}

/// The bot being banned: Telegram reports `kicked`, which is not `left`.
fn banned_update() -> ChatMemberUpdated {
    chat_member_update(serde_json::json!({
        "status": "kicked",
        "user": bot_user(),
        "until_date": 0
    }))
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

/// Seed the five tables the cleanup reaches, returning the recap log id.
async fn seed_recap_chat(db: &Database) -> String {
    feature_flags::enable_recap(db, CHAT_ID, CHAT_TYPE_GROUP, "Parity Lab")
        .await
        .expect("seed the recap feature flag");
    recap_options::find_one_or_create(db, CHAT_ID)
        .await
        .expect("seed the recap options");
    subscribers::insert_unchecked(db, CHAT_ID, MEMBER_USER_ID)
        .await
        .expect("seed one subscriber");
    chat_history::save_one(db, &sample_message(CHAT_ID, 1))
        .await
        .expect("seed one chat history row");
    recap_logs::create_group_recap(
        db,
        CHAT_ID,
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
    .expect("seed one recap log row")
}

/// The log row's text columns, proving the row itself still exists.
async fn read_recap_log(pool: &AnyPool, log_id: &str) -> (String, String) {
    let row: (String, String) = sqlx::query_as(
        "SELECT CAST(recap_inputs AS TEXT), CAST(recap_outputs AS TEXT)
         FROM log_chat_histories_recaps WHERE CAST(id AS TEXT) = $1",
    )
    .bind(log_id)
    .fetch_one(pool)
    .await
    .expect("the recap log row is readable");
    (row.0, row.1)
}

#[tokio::test]
async fn a_left_update_runs_the_five_step_cleanup_without_a_telegram_reply() {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let log_id = seed_recap_chat(&database).await;
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(left_update(), context)
        .await
        .expect("bot-left update");

    assert!(
        subscribers::list(&database, CHAT_ID)
            .await
            .expect("subscribers are readable")
            .is_empty(),
        "every subscriber of the chat is deleted"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_none(),
        "the feature flag row is deleted"
    );
    assert!(
        recap_options::find_one(&database, CHAT_ID)
            .await
            .expect("options are readable")
            .is_none(),
        "the recap options row is deleted"
    );
    assert!(
        chat_history::find_chatted_after(&database, CHAT_ID, 0)
            .await
            .expect("histories are readable")
            .is_empty(),
        "every chat history row is deleted"
    );
    let (inputs, outputs) = read_recap_log(&database.pool, &log_id).await;
    assert_eq!(inputs, "", "the recap log input text is blanked");
    assert_eq!(outputs, "", "the recap log output text is blanked");
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty(),
        "Go's bot-left cleanup sends no Telegram reply"
    );
}

#[tokio::test]
async fn a_banned_update_leaves_every_row_alone() {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let log_id = seed_recap_chat(&database).await;
    let context = test_context(&server, database.clone()).await;

    handle_my_chat_member(banned_update(), context)
        .await
        .expect("bot-banned update");

    assert_eq!(
        subscribers::list(&database, CHAT_ID)
            .await
            .expect("subscribers are readable")
            .len(),
        1,
        "a ban is not Go's left status, so the subscriber survives"
    );
    assert!(
        feature_flags::find_one_for_groups(&database, CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_some()
    );
    assert!(
        recap_options::find_one(&database, CHAT_ID)
            .await
            .expect("options are readable")
            .is_some()
    );
    assert_eq!(
        chat_history::find_chatted_after(&database, CHAT_ID, 0)
            .await
            .expect("histories are readable")
            .len(),
        1
    );
    let (inputs, outputs) = read_recap_log(&database.pool, &log_id).await;
    assert_eq!(inputs, "the recap inputs");
    assert_eq!(outputs, "the recap outputs");
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
}
