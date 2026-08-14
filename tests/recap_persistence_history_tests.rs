//! Task 4B3 — chat histories, group migration, and bot-left cleanup.
//!
//! Behaviour is pinned to Go v1.0.0:
//! `internal/models/chathistories/chat_histories.go` for the row operations,
//! `internal/bots/telegram/handlers/chatmigrate/chatmigrate.go` for the
//! supergroup upgrade, and
//! `internal/bots/telegram/handlers/welcome/welcome.go` for the bot-left prune.
//!
//! Several parity tables expose no read API, because Go exposes none either.
//! Those tables are inspected through local parameterized helpers whose table
//! names are fixed literals and whose every value is bound.

mod support;

use anyhow::Result;
use insights_bot_telegram_rs::db::{
    Database, chat_cleanup, chat_history, feature_flags, feedback, migration,
    models::{
        CHAT_TYPE_GROUP, CHAT_TYPE_SUPERGROUP, FROM_PLATFORM_TELEGRAM, MessageKind,
        NewTelegramChatHistory, ReactionType, TelegramChatHistory, TokenUsage,
    },
    recap_logs, recap_options, sent_messages, subscribers, usage_metrics,
};
use sqlx::AnyPool;
use support::sqlite_fixture::SchemaFixture;
use uuid::Uuid;

/// The basic group a supergroup upgrade starts from.
const GROUP_CHAT_ID: i64 = -1_001_234_567_890;
/// The supergroup identifier the upgrade moves onto.
const SUPERGROUP_CHAT_ID: i64 = -1_009_876_543_210;
/// A third chat that must never be touched by a single-chat operation.
const BYSTANDER_CHAT_ID: i64 = -1_005_555_555_555;

const BIG_USER_ID: i64 = 7_654_321_098;
const BIG_MESSAGE_ID: i64 = 5_000_000_000;
/// A Unix-millisecond instant well past the 32-bit boundary.
const BIG_TIMESTAMP_MS: i64 = 1_700_000_000_000;

async fn database() -> (SchemaFixture, Database) {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    (fixture, database)
}

/// A fully populated Go-shaped message, including the reply snapshot.
fn sample_message(
    chat_id: i64,
    message_id: i64,
    text: &str,
    chatted_at: i64,
) -> NewTelegramChatHistory {
    NewTelegramChatHistory {
        chat_id,
        chat_type: CHAT_TYPE_GROUP.to_owned(),
        chat_title: "Parity Group".to_owned(),
        message_id,
        user_id: BIG_USER_ID,
        username: "sender_one".to_owned(),
        full_name: "Sender One".to_owned(),
        text: text.to_owned(),
        replied_to_message_id: BIG_MESSAGE_ID,
        replied_to_user_id: BIG_USER_ID + 1,
        replied_to_full_name: "Replied To".to_owned(),
        replied_to_username: "replied_to".to_owned(),
        replied_to_text: "the quoted text".to_owned(),
        replied_to_chat_type: CHAT_TYPE_GROUP.to_owned(),
        chatted_at,
    }
}

/// Every stored row of a chat, cutoff-free, ordered by the repository's rule.
///
/// Every seeded `chatted_at` in this suite is strictly positive, so a zero
/// cutoff selects all of them through the production query.
async fn all_rows(db: &Database, chat_id: i64) -> Result<Vec<TelegramChatHistory>> {
    chat_history::find_chatted_after(db, chat_id, 0).await
}

/// The fields that must survive an edit, as one comparable value.
fn without_text(row: &TelegramChatHistory) -> TelegramChatHistory {
    TelegramChatHistory {
        text: String::new(),
        ..row.clone()
    }
}

// ---------------------------------------------------------------------------
// Local readers for tables Go exposes no query for
// ---------------------------------------------------------------------------

async fn count_where_chat_id(pool: &AnyPool, table: &str, chat_id: i64) -> Result<i64> {
    // The table name is one of this file's fixed literals; the value is bound.
    let count: i64 =
        sqlx::query_scalar(&format!("SELECT COUNT(*) FROM {table} WHERE chat_id = $1"))
            .bind(chat_id)
            .fetch_one(pool)
            .await?;
    Ok(count)
}

/// One recap log row, read as text so the `Any` driver cannot narrow it.
struct RecapLogRow {
    recap_inputs: String,
    recap_outputs: String,
    total_token_usage: String,
    created_at: String,
}

async fn read_recap_log(pool: &AnyPool, log_id: &str) -> Result<RecapLogRow> {
    let row: (String, String, String, String) = sqlx::query_as(
        "SELECT CAST(recap_inputs AS TEXT), CAST(recap_outputs AS TEXT),
                CAST(total_token_usage AS TEXT), CAST(created_at AS TEXT)
         FROM log_chat_histories_recaps WHERE CAST(id AS TEXT) = $1",
    )
    .bind(log_id)
    .fetch_one(pool)
    .await?;
    Ok(RecapLogRow {
        recap_inputs: row.0,
        recap_outputs: row.1,
        total_token_usage: row.2,
        created_at: row.3,
    })
}

async fn count_metrics(pool: &AnyPool) -> Result<i64> {
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM metric_open_ai_chat_completion_token_usages")
            .fetch_one(pool)
            .await?;
    Ok(count)
}

async fn count_chats(pool: &AnyPool, chat_id: i64) -> Result<i64> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM chats WHERE id = $1")
        .bind(chat_id)
        .fetch_one(pool)
        .await?;
    Ok(count)
}

async fn insert_chat_row(pool: &AnyPool, chat_id: i64) -> Result<()> {
    sqlx::query(
        "INSERT INTO chats (id, title, username, kind, created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(chat_id)
    .bind("Parity Group")
    .bind("parity_group")
    .bind(CHAT_TYPE_GROUP)
    .bind(BIG_TIMESTAMP_MS)
    .bind(BIG_TIMESTAMP_MS)
    .execute(pool)
    .await?;
    Ok(())
}

fn usage() -> TokenUsage {
    TokenUsage {
        prompt_tokens: 11,
        completion_tokens: 22,
        total_tokens: 33,
    }
}

/// Seed every table the two orchestrations could plausibly reach.
///
/// The return value is the recap log identifier, which the reaction rows and
/// the retention assertions both need.
async fn seed_every_table(db: &Database, chat_id: i64) -> Result<String> {
    feature_flags::enable_recap(db, chat_id, CHAT_TYPE_GROUP, "Parity Group").await?;
    recap_options::find_one_or_create(db, chat_id).await?;
    subscribers::insert_unchecked(db, chat_id, BIG_USER_ID).await?;
    chat_history::save_one(db, &sample_message(chat_id, 1, "first", BIG_TIMESTAMP_MS)).await?;
    chat_history::save_one(
        db,
        &sample_message(chat_id, 2, "second", BIG_TIMESTAMP_MS + 1),
    )
    .await?;

    let log_id = recap_logs::create_group_recap(
        db,
        chat_id,
        "the recap inputs",
        "the recap outputs",
        usage(),
        "gpt-parity",
    )
    .await?;

    sent_messages::create_auto_recap_message(db, chat_id, BIG_MESSAGE_ID, "a recap part", true)
        .await?;
    feedback::react(
        db,
        feedback::ReactionTable::ChatHistoriesRecaps,
        chat_id,
        Uuid::parse_str(&log_id).expect("the repository generates a canonical UUID"),
        BIG_USER_ID,
        ReactionType::UpVote,
    )
    .await?;
    usage_metrics::create(db, "recap", usage(), "gpt-parity").await?;
    insert_chat_row(&db.pool, chat_id).await?;

    Ok(log_id)
}

// ---------------------------------------------------------------------------
// Inserting: physical duplicates are permitted
// ---------------------------------------------------------------------------

#[tokio::test]
async fn two_identical_messages_are_stored_as_two_physical_rows() {
    let (_fixture, db) = database().await;
    let message = sample_message(GROUP_CHAT_ID, 42, "the same text", BIG_TIMESTAMP_MS);

    for _ in 0..2 {
        chat_history::save_one(&db, &message)
            .await
            .expect("the insert carries no uniqueness rule");
    }

    let rows = all_rows(&db, GROUP_CHAT_ID)
        .await
        .expect("both rows are readable");
    assert_eq!(rows.len(), 2, "Go's create adds a row per call");
    assert_ne!(rows[0].id, rows[1].id, "each row gets its own key");
    for row in &rows {
        assert_eq!(row.message_id, 42);
        assert_eq!(row.text, "the same text");
    }
}

#[tokio::test]
async fn a_stored_row_carries_every_go_column_it_was_given() {
    let (_fixture, db) = database().await;
    let message = sample_message(GROUP_CHAT_ID, 7, "hello", BIG_TIMESTAMP_MS);

    chat_history::save_one(&db, &message)
        .await
        .expect("the row is written");

    let rows = all_rows(&db, GROUP_CHAT_ID).await.expect("the row is read");
    let [row] = rows.as_slice() else {
        panic!("exactly one row was written, got {}", rows.len());
    };

    assert_eq!(row.chat_id, GROUP_CHAT_ID);
    assert_eq!(row.chat_type, CHAT_TYPE_GROUP);
    assert_eq!(row.chat_title, "Parity Group");
    assert_eq!(row.message_id, 7);
    assert_eq!(row.user_id, BIG_USER_ID);
    assert_eq!(row.username, "sender_one");
    assert_eq!(row.full_name, "Sender One");
    assert_eq!(row.text, "hello");
    assert_eq!(row.replied_to_message_id, BIG_MESSAGE_ID);
    assert_eq!(row.replied_to_user_id, BIG_USER_ID + 1);
    assert_eq!(row.replied_to_full_name, "Replied To");
    assert_eq!(row.replied_to_username, "replied_to");
    assert_eq!(row.replied_to_text, "the quoted text");
    assert_eq!(row.replied_to_chat_type, CHAT_TYPE_GROUP);
    assert_eq!(row.chatted_at, BIG_TIMESTAMP_MS);
    assert!(!row.embedded, "Go's create leaves the schema default");
    assert_eq!(row.from_platform, FROM_PLATFORM_TELEGRAM);
}

// ---------------------------------------------------------------------------
// Editing: every matching row, and nothing but the text
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_edit_rewrites_every_duplicate_row_of_the_pair() {
    let (_fixture, db) = database().await;
    let message = sample_message(GROUP_CHAT_ID, 42, "before", BIG_TIMESTAMP_MS);
    for _ in 0..3 {
        chat_history::save_one(&db, &message)
            .await
            .expect("three physical duplicates");
    }

    chat_history::update_one_text(&db, GROUP_CHAT_ID, 42, "after")
        .await
        .expect("the update runs");

    let rows = all_rows(&db, GROUP_CHAT_ID).await.expect("rows are read");
    assert_eq!(rows.len(), 3, "an update must not remove a row");
    for row in &rows {
        assert_eq!(
            row.text, "after",
            "Go's Update().Where(..).Exec() has no LIMIT"
        );
    }
}

#[tokio::test]
async fn an_edit_changes_the_text_and_no_other_column() {
    let (_fixture, db) = database().await;
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 42, "before", BIG_TIMESTAMP_MS),
    )
    .await
    .expect("the row is written");

    let before = all_rows(&db, GROUP_CHAT_ID)
        .await
        .expect("the row is read")
        .remove(0);

    chat_history::update_one_text(&db, GROUP_CHAT_ID, 42, "after")
        .await
        .expect("the update runs");

    let after = all_rows(&db, GROUP_CHAT_ID)
        .await
        .expect("the row is read again")
        .remove(0);

    assert_eq!(before.text, "before");
    assert_eq!(after.text, "after");
    assert_eq!(
        without_text(&before),
        without_text(&after),
        "the actor, the reply snapshot, the title and type, the timestamps and \
         the forwarding metadata all stay as they were"
    );
}

#[tokio::test]
async fn an_edit_is_scoped_to_one_chat_and_one_message() {
    let (_fixture, db) = database().await;
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 42, "target", BIG_TIMESTAMP_MS),
    )
    .await
    .expect("the target row");
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 43, "other message", BIG_TIMESTAMP_MS),
    )
    .await
    .expect("a sibling message");
    chat_history::save_one(
        &db,
        &sample_message(BYSTANDER_CHAT_ID, 42, "other chat", BIG_TIMESTAMP_MS),
    )
    .await
    .expect("the same message id in another chat");

    chat_history::update_one_text(&db, GROUP_CHAT_ID, 42, "after")
        .await
        .expect("the update runs");

    let group = all_rows(&db, GROUP_CHAT_ID).await.expect("rows are read");
    assert_eq!(
        group
            .iter()
            .find(|row| row.message_id == 43)
            .expect("the sibling survives")
            .text,
        "other message"
    );
    let bystander = all_rows(&db, BYSTANDER_CHAT_ID)
        .await
        .expect("rows are read");
    assert_eq!(bystander[0].text, "other chat");
}

// ---------------------------------------------------------------------------
// Windowing: strict cutoff, message-id order
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_window_excludes_a_row_sitting_exactly_on_the_cutoff() {
    let (_fixture, db) = database().await;
    let cutoff = BIG_TIMESTAMP_MS;
    for (message_id, chatted_at) in [(1_i64, cutoff - 1), (2, cutoff), (3, cutoff + 1)] {
        chat_history::save_one(
            &db,
            &sample_message(GROUP_CHAT_ID, message_id, "text", chatted_at),
        )
        .await
        .expect("the row is written");
    }

    let rows = chat_history::find_chatted_after(&db, GROUP_CHAT_ID, cutoff)
        .await
        .expect("the window query runs");

    assert_eq!(
        rows.iter().map(|row| row.message_id).collect::<Vec<_>>(),
        vec![3],
        "Go compares with ChattedAtGT, which is strictly greater"
    );
}

#[tokio::test]
async fn the_window_orders_by_message_id_and_not_by_time_or_insertion() {
    let (_fixture, db) = database().await;
    // Insertion order, chatted_at order, and message_id order all disagree.
    for (message_id, chatted_at) in [
        (30_i64, BIG_TIMESTAMP_MS + 1),
        (10, BIG_TIMESTAMP_MS + 3),
        (20, BIG_TIMESTAMP_MS + 2),
    ] {
        chat_history::save_one(
            &db,
            &sample_message(GROUP_CHAT_ID, message_id, "text", chatted_at),
        )
        .await
        .expect("the row is written");
    }

    let rows = chat_history::find_chatted_after(&db, GROUP_CHAT_ID, BIG_TIMESTAMP_MS)
        .await
        .expect("the window query runs");

    assert_eq!(
        rows.iter().map(|row| row.message_id).collect::<Vec<_>>(),
        vec![10, 20, 30],
        "Go orders ByMessageID ascending"
    );
}

#[tokio::test]
async fn the_window_is_scoped_to_one_chat() {
    let (_fixture, db) = database().await;
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 1, "mine", BIG_TIMESTAMP_MS + 1),
    )
    .await
    .expect("the row is written");
    chat_history::save_one(
        &db,
        &sample_message(BYSTANDER_CHAT_ID, 2, "theirs", BIG_TIMESTAMP_MS + 1),
    )
    .await
    .expect("the row is written");

    let rows = chat_history::find_chatted_after(&db, GROUP_CHAT_ID, BIG_TIMESTAMP_MS)
        .await
        .expect("the window query runs");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "mine");
}

#[tokio::test]
async fn the_duration_window_measures_backwards_from_now() {
    let (_fixture, db) = database().await;
    let now = chrono::Utc::now().timestamp_millis();
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 1, "half an hour ago", now - 30 * 60 * 1000),
    )
    .await
    .expect("a recent row");
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 2, "two hours ago", now - 2 * 60 * 60 * 1000),
    )
    .await
    .expect("an older row");

    let rows = chat_history::find_by_time_before(&db, GROUP_CHAT_ID, chrono::Duration::hours(1))
        .await
        .expect("the duration window runs");

    assert_eq!(
        rows.iter().map(|row| row.message_id).collect::<Vec<_>>(),
        vec![1],
        "only the row inside the last hour is returned"
    );
}

// ---------------------------------------------------------------------------
// Group migration
// ---------------------------------------------------------------------------

#[tokio::test]
async fn migration_moves_the_five_go_parity_tables() {
    let (_fixture, db) = database().await;
    let log_id = seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");

    migration::migrate_chat_data(&db, GROUP_CHAT_ID, SUPERGROUP_CHAT_ID).await;

    let flags = feature_flags::find_one_for_groups(&db, SUPERGROUP_CHAT_ID, "")
        .await
        .expect("the flag row is readable")
        .expect("the flag row moved");
    assert!(flags.feature_chat_histories_recap);
    assert_eq!(
        flags.chat_type, CHAT_TYPE_SUPERGROUP,
        "only a supergroup upgrade reaches this path"
    );
    assert!(
        feature_flags::find_one_for_groups(&db, GROUP_CHAT_ID, "")
            .await
            .expect("the old identifier is readable")
            .is_none()
    );

    assert!(
        recap_options::find_one(&db, SUPERGROUP_CHAT_ID)
            .await
            .expect("the options row is readable")
            .is_some()
    );
    assert!(
        recap_options::find_one(&db, GROUP_CHAT_ID)
            .await
            .expect("the old identifier is readable")
            .is_none()
    );

    assert_eq!(
        subscribers::list(&db, SUPERGROUP_CHAT_ID)
            .await
            .expect("subscribers are readable")
            .len(),
        1
    );
    assert!(
        subscribers::list(&db, GROUP_CHAT_ID)
            .await
            .expect("the old identifier is readable")
            .is_empty()
    );

    let moved = all_rows(&db, SUPERGROUP_CHAT_ID)
        .await
        .expect("histories are readable");
    assert_eq!(moved.len(), 2);
    assert!(
        all_rows(&db, GROUP_CHAT_ID)
            .await
            .expect("the old identifier is readable")
            .is_empty()
    );

    assert_eq!(
        count_where_chat_id(&db.pool, "log_chat_histories_recaps", SUPERGROUP_CHAT_ID)
            .await
            .expect("the log row is countable"),
        1
    );
    assert_eq!(
        count_where_chat_id(&db.pool, "log_chat_histories_recaps", GROUP_CHAT_ID)
            .await
            .expect("the old identifier is countable"),
        0
    );
    // The row itself is intact, not recreated.
    let log = read_recap_log(&db.pool, &log_id)
        .await
        .expect("the same log row is still there");
    assert_eq!(log.recap_inputs, "the recap inputs");
    assert_eq!(log.total_token_usage, "33");
}

#[tokio::test]
async fn migration_forces_the_supergroup_chat_type_on_histories() {
    let (_fixture, db) = database().await;
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 1, "before the upgrade", BIG_TIMESTAMP_MS),
    )
    .await
    .expect("the row is written");
    let before = all_rows(&db, GROUP_CHAT_ID)
        .await
        .expect("the row is read")
        .remove(0);
    assert_eq!(before.chat_type, CHAT_TYPE_GROUP);

    migration::migrate_chat_data(&db, GROUP_CHAT_ID, SUPERGROUP_CHAT_ID).await;

    let after = all_rows(&db, SUPERGROUP_CHAT_ID)
        .await
        .expect("the row moved")
        .remove(0);
    assert_eq!(after.chat_id, SUPERGROUP_CHAT_ID);
    assert_eq!(after.chat_type, CHAT_TYPE_SUPERGROUP);
    assert_eq!(
        TelegramChatHistory {
            chat_id: GROUP_CHAT_ID,
            chat_type: CHAT_TYPE_GROUP.to_owned(),
            ..after.clone()
        },
        before,
        "the migration rewrites the identifier and the type, and nothing else"
    );
}

#[tokio::test]
async fn migration_leaves_the_tables_go_deliberately_skips() {
    let (_fixture, db) = database().await;
    let log_id = seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");
    let log_uuid = Uuid::parse_str(&log_id).expect("a canonical UUID");

    migration::migrate_chat_data(&db, GROUP_CHAT_ID, SUPERGROUP_CHAT_ID).await;

    assert_eq!(
        count_where_chat_id(&db.pool, "sent_messages", GROUP_CHAT_ID)
            .await
            .expect("sent messages are countable"),
        1,
        "Go's chatmigrate never touches sent_messages"
    );
    assert_eq!(
        count_where_chat_id(&db.pool, "sent_messages", SUPERGROUP_CHAT_ID)
            .await
            .expect("sent messages are countable"),
        0
    );

    let counts = feedback::counts(
        &db,
        feedback::ReactionTable::ChatHistoriesRecaps,
        GROUP_CHAT_ID,
        log_uuid,
    )
    .await
    .expect("reactions are countable");
    assert_eq!(
        counts.up_votes, 1,
        "Go's chatmigrate never touches the reaction tables"
    );

    assert_eq!(
        count_metrics(&db.pool)
            .await
            .expect("metrics are countable"),
        1,
        "the metric table carries no chat identifier and is never rewritten"
    );

    assert_eq!(
        count_chats(&db.pool, GROUP_CHAT_ID)
            .await
            .expect("the chats row is countable"),
        1,
        "Go's chatmigrate never touches telegram_chats"
    );
    assert_eq!(
        count_chats(&db.pool, SUPERGROUP_CHAT_ID)
            .await
            .expect("the chats row is countable"),
        0
    );
}

#[tokio::test]
async fn migration_leaves_a_bystander_chat_alone() {
    let (_fixture, db) = database().await;
    seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");
    seed_every_table(&db, BYSTANDER_CHAT_ID)
        .await
        .expect("the bystander is seeded");

    migration::migrate_chat_data(&db, GROUP_CHAT_ID, SUPERGROUP_CHAT_ID).await;

    assert!(
        feature_flags::find_one_for_groups(&db, BYSTANDER_CHAT_ID, "")
            .await
            .expect("the bystander is readable")
            .is_some()
    );
    assert_eq!(
        all_rows(&db, BYSTANDER_CHAT_ID)
            .await
            .expect("the bystander is readable")
            .len(),
        2
    );
}

// ---------------------------------------------------------------------------
// Bot-left cleanup
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bot_left_deletes_the_four_owned_tables() {
    let (_fixture, db) = database().await;
    seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");

    chat_cleanup::prune_chat_data_after_bot_left(&db, GROUP_CHAT_ID).await;

    assert!(
        subscribers::list(&db, GROUP_CHAT_ID)
            .await
            .expect("subscribers are readable")
            .is_empty()
    );
    assert!(
        feature_flags::find_one_for_groups(&db, GROUP_CHAT_ID, "")
            .await
            .expect("flags are readable")
            .is_none()
    );
    assert!(
        recap_options::find_one(&db, GROUP_CHAT_ID)
            .await
            .expect("options are readable")
            .is_none()
    );
    assert!(
        all_rows(&db, GROUP_CHAT_ID)
            .await
            .expect("histories are readable")
            .is_empty()
    );
}

#[tokio::test]
async fn bot_left_blanks_the_recap_log_text_and_keeps_the_row() {
    let (_fixture, db) = database().await;
    let log_id = seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");
    let before = read_recap_log(&db.pool, &log_id)
        .await
        .expect("the log row exists");

    chat_cleanup::prune_chat_data_after_bot_left(&db, GROUP_CHAT_ID).await;

    let after = read_recap_log(&db.pool, &log_id)
        .await
        .expect("the log row survives so an older feedback button still resolves");
    assert_eq!(after.recap_inputs, "");
    assert_eq!(after.recap_outputs, "");
    assert_eq!(
        after.total_token_usage, before.total_token_usage,
        "the token counters are not part of the prune"
    );
    assert_eq!(after.created_at, before.created_at);
}

#[tokio::test]
async fn bot_left_retains_the_tables_go_deliberately_keeps() {
    let (_fixture, db) = database().await;
    let log_id = seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");
    let log_uuid = Uuid::parse_str(&log_id).expect("a canonical UUID");

    chat_cleanup::prune_chat_data_after_bot_left(&db, GROUP_CHAT_ID).await;

    assert_eq!(
        count_where_chat_id(&db.pool, "sent_messages", GROUP_CHAT_ID)
            .await
            .expect("sent messages are countable"),
        1
    );
    let pinned = sent_messages::find_latest_pinned(&db, GROUP_CHAT_ID)
        .await
        .expect("the pinned record is still resolvable");
    assert_eq!(
        pinned.message_id, BIG_MESSAGE_ID,
        "the retained row is the one that was seeded"
    );
    assert_eq!(
        feedback::counts(
            &db,
            feedback::ReactionTable::ChatHistoriesRecaps,
            GROUP_CHAT_ID,
            log_uuid,
        )
        .await
        .expect("reactions are countable")
        .up_votes,
        1
    );
    assert_eq!(
        count_metrics(&db.pool)
            .await
            .expect("metrics are countable"),
        1
    );
    assert_eq!(
        count_chats(&db.pool, GROUP_CHAT_ID)
            .await
            .expect("the chats row is countable"),
        1,
        "Go's bot-left prune never touches telegram_chats"
    );
}

#[tokio::test]
async fn bot_left_leaves_a_bystander_chat_alone() {
    let (_fixture, db) = database().await;
    seed_every_table(&db, GROUP_CHAT_ID)
        .await
        .expect("the group is seeded");
    let bystander_log = seed_every_table(&db, BYSTANDER_CHAT_ID)
        .await
        .expect("the bystander is seeded");

    chat_cleanup::prune_chat_data_after_bot_left(&db, GROUP_CHAT_ID).await;

    assert_eq!(
        all_rows(&db, BYSTANDER_CHAT_ID)
            .await
            .expect("the bystander is readable")
            .len(),
        2
    );
    assert_eq!(
        read_recap_log(&db.pool, &bystander_log)
            .await
            .expect("the bystander log is readable")
            .recap_inputs,
        "the recap inputs"
    );
}

// ---------------------------------------------------------------------------
// Restart against the current Rust schema
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_repository_keeps_working_across_a_restart_of_the_current_schema() {
    let fixture = SchemaFixture::new();

    let first = fixture.bootstrap_database().await;
    chat_history::save_one(
        &first,
        &sample_message(
            GROUP_CHAT_ID,
            1,
            "written before the restart",
            BIG_TIMESTAMP_MS,
        ),
    )
    .await
    .expect("the row is written");
    first.pool.close().await;

    // A normal current Rust schema must be accepted a second time: the guard
    // rejects a UUID-keyed Go chat_histories, never a table that merely carries
    // a Go name. The unit tests in `src/db/mod.rs` pin the rejection itself.
    let second = fixture.bootstrap_database().await;
    let rows = all_rows(&second, GROUP_CHAT_ID)
        .await
        .expect("the row survives the restart");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].text, "written before the restart");
    assert_eq!(rows[0].chatted_at, BIG_TIMESTAMP_MS);

    chat_history::save_one(
        &second,
        &sample_message(
            GROUP_CHAT_ID,
            2,
            "written after the restart",
            BIG_TIMESTAMP_MS + 1,
        ),
    )
    .await
    .expect("the repository still writes after a re-run of every migration");
    assert_eq!(
        all_rows(&second, GROUP_CHAT_ID)
            .await
            .expect("both rows are readable")
            .len(),
        2
    );
}

// ---------------------------------------------------------------------------
// The legacy reader across both generations
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_legacy_window_spans_both_generations_in_chronological_order() {
    let (_fixture, db) = database().await;
    let now_millis = chrono::Utc::now().timestamp_millis();
    let now_seconds = now_millis / 1_000;

    // A parity row two hours old: outside a one-hour window.
    chat_history::save_one(
        &db,
        &sample_message(
            GROUP_CHAT_ID,
            1,
            "two hours old",
            now_millis - 2 * 3_600_000,
        ),
    )
    .await
    .expect("the stale parity row is stored");

    // A parity row ten minutes old: inside the window, stamped in milliseconds.
    chat_history::save_one(
        &db,
        &sample_message(GROUP_CHAT_ID, 3, "ten minutes old", now_millis - 600_000),
    )
    .await
    .expect("the recent parity row is stored");

    // A legacy row thirty minutes old: `chatted_at` stays at the migration
    // default of zero and the instant lives in `created_at`, in seconds.
    chat_history::insert_message(
        &db.pool,
        GROUP_CHAT_ID,
        2,
        Some(BIG_USER_ID),
        Some("Contributor".to_owned()),
        Some("contributor".to_owned()),
        MessageKind::Text,
        Some("thirty minutes old".to_owned()),
        None,
        now_seconds - 1_800,
    )
    .await
    .expect("the legacy row is stored");

    let window = chat_history::messages_since_hours(&db.pool, GROUP_CHAT_ID, 1)
        .await
        .expect("the one-hour window is readable");

    let texts: Vec<&str> = window.iter().map(|row| row.text.as_str()).collect();
    assert_eq!(
        texts,
        vec!["thirty minutes old", "ten minutes old"],
        "both generations are visible and interleave chronologically"
    );
    assert!(
        !texts.contains(&"two hours old"),
        "a millisecond stamp must not be compared against a seconds cutoff"
    );

    // A wider window admits the stale row and keeps the ordering.
    let wide = chat_history::messages_since_hours(&db.pool, GROUP_CHAT_ID, 6)
        .await
        .expect("the six-hour window is readable");
    assert_eq!(
        wide.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
        vec!["two hours old", "thirty minutes old", "ten minutes old"]
    );
}

// ---------------------------------------------------------------------------
// The empty-text guard
// ---------------------------------------------------------------------------

#[tokio::test]
async fn save_one_skips_a_message_whose_text_is_exactly_empty() {
    let (_fixture, db) = database().await;

    chat_history::save_one(&db, &sample_message(GROUP_CHAT_ID, 1, "", BIG_TIMESTAMP_MS))
        .await
        .expect("the guard reports success without inserting");

    assert!(
        all_rows(&db, GROUP_CHAT_ID)
            .await
            .expect("histories are readable")
            .is_empty(),
        "Go's model-layer guard returns before the create"
    );
}

#[tokio::test]
async fn save_one_persists_whitespace_only_text() {
    let (_fixture, db) = database().await;

    for (message_id, text) in [(1_i64, " "), (2, "\n"), (3, "\t  \n")] {
        chat_history::save_one(
            &db,
            &sample_message(GROUP_CHAT_ID, message_id, text, BIG_TIMESTAMP_MS),
        )
        .await
        .expect("whitespace is not empty");
    }

    let rows = all_rows(&db, GROUP_CHAT_ID)
        .await
        .expect("histories are readable");
    assert_eq!(rows.len(), 3, "only an exactly empty text is skipped");
    assert_eq!(
        rows.iter().map(|row| row.text.as_str()).collect::<Vec<_>>(),
        vec![" ", "\n", "\t  \n"],
        "the text is stored exactly as it arrived"
    );
}
