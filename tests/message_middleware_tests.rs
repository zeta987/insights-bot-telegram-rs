//! Go v1.0.0 message middleware parity.
//!
//! Behaviour is pinned to
//! `internal/bots/telegram/middlewares/record_messsages.go`,
//! `internal/bots/telegram/middlewares/sync_with_edit_messages.go`, and
//! `internal/models/chathistories/private_forwarded.go`.
//!
//! Nothing here opens a socket: the link previewer is the deliberately failing
//! [`UnavailableLinkPreviewer`], the summarizer is a local double, the database
//! is a temporary SQLite file, and the recap state store is the in-memory one.

mod support;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use insights_bot_telegram_rs::{
    bot::middleware,
    db::{Database, chat_history, feature_flags, models::NewTelegramChatHistory},
    redis::{
        keys::StartContextDomain,
        recap_state::{Clock, InMemoryRecapStateStore, RecapStateStore, SystemClock},
    },
    services::message_capture::{
        CapturedChat, CapturedMessage, CapturedUser, DynMessagePreprocessor, LinkPreviewer,
        MessagePreprocessor, Summarizer, UnavailableLinkPreviewer, captured_message_from_teloxide,
        private_forwarded_replay_entry,
    },
};
use serde_json::json;
use support::sqlite_fixture::SchemaFixture;

const GROUP_CHAT_ID: i64 = -1_001_234_567_890;
const GROUP_CHAT_TITLE: &str = "Parity Lab";
const SENDER_USER_ID: i64 = 7_654_321_098;
const MESSAGE_ID: i64 = 42;
/// A Telegram `date`, in Unix seconds.
const MESSAGE_DATE: i64 = 1_700_000_000;
const MESSAGE_DATE_MS: i64 = 1_700_000_000_000;

// ---------------------------------------------------------------------------
// Offline doubles
// ---------------------------------------------------------------------------

/// A summarizer that returns no choices, so Go's "leave it alone" path runs.
struct SilentSummarizer;

#[async_trait]
impl Summarizer for SilentSummarizer {
    async fn summarize_any(
        &self,
        _content: &str,
        _deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        Ok(Vec::new())
    }

    async fn summarize_one_chat_history(
        &self,
        _content: &str,
        _deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        Ok(Vec::new())
    }
}

/// A summarizer that always fails, which is Go's returned error.
struct FailingSummarizer;

#[async_trait]
impl Summarizer for FailingSummarizer {
    async fn summarize_any(
        &self,
        _content: &str,
        _deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        Err(anyhow!("summarization failed"))
    }

    async fn summarize_one_chat_history(
        &self,
        _content: &str,
        _deadline: Option<Duration>,
    ) -> Result<Vec<String>> {
        Err(anyhow!("summarization failed"))
    }
}

fn preprocessor_with(summarizer: Arc<dyn Summarizer>) -> Arc<DynMessagePreprocessor> {
    let previewer: Arc<dyn LinkPreviewer> = Arc::new(UnavailableLinkPreviewer);
    Arc::new(MessagePreprocessor::new(previewer, summarizer))
}

fn preprocessor() -> Arc<DynMessagePreprocessor> {
    preprocessor_with(Arc::new(SilentSummarizer))
}

/// An in-memory recap state store that records every `ZADD` argument.
///
/// The score is invisible through [`RecapStateStore::forwarded_batch`], so it
/// is captured here instead of being inferred from the replay order.
struct RecordingRecapStateStore {
    inner: InMemoryRecapStateStore,
    appends: Mutex<Vec<(i64, i64, String)>>,
    forwarded_active_fails: bool,
}

impl RecordingRecapStateStore {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryRecapStateStore::new(Arc::new(SystemClock) as Arc<dyn Clock>),
            appends: Mutex::new(Vec::new()),
            forwarded_active_fails: false,
        })
    }

    fn failing() -> Arc<Self> {
        Arc::new(Self {
            inner: InMemoryRecapStateStore::new(Arc::new(SystemClock) as Arc<dyn Clock>),
            appends: Mutex::new(Vec::new()),
            forwarded_active_fails: true,
        })
    }

    fn appends(&self) -> Vec<(i64, i64, String)> {
        self.appends.lock().expect("recorded appends").clone()
    }
}

#[async_trait]
impl RecapStateStore for RecordingRecapStateStore {
    async fn put_callback(&self, route: &str, payload_json: &str) -> Result<String> {
        self.inner.put_callback(route, payload_json).await
    }

    async fn get_callback(&self, route: &str, action_hash: &str) -> Result<Option<String>> {
        self.inner.get_callback(route, action_hash).await
    }

    async fn check_manual_recap_rate(
        &self,
        chat_id: i64,
        rate: i64,
        per_seconds: i64,
    ) -> Result<insights_bot_telegram_rs::redis::recap_state::ManualRecapRateResult> {
        self.inner
            .check_manual_recap_rate(chat_id, rate, per_seconds)
            .await
    }

    async fn put_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
        json: &str,
    ) -> Result<()> {
        self.inner.put_start_context(domain, token, json).await
    }

    async fn get_start_context(
        &self,
        domain: StartContextDomain,
        token: &str,
    ) -> Result<Option<String>> {
        self.inner.get_start_context(domain, token).await
    }

    async fn forwarded_active(&self, user_id: i64) -> Result<bool> {
        if self.forwarded_active_fails {
            return Err(anyhow!("recap Redis GET failed"));
        }
        self.inner.forwarded_active(user_id).await
    }

    async fn start_forwarded(&self, user_id: i64) -> Result<()> {
        self.inner.start_forwarded(user_id).await
    }

    async fn append_forwarded(&self, user_id: i64, score_ms: i64, json: &str) -> Result<()> {
        self.appends
            .lock()
            .expect("recorded appends")
            .push((user_id, score_ms, json.to_owned()));
        self.inner.append_forwarded(user_id, score_ms, json).await
    }

    async fn forwarded_batch(&self, user_id: i64) -> Result<Vec<String>> {
        self.inner.forwarded_batch(user_id).await
    }

    async fn cancel_forwarded(&self, user_id: i64) -> Result<bool> {
        self.inner.cancel_forwarded(user_id).await
    }

    async fn push_delete_later(&self, user_id: i64, chat_id: i64, message_id: i32) -> Result<()> {
        self.inner
            .push_delete_later(user_id, chat_id, message_id)
            .await
    }

    async fn drain_delete_later(&self, user_id: i64) -> Result<Vec<(i64, i32)>> {
        self.inner.drain_delete_later(user_id).await
    }
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

async fn database() -> (SchemaFixture, Database) {
    let fixture = SchemaFixture::new();
    let db = fixture.bootstrap_database().await;
    (fixture, db)
}

fn sender() -> CapturedUser {
    CapturedUser {
        id: SENDER_USER_ID,
        username: "sender".to_string(),
        first_name: "John".to_string(),
        last_name: "Smith".to_string(),
    }
}

fn group_message(text: &str) -> CapturedMessage {
    CapturedMessage {
        message_id: MESSAGE_ID,
        date: MESSAGE_DATE,
        chat: CapturedChat {
            id: GROUP_CHAT_ID,
            kind: "supergroup".to_string(),
            title: GROUP_CHAT_TITLE.to_string(),
        },
        from: sender(),
        text: text.to_string(),
        ..Default::default()
    }
}

fn private_message(text: &str) -> CapturedMessage {
    CapturedMessage {
        message_id: MESSAGE_ID,
        date: MESSAGE_DATE,
        chat: CapturedChat {
            id: SENDER_USER_ID,
            kind: "private".to_string(),
            title: String::new(),
        },
        from: sender(),
        text: text.to_string(),
        ..Default::default()
    }
}

async fn stored_rows(db: &Database, chat_id: i64) -> Vec<String> {
    chat_history::find_chatted_after(db, chat_id, 0)
        .await
        .expect("the parity window query must run")
        .into_iter()
        .map(|row| row.text)
        .collect()
}

// ---------------------------------------------------------------------------
// Group and supergroup persistence
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_enabled_group_persists_the_preprocessed_row() {
    let (_fixture, db) = database().await;
    feature_flags::enable_recap(&db, GROUP_CHAT_ID, "supergroup", GROUP_CHAT_TITLE)
        .await
        .expect("the feature flag must be writable");

    middleware::record_captured_message(
        &db,
        None,
        Some(&preprocessor()),
        &group_message("hello parity"),
        Some(SENDER_USER_ID),
    )
    .await;

    assert_eq!(stored_rows(&db, GROUP_CHAT_ID).await, vec!["hello parity"]);
}

/// The write is complete when the call returns, which a detached task could not
/// promise. No sleep, no polling: a background `tokio::spawn` fails this.
#[tokio::test]
async fn the_group_write_completes_before_the_middleware_returns() {
    let (_fixture, db) = database().await;
    feature_flags::enable_recap(&db, GROUP_CHAT_ID, "supergroup", GROUP_CHAT_TITLE)
        .await
        .expect("the feature flag must be writable");

    middleware::record_captured_message(
        &db,
        None,
        Some(&preprocessor()),
        &group_message("awaited"),
        Some(SENDER_USER_ID),
    )
    .await;

    assert_eq!(stored_rows(&db, GROUP_CHAT_ID).await.len(), 1);
}

#[tokio::test]
async fn a_disabled_group_persists_nothing() {
    let (_fixture, db) = database().await;
    // No row at all is Go's disabled state, and so is an explicitly disabled one.
    feature_flags::disable_recap(&db, GROUP_CHAT_ID, "supergroup", GROUP_CHAT_TITLE)
        .await
        .expect("the feature flag must be writable");

    middleware::record_captured_message(
        &db,
        None,
        Some(&preprocessor()),
        &group_message("hello parity"),
        Some(SENDER_USER_ID),
    )
    .await;

    assert!(stored_rows(&db, GROUP_CHAT_ID).await.is_empty());
}

#[tokio::test]
async fn a_feature_flag_read_failure_persists_nothing_and_does_not_panic() {
    let (_fixture, db) = database().await;
    sqlx::query("DROP TABLE telegram_chat_feature_flags")
        .execute(&db.pool)
        .await
        .expect("the parity table must exist before it is dropped");

    middleware::record_captured_message(
        &db,
        None,
        Some(&preprocessor()),
        &group_message("hello parity"),
        Some(SENDER_USER_ID),
    )
    .await;

    assert!(stored_rows(&db, GROUP_CHAT_ID).await.is_empty());
}

#[tokio::test]
async fn a_preprocessing_failure_persists_nothing_and_does_not_panic() {
    let (_fixture, db) = database().await;
    feature_flags::enable_recap(&db, GROUP_CHAT_ID, "supergroup", GROUP_CHAT_TITLE)
        .await
        .expect("the feature flag must be writable");

    // Three hundred runes is Go's summarization threshold, so the failing
    // summarizer is reached and its error propagates out of the capture.
    let long_text = "a".repeat(300);

    middleware::record_captured_message(
        &db,
        None,
        Some(&preprocessor_with(Arc::new(FailingSummarizer))),
        &group_message(&long_text),
        Some(SENDER_USER_ID),
    )
    .await;

    assert!(stored_rows(&db, GROUP_CHAT_ID).await.is_empty());
}

#[tokio::test]
async fn a_chat_type_outside_gos_gate_is_skipped_entirely() {
    let (_fixture, db) = database().await;
    let store = RecordingRecapStateStore::new();
    let store_handle: Arc<dyn RecapStateStore> = store.clone();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    let mut message = group_message("channel post");
    message.chat.kind = "channel".to_string();

    middleware::record_captured_message(
        &db,
        Some(&store_handle),
        Some(&preprocessor()),
        &message,
        Some(SENDER_USER_ID),
    )
    .await;

    assert!(stored_rows(&db, GROUP_CHAT_ID).await.is_empty());
    assert!(store.appends().is_empty());
}

// ---------------------------------------------------------------------------
// Private forwarded replay capture
// ---------------------------------------------------------------------------

async fn capture_private(
    store: &Arc<RecordingRecapStateStore>,
    message: &CapturedMessage,
    sender_user_id: Option<i64>,
) {
    let (_fixture, db) = database().await;
    let store_handle: Arc<dyn RecapStateStore> = store.clone();

    middleware::record_captured_message(
        &db,
        Some(&store_handle),
        Some(&preprocessor()),
        message,
        sender_user_id,
    )
    .await;

    // A private message never reaches the relational table.
    assert!(stored_rows(&db, SENDER_USER_ID).await.is_empty());
}

#[tokio::test]
async fn a_private_message_without_an_open_session_appends_nothing() {
    let store = RecordingRecapStateStore::new();
    capture_private(&store, &private_message("hello"), Some(SENDER_USER_ID)).await;
    assert!(store.appends().is_empty());
}

#[tokio::test]
async fn a_private_message_without_a_sender_appends_nothing() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    capture_private(&store, &private_message("hello"), None).await;
    assert!(store.appends().is_empty());
}

#[tokio::test]
async fn a_session_state_read_failure_appends_nothing_and_does_not_panic() {
    let store = RecordingRecapStateStore::failing();
    capture_private(&store, &private_message("hello"), Some(SENDER_USER_ID)).await;
    assert!(store.appends().is_empty());
}

#[tokio::test]
async fn an_empty_private_message_appends_nothing() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    capture_private(&store, &private_message(""), Some(SENDER_USER_ID)).await;
    assert!(store.appends().is_empty());
}

#[tokio::test]
async fn an_active_session_appends_gos_exact_compact_payload_and_score() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    capture_private(&store, &private_message("hello"), Some(SENDER_USER_ID)).await;

    assert_eq!(
        store.appends(),
        vec![(
            SENDER_USER_ID,
            MESSAGE_DATE_MS,
            format!(
                "{{\"chat_id\":{SENDER_USER_ID},\"chat_type\":\"private\",\
                 \"chat_title\":\"John Smith\",\"message_id\":{MESSAGE_ID},\
                 \"actor_id\":0,\"actor_username\":\"sender\",\
                 \"actor_display_name\":\"John Smith\",\"text\":\"hello\",\
                 \"chatted_at\":{MESSAGE_DATE_MS}}}"
            )
        )]
    );
}

#[tokio::test]
async fn a_private_payload_uses_go_html_safe_json_escaping() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    capture_private(
        &store,
        &private_message("<&>\u{2028}\u{2029}"),
        Some(SENDER_USER_ID),
    )
    .await;

    let payload = store.appends().first().expect("one append").2.clone();
    assert!(payload.contains(r#""text":"\u003c\u0026\u003e\u2028\u2029""#));
    assert!(!payload.contains('<'));
    assert!(!payload.contains('>'));
    assert!(!payload.contains('&'));
    assert!(!payload.contains('\u{2028}'));
    assert!(!payload.contains('\u{2029}'));
}

#[tokio::test]
async fn an_active_session_captures_the_generate_command_before_dispatch() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    capture_private(
        &store,
        &private_message("/recap_forwarded"),
        Some(SENDER_USER_ID),
    )
    .await;

    let payload = store.appends().first().expect("one append").2.clone();
    assert!(payload.contains(r#""text":"/recap_forwarded""#));
}

#[tokio::test]
async fn a_forward_from_user_replaces_the_actor() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    let mut message = private_message("hello");
    message.forward_from = Some(CapturedUser {
        id: 555,
        username: "origin".to_string(),
        first_name: "Ada".to_string(),
        last_name: "Lovelace".to_string(),
    });

    capture_private(&store, &message, Some(SENDER_USER_ID)).await;

    let payload = store.appends().first().expect("one append").2.clone();
    assert!(payload.contains("\"actor_id\":555"));
    assert!(payload.contains("\"actor_username\":\"origin\""));
    assert!(payload.contains("\"actor_display_name\":\"Ada Lovelace\""));
    // The chat title stays the *sender's* name, not the forwarded author's.
    assert!(payload.contains("\"chat_title\":\"John Smith\""));
}

#[tokio::test]
async fn a_hidden_forward_sender_name_fills_both_actor_name_fields() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    let mut message = private_message("hello");
    message.forward_sender_name = "Hidden Person".to_string();

    capture_private(&store, &message, Some(SENDER_USER_ID)).await;

    let payload = store.appends().first().expect("one append").2.clone();
    assert!(payload.contains("\"actor_id\":0"));
    assert!(payload.contains("\"actor_username\":\"Hidden Person\""));
    assert!(payload.contains("\"actor_display_name\":\"Hidden Person\""));
}

#[tokio::test]
async fn a_forward_from_user_wins_over_a_hidden_sender_name() {
    let mut message = private_message("hello");
    message.forward_from = Some(CapturedUser {
        id: 555,
        username: "origin".to_string(),
        first_name: "Ada".to_string(),
        last_name: String::new(),
    });
    message.forward_sender_name = "Hidden Person".to_string();

    let entry = private_forwarded_replay_entry(&message, "hello");
    assert_eq!(entry.actor_id, 555);
    assert_eq!(entry.actor_username, "origin");
    assert_eq!(entry.actor_display_name, "Ada");
}

#[tokio::test]
async fn a_forward_from_chat_prefixes_the_text() {
    let store = RecordingRecapStateStore::new();
    store
        .start_forwarded(SENDER_USER_ID)
        .await
        .expect("the session must open");

    let mut message = private_message("hello");
    message.forward_from_chat = Some(CapturedChat {
        id: -1_002,
        kind: "channel".to_string(),
        title: "Parity Channel".to_string(),
    });

    capture_private(&store, &message, Some(SENDER_USER_ID)).await;

    let payload = store.appends().first().expect("one append").2.clone();
    assert!(payload.contains("\"text\":\"[forwarded from Parity Channel]: hello\""));
}

// ---------------------------------------------------------------------------
// Edited messages
// ---------------------------------------------------------------------------

async fn seed_row(db: &Database, chat_id: i64, chat_type: &str) {
    chat_history::save_one(
        db,
        &NewTelegramChatHistory {
            chat_id,
            chat_type: chat_type.to_string(),
            message_id: MESSAGE_ID,
            text: "before".to_string(),
            chatted_at: MESSAGE_DATE_MS,
            ..Default::default()
        },
    )
    .await
    .expect("the seed row must be writable");
}

#[tokio::test]
async fn an_edit_rewrites_the_row_without_consulting_the_feature_flag() {
    let (_fixture, db) = database().await;
    seed_row(&db, GROUP_CHAT_ID, "supergroup").await;
    // The flag stays off; Go's edit middleware never reads it.

    let mut message = group_message("after");
    message.message_id = MESSAGE_ID;

    middleware::record_captured_edited_message(&db, Some(&preprocessor()), &message).await;

    assert_eq!(stored_rows(&db, GROUP_CHAT_ID).await, vec!["after"]);
}

#[tokio::test]
async fn an_edit_in_a_private_chat_is_synced_too() {
    let (_fixture, db) = database().await;
    seed_row(&db, SENDER_USER_ID, "private").await;

    middleware::record_captured_edited_message(
        &db,
        Some(&preprocessor()),
        &private_message("after"),
    )
    .await;

    assert_eq!(stored_rows(&db, SENDER_USER_ID).await, vec!["after"]);
}

#[tokio::test]
async fn an_edit_with_no_text_or_caption_rewrites_nothing() {
    let (_fixture, db) = database().await;
    seed_row(&db, GROUP_CHAT_ID, "supergroup").await;

    middleware::record_captured_edited_message(&db, Some(&preprocessor()), &group_message(""))
        .await;

    assert_eq!(stored_rows(&db, GROUP_CHAT_ID).await, vec!["before"]);
}

#[tokio::test]
async fn an_edit_whose_preprocessing_fails_rewrites_nothing_and_does_not_panic() {
    let (_fixture, db) = database().await;
    seed_row(&db, GROUP_CHAT_ID, "supergroup").await;

    let long_text = "a".repeat(300);
    middleware::record_captured_edited_message(
        &db,
        Some(&preprocessor_with(Arc::new(FailingSummarizer))),
        &group_message(&long_text),
    )
    .await;

    assert_eq!(stored_rows(&db, GROUP_CHAT_ID).await, vec!["before"]);
}

// ---------------------------------------------------------------------------
// teloxide conversion
// ---------------------------------------------------------------------------

fn teloxide_message(forward_origin: Option<serde_json::Value>) -> teloxide::types::Message {
    let mut value = json!({
        "message_id": MESSAGE_ID,
        "date": MESSAGE_DATE,
        "chat": {"id": SENDER_USER_ID, "type": "private", "first_name": "John"},
        "from": {"id": SENDER_USER_ID, "is_bot": false, "first_name": "John", "last_name": "Smith"},
        "text": "hello",
    });
    if let Some(origin) = forward_origin {
        value["forward_origin"] = origin;
    }
    serde_json::from_value(value).expect("a valid Telegram message")
}

#[test]
fn a_channel_forward_origin_becomes_the_forward_from_chat() {
    let message = teloxide_message(Some(json!({
        "type": "channel",
        "date": MESSAGE_DATE,
        "chat": {"id": -1_002_i64, "type": "channel", "title": "Parity Channel"},
        "message_id": 9,
    })));

    let captured = captured_message_from_teloxide(&message);
    let forward_from_chat = captured
        .forward_from_chat
        .expect("a channel forward carries a forward-from-chat");
    assert_eq!(forward_from_chat.title, "Parity Channel");
    assert_eq!(forward_from_chat.kind, "channel");
    assert_eq!(forward_from_chat.id, -1_002);
    assert!(captured.forward_sender_name.is_empty());
    assert!(captured.forward_from.is_none());
}

#[test]
fn a_chat_forward_origin_still_becomes_the_forward_from_chat() {
    let message = teloxide_message(Some(json!({
        "type": "chat",
        "date": MESSAGE_DATE,
        "sender_chat": {"id": -1_003_i64, "type": "supergroup", "title": "Parity Group"},
    })));

    let captured = captured_message_from_teloxide(&message);
    assert_eq!(
        captured
            .forward_from_chat
            .expect("a chat forward carries a forward-from-chat")
            .title,
        "Parity Group"
    );
}

#[test]
fn a_hidden_user_forward_origin_becomes_the_forward_sender_name() {
    let message = teloxide_message(Some(json!({
        "type": "hidden_user",
        "date": MESSAGE_DATE,
        "sender_user_name": "Hidden Person",
    })));

    let captured = captured_message_from_teloxide(&message);
    assert_eq!(captured.forward_sender_name, "Hidden Person");
    assert!(captured.forward_from.is_none());
    assert!(captured.forward_from_chat.is_none());
}

#[test]
fn a_message_with_no_forward_header_carries_no_sender_name() {
    let captured = captured_message_from_teloxide(&teloxide_message(None));
    assert!(captured.forward_sender_name.is_empty());
    assert!(captured.forward_from_chat.is_none());
}

// ---------------------------------------------------------------------------
// Logging and dispatch shape
// ---------------------------------------------------------------------------

const MIDDLEWARE_SOURCE: &str = include_str!("../src/bot/middleware.rs");
const ROUTER_SOURCE: &str = include_str!("../src/bot/router.rs");

/// Every `tracing` call in the middleware must carry a fixed string.
///
/// An interpolated field would put a chat identifier, a user name, a message
/// body, or a formatted error into the log, which is exactly what this slice
/// removes.
#[test]
fn every_middleware_log_line_is_a_fixed_string() {
    for macro_name in ["trace!", "debug!", "info!", "warn!", "error!"] {
        let mut rest = MIDDLEWARE_SOURCE;
        while let Some(position) = rest.find(macro_name) {
            rest = &rest[position + macro_name.len()..];
            let Some(body) = rest.strip_prefix('(') else {
                continue;
            };
            let end = body.find(')').expect("a closing parenthesis");
            let arguments = body[..end].trim();

            assert!(
                arguments.starts_with('"') && arguments.ends_with('"'),
                "{macro_name} takes a single literal: {arguments}"
            );
            assert!(
                !arguments[1..arguments.len() - 1].contains(['{', '}', '"']),
                "{macro_name} must not interpolate: {arguments}"
            );
        }
    }
}

/// The tap is awaited, not detached.
#[test]
fn the_router_awaits_the_message_tap_instead_of_spawning_it() {
    assert!(
        !ROUTER_SOURCE.contains("tokio::spawn"),
        "a detached task would let dispatch overtake persistence"
    );
    assert_eq!(
        ROUTER_SOURCE.matches(".inspect_async(").count(),
        2,
        "one awaited tap for messages and one for edited messages"
    );
}

/// Go registers no channel-post handler, so neither does this router.
#[test]
fn the_router_registers_no_channel_post_handler() {
    for absent in ["filter_channel_post", "filter_edited_channel_post"] {
        assert!(
            !ROUTER_SOURCE.contains(absent),
            "Go handles no channel posts: {absent}"
        );
    }
    assert!(ROUTER_SOURCE.contains("filter_message"));
    assert!(ROUTER_SOURCE.contains("filter_edited_message"));
}
