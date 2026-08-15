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
    db::{Database, feature_flags, recap_options},
    i18n::I18n,
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    },
    services::{
        autorecap::spawn_autorecap,
        autorecap_queue::decode_auto_recap_member,
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
    },
};
use support::sqlite_fixture::SchemaFixture;

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
        CommandRateLimiter::new(1, Duration::from_secs(1)),
        None,
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
