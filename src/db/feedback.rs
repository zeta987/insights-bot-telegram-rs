//! Feedback reaction repositories for both physical tables.
//!
//! Ported from Go v1.0.0 `internal/models/chathistories/feedbacks.go` and
//! `internal/models/smr/feedbacks.go`, which carry byte-identical algorithms
//! against two separate tables. `/smr` generation stays out of scope, but the
//! `smr/summarization/feedback/react` compatibility callback does not, so the
//! summarization table keeps its own repository.
//!
//! Every entry point takes a parsed [`Uuid`], exactly as the Go methods take
//! `uuid.UUID` and the handlers parse before calling. An arbitrary string can
//! therefore never reach a statement.
//!
//! The react algorithm is deliberately non-transactional. Go issues three
//! independent statements, so a failed insert leaves the preceding deletion
//! committed and the user's previous reaction gone.

use anyhow::Result;
use sqlx::any::AnyRow;
use uuid::Uuid;

use crate::db::{
    Database, DbBackend, codec,
    models::{FeedbackReaction, ReactionCounts, ReactionType},
};

/// Which physical reaction table an operation addresses.
///
/// The enum exists so a table name can never be supplied by a caller: every
/// statement below is a static literal chosen by this selector.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactionTable {
    /// Reactions on a chat-histories recap.
    ChatHistoriesRecaps,
    /// Reactions on a summarization, kept for callback compatibility.
    Summarizations,
}

/// The static statements one reaction table needs on one backend.
///
/// `log_id` is a `UUID` column on PostgreSQL and `TEXT` on SQLite, so the
/// comparison is cast on the parameter side rather than on the column. Casting
/// the column would discard its type and its index.
struct Statements {
    select_by_chat_and_log: &'static str,
    select_one_typed: &'static str,
    delete_typed: &'static str,
    delete_every_type: &'static str,
    insert: &'static str,
}

const RECAPS_POSTGRES: Statements = Statements {
    select_by_chat_and_log: "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT), CAST(log_id AS TEXT),
        CAST(user_id AS TEXT), CAST(\"type\" AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID)",
    select_one_typed: "SELECT CAST(id AS TEXT)
     FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID) AND user_id = $3 AND \"type\" = $4
     LIMIT 1",
    delete_typed: "DELETE FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID) AND user_id = $3 AND \"type\" = $4",
    delete_every_type: "DELETE FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID) AND user_id = $3",
    insert: "INSERT INTO feedback_chat_histories_recaps_reactions
        (id, chat_id, log_id, user_id, \"type\", created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, CAST($3 AS UUID), $4, $5, $6, $7)",
};

const RECAPS_SQLITE: Statements = Statements {
    select_by_chat_and_log: "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT), CAST(log_id AS TEXT),
        CAST(user_id AS TEXT), CAST(\"type\" AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = $2",
    select_one_typed: "SELECT CAST(id AS TEXT)
     FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = $2 AND user_id = $3 AND \"type\" = $4
     LIMIT 1",
    delete_typed: "DELETE FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = $2 AND user_id = $3 AND \"type\" = $4",
    delete_every_type: "DELETE FROM feedback_chat_histories_recaps_reactions
     WHERE chat_id = $1 AND log_id = $2 AND user_id = $3",
    insert: "INSERT INTO feedback_chat_histories_recaps_reactions
        (id, chat_id, log_id, user_id, \"type\", created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7)",
};

const SUMMARIZATIONS_POSTGRES: Statements = Statements {
    select_by_chat_and_log: "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT), CAST(log_id AS TEXT),
        CAST(user_id AS TEXT), CAST(\"type\" AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID)",
    select_one_typed: "SELECT CAST(id AS TEXT)
     FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID) AND user_id = $3 AND \"type\" = $4
     LIMIT 1",
    delete_typed: "DELETE FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID) AND user_id = $3 AND \"type\" = $4",
    delete_every_type: "DELETE FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = CAST($2 AS UUID) AND user_id = $3",
    insert: "INSERT INTO feedback_summarizations_reactions
        (id, chat_id, log_id, user_id, \"type\", created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, CAST($3 AS UUID), $4, $5, $6, $7)",
};

const SUMMARIZATIONS_SQLITE: Statements = Statements {
    select_by_chat_and_log: "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT), CAST(log_id AS TEXT),
        CAST(user_id AS TEXT), CAST(\"type\" AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = $2",
    select_one_typed: "SELECT CAST(id AS TEXT)
     FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = $2 AND user_id = $3 AND \"type\" = $4
     LIMIT 1",
    delete_typed: "DELETE FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = $2 AND user_id = $3 AND \"type\" = $4",
    delete_every_type: "DELETE FROM feedback_summarizations_reactions
     WHERE chat_id = $1 AND log_id = $2 AND user_id = $3",
    insert: "INSERT INTO feedback_summarizations_reactions
        (id, chat_id, log_id, user_id, \"type\", created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7)",
};

impl ReactionTable {
    fn statements(self, backend: DbBackend) -> &'static Statements {
        match (self, backend) {
            (Self::ChatHistoriesRecaps, DbBackend::Postgres) => &RECAPS_POSTGRES,
            (Self::ChatHistoriesRecaps, DbBackend::Sqlite) => &RECAPS_SQLITE,
            (Self::Summarizations, DbBackend::Postgres) => &SUMMARIZATIONS_POSTGRES,
            (Self::Summarizations, DbBackend::Sqlite) => &SUMMARIZATIONS_SQLITE,
        }
    }
}

fn decode(row: &AnyRow) -> Result<FeedbackReaction> {
    Ok(FeedbackReaction {
        id: codec::text_at(row, 0)?,
        chat_id: codec::i64_at(row, 1)?,
        log_id: codec::text_at(row, 2)?,
        user_id: codec::i64_at(row, 3)?,
        reaction_type: codec::text_at(row, 4)?,
        created_at: codec::i64_at(row, 5)?,
        updated_at: codec::i64_at(row, 6)?,
    })
}

/// Every physical reaction row for a chat and log.
///
/// Duplicates are returned as stored and nothing is sorted, matching Go's
/// unordered `All`. Only [`counts`] consumes this; Go exposes no listing.
async fn list(
    db: &Database,
    table: ReactionTable,
    chat_id: i64,
    log_id: Uuid,
) -> Result<Vec<FeedbackReaction>> {
    let rows = sqlx::query(table.statements(db.backend).select_by_chat_and_log)
        .bind(chat_id)
        .bind(log_id.to_string())
        .fetch_all(&db.pool)
        .await?;
    rows.iter().map(decode).collect()
}

/// The three button counters for a chat and log.
///
/// Go loads every row and filters in memory, so each physical duplicate counts
/// and `none` belongs to no bucket.
pub async fn counts(
    db: &Database,
    table: ReactionTable,
    chat_id: i64,
    log_id: Uuid,
) -> Result<ReactionCounts> {
    let mut counts = ReactionCounts::default();
    for row in list(db, table, chat_id, log_id).await? {
        match row.reaction() {
            Some(ReactionType::UpVote) => counts.up_votes += 1,
            Some(ReactionType::DownVote) => counts.down_votes += 1,
            Some(ReactionType::Lmao) => counts.lmao += 1,
            Some(ReactionType::None) | None => {}
        }
    }
    Ok(counts)
}

/// Whether this exact tuple already carries this exact reaction.
///
/// Only the summarization side has this in Go; the chat-histories repository
/// exposes no equivalent, so none is offered here either.
pub async fn has_summarization_reacted(
    db: &Database,
    chat_id: i64,
    log_id: Uuid,
    user_id: i64,
    reaction: ReactionType,
) -> Result<bool> {
    let statements = ReactionTable::Summarizations.statements(db.backend);
    let existing = sqlx::query(statements.select_one_typed)
        .bind(chat_id)
        .bind(log_id.to_string())
        .bind(user_id)
        .bind(reaction.as_stored())
        .fetch_optional(&db.pool)
        .await?;
    Ok(existing.is_some())
}

/// Apply Go's toggle algorithm.
///
/// The three statements run independently, with no transaction around them, so
/// a failure at the insert leaves the preceding deletion committed. Reproducing
/// that is the point: wrapping the steps would change the observable outcome of
/// a partial failure.
pub async fn react(
    db: &Database,
    table: ReactionTable,
    chat_id: i64,
    log_id: Uuid,
    user_id: i64,
    reaction: ReactionType,
) -> Result<()> {
    let statements = table.statements(db.backend);

    let removed = sqlx::query(statements.delete_typed)
        .bind(chat_id)
        .bind(log_id.to_string())
        .bind(user_id)
        .bind(reaction.as_stored())
        .execute(&db.pool)
        .await?;
    if removed.rows_affected() > 0 {
        return Ok(());
    }

    sqlx::query(statements.delete_every_type)
        .bind(chat_id)
        .bind(log_id.to_string())
        .bind(user_id)
        .execute(&db.pool)
        .await?;

    insert_unchecked(db, table, chat_id, log_id, user_id, reaction).await
}

/// Insert one reaction row without the toggle algorithm.
///
/// This is the raw create Go's builder maps to. It stays private because Go
/// exposes no such entry point.
async fn insert_unchecked(
    db: &Database,
    table: ReactionTable,
    chat_id: i64,
    log_id: Uuid,
    user_id: i64,
    reaction: ReactionType,
) -> Result<()> {
    let now = codec::now_unix_millis();
    sqlx::query(table.statements(db.backend).insert)
        .bind(codec::new_identifier())
        .bind(chat_id)
        .bind(log_id.to_string())
        .bind(user_id)
        .bind(reaction.as_stored())
        .bind(now)
        .bind(now)
        .execute(&db.pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        RECAPS_POSTGRES, RECAPS_SQLITE, SUMMARIZATIONS_POSTGRES, SUMMARIZATIONS_SQLITE, Statements,
    };
    use crate::db::models::ReactionType;

    fn placeholder_count(sql: &str) -> usize {
        (1..=32)
            .take_while(|index| sql.contains(&format!("${index}")))
            .count()
    }

    fn every_statement(statements: &'static Statements) -> [&'static str; 5] {
        [
            statements.select_by_chat_and_log,
            statements.select_one_typed,
            statements.delete_typed,
            statements.delete_every_type,
            statements.insert,
        ]
    }

    #[test]
    fn the_postgres_inserts_cast_both_identifiers_to_uuid() {
        for statements in [&RECAPS_POSTGRES, &SUMMARIZATIONS_POSTGRES] {
            let sql = statements.insert;
            assert!(
                sql.contains("VALUES (CAST($1 AS UUID), $2, CAST($3 AS UUID), $4, $5, $6, $7)"),
                "both id and log_id are UUID on PostgreSQL: {sql}"
            );
            assert_eq!(placeholder_count(sql), 7);
        }
    }

    #[test]
    fn the_sqlite_inserts_bind_both_identifiers_directly() {
        for statements in [&RECAPS_SQLITE, &SUMMARIZATIONS_SQLITE] {
            let sql = statements.insert;
            assert!(!sql.contains("UUID"), "both columns are TEXT: {sql}");
            assert!(sql.contains("VALUES ($1, $2, $3, $4, $5, $6, $7)"));
            assert_eq!(placeholder_count(sql), 7);
        }
    }

    #[test]
    fn postgres_compares_log_id_by_casting_the_parameter_not_the_column() {
        for statements in [&RECAPS_POSTGRES, &SUMMARIZATIONS_POSTGRES] {
            for sql in every_statement(statements) {
                // Selecting the column as text is how it is decoded; comparing
                // it as text would discard its type and its index.
                assert!(
                    !sql.contains("CAST(log_id AS TEXT) ="),
                    "the comparison must not cast the column: {sql}"
                );
            }
            for sql in [
                statements.select_by_chat_and_log,
                statements.select_one_typed,
                statements.delete_typed,
                statements.delete_every_type,
            ] {
                assert!(
                    sql.contains("log_id = CAST($2 AS UUID)"),
                    "the parameter carries the cast: {sql}"
                );
            }
        }
    }

    #[test]
    fn sqlite_compares_log_id_as_the_bound_text() {
        for statements in [&RECAPS_SQLITE, &SUMMARIZATIONS_SQLITE] {
            for sql in every_statement(statements) {
                assert!(!sql.contains("UUID"), "SQLite has no UUID type: {sql}");
            }
            for sql in [
                statements.select_by_chat_and_log,
                statements.select_one_typed,
                statements.delete_typed,
                statements.delete_every_type,
            ] {
                assert!(sql.contains("log_id = $2"), "a plain comparison: {sql}");
            }
        }
    }

    #[test]
    fn every_statement_names_its_own_table_and_nothing_else() {
        for (statements, own, other) in [
            (
                &RECAPS_POSTGRES,
                "feedback_chat_histories_recaps_reactions",
                "feedback_summarizations_reactions",
            ),
            (
                &RECAPS_SQLITE,
                "feedback_chat_histories_recaps_reactions",
                "feedback_summarizations_reactions",
            ),
            (
                &SUMMARIZATIONS_POSTGRES,
                "feedback_summarizations_reactions",
                "feedback_chat_histories_recaps_reactions",
            ),
            (
                &SUMMARIZATIONS_SQLITE,
                "feedback_summarizations_reactions",
                "feedback_chat_histories_recaps_reactions",
            ),
        ] {
            for sql in every_statement(statements) {
                assert!(sql.contains(own), "{sql} must address {own}");
                assert!(!sql.contains(other), "{sql} must not reach {other}");
            }
        }
    }

    #[test]
    fn both_inserts_name_the_same_columns_in_the_same_order() {
        const COLUMNS: &str = "(id, chat_id, log_id, user_id, \"type\", created_at, updated_at)";
        for statements in [
            &RECAPS_POSTGRES,
            &RECAPS_SQLITE,
            &SUMMARIZATIONS_POSTGRES,
            &SUMMARIZATIONS_SQLITE,
        ] {
            assert!(
                statements.insert.contains(COLUMNS),
                "column order drifted: {}",
                statements.insert
            );
        }
    }

    #[test]
    fn the_counting_read_adds_no_ordering_deduplication_or_limit() {
        for statements in [
            &RECAPS_POSTGRES,
            &RECAPS_SQLITE,
            &SUMMARIZATIONS_POSTGRES,
            &SUMMARIZATIONS_SQLITE,
        ] {
            let upper = statements.select_by_chat_and_log.to_ascii_uppercase();
            for forbidden in ["ORDER BY", "GROUP BY", "DISTINCT", "LIMIT"] {
                assert!(
                    !upper.contains(forbidden),
                    "counting every duplicate needs no {forbidden}"
                );
            }
        }
    }

    #[test]
    fn the_reaction_vocabulary_is_exactly_the_four_go_values() {
        assert_eq!(ReactionType::None.as_stored(), "none");
        assert_eq!(ReactionType::UpVote.as_stored(), "up_vote");
        assert_eq!(ReactionType::DownVote.as_stored(), "down_vote");
        assert_eq!(ReactionType::Lmao.as_stored(), "lmao");
        for unknown in ["", "None", "upvote", "up-vote", "rofl"] {
            assert_eq!(ReactionType::from_stored(unknown), None);
        }
    }
}
