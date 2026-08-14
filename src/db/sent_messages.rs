//! `sent_messages` repository.
//!
//! Ported from Go v1.0.0 `internal/models/chathistories/sent_messages.go`. Only
//! the automatic recap delivery writes here, and it always stores
//! `messageType = autoRecapMessage`; no manual or forwarded shape exists in the
//! pinned source, so none is invented.

use anyhow::Result;
use sqlx::any::AnyRow;

use crate::db::{
    Database, DbBackend, codec,
    models::{FROM_PLATFORM_TELEGRAM, MESSAGE_TYPE_AUTO_RECAP, SentMessage},
};

const SELECT_COLUMNS: &str = "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT),
        CAST(message_id AS TEXT), CAST(text AS TEXT), CAST(is_pinned AS TEXT),
        CAST(from_platform AS TEXT), CAST(message_type AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM sent_messages";

const INSERT_POSTGRES: &str = "INSERT INTO sent_messages
        (id, chat_id, message_id, text, is_pinned, from_platform, message_type,
         created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8, $9)";

const INSERT_SQLITE: &str = "INSERT INTO sent_messages
        (id, chat_id, message_id, text, is_pinned, from_platform, message_type,
         created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

fn insert_statement(backend: DbBackend) -> &'static str {
    match backend {
        DbBackend::Postgres => INSERT_POSTGRES,
        DbBackend::Sqlite => INSERT_SQLITE,
    }
}

/// The complete statement [`find_latest_pinned`] executes.
///
/// Go orders by `created_at` descending and takes the first row, so the ordering
/// and the limit are contractual here rather than forbidden.
fn find_latest_pinned_query() -> String {
    format!(
        "{SELECT_COLUMNS} WHERE chat_id = $1 AND is_pinned = $2
             ORDER BY created_at DESC LIMIT 1"
    )
}

fn decode(row: &AnyRow) -> Result<SentMessage> {
    Ok(SentMessage {
        id: codec::text_at(row, 0)?,
        chat_id: codec::i64_at(row, 1)?,
        message_id: codec::i64_at(row, 2)?,
        text: codec::text_at(row, 3)?,
        is_pinned: codec::bool_at(row, 4)?,
        from_platform: codec::i64_at(row, 5)?,
        message_type: codec::i64_at(row, 6)?,
        created_at: codec::i64_at(row, 7)?,
        updated_at: codec::i64_at(row, 8)?,
    })
}

/// Record one delivered automatic recap part.
pub async fn create_auto_recap_message(
    db: &Database,
    chat_id: i64,
    message_id: i64,
    text: &str,
    is_pinned: bool,
) -> Result<()> {
    // One instant for both stamps: ent seeds each with the same `DefaultFunc`
    // during a create, and nothing ever advances `updated_at` afterwards.
    let now = codec::now_unix_millis();
    let created = SentMessage {
        id: codec::new_identifier(),
        chat_id,
        message_id,
        text: text.to_owned(),
        is_pinned,
        from_platform: FROM_PLATFORM_TELEGRAM,
        message_type: MESSAGE_TYPE_AUTO_RECAP,
        created_at: now,
        updated_at: now,
    };

    sqlx::query(insert_statement(db.backend))
        .bind(&created.id)
        .bind(created.chat_id)
        .bind(created.message_id)
        .bind(&created.text)
        .bind(created.is_pinned)
        .bind(created.from_platform)
        .bind(created.message_type)
        .bind(created.created_at)
        .bind(created.updated_at)
        .execute(&db.pool)
        .await?;

    Ok(())
}

/// The newest pinned message of a chat.
///
/// Go's `First` reports a not-found error when nothing is pinned, and callers
/// branch on that error rather than on an empty value. `fetch_one` reproduces it
/// as [`sqlx::Error::RowNotFound`] travelling through `anyhow`.
pub async fn find_latest_pinned(db: &Database, chat_id: i64) -> Result<SentMessage> {
    let row = sqlx::query(&find_latest_pinned_query())
        .bind(chat_id)
        .bind(true)
        .fetch_one(&db.pool)
        .await?;
    decode(&row)
}

/// Set the pinned flag on every row matching the chat and message pair.
///
/// The table carries no uniqueness, so a duplicated pair is updated in full.
pub async fn set_pinned(
    db: &Database,
    chat_id: i64,
    message_id: i64,
    is_pinned: bool,
) -> Result<()> {
    // `updated_at` is untouched: ent declares it with `DefaultFunc` only, never
    // `UpdateDefault`, and no Go caller sets it on an update.
    sqlx::query("UPDATE sent_messages SET is_pinned = $1 WHERE chat_id = $2 AND message_id = $3")
        .bind(is_pinned)
        .bind(chat_id)
        .bind(message_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INSERT_POSTGRES, INSERT_SQLITE, find_latest_pinned_query, insert_statement};
    use crate::db::DbBackend;

    fn placeholder_count(sql: &str) -> usize {
        (1..=32)
            .take_while(|index| sql.contains(&format!("${index}")))
            .count()
    }

    #[test]
    fn the_postgres_insert_casts_the_generated_identifier_to_uuid() {
        let sql = insert_statement(DbBackend::Postgres);
        assert_eq!(sql, INSERT_POSTGRES);
        assert!(
            sql.contains("VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8, $9)"),
            "the PostgreSQL id column is UUID: {sql}"
        );
        assert_eq!(placeholder_count(sql), 9);
    }

    #[test]
    fn the_sqlite_insert_binds_the_identifier_directly() {
        let sql = insert_statement(DbBackend::Sqlite);
        assert_eq!(sql, INSERT_SQLITE);
        assert!(!sql.contains("UUID"), "the SQLite id column is TEXT: {sql}");
        assert!(sql.contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)"));
        assert_eq!(placeholder_count(sql), 9);
    }

    #[test]
    fn both_inserts_name_the_same_columns_in_the_same_order() {
        const COLUMNS: &str = "(id, chat_id, message_id, text, is_pinned, from_platform, message_type,\n         created_at, updated_at)";
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(sql.contains(COLUMNS), "column order drifted: {sql}");
        }
    }

    #[test]
    fn the_latest_pinned_lookup_orders_by_created_at_descending() {
        let query = find_latest_pinned_query();
        let upper = query.to_ascii_uppercase();
        assert!(
            upper.contains("ORDER BY CREATED_AT DESC"),
            "Go orders the pinned lookup: {query}"
        );
        assert!(upper.contains("LIMIT 1"), "Go takes only the first row");
        assert!(upper.contains("IS_PINNED = $2"));
        assert_eq!(placeholder_count(&query), 2);
    }
}
