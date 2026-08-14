//! `log_chat_histories_recaps` repository.
//!
//! Ported from Go v1.0.0: the group create in
//! `internal/models/chathistories/chat_histories.go`, the forwarded create in
//! `internal/models/chathistories/private_forwarded.go`, and the prune and
//! migrate in `internal/models/logs/logs.go`.
//!
//! The two create shapes differ in one column. The group path calls
//! `SetModelName(modelName)` with the model its caller resolved; the forwarded
//! path never calls it at all, so the row keeps the schema default of an empty
//! string. That asymmetry is Go's and is reproduced deliberately.

use anyhow::Result;

use crate::db::{
    Database, DbBackend, codec,
    models::{
        FROM_PLATFORM_TELEGRAM, LogChatHistoriesRecap, RECAP_TYPE_FOR_GROUP,
        RECAP_TYPE_FOR_PRIVATE_FORWARDED, TokenUsage,
    },
};

/// PostgreSQL keeps `id` as `UUID`, so the generated text needs a cast.
const INSERT_POSTGRES: &str = "INSERT INTO log_chat_histories_recaps
        (id, chat_id, recap_inputs, recap_outputs, from_platform, prompt_token_usage,
         completion_token_usage, total_token_usage, recap_type, model_name,
         created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)";

/// SQLite keeps `id` as `TEXT`, so the generated text binds directly.
const INSERT_SQLITE: &str = "INSERT INTO log_chat_histories_recaps
        (id, chat_id, recap_inputs, recap_outputs, from_platform, prompt_token_usage,
         completion_token_usage, total_token_usage, recap_type, model_name,
         created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)";

fn insert_statement(backend: DbBackend) -> &'static str {
    match backend {
        DbBackend::Postgres => INSERT_POSTGRES,
        DbBackend::Sqlite => INSERT_SQLITE,
    }
}

/// Persist a group recap log, carrying the model its caller resolved.
///
/// Go's group create uses `Save` and hands the caller `saved.ID`, which becomes
/// the `logID` a feedback button carries. Nothing else from the row is read.
pub async fn create_group_recap(
    db: &Database,
    chat_id: i64,
    recap_inputs: &str,
    recap_outputs: &str,
    usage: TokenUsage,
    model_name: &str,
) -> Result<String> {
    let created = insert(
        db,
        chat_id,
        recap_inputs,
        recap_outputs,
        usage,
        RECAP_TYPE_FOR_GROUP,
        model_name,
    )
    .await?;
    Ok(created.id)
}

/// Persist a private-forwarded recap log.
///
/// Go's forwarded create uses `Exec` and never calls `SetModelName`, so no
/// identifier is returned and `model_name` stays at the schema default of an
/// empty string.
pub async fn create_private_forwarded_recap(
    db: &Database,
    chat_id: i64,
    recap_inputs: &str,
    recap_outputs: &str,
    usage: TokenUsage,
) -> Result<()> {
    insert(
        db,
        chat_id,
        recap_inputs,
        recap_outputs,
        usage,
        RECAP_TYPE_FOR_PRIVATE_FORWARDED,
        "",
    )
    .await?;
    Ok(())
}

async fn insert(
    db: &Database,
    chat_id: i64,
    recap_inputs: &str,
    recap_outputs: &str,
    usage: TokenUsage,
    recap_type: i64,
    model_name: &str,
) -> Result<LogChatHistoriesRecap> {
    // One instant for both stamps: ent seeds each with the same `DefaultFunc`
    // during a create, and nothing ever advances `updated_at` afterwards.
    let now = codec::now_unix_millis();
    let created = LogChatHistoriesRecap {
        id: codec::new_identifier(),
        chat_id,
        recap_inputs: recap_inputs.to_owned(),
        recap_outputs: recap_outputs.to_owned(),
        from_platform: FROM_PLATFORM_TELEGRAM,
        prompt_token_usage: usage.prompt_tokens,
        completion_token_usage: usage.completion_tokens,
        total_token_usage: usage.total_tokens,
        recap_type,
        model_name: model_name.to_owned(),
        created_at: now,
        updated_at: now,
    };

    sqlx::query(insert_statement(db.backend))
        .bind(&created.id)
        .bind(created.chat_id)
        .bind(&created.recap_inputs)
        .bind(&created.recap_outputs)
        .bind(created.from_platform)
        .bind(created.prompt_token_usage)
        .bind(created.completion_token_usage)
        .bind(created.total_token_usage)
        .bind(created.recap_type)
        .bind(&created.model_name)
        .bind(created.created_at)
        .bind(created.updated_at)
        .execute(&db.pool)
        .await?;

    Ok(created)
}

/// Blank the recap text of every log row of a chat, used on bot-left cleanup.
///
/// Only `recap_inputs` and `recap_outputs` are cleared. The rows, their
/// identifiers, their token counters, and their timestamps all survive, so a
/// feedback button minted earlier keeps resolving.
pub async fn prune_content_by_chat_id(db: &Database, chat_id: i64) -> Result<()> {
    sqlx::query(
        "UPDATE log_chat_histories_recaps SET recap_inputs = $1, recap_outputs = $2
         WHERE chat_id = $3",
    )
    .bind("")
    .bind("")
    .bind(chat_id)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// Move every log row onto the new chat identifier.
pub async fn migrate_chat_id(db: &Database, from_chat_id: i64, to_chat_id: i64) -> Result<()> {
    sqlx::query("UPDATE log_chat_histories_recaps SET chat_id = $1 WHERE chat_id = $2")
        .bind(to_chat_id)
        .bind(from_chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{INSERT_POSTGRES, INSERT_SQLITE, insert_statement};
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
            sql.contains(
                "VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"
            ),
            "the PostgreSQL id column is UUID: {sql}"
        );
        assert_eq!(placeholder_count(sql), 12);
    }

    #[test]
    fn the_sqlite_insert_binds_the_identifier_directly() {
        let sql = insert_statement(DbBackend::Sqlite);
        assert_eq!(sql, INSERT_SQLITE);
        assert!(!sql.contains("UUID"), "the SQLite id column is TEXT: {sql}");
        assert!(sql.contains("VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)"));
        assert_eq!(placeholder_count(sql), 12);
    }

    #[test]
    fn both_inserts_name_the_same_columns_in_the_same_order() {
        const COLUMNS: &str = "(id, chat_id, recap_inputs, recap_outputs, from_platform, prompt_token_usage,\n         completion_token_usage, total_token_usage, recap_type, model_name,\n         created_at, updated_at)";
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(sql.contains(COLUMNS), "column order drifted: {sql}");
        }
    }
}
