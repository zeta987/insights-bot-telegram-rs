//! `metric_open_ai_chat_completion_token_usages` repository.
//!
//! Ported from Go v1.0.0 `internal/thirdparty/openai/openai.go`, where every
//! metric create sets the operation, the three token counters, and the model
//! name. The two character-length columns are never set, so they stay at the
//! schema default of zero, and the table has no update path at all.

use anyhow::Result;

use crate::db::{
    Database, DbBackend, codec,
    models::{MetricOpenAiChatCompletionTokenUsage, TokenUsage},
};

const INSERT_POSTGRES: &str = "INSERT INTO metric_open_ai_chat_completion_token_usages
        (id, prompt_operation, prompt_character_length, prompt_token_usage,
         completion_character_length, completion_token_usage, total_token_usage,
         model_name, created_at)
     VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8, $9)";

const INSERT_SQLITE: &str = "INSERT INTO metric_open_ai_chat_completion_token_usages
        (id, prompt_operation, prompt_character_length, prompt_token_usage,
         completion_character_length, completion_token_usage, total_token_usage,
         model_name, created_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)";

fn insert_statement(backend: DbBackend) -> &'static str {
    match backend {
        DbBackend::Postgres => INSERT_POSTGRES,
        DbBackend::Sqlite => INSERT_SQLITE,
    }
}

/// Record one chat-completion token usage sample.
///
/// The character-length columns keep their schema default of zero because Go's
/// create never sets them.
pub async fn create(
    db: &Database,
    prompt_operation: &str,
    usage: TokenUsage,
    model_name: &str,
) -> Result<()> {
    let created = MetricOpenAiChatCompletionTokenUsage {
        id: codec::new_identifier(),
        prompt_operation: prompt_operation.to_owned(),
        prompt_character_length: 0,
        prompt_token_usage: usage.prompt_tokens,
        completion_character_length: 0,
        completion_token_usage: usage.completion_tokens,
        total_token_usage: usage.total_tokens,
        model_name: model_name.to_owned(),
        created_at: codec::now_unix_millis(),
    };

    sqlx::query(insert_statement(db.backend))
        .bind(&created.id)
        .bind(&created.prompt_operation)
        .bind(created.prompt_character_length)
        .bind(created.prompt_token_usage)
        .bind(created.completion_character_length)
        .bind(created.completion_token_usage)
        .bind(created.total_token_usage)
        .bind(&created.model_name)
        .bind(created.created_at)
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
        const COLUMNS: &str = "(id, prompt_operation, prompt_character_length, prompt_token_usage,\n         completion_character_length, completion_token_usage, total_token_usage,\n         model_name, created_at)";
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(sql.contains(COLUMNS), "column order drifted: {sql}");
        }
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(
                !sql.contains("updated_at"),
                "the metric table is append-only: {sql}"
            );
        }
    }
}
