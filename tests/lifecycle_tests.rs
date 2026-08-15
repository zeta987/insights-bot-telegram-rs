//! Coverage for the process lifecycle surface added to track Go parity:
//! the composite `/health` JSON endpoint (`docs/adr/0001-go-parity-adjudication.md`,
//! decision 7) and the automatic-recap poller's cooperative shutdown signal
//! (decision 8).

mod support;

use std::{collections::BTreeMap, sync::Arc, time::Duration};

use insights_bot_telegram_rs::{
    bot::context::{AppContext, RecapRuntimeDependencies},
    config::AppConfig,
    db::Database,
    http::health,
    i18n::I18n,
    lifecycle::LifecycleFlags,
    redis::recap_state::{InMemoryRecapStateStore, RecapStateStore, TestClock},
    services::{
        autorecap::run_autorecap_poll_loop,
        autorecap_queue::AUTO_RECAP_POLL_INTERVAL,
        openai::OpenAiClient,
        rate_limit::{CommandRateLimiter, GoRateLimiter},
    },
};
use serde_json::Value;
use support::sqlite_fixture::SchemaFixture;

const START_MS: i64 = 1_700_000_000_000;

/// A minimal, network-inert [`AppContext`], mirroring the fixture used by
/// `tests/autorecap_runtime_tests.rs`: nothing under test here dials
/// Telegram, Redis, or OpenAI, so the endpoints only need to parse.
async fn runtime_context(database: Database, state: Arc<dyn RecapStateStore>) -> Arc<AppContext> {
    let values = BTreeMap::from([
        (
            "TELEGRAM_BOT_TOKEN".to_owned(),
            "lifecycle-token".to_owned(),
        ),
        (
            "TELEGRAM_BOT_API_ENDPOINT".to_owned(),
            "http://127.0.0.1:9".to_owned(),
        ),
        (
            "OPENAI_API_SECRET".to_owned(),
            "lifecycle-test-key".to_owned(),
        ),
        (
            "OPENAI_API_HOST".to_owned(),
            "http://127.0.0.1:9/v1".to_owned(),
        ),
        ("REDIS_PORT".to_owned(), "6379".to_owned()),
    ]);
    let config =
        AppConfig::from_lookup(|key| values.get(key).cloned()).expect("lifecycle test config");
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

/// Go's `alexliesenfeld/health` JSON shape (`status` + `details` keyed by
/// check name) is reproduced over [`LifecycleFlags`] instead of Go's checker
/// library; the aggregate flips `up`/200 only once every named check is up,
/// and each flag transition is independently observable through `/health`.
#[tokio::test]
async fn health_reports_down_until_all_three_flags_flip_true() {
    let lifecycle = LifecycleFlags::new();
    let addr = "127.0.0.1:0".parse().expect("loopback address parses");
    let server = health::serve(lifecycle.clone(), addr)
        .await
        .expect("health server binds on an OS-assigned port");
    let base = format!("http://{}", server.local_addr);
    let client = reqwest::Client::new();

    let response = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), reqwest::StatusCode::SERVICE_UNAVAILABLE);
    let body: Value = response.json().await.expect("valid JSON body");
    assert_eq!(body["status"], "down");
    assert_eq!(body["details"]["telegram_bot"]["status"], "down");
    assert_eq!(
        body["details"]["auto recap timecapsule digger"]["status"],
        "down"
    );
    assert_eq!(body["details"]["auto_recap"]["status"], "down");

    lifecycle.mark_bot_authorized();
    lifecycle.mark_poller_started();
    let response = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(
        response.status(),
        reqwest::StatusCode::SERVICE_UNAVAILABLE,
        "aggregate stays down while auto_recap has not started"
    );
    let body: Value = response.json().await.expect("valid JSON body");
    assert_eq!(body["status"], "down");
    assert_eq!(body["details"]["telegram_bot"]["status"], "up");
    assert_eq!(
        body["details"]["auto recap timecapsule digger"]["status"],
        "up"
    );
    assert_eq!(body["details"]["auto_recap"]["status"], "down");

    lifecycle.mark_auto_recap_started();
    let response = client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("request succeeds");
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let body: Value = response.json().await.expect("valid JSON body");
    assert_eq!(body["status"], "up");
    assert_eq!(body["details"]["telegram_bot"]["status"], "up");
    assert_eq!(
        body["details"]["auto recap timecapsule digger"]["status"],
        "up"
    );
    assert_eq!(body["details"]["auto_recap"]["status"], "up");

    server.shutdown().await;
}

/// The health server's own graceful shutdown must actually stop accepting
/// connections, bounded well inside Go's ten-second `Shutdown` timeout.
#[tokio::test]
async fn health_server_stops_accepting_connections_after_shutdown() {
    let lifecycle = LifecycleFlags::new();
    let addr = "127.0.0.1:0".parse().expect("loopback address parses");
    let server = health::serve(lifecycle, addr)
        .await
        .expect("health server binds");
    let base = format!("http://{}", server.local_addr);
    let client = reqwest::Client::new();

    client
        .get(format!("{base}/health"))
        .send()
        .await
        .expect("server answers before shutdown");

    server.shutdown().await;

    let after_shutdown = client.get(format!("{base}/health")).send().await;
    assert!(
        after_shutdown.is_err(),
        "the listener must be gone once shutdown() returns"
    );
}

/// The automatic-recap poller loop (Go's digger polling goroutine) must stop
/// promptly once the shared shutdown signal flips, without needing real
/// sleeps: a paused clock proves the loop is actually ticking, then the
/// shutdown signal is expected to end the task well before the loop would
/// tick again.
///
/// The clock is paused manually (rather than via `#[tokio::test(start_paused
/// = true)]`) so the SQLite fixture's own connection-pool timers run on real
/// time during setup; pausing the clock before the pool opens makes its
/// internal wait time out instantly.
#[tokio::test]
async fn autorecap_poll_loop_stops_promptly_after_shutdown_signal() {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    let state: Arc<dyn RecapStateStore> = Arc::new(InMemoryRecapStateStore::new(Arc::new(
        TestClock::new(START_MS),
    )));
    let ctx = runtime_context(database, state.clone()).await;

    tokio::time::pause();

    let shutdown_rx = ctx.shutdown_rx.clone();
    let handle = tokio::spawn(run_autorecap_poll_loop(ctx.clone(), state, shutdown_rx));

    // Advance past several poll ticks (the queue is empty, so each tick is a
    // no-op `pop` against the in-memory store) to prove the loop is actually
    // running before it is asked to stop.
    tokio::time::advance(AUTO_RECAP_POLL_INTERVAL * 3).await;
    assert!(
        !handle.is_finished(),
        "the loop must still be running before the shutdown signal is sent"
    );

    ctx.shutdown_tx
        .send(true)
        .expect("the loop task is still alive to receive the signal");

    tokio::time::timeout(Duration::from_secs(5), handle)
        .await
        .expect("the loop must exit promptly once the shutdown signal flips")
        .expect("the loop task must not panic");
}
