//! Integration coverage for the production `spawn_autorecap` wiring itself,
//! rather than the scripted fakes exercised in `autorecap_worker_tests.rs`.
//!
//! `queue_all_enabled_chats` and its `DatabaseAutoRecapStartupSeeder` adapter
//! are only reachable from an external test crate through the public
//! `spawn_autorecap` entry point: the seeder type and the capsule dispatcher
//! it feeds are both module-private. `spawn_autorecap` awaits the startup
//! seeding pass directly (its two `tokio::spawn` calls happen only after that
//! await resolves), so calling it to completion is enough to observe the real
//! database adapter's effects without any sleep or polling.

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::context::{AppContext, RecapRuntimeDependencies},
    config::AppConfig,
    db::{
        Database, chat_history, codec, feature_flags,
        models::{AutoRecapSendMode, NewTelegramChatHistory},
        recap_options, subscribers,
    },
    i18n::I18n,
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
    services::{
        autorecap::{
            AutoRecapPreparation, generate_and_deliver_auto_recap, handle_auto_recap_capsule,
            spawn_autorecap,
        },
        autorecap_queue::decode_auto_recap_member,
        openai::OpenAiClient,
        rate_limit::GoRateLimiter,
    },
};
use support::sqlite_fixture::SchemaFixture;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, body_string_contains, method, path},
};

const START_MS: i64 = 1_700_000_000_000;
const CUSTOM_RATE_CHAT_ID: i64 = -100_611_111;
const DEFAULT_RATE_CHAT_ID: i64 = -100_622_222;
const DISABLED_CHAT_ID: i64 = -100_633_333;

/// A minimal, network-inert [`AppContext`]: `queue_all_enabled_chats` never
/// dials Telegram or OpenAI, so the endpoints only need to parse.
async fn runtime_context(database: Database, state: Arc<dyn RecapStateStore>) -> Arc<AppContext> {
    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), "runtime-token".to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            "http://127.0.0.1:9".to_owned(),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "runtime-test-key".to_owned(),
        ),
        (
            "OPENAI_API_HOST".to_owned(),
            "http://127.0.0.1:9/v1".to_owned(),
        ),
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
        ("TIMEZONE_SHIFT_SECONDS".to_owned(), "28800".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("runtime test config");
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

#[tokio::test]
async fn spawn_autorecap_seeds_every_enabled_chat_through_the_real_database_adapter() {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;

    // A chat whose rate was already customised before startup: the adapter's
    // `find_or_create_rate` must read it back, not clobber it with the
    // find-or-create default.
    feature_flags::enable_recap(&database, CUSTOM_RATE_CHAT_ID, "supergroup", "Custom Rate")
        .await
        .expect("enable the custom-rate chat");
    recap_options::set_rates_per_day(&database, CUSTOM_RATE_CHAT_ID, 2)
        .await
        .expect("seed a custom rate before startup");

    // A chat enabled with no options row yet: the adapter must create one via
    // the find-or-create path (daily rate of four).
    feature_flags::enable_recap(&database, DEFAULT_RATE_CHAT_ID, "group", "Default Rate")
        .await
        .expect("enable the default-rate chat");

    // A chat that is not enabled at all: `list_enabled_chat_ids` must exclude it.
    feature_flags::disable_recap(&database, DISABLED_CHAT_ID, "group", "Disabled")
        .await
        .expect("leave the third chat disabled");

    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let ctx = runtime_context(database.clone(), state.clone()).await;

    spawn_autorecap(ctx).await;

    let queued_chat_ids = state
        .raw_zset(keys::AUTO_RECAP_QUEUE_KEY)
        .expect("the two enabled chats were queued")
        .into_iter()
        .map(|(_score, member)| {
            decode_auto_recap_member(&member)
                .expect("every queued member decodes")
                .chat_id
        })
        .collect::<Vec<_>>();
    assert_eq!(
        queued_chat_ids.len(),
        2,
        "only the two enabled chats are queued: {queued_chat_ids:?}"
    );
    assert!(queued_chat_ids.contains(&CUSTOM_RATE_CHAT_ID));
    assert!(queued_chat_ids.contains(&DEFAULT_RATE_CHAT_ID));
    assert!(
        !queued_chat_ids.contains(&DISABLED_CHAT_ID),
        "the disabled chat must never reach the queue"
    );

    let custom_rate_options = recap_options::find_one(&database, CUSTOM_RATE_CHAT_ID)
        .await
        .expect("read back the custom-rate options")
        .expect("the pre-seeded row still exists");
    assert_eq!(
        custom_rate_options.auto_recap_rates_per_day, 2,
        "the adapter must read the existing rate, not overwrite it"
    );

    let default_rate_options = recap_options::find_one(&database, DEFAULT_RATE_CHAT_ID)
        .await
        .expect("read back the created options")
        .expect("find_or_create_rate must have materialised a row");
    assert_eq!(
        default_rate_options.auto_recap_rates_per_day, 4,
        "a missing options row is created with Go's find-or-create default rate"
    );
}

// Coverage below targets ADR 0001 decision 10: `handle_auto_recap_capsule`
// and `generate_and_deliver_auto_recap` gained a crate-visible, awaitable
// test seam (see `src/services/autorecap.rs`) precisely so this module can
// drive the three `AutoRecapPreparation` dispatch branches and the full
// generation-and-delivery pipeline without sleeping or polling.

const DISPATCH_DISABLED_CHAT_ID: i64 = -100_711_111;
const DISPATCH_PRIVATE_NO_SUBSCRIBERS_CHAT_ID: i64 = -100_722_222;
const DISPATCH_GENERATE_CHAT_ID: i64 = -100_733_333;
const DIRECT_INSUFFICIENT_HISTORY_CHAT_ID: i64 = -100_744_444;
const SUBSCRIBER_USER_ID: i64 = 900_555;
const BOT_TOKEN: &str = "test-token";

/// A network-wired [`AppContext`]: unlike [`runtime_context`], every
/// Telegram and OpenAI endpoint routes to `server`, for the tests below that
/// drive `generate_and_deliver_auto_recap`'s real HTTP calls.
async fn wired_runtime_context(
    server: &MockServer,
    database: Database,
    state: Arc<dyn RecapStateStore>,
) -> Arc<AppContext> {
    let values = BTreeMap::from([
        ("TELEGRAM_BOT_TOKEN".to_owned(), BOT_TOKEN.to_owned()),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            format!("{}/telegram", server.uri()),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "wired-runtime-test-key".to_owned(),
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
        ("TIMEZONE_SHIFT_SECONDS".to_owned(), "28800".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("wired runtime test config");
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

async fn insert_auto_recap_histories(
    database: &Database,
    chat_id: i64,
    chat_title: &str,
    count: i64,
) {
    let now = chrono::Utc::now().timestamp_millis();
    for index in 1..=count {
        chat_history::save_one(
            database,
            &NewTelegramChatHistory {
                chat_id,
                chat_type: "supergroup".to_owned(),
                chat_title: chat_title.to_owned(),
                message_id: index,
                user_id: 2_000 + index,
                username: format!("autorecap_user{index}"),
                full_name: format!("Auto Recap User {index}"),
                text: format!("auto recap message {index}"),
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
        .expect("insert automatic recap history");
    }
}

async fn mount_get_chat(server: &MockServer, chat_id: i64, chat_title: &str) {
    Mock::given(path(format!("/telegram/bot{BOT_TOKEN}/GetChat")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "id": chat_id,
                "type": "group",
                "title": chat_title,
                "max_reaction_count": 0
            }
        })))
        .expect(1)
        .mount(server)
        .await;
}

/// Mounts one `sendRichMessage` expectation scoped to `chat_id`'s
/// form-encoded request (matched via the literal `chat_id=<id>` substring,
/// the same field `recap_manual_tests.rs` decodes from the request form), so
/// each target's mocked Telegram response echoes back its own chat: the
/// delivery pipeline records `sent_messages.chat_id` from that echoed
/// response, not from the outbound request.
async fn mount_send_rich_message_success(server: &MockServer, chat_id: i64, message_id: i32) {
    Mock::given(path(format!("/telegram/bot{BOT_TOKEN}/sendRichMessage")))
        .and(body_string_contains(format!("chat_id={chat_id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "ok": true,
            "result": {
                "message_id": message_id,
                "date": 1_710_000_501,
                "chat": {"id": chat_id, "type": "supergroup", "title": "irrelevant"},
                "text": "delivered automatic recap"
            }
        })))
        .expect(1)
        .mount(server)
        .await;
}

fn completion_response(model: &str, content: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-autorecap-runtime",
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

struct StoredAutoRecapMessage {
    chat_id: i64,
    text: String,
}

/// Every stored row, in insertion order. Each test below owns an isolated
/// [`SchemaFixture`] database, so no `chat_id` filter is needed.
async fn stored_auto_recap_messages(database: &Database) -> Vec<StoredAutoRecapMessage> {
    let rows = sqlx::query(
        "SELECT CAST(chat_id AS TEXT), CAST(text AS TEXT) FROM sent_messages ORDER BY rowid",
    )
    .fetch_all(&database.pool)
    .await
    .expect("read sent messages");

    rows.iter()
        .map(|row| StoredAutoRecapMessage {
            chat_id: codec::i64_at(row, 0).expect("chat id"),
            text: codec::text_at(row, 1).expect("text"),
        })
        .collect()
}

async fn recap_log_count(database: &Database, chat_id: i64) -> i64 {
    let row = sqlx::query(
        "SELECT CAST(COUNT(*) AS TEXT) FROM log_chat_histories_recaps WHERE chat_id = $1",
    )
    .bind(chat_id)
    .fetch_one(&database.pool)
    .await
    .expect("count recap logs");
    codec::i64_at(&row, 0).expect("recap log count")
}

#[tokio::test]
async fn handle_auto_recap_capsule_dispatches_disabled_preparation_without_spawning_generation() {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let ctx = wired_runtime_context(&server, database, state.clone()).await;

    let dispatch = handle_auto_recap_capsule(ctx, state, DISPATCH_DISABLED_CHAT_ID)
        .await
        .expect("a never-enabled chat still resolves a preparation");

    assert_eq!(dispatch.preparation, AutoRecapPreparation::Disabled);
    assert!(
        dispatch.generation.is_none(),
        "the Disabled branch must never spawn a generation task"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("mock server request log")
            .is_empty(),
        "a disabled capsule must never reach Telegram or OpenAI"
    );
}

#[tokio::test]
async fn handle_auto_recap_capsule_dispatches_private_without_subscribers_without_spawning_generation()
 {
    let server = MockServer::start().await;
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(
        &database,
        DISPATCH_PRIVATE_NO_SUBSCRIBERS_CHAT_ID,
        "supergroup",
        "Private Only Auto Recap",
    )
    .await
    .expect("enable the private-only chat");
    recap_options::set_send_mode(
        &database,
        DISPATCH_PRIVATE_NO_SUBSCRIBERS_CHAT_ID,
        AutoRecapSendMode::OnlyPrivateSubscriptions,
    )
    .await
    .expect("switch the chat to private-only delivery");
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let ctx = wired_runtime_context(&server, database, state.clone()).await;

    let dispatch = handle_auto_recap_capsule(ctx, state, DISPATCH_PRIVATE_NO_SUBSCRIBERS_CHAT_ID)
        .await
        .expect("a private-only chat with no subscribers still resolves a preparation");

    match dispatch.preparation {
        AutoRecapPreparation::PrivateWithoutSubscribers { options } => {
            assert_eq!(options.auto_recap_send_mode, 1);
        }
        other => panic!("expected PrivateWithoutSubscribers, got {other:?}"),
    }
    assert!(
        dispatch.generation.is_none(),
        "the PrivateWithoutSubscribers branch must never spawn a generation task"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("mock server request log")
            .is_empty(),
        "a private-only capsule with no subscribers must never reach Telegram or OpenAI"
    );
}

#[tokio::test]
async fn handle_auto_recap_capsule_dispatches_generate_and_the_awaited_handle_delivers_publicly_first()
 {
    let server = MockServer::start().await;
    mount_get_chat(&server, DISPATCH_GENERATE_CHAT_ID, "Auto Recap Chat").await;
    mount_openai_response(
        &server,
        "detail-model",
        "detail-model-resolved",
        "## 討論主題\n- 自動回顧涵蓋六筆歷史",
    )
    .await;
    mount_openai_response(
        &server,
        "condensed-model",
        "condensed-model-resolved",
        "**濃縮**——自動回顧完成。",
    )
    .await;
    mount_send_rich_message_success(&server, DISPATCH_GENERATE_CHAT_ID, 501).await;
    mount_send_rich_message_success(&server, SUBSCRIBER_USER_ID, 502).await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    feature_flags::enable_recap(
        &database,
        DISPATCH_GENERATE_CHAT_ID,
        "supergroup",
        "Auto Recap Chat",
    )
    .await
    .expect("enable the generate-path chat");
    // Publicly mode with a daily rate of four, matching Go's find-or-create
    // default: `enable_recap` only flips the feature flag, it never
    // materialises the `telegram_chat_recaps_options` row `prepare_auto_recap`
    // requires once the chat is enabled.
    recap_options::find_one_or_create(&database, DISPATCH_GENERATE_CHAT_ID)
        .await
        .expect("materialise default public options");
    subscribers::subscribe(&database, DISPATCH_GENERATE_CHAT_ID, SUBSCRIBER_USER_ID)
        .await
        .expect("seed one private subscriber");
    insert_auto_recap_histories(&database, DISPATCH_GENERATE_CHAT_ID, "Auto Recap Chat", 6).await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let ctx = wired_runtime_context(&server, database.clone(), state.clone()).await;

    let dispatch = handle_auto_recap_capsule(ctx, state, DISPATCH_GENERATE_CHAT_ID)
        .await
        .expect("an enabled public chat with enough history resolves a preparation");

    assert!(
        matches!(dispatch.preparation, AutoRecapPreparation::Generate { .. }),
        "an enabled public chat with six histories must dispatch to Generate"
    );
    let handle = dispatch
        .generation
        .expect("the Generate branch must spawn a generation task");
    tokio::time::timeout(Duration::from_secs(15), handle)
        .await
        .expect(
            "the 5/s delivery limiter must not block the generation task past a bounded timeout",
        )
        .expect("the generation task must not panic");

    let requests = server.received_requests().await.expect("mock request log");
    let paths = requests
        .iter()
        .map(|request| request.url.path().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        paths,
        [
            format!("/telegram/bot{BOT_TOKEN}/GetChat"),
            "/v1/chat/completions".to_owned(),
            "/v1/chat/completions".to_owned(),
            format!("/telegram/bot{BOT_TOKEN}/sendRichMessage"),
            format!("/telegram/bot{BOT_TOKEN}/sendRichMessage"),
        ],
        "history is fetched and generated once, then delivered publicly before the subscriber"
    );

    let messages = stored_auto_recap_messages(&database).await;
    assert_eq!(
        messages.iter().map(|row| row.chat_id).collect::<Vec<_>>(),
        vec![DISPATCH_GENERATE_CHAT_ID, SUBSCRIBER_USER_ID],
        "the public group delivery is stored before the private subscriber delivery"
    );
    assert!(messages.iter().all(|row| !row.text.is_empty()));
    assert_eq!(
        recap_log_count(&database, DISPATCH_GENERATE_CHAT_ID).await,
        1,
        "one detailed generation pass produces exactly one recap log row"
    );
}

#[tokio::test]
async fn generate_and_deliver_auto_recap_returns_ok_and_sends_nothing_when_histories_are_insufficient()
 {
    let server = MockServer::start().await;
    mount_get_chat(
        &server,
        DIRECT_INSUFFICIENT_HISTORY_CHAT_ID,
        "Sparse Auto Recap Chat",
    )
    .await;

    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let options = recap_options::find_one_or_create(&database, DIRECT_INSUFFICIENT_HISTORY_CHAT_ID)
        .await
        .expect("materialise default options");
    insert_auto_recap_histories(
        &database,
        DIRECT_INSUFFICIENT_HISTORY_CHAT_ID,
        "Sparse Auto Recap Chat",
        5,
    )
    .await;
    let state = Arc::new(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        START_MS,
    ))));
    let ctx = wired_runtime_context(&server, database.clone(), state.clone()).await;

    generate_and_deliver_auto_recap(
        ctx,
        state,
        DIRECT_INSUFFICIENT_HISTORY_CHAT_ID,
        options,
        Vec::new(),
    )
    .await
    .expect("an insufficient-history capsule still resolves to Ok(())");

    let requests = server.received_requests().await.expect("mock request log");
    assert_eq!(
        requests
            .iter()
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>(),
        [format!("/telegram/bot{BOT_TOKEN}/GetChat")],
        "generation must short-circuit right after the history fetch, before any OpenAI call"
    );
    assert!(
        stored_auto_recap_messages(&database).await.is_empty(),
        "no message may be delivered when history is insufficient"
    );
    assert_eq!(
        recap_log_count(&database, DIRECT_INSUFFICIENT_HISTORY_CHAT_ID).await,
        0,
        "no recap log may be written when generation never starts"
    );
}
