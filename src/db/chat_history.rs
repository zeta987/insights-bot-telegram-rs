//! `chat_histories` repository.
//!
//! Two generations of code share this one physical table. The first half of
//! this module is the original Rust reader and writer, which stores a message
//! kind, a media URL, and a `created_at` in Unix *seconds*. The second half is
//! the Go v1.0.0 parity surface added by the Rich recap parity work, ported
//! from `internal/models/chathistories/chat_histories.go`; it stores Go's
//! columns and, like ent, keeps `created_at` and `updated_at` in Unix
//! *milliseconds*.
//!
//! That unit collision on `created_at` is real: the column holds seconds or
//! milliseconds depending on which generation performed the insert. The parity
//! read path sidesteps it entirely by measuring Go's window on `chatted_at`.
//! The still-live legacy reader cannot, because it must see both generations,
//! so it normalises each row to milliseconds first — see
//! [`NORMALIZED_MILLIS`].

use anyhow::Result;
use sqlx::{AnyPool, any::AnyRow};

use super::{
    Database, codec,
    models::{
        CHAT_TYPE_SUPERGROUP, ChatHistory, FROM_PLATFORM_TELEGRAM, MessageKind,
        NewTelegramChatHistory, TelegramChatHistory,
    },
};

#[allow(clippy::too_many_arguments)]
pub async fn insert_message(
    pool: &AnyPool,
    chat_id: i64,
    message_id: i64,
    from_id: Option<i64>,
    from_full_name: Option<String>,
    from_username: Option<String>,
    kind: MessageKind,
    text: Option<String>,
    media_url: Option<String>,
    created_at: i64,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO chat_histories (chat_id, message_id, from_id, from_full_name, from_username, kind, text, media_url, created_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(chat_id)
    .bind(message_id)
    .bind(from_id)
    .bind(from_full_name)
    .bind(from_username)
    .bind(kind.as_str())
    .bind(text)
    .bind(media_url)
    .bind(created_at)
    .execute(pool)
    .await?;
    Ok(())
}

#[allow(dead_code)] // Used by integration tests in tests/recap_scope_tests.rs
pub async fn recent_messages(pool: &AnyPool, chat_id: i64, limit: i64) -> Result<Vec<ChatHistory>> {
    // Use explicit column selection with COALESCE to handle SQLx Any driver NULL issues.
    let rows = sqlx::query_as::<_, ChatHistory>(
        "SELECT id, chat_id, message_id, COALESCE(from_id, 0) as from_id,
                COALESCE(from_full_name, '') as from_full_name,
                COALESCE(from_username, '') as from_username,
                kind,
                COALESCE(text, '') as text,
                COALESCE(media_url, '') as media_url,
                created_at
         FROM chat_histories WHERE chat_id = $1 ORDER BY created_at DESC LIMIT $2",
    )
    .bind(chat_id)
    .bind(limit)
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn is_recap_enabled(pool: &AnyPool, chat_id: i64) -> Result<bool> {
    let row: Option<(i64,)> = sqlx::query_as(
        "SELECT CASE WHEN enabled THEN 1 ELSE 0 END
         FROM recap_configs
         WHERE chat_id = $1
         LIMIT 1",
    )
    .bind(chat_id)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(enabled,)| enabled != 0).unwrap_or(true))
}

/// The instant a row was chatted, in Unix milliseconds, for either generation.
///
/// A parity row carries a real `chatted_at`; a legacy row carries the migration
/// default of zero there and keeps its instant in `created_at`, in Unix
/// *seconds*. Scaling the legacy value puts both generations on one scale, so a
/// single comparison and a single ordering serve the whole table.
///
/// A Telegram message date is never zero, so the sentinel cannot collide with a
/// genuine parity instant.
const NORMALIZED_MILLIS: &str =
    "(CASE WHEN chatted_at <> 0 THEN chatted_at ELSE created_at * 1000 END)";

/// The complete statement [`messages_since_hours`] executes.
///
/// The whole query lives here so the unit test below inspects the exact text
/// that reaches the engine rather than a prefix.
fn messages_since_query() -> String {
    format!(
        "SELECT id, chat_id, message_id, COALESCE(from_id, 0) as from_id,
                COALESCE(from_full_name, '') as from_full_name,
                COALESCE(from_username, '') as from_username,
                kind,
                COALESCE(text, '') as text,
                COALESCE(media_url, '') as media_url,
                created_at
         FROM chat_histories
         WHERE chat_id = $1 AND {NORMALIZED_MILLIS} >= $2
         ORDER BY {NORMALIZED_MILLIS} ASC"
    )
}

/// Find chat messages within the specified time duration (hours) before now.
///
/// Both generations are visible and interleave chronologically, because each row
/// is normalised to milliseconds before it is filtered and ordered.
pub async fn messages_since_hours(
    pool: &AnyPool,
    chat_id: i64,
    hours: i64,
) -> Result<Vec<ChatHistory>> {
    let since_millis = chrono::Utc::now().timestamp_millis() - (hours * 3_600_000);
    let rows = sqlx::query_as::<_, ChatHistory>(&messages_since_query())
        .bind(chat_id)
        .bind(since_millis)
        .fetch_all(pool)
        .await?;
    Ok(rows)
}

/// Update the text of an existing message (for edited-message sync).
pub async fn update_message_text(
    pool: &AnyPool,
    chat_id: i64,
    message_id: i64,
    new_text: &str,
) -> Result<()> {
    sqlx::query("UPDATE chat_histories SET text = $1 WHERE chat_id = $2 AND message_id = $3")
        .bind(new_text)
        .bind(chat_id)
        .bind(message_id)
        .execute(pool)
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Go v1.0.0 parity surface
// ---------------------------------------------------------------------------

/// Every Go column, cast to text so the `Any` driver cannot narrow or refuse it.
///
/// `text` is the one column the Rust table declares nullable, so it is
/// coalesced: a row written by the legacy insert may hold SQL `NULL` there,
/// while Go's model has no null text.
const SELECT_COLUMNS: &str = "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT),
        CAST(chat_type AS TEXT), CAST(chat_title AS TEXT), CAST(message_id AS TEXT),
        CAST(user_id AS TEXT), CAST(username AS TEXT), CAST(full_name AS TEXT),
        CAST(COALESCE(text, '') AS TEXT),
        CAST(replied_to_message_id AS TEXT), CAST(replied_to_user_id AS TEXT),
        CAST(replied_to_full_name AS TEXT), CAST(replied_to_username AS TEXT),
        CAST(replied_to_text AS TEXT), CAST(replied_to_chat_type AS TEXT),
        CAST(chatted_at AS TEXT), CAST(embedded AS TEXT), CAST(from_platform AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM chat_histories";

/// The insert both engines run.
///
/// `id` is omitted so the Rust-owned integer primary key keeps generating
/// itself, and no conflict clause is present: Go's create adds a physical row
/// per call and the table declares no uniqueness over `(chat_id, message_id)`.
///
/// `kind` is a Rust-only `NOT NULL` column with no default, so the statement
/// supplies it. `text` is the correct value for every parity row, because Go
/// only ever persists a message that yielded text or a caption.
const INSERT: &str = "INSERT INTO chat_histories
        (chat_id, chat_type, chat_title, message_id, user_id, username, full_name,
         text, replied_to_message_id, replied_to_user_id, replied_to_full_name,
         replied_to_username, replied_to_text, replied_to_chat_type, chatted_at,
         embedded, from_platform, created_at, updated_at, kind)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
             $16, $17, $18, $19, $20)";

/// Rewrite the text of every row matching the chat and message pair.
///
/// No `LIMIT` is present: Go's `Update().Where(..).Exec()` touches every
/// matching row, and physical duplicates of a pair are expected. Only `text` is
/// assigned, so the actor, the reply snapshot, the chat title and type, the
/// timestamps, and the forwarding prefix already folded into an older `text`
/// are all left as they were. `updated_at` is untouched: ent declares it with
/// `DefaultFunc` only, never `UpdateDefault`, and no Go caller sets it on an
/// update.
const UPDATE_TEXT: &str =
    "UPDATE chat_histories SET text = $1 WHERE chat_id = $2 AND message_id = $3";

/// Remove every history row of a chat.
const DELETE_BY_CHAT_ID: &str = "DELETE FROM chat_histories WHERE chat_id = $1";

/// Move every history row onto the new chat identifier.
///
/// Go rewrites the chat type as well, because only a supergroup upgrade
/// reaches this path.
const MIGRATE_CHAT_ID: &str =
    "UPDATE chat_histories SET chat_id = $1, chat_type = $2 WHERE chat_id = $3";

/// The complete statement [`find_chatted_after`] executes.
///
/// The whole query lives here rather than being assembled at the call site, so
/// the unit test below inspects the exact text that reaches the engine. A
/// clause appended to a suffix could otherwise slip past it.
fn find_chatted_after_query() -> String {
    format!("{SELECT_COLUMNS} WHERE chat_id = $1 AND chatted_at > $2 ORDER BY message_id ASC")
}

fn decode(row: &AnyRow) -> Result<TelegramChatHistory> {
    Ok(TelegramChatHistory {
        id: codec::i64_at(row, 0)?,
        chat_id: codec::i64_at(row, 1)?,
        chat_type: codec::text_at(row, 2)?,
        chat_title: codec::text_at(row, 3)?,
        message_id: codec::i64_at(row, 4)?,
        user_id: codec::i64_at(row, 5)?,
        username: codec::text_at(row, 6)?,
        full_name: codec::text_at(row, 7)?,
        text: codec::text_at(row, 8)?,
        replied_to_message_id: codec::i64_at(row, 9)?,
        replied_to_user_id: codec::i64_at(row, 10)?,
        replied_to_full_name: codec::text_at(row, 11)?,
        replied_to_username: codec::text_at(row, 12)?,
        replied_to_text: codec::text_at(row, 13)?,
        replied_to_chat_type: codec::text_at(row, 14)?,
        chatted_at: codec::i64_at(row, 15)?,
        embedded: codec::bool_at(row, 16)?,
        from_platform: codec::i64_at(row, 17)?,
        created_at: codec::i64_at(row, 18)?,
        updated_at: codec::i64_at(row, 19)?,
    })
}

/// Persist one chat history row.
///
/// A message whose text is exactly empty is skipped without an insert, which is
/// Go's model-layer guard. Whitespace is not empty and is stored as it arrived.
///
/// Every other call adds a row. Go performs no lookup first and the table
/// carries no uniqueness, so re-delivering the same Telegram message stores it
/// twice.
pub async fn save_one(db: &Database, message: &NewTelegramChatHistory) -> Result<()> {
    if message.text.is_empty() {
        return Ok(());
    }

    // One instant for both stamps: ent seeds each with the same `DefaultFunc`
    // during a create, and nothing ever advances `updated_at` afterwards.
    let now = codec::now_unix_millis();

    sqlx::query(INSERT)
        .bind(message.chat_id)
        .bind(&message.chat_type)
        .bind(&message.chat_title)
        .bind(message.message_id)
        .bind(message.user_id)
        .bind(&message.username)
        .bind(&message.full_name)
        .bind(&message.text)
        .bind(message.replied_to_message_id)
        .bind(message.replied_to_user_id)
        .bind(&message.replied_to_full_name)
        .bind(&message.replied_to_username)
        .bind(&message.replied_to_text)
        .bind(&message.replied_to_chat_type)
        .bind(message.chatted_at)
        .bind(false)
        .bind(FROM_PLATFORM_TELEGRAM)
        .bind(now)
        .bind(now)
        .bind(MessageKind::Text.as_str())
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Rewrite the text of every row matching the chat and message pair.
pub async fn update_one_text(
    db: &Database,
    chat_id: i64,
    message_id: i64,
    text: &str,
) -> Result<()> {
    sqlx::query(UPDATE_TEXT)
        .bind(text)
        .bind(chat_id)
        .bind(message_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Every row of a chat chatted strictly after `cutoff_unix_millis`.
///
/// The comparison is strict and the order is by `message_id` ascending, which
/// is what Go's `ChattedAtGT` plus `ByMessageID(OrderAsc)` produce. Physical
/// duplicates are returned as they are stored.
pub async fn find_chatted_after(
    db: &Database,
    chat_id: i64,
    cutoff_unix_millis: i64,
) -> Result<Vec<TelegramChatHistory>> {
    let rows = sqlx::query(&find_chatted_after_query())
        .bind(chat_id)
        .bind(cutoff_unix_millis)
        .fetch_all(&db.pool)
        .await?;
    rows.iter().map(decode).collect()
}

/// Every row of a chat chatted within `before` of now.
///
/// This is Go's `FindChatHistoriesByTimeBefore`, which the one, six, eight, and
/// twelve hour helpers all delegate to.
pub async fn find_by_time_before(
    db: &Database,
    chat_id: i64,
    before: chrono::Duration,
) -> Result<Vec<TelegramChatHistory>> {
    let cutoff = codec::now_unix_millis() - before.num_milliseconds();
    find_chatted_after(db, chat_id, cutoff).await
}

/// Remove every history row of a chat.
pub async fn delete_all_by_chat_id(db: &Database, chat_id: i64) -> Result<()> {
    sqlx::query(DELETE_BY_CHAT_ID)
        .bind(chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Move every history row onto the new chat identifier, forcing `supergroup`.
pub async fn migrate_chat_id(db: &Database, from_chat_id: i64, to_chat_id: i64) -> Result<()> {
    sqlx::query(MIGRATE_CHAT_ID)
        .bind(to_chat_id)
        .bind(CHAT_TYPE_SUPERGROUP)
        .bind(from_chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DELETE_BY_CHAT_ID, INSERT, MIGRATE_CHAT_ID, UPDATE_TEXT, find_chatted_after_query,
        messages_since_query,
    };

    /// The highest `$n` placeholder a static statement uses.
    fn placeholder_count(sql: &str) -> usize {
        (1..=32)
            .take_while(|index| sql.contains(&format!("${index}")))
            .count()
    }

    #[test]
    fn the_insert_permits_a_physical_duplicate() {
        let upper = INSERT.to_ascii_uppercase();
        for forbidden in ["ON CONFLICT", "ON DUPLICATE", "OR REPLACE", "OR IGNORE"] {
            assert!(
                !upper.contains(forbidden),
                "Go's create adds a row per call: {INSERT}"
            );
        }
        assert_eq!(placeholder_count(INSERT), 20);
    }

    #[test]
    fn the_insert_leaves_the_rust_owned_integer_key_to_the_engine() {
        assert!(
            !INSERT.contains("(id,"),
            "the primary key generates itself: {INSERT}"
        );
    }

    #[test]
    fn the_text_update_assigns_only_the_text_and_bounds_nothing() {
        let upper = UPDATE_TEXT.to_ascii_uppercase();
        assert!(upper.contains("SET TEXT = $1"));
        assert!(
            !upper.contains("LIMIT"),
            "every matching row is rewritten: {UPDATE_TEXT}"
        );
        // A second assignment would show up as another `=` before the WHERE.
        let assignments = UPDATE_TEXT
            .split(" WHERE ")
            .next()
            .expect("the statement has a WHERE clause");
        assert_eq!(
            assignments.matches('=').count(),
            1,
            "no other column may be assigned: {UPDATE_TEXT}"
        );
        assert_eq!(placeholder_count(UPDATE_TEXT), 3);
    }

    #[test]
    fn the_window_query_is_strictly_after_the_cutoff_and_ordered_by_message_id() {
        let query = find_chatted_after_query();
        let upper = query.to_ascii_uppercase();

        assert!(
            upper.contains("CHATTED_AT > $2"),
            "Go's ChattedAtGT excludes the cutoff instant: {query}"
        );
        assert!(
            !upper.contains("CHATTED_AT >= $2"),
            "an inclusive bound would return one extra row: {query}"
        );
        assert!(upper.contains("ORDER BY MESSAGE_ID ASC"));
        for forbidden in ["GROUP BY", "DISTINCT", "LIMIT"] {
            assert!(
                !upper.contains(forbidden),
                "Go's query adds no {forbidden}: {query}"
            );
        }
        // `created_at` is a selected Go column, so only the clauses after the
        // table name may be inspected for it.
        let clauses = upper
            .split("FROM CHAT_HISTORIES")
            .nth(1)
            .expect("the statement names its table")
            .to_owned();
        assert!(
            !clauses.contains("CREATED_AT"),
            "the window is measured on chatted_at, never on the legacy column: {query}"
        );
    }

    #[test]
    fn the_migration_rewrites_the_identifier_and_the_type_and_nothing_else() {
        let assignments = MIGRATE_CHAT_ID
            .split(" WHERE ")
            .next()
            .expect("the statement has a WHERE clause");
        assert_eq!(
            assignments.matches('=').count(),
            2,
            "only chat_id and chat_type move: {MIGRATE_CHAT_ID}"
        );
        assert!(assignments.contains("chat_id = $1"));
        assert!(assignments.contains("chat_type = $2"));
        assert_eq!(placeholder_count(MIGRATE_CHAT_ID), 3);
    }

    #[test]
    fn the_legacy_window_normalizes_both_generations_to_milliseconds() {
        let query = messages_since_query();
        let upper = query.to_ascii_uppercase();

        // The comparison and the ordering must use the same expression, or the
        // two generations would interleave incorrectly.
        assert_eq!(
            upper
                .matches("CASE WHEN CHATTED_AT <> 0 THEN CHATTED_AT ELSE CREATED_AT * 1000 END")
                .count(),
            2,
            "one normalisation for the filter and one for the order: {query}"
        );
        assert!(
            upper.contains("END) >= $2"),
            "the window is inclusive: {query}"
        );
        assert!(
            upper.contains("END) ASC"),
            "chronological across both: {query}"
        );

        // A bare comparison on either raw column would reintroduce the unit
        // collision this expression exists to remove.
        let clauses = upper
            .split("FROM CHAT_HISTORIES")
            .nth(1)
            .expect("the statement names its table")
            .to_owned();
        assert!(!clauses.contains("CREATED_AT >="));
        assert!(!clauses.contains("CHATTED_AT >="));
        assert!(!clauses.contains("BY CREATED_AT"));
        assert!(!clauses.contains("BY CHATTED_AT"));
    }

    #[test]
    fn the_delete_is_scoped_to_one_chat() {
        assert!(DELETE_BY_CHAT_ID.contains("WHERE chat_id = $1"));
        assert_eq!(placeholder_count(DELETE_BY_CHAT_ID), 1);
    }
}
