//! `telegram_chat_feature_flags` repository.
//!
//! Ported from Go v1.0.0 `internal/models/tgchats/feature_flags.go`. Only the
//! group and supergroup chat types are eligible; every setter is a silent no-op
//! for anything else, exactly as Go returns `nil` without touching the database.

use anyhow::Result;
use sqlx::any::AnyRow;

use crate::db::{
    Database, DbBackend, codec,
    models::{
        CHAT_TYPE_SUPERGROUP, DEFAULT_FEATURE_LANGUAGE, RECAP_ELIGIBLE_CHAT_TYPES,
        TelegramChatFeatureFlags,
    },
};

/// Every column, cast to text so the `Any` driver cannot narrow or refuse it.
const SELECT_COLUMNS: &str = "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT),
        CAST(chat_type AS TEXT), CAST(chat_title AS TEXT),
        CAST(feature_chat_histories_recap AS TEXT), CAST(feature_language AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM telegram_chat_feature_flags";

/// PostgreSQL keeps `id` as `UUID`, so the generated text needs a cast.
const INSERT_POSTGRES: &str = "INSERT INTO telegram_chat_feature_flags
        (id, chat_id, chat_type, chat_title, feature_chat_histories_recap,
         feature_language, created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8)";

/// SQLite keeps `id` as `TEXT`, so the generated text binds directly.
const INSERT_SQLITE: &str = "INSERT INTO telegram_chat_feature_flags
        (id, chat_id, chat_type, chat_title, feature_chat_histories_recap,
         feature_language, created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)";

fn insert_statement(backend: DbBackend) -> &'static str {
    match backend {
        DbBackend::Postgres => INSERT_POSTGRES,
        DbBackend::Sqlite => INSERT_SQLITE,
    }
}

/// The complete statement [`list_recap_enabled_groups`] executes.
///
/// The whole query lives here rather than being assembled at the call site, so
/// the unit test below inspects the exact text that reaches the engine. A
/// clause appended to a suffix could otherwise slip past it.
fn list_recap_enabled_groups_query() -> String {
    format!(
        "{SELECT_COLUMNS} WHERE chat_type IN ('group', 'supergroup')
             AND feature_chat_histories_recap = $1"
    )
}

fn decode(row: &AnyRow) -> Result<TelegramChatFeatureFlags> {
    Ok(TelegramChatFeatureFlags {
        id: codec::text_at(row, 0)?,
        chat_id: codec::i64_at(row, 1)?,
        chat_type: codec::text_at(row, 2)?,
        chat_title: codec::text_at(row, 3)?,
        feature_chat_histories_recap: codec::bool_at(row, 4)?,
        feature_language: codec::text_at(row, 5)?,
        created_at: codec::i64_at(row, 6)?,
        updated_at: codec::i64_at(row, 7)?,
    })
}

/// Whether the recap feature is offered for this Telegram chat type.
pub fn is_eligible_chat_type(chat_type: &str) -> bool {
    RECAP_ELIGIBLE_CHAT_TYPES.contains(&chat_type)
}

/// Find the eligible row for `chat_id`, repairing an empty stored title.
///
/// The repair is written to storage but is deliberately not reflected in the
/// returned model. Go builds the update from the queried entity, discards the
/// refreshed node that `Save` returns, and hands back the original, so the
/// caller of the repairing call still sees the empty title. Any later read
/// observes the persisted value.
pub async fn find_one_for_groups(
    db: &Database,
    chat_id: i64,
    chat_title: &str,
) -> Result<Option<TelegramChatFeatureFlags>> {
    let query = format!(
        "{SELECT_COLUMNS} WHERE chat_id = $1 AND chat_type IN ('group', 'supergroup') LIMIT 1"
    );
    let Some(row) = sqlx::query(&query)
        .bind(chat_id)
        .fetch_optional(&db.pool)
        .await?
    else {
        return Ok(None);
    };

    let flags = decode(&row)?;
    if flags.chat_title.is_empty() && !chat_title.is_empty() {
        // `updated_at` is untouched: ent declares it with `DefaultFunc` only,
        // never `UpdateDefault`, and no Go caller sets it on an update.
        sqlx::query("UPDATE telegram_chat_feature_flags SET chat_title = $1 WHERE chat_id = $2")
            .bind(chat_title)
            .bind(chat_id)
            .execute(&db.pool)
            .await?;
    }

    Ok(Some(flags))
}

/// Find the eligible row, creating a disabled English row when absent.
///
/// Callers are responsible for the eligibility check; the public setters below
/// perform it before reaching here.
pub async fn find_or_create_for_groups(
    db: &Database,
    chat_id: i64,
    chat_type: &str,
    chat_title: &str,
) -> Result<TelegramChatFeatureFlags> {
    if let Some(existing) = find_one_for_groups(db, chat_id, chat_title).await? {
        return Ok(existing);
    }
    insert(
        db,
        chat_id,
        chat_type,
        chat_title,
        false,
        DEFAULT_FEATURE_LANGUAGE,
    )
    .await
}

async fn insert(
    db: &Database,
    chat_id: i64,
    chat_type: &str,
    chat_title: &str,
    recap_enabled: bool,
    language: &str,
) -> Result<TelegramChatFeatureFlags> {
    // One instant for both stamps: ent seeds each with the same `DefaultFunc`
    // during a create, and nothing ever advances `updated_at` afterwards.
    let now = codec::now_unix_millis();
    let created = TelegramChatFeatureFlags {
        id: codec::new_identifier(),
        chat_id,
        chat_type: chat_type.to_owned(),
        chat_title: chat_title.to_owned(),
        feature_chat_histories_recap: recap_enabled,
        feature_language: language.to_owned(),
        created_at: now,
        updated_at: now,
    };

    sqlx::query(insert_statement(db.backend))
        .bind(&created.id)
        .bind(created.chat_id)
        .bind(&created.chat_type)
        .bind(&created.chat_title)
        .bind(created.feature_chat_histories_recap)
        .bind(&created.feature_language)
        .bind(created.created_at)
        .bind(created.updated_at)
        .execute(&db.pool)
        .await?;

    Ok(created)
}

/// Whether chat-histories recap is switched on. Absent rows read as disabled.
pub async fn has_recap_enabled(db: &Database, chat_id: i64, chat_title: &str) -> Result<bool> {
    Ok(find_one_for_groups(db, chat_id, chat_title)
        .await?
        .is_some_and(|flags| flags.feature_chat_histories_recap))
}

/// Whether the bot has ever recorded this group before.
pub async fn has_joined_before(db: &Database, chat_id: i64, chat_title: &str) -> Result<bool> {
    Ok(find_one_for_groups(db, chat_id, chat_title)
        .await?
        .is_some())
}

/// The stored language, or `en` when no row exists.
pub async fn find_language(db: &Database, chat_id: i64, chat_title: &str) -> Result<String> {
    Ok(find_one_for_groups(db, chat_id, chat_title)
        .await?
        .map_or_else(
            || DEFAULT_FEATURE_LANGUAGE.to_owned(),
            |flags| flags.feature_language,
        ))
}

/// Store `language`, creating an eligible row first when needed.
pub async fn set_language(
    db: &Database,
    chat_id: i64,
    chat_type: &str,
    chat_title: &str,
    language: &str,
) -> Result<()> {
    if !is_eligible_chat_type(chat_type) {
        return Ok(());
    }

    let flags = find_or_create_for_groups(db, chat_id, chat_type, chat_title).await?;
    if flags.feature_language == language {
        return Ok(());
    }

    sqlx::query("UPDATE telegram_chat_feature_flags SET feature_language = $1 WHERE chat_id = $2")
        .bind(language)
        .bind(chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Switch chat-histories recap on, creating an English row first when needed.
pub async fn enable_recap(
    db: &Database,
    chat_id: i64,
    chat_type: &str,
    chat_title: &str,
) -> Result<()> {
    if !is_eligible_chat_type(chat_type) {
        return Ok(());
    }

    let flags = find_or_create_for_groups(db, chat_id, chat_type, chat_title).await?;
    if flags.feature_chat_histories_recap {
        return Ok(());
    }
    set_recap_flag(db, chat_id, true).await
}

/// Switch chat-histories recap off.
///
/// A missing row is created disabled and, unlike the find-or-create path, with
/// no language seeded. That asymmetry is Go's and is reproduced deliberately.
pub async fn disable_recap(
    db: &Database,
    chat_id: i64,
    chat_type: &str,
    chat_title: &str,
) -> Result<()> {
    if !is_eligible_chat_type(chat_type) {
        return Ok(());
    }

    let Some(flags) = find_one_for_groups(db, chat_id, chat_title).await? else {
        insert(db, chat_id, chat_type, chat_title, false, "").await?;
        return Ok(());
    };
    if !flags.feature_chat_histories_recap {
        return Ok(());
    }
    set_recap_flag(db, chat_id, false).await
}

async fn set_recap_flag(db: &Database, chat_id: i64, enabled: bool) -> Result<()> {
    sqlx::query(
        "UPDATE telegram_chat_feature_flags
         SET feature_chat_histories_recap = $1
         WHERE chat_id = $2",
    )
    .bind(enabled)
    .bind(chat_id)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// Every eligible group with recap enabled, in whatever order the engine
/// returns. Go adds no `ORDER BY` and no deduplication.
pub async fn list_recap_enabled_groups(db: &Database) -> Result<Vec<TelegramChatFeatureFlags>> {
    let rows = sqlx::query(&list_recap_enabled_groups_query())
        .bind(true)
        .fetch_all(&db.pool)
        .await?;
    rows.iter().map(decode).collect()
}

/// Remove every feature-flag row for a chat.
pub async fn delete_by_chat_id(db: &Database, chat_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM telegram_chat_feature_flags WHERE chat_id = $1")
        .bind(chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Move every feature-flag row onto the new chat identifier.
///
/// Go also rewrites the chat type, because only a supergroup upgrade triggers
/// this path.
pub async fn migrate_chat_id(db: &Database, from_chat_id: i64, to_chat_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE telegram_chat_feature_flags SET chat_id = $1, chat_type = $2 WHERE chat_id = $3",
    )
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
        INSERT_POSTGRES, INSERT_SQLITE, insert_statement, list_recap_enabled_groups_query,
    };
    use crate::db::DbBackend;

    /// The highest `$n` placeholder a static statement uses.
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
            sql.contains("VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8)"),
            "the PostgreSQL id column is UUID: {sql}"
        );
        assert_eq!(placeholder_count(sql), 8);
    }

    #[test]
    fn the_sqlite_insert_binds_the_identifier_directly() {
        let sql = insert_statement(DbBackend::Sqlite);
        assert_eq!(sql, INSERT_SQLITE);
        assert!(!sql.contains("UUID"), "the SQLite id column is TEXT: {sql}");
        assert!(sql.contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8)"));
        assert_eq!(placeholder_count(sql), 8);
    }

    #[test]
    fn both_inserts_name_the_same_columns_in_the_same_order() {
        const COLUMNS: &str = "(id, chat_id, chat_type, chat_title, feature_chat_histories_recap,\n         feature_language, created_at, updated_at)";
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(sql.contains(COLUMNS), "column order drifted: {sql}");
        }
    }

    #[test]
    fn the_executed_list_query_adds_no_ordering_deduplication_or_limit() {
        // The whole statement, exactly as it reaches the engine.
        let query = list_recap_enabled_groups_query();
        let upper = query.to_ascii_uppercase();

        for forbidden in ["ORDER BY", "GROUP BY", "DISTINCT", "LIMIT"] {
            assert!(
                !upper.contains(forbidden),
                "Go's list adds no {forbidden}: {query}"
            );
        }
        assert!(upper.contains("WHERE CHAT_TYPE IN ('GROUP', 'SUPERGROUP')"));
        assert!(upper.contains("FEATURE_CHAT_HISTORIES_RECAP = $1"));
    }
}
