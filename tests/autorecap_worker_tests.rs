use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

use anyhow::anyhow;
use async_trait::async_trait;
use insights_bot_telegram_rs::{
    db::models::{TelegramChatAutoRecapsSubscriber, TelegramChatRecapsOptions},
    redis::{
        keys,
        recap_state::{InMemoryRecapStateStore, ManualRecapRateResult, RecapStateStore, TestClock},
    },
    services::autorecap::{
        AutoRecapPreparation, AutoRecapStartupSeeder, AutoRecapStateReader,
        build_auto_recap_targets, has_enough_auto_recap_histories, prepare_auto_recap,
        read_auto_recap_state, seed_enabled_auto_recaps,
    },
};

const CHAT_ID: i64 = -100_123_456;
const NOW_MS: i64 = 1_767_254_400_000;

struct ScriptedReader {
    enabled_failures: usize,
    options_failures: usize,
    subscriber_failures: usize,
    enabled_calls: AtomicUsize,
    options_calls: AtomicUsize,
    subscriber_calls: AtomicUsize,
}

impl ScriptedReader {
    fn new(enabled_failures: usize, options_failures: usize, subscriber_failures: usize) -> Self {
        Self {
            enabled_failures,
            options_failures,
            subscriber_failures,
            enabled_calls: AtomicUsize::new(0),
            options_calls: AtomicUsize::new(0),
            subscriber_calls: AtomicUsize::new(0),
        }
    }

    fn options() -> TelegramChatRecapsOptions {
        TelegramChatRecapsOptions {
            id: "options-id".to_owned(),
            chat_id: CHAT_ID,
            auto_recap_send_mode: 0,
            manual_recap_rate_per_seconds: 0,
            auto_recap_rates_per_day: 4,
            pin_auto_recap_message: false,
            created_at: 1,
            updated_at: 1,
        }
    }

    fn subscribers() -> Vec<TelegramChatAutoRecapsSubscriber> {
        vec![TelegramChatAutoRecapsSubscriber {
            id: "subscriber-id".to_owned(),
            chat_id: CHAT_ID,
            user_id: 42,
            created_at: 1,
            updated_at: 1,
        }]
    }

    fn enabled_calls(&self) -> usize {
        self.enabled_calls.load(Ordering::SeqCst)
    }

    fn options_calls(&self) -> usize {
        self.options_calls.load(Ordering::SeqCst)
    }

    fn subscriber_calls(&self) -> usize {
        self.subscriber_calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl AutoRecapStateReader for ScriptedReader {
    async fn recap_enabled(&self, _chat_id: i64) -> anyhow::Result<bool> {
        let attempt = self.enabled_calls.fetch_add(1, Ordering::SeqCst);
        if attempt < self.enabled_failures {
            Err(anyhow!("enabled read failed"))
        } else {
            Ok(true)
        }
    }

    async fn recap_options(
        &self,
        _chat_id: i64,
    ) -> anyhow::Result<Option<TelegramChatRecapsOptions>> {
        let attempt = self.options_calls.fetch_add(1, Ordering::SeqCst);
        if attempt < self.options_failures {
            Err(anyhow!("options read failed"))
        } else {
            Ok(Some(Self::options()))
        }
    }

    async fn recap_subscribers(
        &self,
        _chat_id: i64,
    ) -> anyhow::Result<Vec<TelegramChatAutoRecapsSubscriber>> {
        let attempt = self.subscriber_calls.fetch_add(1, Ordering::SeqCst);
        if attempt < self.subscriber_failures {
            Err(anyhow!("subscriber read failed"))
        } else {
            Ok(Self::subscribers())
        }
    }
}

struct FixedReader {
    enabled: bool,
    options: Option<TelegramChatRecapsOptions>,
    subscribers: Vec<TelegramChatAutoRecapsSubscriber>,
}

#[derive(Default)]
struct RecordingStartupSeeder {
    events: Mutex<Vec<String>>,
}

impl RecordingStartupSeeder {
    fn events(&self) -> Vec<String> {
        self.events.lock().expect("event lock").clone()
    }

    fn record(&self, event: impl Into<String>) {
        self.events.lock().expect("event lock").push(event.into());
    }
}

#[async_trait]
impl AutoRecapStartupSeeder for RecordingStartupSeeder {
    async fn list_enabled_chat_ids(&self) -> anyhow::Result<Vec<i64>> {
        self.record("list");
        Ok(vec![11, 22, 33])
    }

    async fn find_or_create_rate(&self, chat_id: i64) -> anyhow::Result<i64> {
        self.record(format!("options:{chat_id}"));
        if chat_id == 22 {
            Err(anyhow!("options unavailable"))
        } else {
            Ok(if chat_id == 11 { 2 } else { 4 })
        }
    }

    async fn queue_chat(&self, chat_id: i64, rates_per_day: i64) {
        self.record(format!("queue:{chat_id}:{rates_per_day}"));
    }
}

#[async_trait]
impl AutoRecapStateReader for FixedReader {
    async fn recap_enabled(&self, _chat_id: i64) -> anyhow::Result<bool> {
        Ok(self.enabled)
    }

    async fn recap_options(
        &self,
        _chat_id: i64,
    ) -> anyhow::Result<Option<TelegramChatRecapsOptions>> {
        Ok(self.options.clone())
    }

    async fn recap_subscribers(
        &self,
        _chat_id: i64,
    ) -> anyhow::Result<Vec<TelegramChatAutoRecapsSubscriber>> {
        Ok(self.subscribers.clone())
    }
}

fn queue_store() -> InMemoryRecapStateStore {
    InMemoryRecapStateStore::new(Arc::new(TestClock::new(NOW_MS)))
}

#[tokio::test]
async fn startup_seed_loads_every_option_before_queueing_successful_chats() {
    let seeder = RecordingStartupSeeder::default();

    seed_enabled_auto_recaps(&seeder).await;

    assert_eq!(
        seeder.events(),
        [
            "list",
            "options:11",
            "options:22",
            "options:33",
            "queue:11:2",
            "queue:33:4",
        ]
    );
}

#[tokio::test]
async fn each_state_read_is_attempted_at_most_ten_times() {
    let reader = ScriptedReader::new(usize::MAX, usize::MAX, usize::MAX);

    let state = read_auto_recap_state(&reader, CHAT_ID).await;

    assert_eq!(reader.enabled_calls(), 10);
    assert_eq!(reader.options_calls(), 10);
    assert_eq!(reader.subscriber_calls(), 10);
    assert!(!state.enabled);
    assert!(state.options.is_none());
    assert!(state.subscribers.is_empty());
    assert_eq!(state.read_error_count, 3);
}

#[tokio::test]
async fn state_reads_stop_retrying_after_the_first_success() {
    let reader = ScriptedReader::new(2, 4, 1);

    let state = read_auto_recap_state(&reader, CHAT_ID).await;

    assert_eq!(reader.enabled_calls(), 3);
    assert_eq!(reader.options_calls(), 5);
    assert_eq!(reader.subscriber_calls(), 2);
    assert!(state.enabled);
    assert_eq!(state.options, Some(ScriptedReader::options()));
    assert_eq!(state.subscribers, ScriptedReader::subscribers());
    assert_eq!(state.read_error_count, 0);
}

#[test]
fn public_target_precedes_every_physical_subscriber_row() {
    let targets = build_auto_recap_targets(CHAT_ID, 0, &[42, 42, 84]);

    assert_eq!(
        targets
            .iter()
            .map(|target| (target.chat_id, target.is_private_subscriber))
            .collect::<Vec<_>>(),
        vec![(CHAT_ID, false), (42, true), (42, true), (84, true)]
    );
}

#[test]
fn private_and_unknown_modes_keep_only_subscriber_targets() {
    let private = build_auto_recap_targets(CHAT_ID, 1, &[42, 84]);
    let unknown = build_auto_recap_targets(CHAT_ID, 99, &[42, 84]);

    for targets in [private, unknown] {
        assert_eq!(
            targets
                .iter()
                .map(|target| (target.chat_id, target.is_private_subscriber))
                .collect::<Vec<_>>(),
            vec![(42, true), (84, true)]
        );
    }
}

#[tokio::test]
async fn disabled_capsule_is_consumed_without_requeueing() {
    let store = queue_store();
    let reader = FixedReader {
        enabled: false,
        options: Some(ScriptedReader::options()),
        subscribers: ScriptedReader::subscribers(),
    };

    let preparation = prepare_auto_recap(&reader, &store, CHAT_ID, 0, NOW_MS)
        .await
        .expect("disabled state is readable");

    assert_eq!(preparation, AutoRecapPreparation::Disabled);
    assert_eq!(store.raw_zset(keys::AUTO_RECAP_QUEUE_KEY), None);
}

#[tokio::test]
async fn private_only_without_subscribers_requeues_before_skipping() {
    let store = queue_store();
    let mut options = ScriptedReader::options();
    options.auto_recap_send_mode = 1;
    options.auto_recap_rates_per_day = 99;
    let reader = FixedReader {
        enabled: true,
        options: Some(options),
        subscribers: Vec::new(),
    };

    let preparation = prepare_auto_recap(&reader, &store, CHAT_ID, 0, NOW_MS)
        .await
        .expect("private state is readable");

    let AutoRecapPreparation::PrivateWithoutSubscribers { options } = preparation else {
        panic!("private-only state should skip generation");
    };
    assert_eq!(options.auto_recap_rates_per_day, 4);
    assert_eq!(
        store
            .raw_zset(keys::AUTO_RECAP_QUEUE_KEY)
            .expect("next capsule is queued")
            .len(),
        1
    );
}

#[tokio::test]
async fn enabled_capsule_requeues_before_returning_generation_inputs() {
    let store = queue_store();
    let reader = FixedReader {
        enabled: true,
        options: Some(ScriptedReader::options()),
        subscribers: ScriptedReader::subscribers(),
    };

    let preparation = prepare_auto_recap(&reader, &store, CHAT_ID, 8 * 3_600, NOW_MS)
        .await
        .expect("enabled state is readable");

    let AutoRecapPreparation::Generate {
        options,
        subscribers,
    } = preparation
    else {
        panic!("enabled public state should generate");
    };
    assert_eq!(options.auto_recap_rates_per_day, 4);
    assert_eq!(subscribers, ScriptedReader::subscribers());
    assert!(store.raw_zset(keys::AUTO_RECAP_QUEUE_KEY).is_some());
}

#[tokio::test]
async fn enabled_state_without_options_returns_the_bounded_rust_error() {
    let store = queue_store();
    let reader = FixedReader {
        enabled: true,
        options: None,
        subscribers: Vec::new(),
    };

    let error = prepare_auto_recap(&reader, &store, CHAT_ID, 0, NOW_MS)
        .await
        .expect_err("Rust replaces Go's nil dereference with an error");

    assert_eq!(
        error.to_string(),
        "automatic recap is enabled but its options row is unavailable"
    );
    assert_eq!(store.raw_zset(keys::AUTO_RECAP_QUEUE_KEY), None);
}

#[test]
fn automatic_generation_requires_more_than_five_physical_rows() {
    assert!(!has_enough_auto_recap_histories(0));
    assert!(!has_enough_auto_recap_histories(5));
    assert!(has_enough_auto_recap_histories(6));
}

/// Wraps [`InMemoryRecapStateStore`] to count `auto_recap_zadd` calls, so a test
/// can observe how many times `queue_next_auto_recap` actually ran without a
/// production-side counter.
struct CountingQueueStore {
    inner: InMemoryRecapStateStore,
    zadd_calls: AtomicUsize,
}

impl CountingQueueStore {
    fn wrap(inner: InMemoryRecapStateStore) -> Self {
        Self {
            inner,
            zadd_calls: AtomicUsize::new(0),
        }
    }

    fn zadd_calls(&self) -> usize {
        self.zadd_calls.load(Ordering::SeqCst)
    }

    fn raw_zset(&self, key: &str) -> Option<Vec<(i64, String)>> {
        self.inner.raw_zset(key)
    }
}

#[async_trait]
impl RecapStateStore for CountingQueueStore {
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

    async fn auto_recap_zadd(&self, member: &str, score_ms: i64) -> anyhow::Result<()> {
        self.zadd_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.auto_recap_zadd(member, score_ms).await
    }

    async fn auto_recap_zpop_due(&self, now_ms: i64) -> anyhow::Result<Option<String>> {
        self.inner.auto_recap_zpop_due(now_ms).await
    }

    async fn auto_recap_zrem(&self, member: &str) -> anyhow::Result<()> {
        self.inner.auto_recap_zrem(member).await
    }
}

/// A chat that stays enabled but whose subscriber read never recovers within
/// the ten allotted attempts still requeues twice, matching Go's error-path
/// requeue followed by the normal requeue, and still reaches `Generate` with
/// whatever subscribers were readable (none, here).
#[tokio::test]
async fn partial_subscriber_read_failure_still_requeues_twice_before_generating() {
    let reader = ScriptedReader::new(0, 0, usize::MAX);
    let store = CountingQueueStore::wrap(InMemoryRecapStateStore::new(Arc::new(TestClock::new(
        NOW_MS,
    ))));

    let preparation = prepare_auto_recap(&reader, &store, CHAT_ID, 0, NOW_MS)
        .await
        .expect("a recovered options read still resolves a preparation");

    let AutoRecapPreparation::Generate {
        options,
        subscribers,
    } = preparation
    else {
        panic!("an enabled chat with recovered options should still generate");
    };
    assert_eq!(
        reader.subscriber_calls(),
        10,
        "the subscriber read exhausts all ten attempts"
    );
    assert_eq!(options.auto_recap_rates_per_day, 4);
    assert!(
        subscribers.is_empty(),
        "an exhausted subscriber read degrades to an empty list rather than failing"
    );
    assert_eq!(
        store.zadd_calls(),
        2,
        "the read-error requeue and the normal enabled-path requeue both run"
    );
    assert_eq!(
        store
            .raw_zset(keys::AUTO_RECAP_QUEUE_KEY)
            .map(|zset| zset.len()),
        Some(1),
        "both requeues upsert the same deterministic member, so only one entry remains"
    );
}
