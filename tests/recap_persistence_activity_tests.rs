//! Task 4B2 — recap logs, feedback reactions, sent messages, usage metrics.
//!
//! Behaviour is pinned to Go v1.0.0:
//! `internal/models/chathistories/{chat_histories,private_forwarded,feedbacks,
//! sent_messages}.go`, `internal/models/smr/feedbacks.go`,
//! `internal/models/logs/logs.go`, and the metric creates in
//! `internal/thirdparty/openai/openai.go`.
//!
//! The repositories expose only what Go exposes, so these tests read and seed
//! rows through local parameterized helpers rather than through production
//! query APIs. Table names are fixed literals; every value is bound.

mod support;

use anyhow::Result;
use insights_bot_telegram_rs::db::{
    Database, codec, feedback,
    models::{
        FROM_PLATFORM_TELEGRAM, MESSAGE_TYPE_AUTO_RECAP, RECAP_TYPE_FOR_GROUP,
        RECAP_TYPE_FOR_PRIVATE_FORWARDED, ReactionCounts, ReactionType, TokenUsage,
    },
    recap_logs, sent_messages, usage_metrics,
};
use support::sqlite_fixture::SchemaFixture;
use uuid::Uuid;

const BIG_CHAT_ID: i64 = -1_001_234_567_890;
const OTHER_CHAT_ID: i64 = -1_009_999_999_999;
const BIG_USER_ID: i64 = 7_654_321_098;
const BIG_MESSAGE_ID: i64 = 5_000_000_000;
const BIG_TIMESTAMP_MS: i64 = 1_700_000_000_000;
const SENTINEL_MS: i64 = 1_600_000_000_000;

/// A log identifier a callback payload would carry, in canonical spelling.
const SAMPLE_LOG_ID: &str = "00000000-0000-4000-8000-000000000001";
const OTHER_LOG_ID: &str = "00000000-0000-4000-8000-000000000002";

/// The two fixed reaction table names, used only as literals.
const RECAPS_TABLE: &str = "feedback_chat_histories_recaps_reactions";
const SUMMARIZATIONS_TABLE: &str = "feedback_summarizations_reactions";

const REACTION_TABLES: [(feedback::ReactionTable, &str); 2] = [
    (feedback::ReactionTable::ChatHistoriesRecaps, RECAPS_TABLE),
    (
        feedback::ReactionTable::Summarizations,
        SUMMARIZATIONS_TABLE,
    ),
];

async fn database() -> (SchemaFixture, Database) {
    let fixture = SchemaFixture::new();
    let database = fixture.bootstrap_database().await;
    (fixture, database)
}

fn usage(prompt: i64, completion: i64, total: i64) -> TokenUsage {
    TokenUsage {
        prompt_tokens: prompt,
        completion_tokens: completion,
        total_tokens: total,
    }
}

fn sample_log_id() -> Uuid {
    Uuid::parse_str(SAMPLE_LOG_ID).expect("a canonical UUID")
}

fn other_log_id() -> Uuid {
    Uuid::parse_str(OTHER_LOG_ID).expect("a canonical UUID")
}

async fn pin_timestamps(db: &Database, table: &'static str, chat_id: i64) {
    sqlx::query(&format!(
        "UPDATE {table} SET created_at = $1, updated_at = $1 WHERE chat_id = $2"
    ))
    .bind(SENTINEL_MS)
    .bind(chat_id)
    .execute(&db.pool)
    .await
    .expect("pin the stored timestamps");
}

// ---------------------------------------------------------------------------
// Local read and seed helpers
// ---------------------------------------------------------------------------

/// One recap-log row, read without a production query API.
struct RecapLogRow {
    id: String,
    recap_inputs: String,
    recap_outputs: String,
    from_platform: i64,
    total_token_usage: i64,
    recap_type: i64,
    model_name: String,
    created_at: i64,
    updated_at: i64,
}

async fn recap_log_rows(db: &Database, chat_id: i64) -> Result<Vec<RecapLogRow>> {
    let rows = sqlx::query(
        "SELECT CAST(id AS TEXT), CAST(recap_inputs AS TEXT), CAST(recap_outputs AS TEXT),
                CAST(from_platform AS TEXT), CAST(total_token_usage AS TEXT),
                CAST(recap_type AS TEXT), CAST(model_name AS TEXT),
                CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
         FROM log_chat_histories_recaps WHERE chat_id = $1",
    )
    .bind(chat_id)
    .fetch_all(&db.pool)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(RecapLogRow {
                id: codec::text_at(row, 0)?,
                recap_inputs: codec::text_at(row, 1)?,
                recap_outputs: codec::text_at(row, 2)?,
                from_platform: codec::i64_at(row, 3)?,
                total_token_usage: codec::i64_at(row, 4)?,
                recap_type: codec::i64_at(row, 5)?,
                model_name: codec::text_at(row, 6)?,
                created_at: codec::i64_at(row, 7)?,
                updated_at: codec::i64_at(row, 8)?,
            })
        })
        .collect()
}

/// One reaction row, read without a production query API.
struct ReactionRow {
    log_id: String,
    user_id: i64,
    reaction_type: String,
    created_at: i64,
    updated_at: i64,
}

async fn reaction_rows(
    db: &Database,
    table: &'static str,
    chat_id: i64,
    log_id: Uuid,
) -> Result<Vec<ReactionRow>> {
    let rows = sqlx::query(&format!(
        "SELECT CAST(log_id AS TEXT), CAST(user_id AS TEXT), CAST(\"type\" AS TEXT),
                CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
         FROM {table} WHERE chat_id = $1 AND log_id = $2"
    ))
    .bind(chat_id)
    .bind(log_id.to_string())
    .fetch_all(&db.pool)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(ReactionRow {
                log_id: codec::text_at(row, 0)?,
                user_id: codec::i64_at(row, 1)?,
                reaction_type: codec::text_at(row, 2)?,
                created_at: codec::i64_at(row, 3)?,
                updated_at: codec::i64_at(row, 4)?,
            })
        })
        .collect()
}

/// Seed one physical reaction row, standing in for a concurrent racer.
async fn seed_reaction(
    db: &Database,
    table: &'static str,
    chat_id: i64,
    log_id: Uuid,
    user_id: i64,
    reaction: ReactionType,
) {
    sqlx::query(&format!(
        "INSERT INTO {table} (id, chat_id, log_id, user_id, \"type\", created_at, updated_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7)"
    ))
    .bind(Uuid::new_v4().to_string())
    .bind(chat_id)
    .bind(log_id.to_string())
    .bind(user_id)
    .bind(reaction.as_stored())
    .bind(BIG_TIMESTAMP_MS)
    .bind(BIG_TIMESTAMP_MS)
    .execute(&db.pool)
    .await
    .expect("seed a physical duplicate");
}

/// One sent-message row, read without a production query API.
struct SentRow {
    id: String,
    message_id: i64,
    text: String,
    is_pinned: bool,
    from_platform: i64,
    message_type: i64,
    created_at: i64,
    updated_at: i64,
}

async fn sent_rows(db: &Database, chat_id: i64) -> Result<Vec<SentRow>> {
    let rows = sqlx::query(
        "SELECT CAST(id AS TEXT), CAST(message_id AS TEXT), CAST(text AS TEXT),
                CAST(is_pinned AS TEXT), CAST(from_platform AS TEXT),
                CAST(message_type AS TEXT), CAST(created_at AS TEXT),
                CAST(updated_at AS TEXT)
         FROM sent_messages WHERE chat_id = $1",
    )
    .bind(chat_id)
    .fetch_all(&db.pool)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(SentRow {
                id: codec::text_at(row, 0)?,
                message_id: codec::i64_at(row, 1)?,
                text: codec::text_at(row, 2)?,
                is_pinned: codec::bool_at(row, 3)?,
                from_platform: codec::i64_at(row, 4)?,
                message_type: codec::i64_at(row, 5)?,
                created_at: codec::i64_at(row, 6)?,
                updated_at: codec::i64_at(row, 7)?,
            })
        })
        .collect()
}

/// One usage-metric row, read without a production query API.
struct MetricRow {
    id: String,
    prompt_operation: String,
    prompt_character_length: i64,
    prompt_token_usage: i64,
    completion_character_length: i64,
    completion_token_usage: i64,
    total_token_usage: i64,
    created_at: i64,
}

async fn metric_rows(db: &Database, model_name: &str) -> Result<Vec<MetricRow>> {
    let rows = sqlx::query(
        "SELECT CAST(id AS TEXT), CAST(prompt_operation AS TEXT),
                CAST(prompt_character_length AS TEXT), CAST(prompt_token_usage AS TEXT),
                CAST(completion_character_length AS TEXT), CAST(completion_token_usage AS TEXT),
                CAST(total_token_usage AS TEXT), CAST(created_at AS TEXT)
         FROM metric_open_ai_chat_completion_token_usages WHERE model_name = $1",
    )
    .bind(model_name)
    .fetch_all(&db.pool)
    .await?;

    rows.iter()
        .map(|row| {
            Ok(MetricRow {
                id: codec::text_at(row, 0)?,
                prompt_operation: codec::text_at(row, 1)?,
                prompt_character_length: codec::i64_at(row, 2)?,
                prompt_token_usage: codec::i64_at(row, 3)?,
                completion_character_length: codec::i64_at(row, 4)?,
                completion_token_usage: codec::i64_at(row, 5)?,
                total_token_usage: codec::i64_at(row, 6)?,
                created_at: codec::i64_at(row, 7)?,
            })
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Recap logs
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_group_recap_log_stores_the_resolved_model_and_the_group_recap_type() {
    let (_fixture, db) = database().await;

    let log_id = recap_logs::create_group_recap(
        &db,
        BIG_CHAT_ID,
        "input histories",
        "output summaries",
        usage(i64::from(i32::MAX) + 1, 4_294_967_296, i64::MAX),
        "gpt-4o-mini",
    )
    .await
    .expect("create");
    Uuid::parse_str(&log_id).expect("the returned identifier is a UUID");

    let rows = recap_log_rows(&db, BIG_CHAT_ID).await.expect("read");
    assert_eq!(rows.len(), 1);
    let stored = &rows[0];
    assert_eq!(stored.id, log_id);
    assert_eq!(stored.recap_inputs, "input histories");
    assert_eq!(stored.recap_outputs, "output summaries");
    assert_eq!(stored.from_platform, FROM_PLATFORM_TELEGRAM);
    assert_eq!(stored.recap_type, RECAP_TYPE_FOR_GROUP);
    assert_eq!(stored.model_name, "gpt-4o-mini");
    assert_eq!(stored.total_token_usage, i64::MAX);
    assert!(stored.created_at > BIG_TIMESTAMP_MS);
    assert_eq!(
        stored.created_at, stored.updated_at,
        "a create stamps both columns from one instant"
    );
}

#[tokio::test]
async fn a_private_forwarded_recap_log_leaves_the_model_name_empty() {
    let (_fixture, db) = database().await;

    recap_logs::create_private_forwarded_recap(
        &db,
        BIG_USER_ID,
        "forwarded histories",
        "forwarded summary",
        usage(11, 22, 33),
    )
    .await
    .expect("create");

    let rows = recap_log_rows(&db, BIG_USER_ID).await.expect("read");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].recap_type, RECAP_TYPE_FOR_PRIVATE_FORWARDED);
    assert_eq!(
        rows[0].model_name, "",
        "Go's forwarded create never calls SetModelName"
    );
    assert_eq!(rows[0].from_platform, FROM_PLATFORM_TELEGRAM);
    assert_eq!(rows[0].created_at, rows[0].updated_at);
}

#[tokio::test]
async fn pruning_blanks_only_the_recap_text_and_retains_every_row() {
    let (_fixture, db) = database().await;

    let first = recap_logs::create_group_recap(
        &db,
        BIG_CHAT_ID,
        "first input",
        "first output",
        usage(1, 2, 3),
        "model-a",
    )
    .await
    .expect("create");
    recap_logs::create_group_recap(
        &db,
        BIG_CHAT_ID,
        "second input",
        "second output",
        usage(4, 5, 6),
        "model-b",
    )
    .await
    .expect("create");
    recap_logs::create_group_recap(
        &db,
        OTHER_CHAT_ID,
        "other input",
        "other output",
        usage(7, 8, 9),
        "model-c",
    )
    .await
    .expect("create");

    pin_timestamps(&db, "log_chat_histories_recaps", BIG_CHAT_ID).await;

    recap_logs::prune_content_by_chat_id(&db, BIG_CHAT_ID)
        .await
        .expect("prune");

    let pruned = recap_log_rows(&db, BIG_CHAT_ID).await.expect("read");
    assert_eq!(pruned.len(), 2, "the rows themselves are retained");
    for row in &pruned {
        assert_eq!(row.recap_inputs, "");
        assert_eq!(row.recap_outputs, "");
        assert_eq!(row.from_platform, FROM_PLATFORM_TELEGRAM);
        assert_eq!(row.recap_type, RECAP_TYPE_FOR_GROUP);
        assert_eq!(row.created_at, SENTINEL_MS);
        assert_eq!(
            row.updated_at, SENTINEL_MS,
            "ent never advances updated_at on an update"
        );
    }
    let mut models: Vec<&str> = pruned.iter().map(|row| row.model_name.as_str()).collect();
    models.sort_unstable();
    assert_eq!(
        models,
        vec!["model-a", "model-b"],
        "every other column survives"
    );
    let mut totals: Vec<i64> = pruned.iter().map(|row| row.total_token_usage).collect();
    totals.sort_unstable();
    assert_eq!(totals, vec![3, 6]);
    assert!(
        pruned.iter().any(|row| row.id == first),
        "the pruned row keeps its identifier"
    );

    let untouched = recap_log_rows(&db, OTHER_CHAT_ID).await.expect("read");
    assert_eq!(
        untouched[0].recap_inputs, "other input",
        "other chats are untouched"
    );
}

#[tokio::test]
async fn migrating_recap_logs_moves_every_row_without_restamping() {
    let (_fixture, db) = database().await;

    for index in 0..3 {
        recap_logs::create_group_recap(
            &db,
            BIG_CHAT_ID,
            "input",
            "output",
            usage(index, index, index),
            "model",
        )
        .await
        .expect("create");
    }
    pin_timestamps(&db, "log_chat_histories_recaps", BIG_CHAT_ID).await;

    recap_logs::migrate_chat_id(&db, BIG_CHAT_ID, OTHER_CHAT_ID)
        .await
        .expect("migrate");

    assert!(
        recap_log_rows(&db, BIG_CHAT_ID)
            .await
            .expect("read")
            .is_empty()
    );
    let moved = recap_log_rows(&db, OTHER_CHAT_ID).await.expect("read");
    assert_eq!(moved.len(), 3);
    for row in moved {
        assert_eq!(row.created_at, SENTINEL_MS);
        assert_eq!(row.updated_at, SENTINEL_MS);
    }
}

// ---------------------------------------------------------------------------
// Feedback reactions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_reaction_create_stamps_both_timestamps_from_one_instant() {
    let (_fixture, db) = database().await;

    for (table, name) in REACTION_TABLES {
        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::UpVote,
        )
        .await
        .expect("react");

        let rows = reaction_rows(&db, name, BIG_CHAT_ID, sample_log_id())
            .await
            .expect("read");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].log_id, SAMPLE_LOG_ID);
        assert_eq!(rows[0].user_id, BIG_USER_ID);
        assert_eq!(rows[0].reaction_type, "up_vote");
        assert!(rows[0].created_at > BIG_TIMESTAMP_MS);
        assert_eq!(rows[0].created_at, rows[0].updated_at);
    }
}

#[tokio::test]
async fn a_non_canonical_uuid_spelling_addresses_the_canonical_stored_row() {
    let (_fixture, db) = database().await;

    // Uppercase, braced, and URN spellings all parse to the same value, and the
    // repository binds the canonical rendering.
    let spellings = [
        "00000000-0000-4000-8000-000000000001".to_ascii_uppercase(),
        format!("{{{SAMPLE_LOG_ID}}}"),
        format!("urn:uuid:{SAMPLE_LOG_ID}"),
    ];

    for (table, name) in REACTION_TABLES {
        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::UpVote,
        )
        .await
        .expect("react with the canonical spelling");

        for spelling in &spellings {
            let parsed = Uuid::parse_str(spelling).expect("an accepted UUID spelling");
            assert_eq!(parsed, sample_log_id(), "every spelling is the same value");

            assert_eq!(
                feedback::counts(&db, table, BIG_CHAT_ID, parsed)
                    .await
                    .expect("counts")
                    .up_votes,
                1,
                "{spelling} must reach the canonical row"
            );
        }

        // Toggling through a non-canonical spelling removes the canonical row.
        let braced = Uuid::parse_str(&spellings[1]).expect("a braced UUID");
        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            braced,
            BIG_USER_ID,
            ReactionType::UpVote,
        )
        .await
        .expect("toggle through a non-canonical spelling");

        assert!(
            reaction_rows(&db, name, BIG_CHAT_ID, sample_log_id())
                .await
                .expect("read")
                .is_empty(),
            "the canonical row is the one that was operated on"
        );
    }
}

#[tokio::test]
async fn reacting_with_the_same_type_toggles_the_reaction_off() {
    let (_fixture, db) = database().await;

    for (table, name) in REACTION_TABLES {
        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::Lmao,
        )
        .await
        .expect("react");
        assert_eq!(
            reaction_rows(&db, name, BIG_CHAT_ID, sample_log_id())
                .await
                .expect("read")
                .len(),
            1
        );

        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::Lmao,
        )
        .await
        .expect("react again");
        assert!(
            reaction_rows(&db, name, BIG_CHAT_ID, sample_log_id())
                .await
                .expect("read")
                .is_empty(),
            "a repeated same-type reaction removes it"
        );
    }
}

#[tokio::test]
async fn a_same_type_toggle_removes_every_duplicate_row_at_once() {
    let (_fixture, db) = database().await;

    for (table, name) in REACTION_TABLES {
        for _ in 0..3 {
            seed_reaction(
                &db,
                name,
                BIG_CHAT_ID,
                sample_log_id(),
                BIG_USER_ID,
                ReactionType::UpVote,
            )
            .await;
        }

        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::UpVote,
        )
        .await
        .expect("react");
        assert!(
            reaction_rows(&db, name, BIG_CHAT_ID, sample_log_id())
                .await
                .expect("read")
                .is_empty(),
            "the typed delete clears every duplicate"
        );
    }
}

#[tokio::test]
async fn reacting_with_a_new_type_replaces_every_previous_reaction() {
    let (_fixture, db) = database().await;

    for (table, name) in REACTION_TABLES {
        for reaction in [ReactionType::UpVote, ReactionType::DownVote] {
            seed_reaction(
                &db,
                name,
                BIG_CHAT_ID,
                sample_log_id(),
                BIG_USER_ID,
                reaction,
            )
            .await;
        }

        feedback::react(
            &db,
            table,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::Lmao,
        )
        .await
        .expect("react");

        let rows = reaction_rows(&db, name, BIG_CHAT_ID, sample_log_id())
            .await
            .expect("read");
        assert_eq!(rows.len(), 1, "every previous type is replaced by one row");
        assert_eq!(rows[0].reaction_type, "lmao");
    }
}

#[tokio::test]
async fn counts_include_every_physical_duplicate_row() {
    let (_fixture, db) = database().await;

    for (table, name) in REACTION_TABLES {
        for (reaction, copies) in [
            (ReactionType::UpVote, 3),
            (ReactionType::DownVote, 2),
            (ReactionType::Lmao, 1),
            (ReactionType::None, 4),
        ] {
            for index in 0..copies {
                seed_reaction(
                    &db,
                    name,
                    BIG_CHAT_ID,
                    sample_log_id(),
                    BIG_USER_ID + i64::from(index),
                    reaction,
                )
                .await;
            }
        }
        // A different log must not contribute.
        seed_reaction(
            &db,
            name,
            BIG_CHAT_ID,
            other_log_id(),
            BIG_USER_ID,
            ReactionType::UpVote,
        )
        .await;

        assert_eq!(
            feedback::counts(&db, table, BIG_CHAT_ID, sample_log_id())
                .await
                .expect("counts"),
            ReactionCounts {
                up_votes: 3,
                down_votes: 2,
                lmao: 1,
            },
            "none is counted by no bucket, exactly as Go filters"
        );
    }
}

#[tokio::test]
async fn counts_for_an_unknown_log_are_all_zero() {
    let (_fixture, db) = database().await;

    for (table, _) in REACTION_TABLES {
        assert_eq!(
            feedback::counts(&db, table, BIG_CHAT_ID, sample_log_id())
                .await
                .expect("counts"),
            ReactionCounts::default()
        );
    }
    assert!(
        !feedback::has_summarization_reacted(
            &db,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::UpVote
        )
        .await
        .expect("has reacted")
    );
}

#[tokio::test]
async fn the_summarization_check_matches_only_the_exact_tuple_and_type() {
    let (_fixture, db) = database().await;

    seed_reaction(
        &db,
        SUMMARIZATIONS_TABLE,
        BIG_CHAT_ID,
        sample_log_id(),
        BIG_USER_ID,
        ReactionType::DownVote,
    )
    .await;

    assert!(
        feedback::has_summarization_reacted(
            &db,
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::DownVote
        )
        .await
        .expect("has reacted")
    );
    for (chat_id, log_id, user_id, reaction) in [
        (
            OTHER_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::DownVote,
        ),
        (
            BIG_CHAT_ID,
            other_log_id(),
            BIG_USER_ID,
            ReactionType::DownVote,
        ),
        (
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID + 1,
            ReactionType::DownVote,
        ),
        (
            BIG_CHAT_ID,
            sample_log_id(),
            BIG_USER_ID,
            ReactionType::UpVote,
        ),
    ] {
        assert!(
            !feedback::has_summarization_reacted(&db, chat_id, log_id, user_id, reaction)
                .await
                .expect("has reacted"),
            "a different tuple must not match"
        );
    }
}

#[tokio::test]
async fn the_two_reaction_tables_stay_isolated() {
    let (_fixture, db) = database().await;

    feedback::react(
        &db,
        feedback::ReactionTable::ChatHistoriesRecaps,
        BIG_CHAT_ID,
        sample_log_id(),
        BIG_USER_ID,
        ReactionType::UpVote,
    )
    .await
    .expect("react");

    assert_eq!(
        feedback::counts(
            &db,
            feedback::ReactionTable::ChatHistoriesRecaps,
            BIG_CHAT_ID,
            sample_log_id()
        )
        .await
        .expect("counts")
        .up_votes,
        1
    );
    assert_eq!(
        feedback::counts(
            &db,
            feedback::ReactionTable::Summarizations,
            BIG_CHAT_ID,
            sample_log_id()
        )
        .await
        .expect("counts"),
        ReactionCounts::default(),
        "the summarization table is a separate physical table"
    );
}

// ---------------------------------------------------------------------------
// Sent messages
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_automatic_recap_message_stores_the_go_platform_and_message_type() {
    let (_fixture, db) = database().await;

    sent_messages::create_auto_recap_message(&db, BIG_CHAT_ID, BIG_MESSAGE_ID, "part one", true)
        .await
        .expect("create");

    let rows = sent_rows(&db, BIG_CHAT_ID).await.expect("read");
    assert_eq!(rows.len(), 1);
    let stored = &rows[0];
    assert_eq!(stored.message_id, BIG_MESSAGE_ID);
    assert_eq!(stored.text, "part one");
    assert!(stored.is_pinned);
    assert_eq!(stored.from_platform, FROM_PLATFORM_TELEGRAM);
    assert_eq!(stored.message_type, MESSAGE_TYPE_AUTO_RECAP);
    assert!(stored.created_at > BIG_TIMESTAMP_MS);
    assert_eq!(stored.created_at, stored.updated_at);
    Uuid::parse_str(&stored.id).expect("a generated UUID");
}

#[tokio::test]
async fn sent_messages_keep_physical_duplicates() {
    let (_fixture, db) = database().await;

    for _ in 0..3 {
        sent_messages::create_auto_recap_message(
            &db,
            BIG_CHAT_ID,
            BIG_MESSAGE_ID,
            "duplicate",
            false,
        )
        .await
        .expect("create");
    }

    let rows = sent_rows(&db, BIG_CHAT_ID).await.expect("read");
    assert_eq!(rows.len(), 3, "no uniqueness collapses the rows");
    assert!(rows.iter().all(|row| row.message_id == BIG_MESSAGE_ID));
}

#[tokio::test]
async fn the_latest_pinned_message_is_the_newest_created_at() {
    let (_fixture, db) = database().await;

    for (message_id, created_at, pinned) in [
        (1_i64, 100_i64, true),
        (2, 300, true),
        (3, 200, true),
        (4, 900, false),
    ] {
        sent_messages::create_auto_recap_message(&db, BIG_CHAT_ID, message_id, "part", pinned)
            .await
            .expect("create");
        sqlx::query(
            "UPDATE sent_messages SET created_at = $1 WHERE chat_id = $2 AND message_id = $3",
        )
        .bind(created_at)
        .bind(BIG_CHAT_ID)
        .bind(message_id)
        .execute(&db.pool)
        .await
        .expect("age the row");
    }

    let latest = sent_messages::find_latest_pinned(&db, BIG_CHAT_ID)
        .await
        .expect("find");
    assert_eq!(
        latest.message_id, 2,
        "created_at DESC LIMIT 1 over pinned rows only"
    );
    assert_eq!(latest.created_at, 300);
}

#[tokio::test]
async fn a_chat_without_a_pinned_message_reports_not_found() {
    let (_fixture, db) = database().await;

    // Go's `First` returns an Ent not-found error rather than an empty value.
    let empty = sent_messages::find_latest_pinned(&db, BIG_CHAT_ID)
        .await
        .expect_err("an empty table has no pinned message");
    assert!(
        matches!(
            empty.downcast_ref::<sqlx::Error>(),
            Some(sqlx::Error::RowNotFound)
        ),
        "the missing row surfaces as RowNotFound: {empty:?}"
    );

    sent_messages::create_auto_recap_message(&db, BIG_CHAT_ID, 1, "unpinned", false)
        .await
        .expect("create");
    let unpinned_only = sent_messages::find_latest_pinned(&db, BIG_CHAT_ID)
        .await
        .expect_err("an unpinned row never satisfies the lookup");
    assert!(matches!(
        unpinned_only.downcast_ref::<sqlx::Error>(),
        Some(sqlx::Error::RowNotFound)
    ));
}

#[tokio::test]
async fn unpinning_affects_every_duplicate_row_of_the_pair() {
    let (_fixture, db) = database().await;

    for _ in 0..3 {
        sent_messages::create_auto_recap_message(&db, BIG_CHAT_ID, BIG_MESSAGE_ID, "part", true)
            .await
            .expect("create");
    }
    sent_messages::create_auto_recap_message(&db, BIG_CHAT_ID, BIG_MESSAGE_ID + 1, "other", true)
        .await
        .expect("create");
    pin_timestamps(&db, "sent_messages", BIG_CHAT_ID).await;

    sent_messages::set_pinned(&db, BIG_CHAT_ID, BIG_MESSAGE_ID, false)
        .await
        .expect("unpin");

    let rows = sent_rows(&db, BIG_CHAT_ID).await.expect("read");
    assert_eq!(
        rows.iter()
            .filter(|row| row.message_id == BIG_MESSAGE_ID && !row.is_pinned)
            .count(),
        3,
        "every duplicate of the pair is updated"
    );
    for row in &rows {
        if row.message_id != BIG_MESSAGE_ID {
            assert!(row.is_pinned, "the other message is untouched");
        }
        assert_eq!(row.created_at, SENTINEL_MS);
        assert_eq!(
            row.updated_at, SENTINEL_MS,
            "ent never advances updated_at on an update"
        );
    }

    // Updating an absent chat is a silent no-op rather than an error.
    sent_messages::set_pinned(&db, OTHER_CHAT_ID, BIG_MESSAGE_ID, false)
        .await
        .expect("unpin an absent chat");
}

// ---------------------------------------------------------------------------
// OpenAI usage metrics
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_usage_metric_round_trips_with_full_width_values() {
    let (_fixture, db) = database().await;

    usage_metrics::create(
        &db,
        "Summarize Chat Histories",
        usage(i64::MAX, i64::MIN, i64::from(i32::MAX) + 1),
        "gpt-4o-mini",
    )
    .await
    .expect("create");

    let rows = metric_rows(&db, "gpt-4o-mini").await.expect("read");
    assert_eq!(rows.len(), 1);
    let stored = &rows[0];
    assert_eq!(stored.prompt_operation, "Summarize Chat Histories");
    assert_eq!(stored.prompt_token_usage, i64::MAX);
    assert_eq!(stored.completion_token_usage, i64::MIN);
    assert_eq!(stored.total_token_usage, i64::from(i32::MAX) + 1);
    assert_eq!(
        stored.prompt_character_length, 0,
        "Go never sets the character lengths"
    );
    assert_eq!(stored.completion_character_length, 0);
    assert!(stored.created_at > BIG_TIMESTAMP_MS);
    Uuid::parse_str(&stored.id).expect("a generated UUID");
}

#[tokio::test]
async fn usage_metrics_accumulate_without_uniqueness() {
    let (_fixture, db) = database().await;

    for _ in 0..3 {
        usage_metrics::create(&db, "Sarcastic Condense", usage(1, 2, 3), "model")
            .await
            .expect("create");
    }

    let rows = metric_rows(&db, "model").await.expect("read");
    assert_eq!(rows.len(), 3);
    let mut identifiers: Vec<&str> = rows.iter().map(|row| row.id.as_str()).collect();
    identifiers.sort_unstable();
    identifiers.dedup();
    assert_eq!(identifiers.len(), 3, "each row gets its own identifier");
}
