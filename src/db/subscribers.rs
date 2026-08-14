//! `telegram_chat_auto_recaps_subscribers` repository.
//!
//! Ported from Go v1.0.0 `internal/models/tgchats/auto_recaps_subscribers.go`.
//! The table carries no uniqueness and Go opens no transaction, so subscribing
//! is only sequentially idempotent: a `LIMIT 1` precheck absorbs repeats from one
//! caller, while two concurrent callers can both insert. Those physical
//! duplicates are preserved rather than papered over, and unsubscribing removes
//! exactly one row.

use anyhow::Result;
use sqlx::any::AnyRow;

use crate::db::{Database, DbBackend, codec, models::TelegramChatAutoRecapsSubscriber};

const SELECT_COLUMNS: &str = "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT),
        CAST(user_id AS TEXT), CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM telegram_chat_auto_recaps_subscribers";

const INSERT_POSTGRES: &str = "INSERT INTO telegram_chat_auto_recaps_subscribers
        (id, chat_id, user_id, created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, $3, $4, $5)";

const INSERT_SQLITE: &str = "INSERT INTO telegram_chat_auto_recaps_subscribers
        (id, chat_id, user_id, created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5)";

/// Delete one row by identifier.
///
/// Comparing the identifier as text keeps a single statement valid on both
/// engines, since PostgreSQL renders a UUID in the same canonical form the
/// repository generated.
const DELETE_ONE_BY_ID: &str =
    "DELETE FROM telegram_chat_auto_recaps_subscribers WHERE CAST(id AS TEXT) = $1";

fn insert_statement(backend: DbBackend) -> &'static str {
    match backend {
        DbBackend::Postgres => INSERT_POSTGRES,
        DbBackend::Sqlite => INSERT_SQLITE,
    }
}

/// The complete statement [`list`] executes.
///
/// The whole query lives here rather than being assembled at the call site, so
/// the unit test below inspects the exact text that reaches the engine. A clause
/// appended to a suffix could otherwise slip past it.
fn list_query() -> String {
    format!("{SELECT_COLUMNS} WHERE chat_id = $1")
}

fn decode(row: &AnyRow) -> Result<TelegramChatAutoRecapsSubscriber> {
    Ok(TelegramChatAutoRecapsSubscriber {
        id: codec::text_at(row, 0)?,
        chat_id: codec::i64_at(row, 1)?,
        user_id: codec::i64_at(row, 2)?,
        created_at: codec::i64_at(row, 3)?,
        updated_at: codec::i64_at(row, 4)?,
    })
}

/// One matching physical row, in whatever order the engine returns.
///
/// No `ORDER BY` is added, matching Go's unordered `First`.
pub async fn find_one(
    db: &Database,
    chat_id: i64,
    user_id: i64,
) -> Result<Option<TelegramChatAutoRecapsSubscriber>> {
    let query = format!("{SELECT_COLUMNS} WHERE chat_id = $1 AND user_id = $2 LIMIT 1");
    let Some(row) = sqlx::query(&query)
        .bind(chat_id)
        .bind(user_id)
        .fetch_optional(&db.pool)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(decode(&row)?))
}

/// Every physical subscriber row for a chat.
///
/// Duplicates are returned as they are stored, and nothing is sorted.
pub async fn list(db: &Database, chat_id: i64) -> Result<Vec<TelegramChatAutoRecapsSubscriber>> {
    let rows = sqlx::query(&list_query())
        .bind(chat_id)
        .fetch_all(&db.pool)
        .await?;
    rows.iter().map(decode).collect()
}

/// Subscribe, skipping the insert when a row already exists.
///
/// The precheck and the insert are separate statements with no transaction
/// between them, exactly as Go issues them.
pub async fn subscribe(db: &Database, chat_id: i64, user_id: i64) -> Result<()> {
    if find_one(db, chat_id, user_id).await?.is_some() {
        return Ok(());
    }
    insert_unchecked(db, chat_id, user_id).await
}

/// Insert a subscriber row without the precheck.
///
/// This is the raw create Go's builder maps to. It is exposed so the racing
/// path that produces physical duplicates stays reachable and testable.
pub async fn insert_unchecked(db: &Database, chat_id: i64, user_id: i64) -> Result<()> {
    let now = codec::now_unix_millis();
    sqlx::query(insert_statement(db.backend))
        .bind(codec::new_identifier())
        .bind(chat_id)
        .bind(user_id)
        .bind(now)
        .bind(now)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Remove exactly one matching physical row.
///
/// Duplicates created by a race therefore survive one call each, which is the
/// released behaviour.
pub async fn unsubscribe(db: &Database, chat_id: i64, user_id: i64) -> Result<()> {
    let Some(subscriber) = find_one(db, chat_id, user_id).await? else {
        return Ok(());
    };

    sqlx::query(DELETE_ONE_BY_ID)
        .bind(&subscriber.id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Remove every subscriber row for a chat.
pub async fn delete_all_by_chat_id(db: &Database, chat_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM telegram_chat_auto_recaps_subscribers WHERE chat_id = $1")
        .bind(chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Move every subscriber row onto the new chat identifier.
pub async fn migrate_chat_id(db: &Database, from_chat_id: i64, to_chat_id: i64) -> Result<()> {
    // `updated_at` is untouched: ent declares it with `DefaultFunc` only, never
    // `UpdateDefault`, and no Go caller sets it on an update.
    sqlx::query("UPDATE telegram_chat_auto_recaps_subscribers SET chat_id = $1 WHERE chat_id = $2")
        .bind(to_chat_id)
        .bind(from_chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{DELETE_ONE_BY_ID, INSERT_POSTGRES, INSERT_SQLITE, insert_statement, list_query};
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
            sql.contains("VALUES (CAST($1 AS UUID), $2, $3, $4, $5)"),
            "the PostgreSQL id column is UUID: {sql}"
        );
        assert_eq!(placeholder_count(sql), 5);
    }

    #[test]
    fn the_sqlite_insert_binds_the_identifier_directly() {
        let sql = insert_statement(DbBackend::Sqlite);
        assert_eq!(sql, INSERT_SQLITE);
        assert!(!sql.contains("UUID"), "the SQLite id column is TEXT: {sql}");
        assert!(sql.contains("VALUES ($1, $2, $3, $4, $5)"));
        assert_eq!(placeholder_count(sql), 5);
    }

    #[test]
    fn both_inserts_name_the_same_columns_in_the_same_order() {
        const COLUMNS: &str = "(id, chat_id, user_id, created_at, updated_at)";
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(sql.contains(COLUMNS), "column order drifted: {sql}");
        }
    }

    #[test]
    fn the_one_row_delete_matches_the_identifier_as_text_on_both_engines() {
        assert!(
            DELETE_ONE_BY_ID.contains("CAST(id AS TEXT) = $1"),
            "a UUID column must be compared as text: {DELETE_ONE_BY_ID}"
        );
        assert_eq!(placeholder_count(DELETE_ONE_BY_ID), 1);
        let upper = DELETE_ONE_BY_ID.to_ascii_uppercase();
        assert!(
            !upper.contains("LIMIT"),
            "PostgreSQL rejects DELETE ... LIMIT; the single row comes from the unique id"
        );
        assert!(
            !upper.contains("CHAT_ID") && !upper.contains("USER_ID"),
            "the identifier alone selects the row Go's DeleteOne removes"
        );
    }

    #[test]
    fn the_executed_list_query_adds_no_ordering_deduplication_or_limit() {
        // The whole statement, exactly as it reaches the engine. A LIMIT here
        // would silently drop physical duplicates Go returns.
        let query = list_query();
        let upper = query.to_ascii_uppercase();

        for forbidden in ["ORDER BY", "GROUP BY", "DISTINCT", "LIMIT"] {
            assert!(
                !upper.contains(forbidden),
                "Go's list adds no {forbidden}: {query}"
            );
        }
        assert!(upper.contains("WHERE CHAT_ID = $1"));
    }
}
