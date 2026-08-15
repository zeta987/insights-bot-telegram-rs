mod support;

use std::{
    collections::VecDeque,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

use anyhow::{Result, anyhow};
use async_trait::async_trait;
use insights_bot_telegram_rs::{
    db::{Database, codec, sent_messages},
    services::{
        recap_delivery::{
            BeforeSendHook, PlainRecapSendRequest, RecapDeliverySender, RichRecapSendRequest,
        },
        telegram_rich_message::{TelegramResponseParameters, TelegramRichMessageError},
    },
};
use support::sqlite_fixture::SchemaFixture;
use teloxide::types::{InlineKeyboardButton, InlineKeyboardMarkup, Message};

// The production module is deliberately not exported until its integration
// owner wires `services/mod.rs`; these aliases let this test drive the new file
// without modifying that existing module.
mod db {
    pub use insights_bot_telegram_rs::db::*;
}

mod services {
    pub mod recap_delivery {
        pub use insights_bot_telegram_rs::services::recap_delivery::*;
    }
}

#[path = "../src/services/autorecap_delivery.rs"]
mod autorecap_delivery;

use autorecap_delivery::{AutoRecapDeliveryTarget, AutoRecapPinClient, deliver_auto_recap_targets};

const PUBLIC_CHAT_ID: i64 = -1_001_234;
const SUBSCRIBER_CHAT_ID: i64 = 7_654_321;
const TELEGRAM_RESPONSE_CHAT_ID: i64 = -1_009_876;

#[derive(Clone, Default)]
struct FakeSender {
    state: Arc<Mutex<SenderState>>,
}

#[derive(Default)]
struct SenderState {
    outcomes: VecDeque<Result<Message, TelegramRichMessageError>>,
    rich_requests: Vec<RichRecapSendRequest>,
    plain_requests: Vec<PlainRecapSendRequest>,
    next_message_id: i32,
}

impl FakeSender {
    fn with_outcomes(
        outcomes: impl IntoIterator<Item = Result<Message, TelegramRichMessageError>>,
    ) -> Self {
        Self {
            state: Arc::new(Mutex::new(SenderState {
                outcomes: outcomes.into_iter().collect(),
                ..Default::default()
            })),
        }
    }

    fn rich_chats(&self) -> Vec<i64> {
        self.state
            .lock()
            .expect("sender state")
            .rich_requests
            .iter()
            .map(|request| request.chat_id)
            .collect()
    }
}

#[async_trait]
impl RecapDeliverySender for FakeSender {
    async fn send_rich(
        &self,
        request: RichRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError> {
        let mut state = self.state.lock().expect("sender state");
        state.rich_requests.push(request.clone());
        if let Some(outcome) = state.outcomes.pop_front() {
            return outcome;
        }
        state.next_message_id += 1;
        Ok(message(
            state.next_message_id,
            request.chat_id,
            &request.markdown,
        ))
    }

    async fn send_plain(
        &self,
        request: PlainRecapSendRequest,
    ) -> Result<Message, TelegramRichMessageError> {
        let mut state = self.state.lock().expect("sender state");
        state.plain_requests.push(request.clone());
        if let Some(outcome) = state.outcomes.pop_front() {
            return outcome;
        }
        state.next_message_id += 1;
        Ok(message(
            state.next_message_id,
            request.chat_id,
            &request.text,
        ))
    }
}

#[derive(Clone, Default)]
struct FakePinner {
    state: Arc<Mutex<PinnerState>>,
}

#[derive(Default)]
struct PinnerState {
    pin_outcomes: VecDeque<Result<()>>,
    unpin_outcomes: VecDeque<Result<()>>,
    pins: Vec<(i64, i32)>,
    unpins: Vec<(i64, i32)>,
}

impl FakePinner {
    fn with_pin_failure() -> Self {
        Self {
            state: Arc::new(Mutex::new(PinnerState {
                pin_outcomes: VecDeque::from([Err(anyhow!("pin rejected"))]),
                ..Default::default()
            })),
        }
    }

    fn with_unpin_failure() -> Self {
        Self {
            state: Arc::new(Mutex::new(PinnerState {
                unpin_outcomes: VecDeque::from([Err(anyhow!("unpin rejected"))]),
                ..Default::default()
            })),
        }
    }

    fn pins(&self) -> Vec<(i64, i32)> {
        self.state.lock().expect("pinner state").pins.clone()
    }

    fn unpins(&self) -> Vec<(i64, i32)> {
        self.state.lock().expect("pinner state").unpins.clone()
    }
}

#[async_trait]
impl AutoRecapPinClient for FakePinner {
    async fn pin_message(&self, chat_id: i64, message_id: i32) -> Result<()> {
        let mut state = self.state.lock().expect("pinner state");
        state.pins.push((chat_id, message_id));
        state.pin_outcomes.pop_front().unwrap_or(Ok(()))
    }

    async fn unpin_message(&self, chat_id: i64, message_id: i32) -> Result<()> {
        let mut state = self.state.lock().expect("pinner state");
        state.unpins.push((chat_id, message_id));
        state.unpin_outcomes.pop_front().unwrap_or(Ok(()))
    }
}

#[derive(Debug, Eq, PartialEq)]
struct StoredMessage {
    chat_id: i64,
    message_id: i64,
    text: String,
    is_pinned: bool,
}

async fn database() -> (SchemaFixture, Database) {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    (fixture, database)
}

async fn stored_messages(db: &Database) -> Vec<StoredMessage> {
    let rows = sqlx::query(
        "SELECT CAST(chat_id AS TEXT), CAST(message_id AS TEXT), CAST(text AS TEXT),
                CAST(is_pinned AS TEXT)
         FROM sent_messages ORDER BY rowid",
    )
    .fetch_all(&db.pool)
    .await
    .expect("read sent messages");

    rows.iter()
        .map(|row| StoredMessage {
            chat_id: codec::i64_at(row, 0).expect("chat id"),
            message_id: codec::i64_at(row, 1).expect("message id"),
            text: codec::text_at(row, 2).expect("text"),
            is_pinned: codec::bool_at(row, 3).expect("pinned flag"),
        })
        .collect()
}

fn target(chat_id: i64, parts: &[&str], pin_first: bool) -> AutoRecapDeliveryTarget {
    AutoRecapDeliveryTarget {
        chat_id,
        parts: parts.iter().map(|part| (*part).to_owned()).collect(),
        keyboard: Some(InlineKeyboardMarkup::new(vec![vec![
            InlineKeyboardButton::callback("vote", "vote"),
        ]])),
        pin_first,
    }
}

fn message(message_id: i32, chat_id: i64, text: &str) -> Message {
    serde_json::from_value(serde_json::json!({
        "message_id": message_id,
        "date": 1_710_000_000,
        "chat": {
            "id": chat_id,
            "type": if chat_id < 0 { "supergroup" } else { "private" }
        },
        "text": text
    }))
    .expect("valid Telegram message")
}

fn api_error(description: &str) -> TelegramRichMessageError {
    TelegramRichMessageError::Api {
        code: 403,
        description: description.to_owned(),
        parameters: TelegramResponseParameters::default(),
    }
}

#[tokio::test]
async fn caller_order_keeps_public_first_and_preserves_duplicate_subscriber_targets() {
    let (_fixture, db) = database().await;
    let sender = FakeSender::default();
    let pinner = FakePinner::default();

    let report = deliver_auto_recap_targets(
        &db,
        &sender,
        &pinner,
        vec![
            target(PUBLIC_CHAT_ID, &["public"], false),
            target(SUBSCRIBER_CHAT_ID, &["subscriber one"], false),
            target(SUBSCRIBER_CHAT_ID, &["subscriber two"], false),
        ],
        None,
    )
    .await;

    assert_eq!(
        report
            .targets
            .iter()
            .map(|target| target.chat_id)
            .collect::<Vec<_>>(),
        vec![PUBLIC_CHAT_ID, SUBSCRIBER_CHAT_ID, SUBSCRIBER_CHAT_ID]
    );
    assert_eq!(
        sender.rich_chats(),
        vec![PUBLIC_CHAT_ID, SUBSCRIBER_CHAT_ID, SUBSCRIBER_CHAT_ID]
    );
    assert_eq!(
        stored_messages(&db)
            .await
            .into_iter()
            .map(|row| (row.chat_id, row.text))
            .collect::<Vec<_>>(),
        vec![
            (PUBLIC_CHAT_ID, "public".to_owned()),
            (SUBSCRIBER_CHAT_ID, "subscriber one".to_owned()),
            (SUBSCRIBER_CHAT_ID, "subscriber two".to_owned()),
        ]
    );
}

#[tokio::test]
async fn one_target_failure_does_not_stop_the_next_target() {
    let (_fixture, db) = database().await;
    let sender = FakeSender::with_outcomes([
        Err(api_error("first target blocked")),
        Ok(message(22, SUBSCRIBER_CHAT_ID, "second delivered")),
    ]);

    let report = deliver_auto_recap_targets(
        &db,
        &sender,
        &FakePinner::default(),
        vec![
            target(PUBLIC_CHAT_ID, &["first"], false),
            target(SUBSCRIBER_CHAT_ID, &["second"], false),
        ],
        None,
    )
    .await;

    assert!(report.targets[0].delivery_error.is_some());
    assert_eq!(report.targets[1].messages.len(), 1);
    assert_eq!(
        sender.rich_chats(),
        vec![PUBLIC_CHAT_ID, SUBSCRIBER_CHAT_ID]
    );
    assert_eq!(
        stored_messages(&db).await,
        vec![StoredMessage {
            chat_id: SUBSCRIBER_CHAT_ID,
            message_id: 22,
            text: "second delivered".to_owned(),
            is_pinned: false,
        }]
    );
}

#[tokio::test]
async fn partial_delivery_persists_every_delivered_prefix_as_unpinned() {
    let (_fixture, db) = database().await;
    let sender = FakeSender::with_outcomes([
        Ok(message(31, TELEGRAM_RESPONSE_CHAT_ID, "telegram first")),
        Ok(message(32, TELEGRAM_RESPONSE_CHAT_ID, "telegram second")),
        Err(api_error("third part blocked")),
    ]);

    let report = deliver_auto_recap_targets(
        &db,
        &sender,
        &FakePinner::default(),
        vec![target(PUBLIC_CHAT_ID, &["one", "two", "three"], true)],
        None,
    )
    .await;

    assert!(report.targets[0].delivery_error.is_some());
    assert_eq!(report.targets[0].messages.len(), 2);
    assert_eq!(
        stored_messages(&db).await,
        vec![
            StoredMessage {
                chat_id: TELEGRAM_RESPONSE_CHAT_ID,
                message_id: 31,
                text: "telegram first".to_owned(),
                is_pinned: false,
            },
            StoredMessage {
                chat_id: TELEGRAM_RESPONSE_CHAT_ID,
                message_id: 32,
                text: "telegram second".to_owned(),
                is_pinned: false,
            },
        ]
    );
}

#[tokio::test]
async fn missing_pinned_row_still_pins_and_only_the_first_response_row_is_true() {
    let (_fixture, db) = database().await;
    let sender = FakeSender::with_outcomes([
        Ok(message(41, PUBLIC_CHAT_ID, "telegram alpha")),
        Ok(message(42, PUBLIC_CHAT_ID, "telegram beta")),
    ]);
    let pinner = FakePinner::default();

    let report = deliver_auto_recap_targets(
        &db,
        &sender,
        &pinner,
        vec![target(
            PUBLIC_CHAT_ID,
            &["source alpha", "source beta"],
            true,
        )],
        None,
    )
    .await;

    assert!(report.targets[0].pin_succeeded);
    assert_eq!(pinner.unpins(), Vec::<(i64, i32)>::new());
    assert_eq!(pinner.pins(), vec![(PUBLIC_CHAT_ID, 41)]);
    assert_eq!(
        stored_messages(&db).await,
        vec![
            StoredMessage {
                chat_id: PUBLIC_CHAT_ID,
                message_id: 41,
                text: "telegram alpha".to_owned(),
                is_pinned: true,
            },
            StoredMessage {
                chat_id: PUBLIC_CHAT_ID,
                message_id: 42,
                text: "telegram beta".to_owned(),
                is_pinned: false,
            },
        ]
    );
}

#[tokio::test]
async fn old_telegram_unpin_failure_still_clears_db_flag_and_pins_the_new_message() {
    let (_fixture, db) = database().await;
    sent_messages::create_auto_recap_message(&db, PUBLIC_CHAT_ID, 50, "old", true)
        .await
        .expect("seed old pinned row");
    let pinner = FakePinner::with_unpin_failure();

    let report = deliver_auto_recap_targets(
        &db,
        &FakeSender::with_outcomes([Ok(message(51, PUBLIC_CHAT_ID, "new"))]),
        &pinner,
        vec![target(PUBLIC_CHAT_ID, &["new source"], true)],
        None,
    )
    .await;

    assert!(report.targets[0].pin_succeeded);
    assert_eq!(pinner.unpins(), vec![(PUBLIC_CHAT_ID, 50)]);
    assert_eq!(pinner.pins(), vec![(PUBLIC_CHAT_ID, 51)]);
    assert_eq!(
        stored_messages(&db)
            .await
            .into_iter()
            .map(|row| (row.message_id, row.is_pinned))
            .collect::<Vec<_>>(),
        vec![(50, false), (51, true)]
    );
}

#[tokio::test]
async fn pin_failure_persists_every_successful_part_as_false() {
    let (_fixture, db) = database().await;
    let report = deliver_auto_recap_targets(
        &db,
        &FakeSender::with_outcomes([
            Ok(message(61, PUBLIC_CHAT_ID, "first")),
            Ok(message(62, PUBLIC_CHAT_ID, "second")),
        ]),
        &FakePinner::with_pin_failure(),
        vec![target(PUBLIC_CHAT_ID, &["first", "second"], true)],
        None,
    )
    .await;

    assert!(!report.targets[0].pin_succeeded);
    assert!(stored_messages(&db).await.iter().all(|row| !row.is_pinned));
}

#[tokio::test]
async fn subscriber_target_never_pins_and_empty_success_continues() {
    let (_fixture, db) = database().await;
    let pinner = FakePinner::default();

    let report = deliver_auto_recap_targets(
        &db,
        &FakeSender::with_outcomes([Ok(message(71, SUBSCRIBER_CHAT_ID, "subscriber response"))]),
        &pinner,
        vec![
            target(PUBLIC_CHAT_ID, &[], true),
            target(SUBSCRIBER_CHAT_ID, &["subscriber source"], false),
        ],
        None,
    )
    .await;

    assert!(report.targets[0].messages.is_empty());
    assert_eq!(report.targets[1].messages.len(), 1);
    assert!(pinner.pins().is_empty());
    assert_eq!(stored_messages(&db).await[0].chat_id, SUBSCRIBER_CHAT_ID);
}

#[tokio::test]
async fn one_shared_hook_is_cloned_for_every_target_send() {
    let (_fixture, db) = database().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let hook_calls = Arc::clone(&calls);
    let hook: BeforeSendHook = Arc::new(move || {
        let hook_calls = Arc::clone(&hook_calls);
        Box::pin(async move {
            hook_calls.fetch_add(1, Ordering::SeqCst);
        })
    });

    deliver_auto_recap_targets(
        &db,
        &FakeSender::default(),
        &FakePinner::default(),
        vec![
            target(PUBLIC_CHAT_ID, &["public"], false),
            target(SUBSCRIBER_CHAT_ID, &["subscriber"], false),
        ],
        Some(hook),
    )
    .await;

    assert_eq!(calls.load(Ordering::SeqCst), 2);
}
