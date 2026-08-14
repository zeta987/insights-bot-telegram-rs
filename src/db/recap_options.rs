//! `telegram_chat_recaps_options` repository.
//!
//! Ported from Go v1.0.0 `internal/models/tgchats/recaps_options.go`. Two create
//! paths exist and they differ: find-or-create materialises public mode with a
//! daily rate of four, while the mode-only setter writes just the mode and
//! leaves every other column at its schema default, daily rate included.

use anyhow::Result;
use sqlx::any::AnyRow;

use crate::db::{
    Database, DbBackend, codec,
    models::{AutoRecapSendMode, TelegramChatRecapsOptions},
};

/// The daily rate the find-or-create path materialises.
const FIRST_ENABLE_RATES_PER_DAY: i64 = 4;

const SELECT_COLUMNS: &str = "SELECT CAST(id AS TEXT), CAST(chat_id AS TEXT),
        CAST(auto_recap_send_mode AS TEXT), CAST(manual_recap_rate_per_seconds AS TEXT),
        CAST(auto_recap_rates_per_day AS TEXT), CAST(pin_auto_recap_message AS TEXT),
        CAST(created_at AS TEXT), CAST(updated_at AS TEXT)
     FROM telegram_chat_recaps_options";

const INSERT_POSTGRES: &str = "INSERT INTO telegram_chat_recaps_options
        (id, chat_id, auto_recap_send_mode, manual_recap_rate_per_seconds,
         auto_recap_rates_per_day, pin_auto_recap_message, created_at, updated_at)
     VALUES (CAST($1 AS UUID), $2, $3, $4, $5, $6, $7, $8)";

const INSERT_SQLITE: &str = "INSERT INTO telegram_chat_recaps_options
        (id, chat_id, auto_recap_send_mode, manual_recap_rate_per_seconds,
         auto_recap_rates_per_day, pin_auto_recap_message, created_at, updated_at)
     VALUES ($1, $2, $3, $4, $5, $6, $7, $8)";

fn insert_statement(backend: DbBackend) -> &'static str {
    match backend {
        DbBackend::Postgres => INSERT_POSTGRES,
        DbBackend::Sqlite => INSERT_SQLITE,
    }
}

fn decode(row: &AnyRow) -> Result<TelegramChatRecapsOptions> {
    Ok(TelegramChatRecapsOptions {
        id: codec::text_at(row, 0)?,
        chat_id: codec::i64_at(row, 1)?,
        auto_recap_send_mode: codec::i64_at(row, 2)?,
        manual_recap_rate_per_seconds: codec::i64_at(row, 3)?,
        auto_recap_rates_per_day: codec::i64_at(row, 4)?,
        pin_auto_recap_message: codec::bool_at(row, 5)?,
        created_at: codec::i64_at(row, 6)?,
        updated_at: codec::i64_at(row, 7)?,
    })
}

/// The stored options for a chat, or `None` when none were ever written.
pub async fn find_one(db: &Database, chat_id: i64) -> Result<Option<TelegramChatRecapsOptions>> {
    let query = format!("{SELECT_COLUMNS} WHERE chat_id = $1 LIMIT 1");
    let Some(row) = sqlx::query(&query)
        .bind(chat_id)
        .fetch_optional(&db.pool)
        .await?
    else {
        return Ok(None);
    };
    Ok(Some(decode(&row)?))
}

/// The find-or-create path used when recap is first enabled.
///
/// A missing row is materialised as public mode with a daily rate of four and
/// pinning off.
pub async fn find_one_or_create(db: &Database, chat_id: i64) -> Result<TelegramChatRecapsOptions> {
    if let Some(existing) = find_one(db, chat_id).await? {
        return Ok(existing);
    }
    insert(
        db,
        chat_id,
        AutoRecapSendMode::Publicly.as_stored(),
        FIRST_ENABLE_RATES_PER_DAY,
        false,
    )
    .await
}

async fn insert(
    db: &Database,
    chat_id: i64,
    send_mode: i64,
    rates_per_day: i64,
    pin: bool,
) -> Result<TelegramChatRecapsOptions> {
    // One instant for both stamps: ent seeds each with the same `DefaultFunc`
    // during a create, and nothing ever advances `updated_at` afterwards.
    let now = codec::now_unix_millis();
    let created = TelegramChatRecapsOptions {
        id: codec::new_identifier(),
        chat_id,
        auto_recap_send_mode: send_mode,
        // The schema default; no create path in Go ever sets it.
        manual_recap_rate_per_seconds: 0,
        auto_recap_rates_per_day: rates_per_day,
        pin_auto_recap_message: pin,
        created_at: now,
        updated_at: now,
    };

    sqlx::query(insert_statement(db.backend))
        .bind(&created.id)
        .bind(created.chat_id)
        .bind(created.auto_recap_send_mode)
        .bind(created.manual_recap_rate_per_seconds)
        .bind(created.auto_recap_rates_per_day)
        .bind(created.pin_auto_recap_message)
        .bind(created.created_at)
        .bind(created.updated_at)
        .execute(&db.pool)
        .await?;

    Ok(created)
}

/// Store the delivery mode.
///
/// A missing row is created carrying only the mode, so the daily rate stays at
/// the schema default of zero rather than the four the find-or-create path uses.
pub async fn set_send_mode(db: &Database, chat_id: i64, mode: AutoRecapSendMode) -> Result<()> {
    let Some(existing) = find_one(db, chat_id).await? else {
        insert(db, chat_id, mode.as_stored(), 0, false).await?;
        return Ok(());
    };
    if existing.auto_recap_send_mode == mode.as_stored() {
        return Ok(());
    }

    // `updated_at` is untouched: ent declares it with `DefaultFunc` only, never
    // `UpdateDefault`, and no Go caller sets it on an update.
    sqlx::query(
        "UPDATE telegram_chat_recaps_options SET auto_recap_send_mode = $1 WHERE chat_id = $2",
    )
    .bind(mode.as_stored())
    .bind(chat_id)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// Store the automatic recap frequency.
///
/// Any integer is accepted, exactly like Go: the schedule builder is what
/// rejects an unusable rate later, not the persistence layer.
pub async fn set_rates_per_day(db: &Database, chat_id: i64, rates_per_day: i64) -> Result<()> {
    find_one_or_create(db, chat_id).await?;

    sqlx::query(
        "UPDATE telegram_chat_recaps_options SET auto_recap_rates_per_day = $1 WHERE chat_id = $2",
    )
    .bind(rates_per_day)
    .bind(chat_id)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// Turn pinning on, materialising the first-enable shape when absent.
pub async fn set_pin_enabled(db: &Database, chat_id: i64) -> Result<()> {
    let existing = find_one_or_create(db, chat_id).await?;
    if existing.pin_auto_recap_message {
        return Ok(());
    }
    set_pin(db, chat_id, true).await
}

/// Turn pinning off, creating the first-enable shape when absent.
pub async fn set_pin_disabled(db: &Database, chat_id: i64) -> Result<()> {
    let Some(existing) = find_one(db, chat_id).await? else {
        insert(
            db,
            chat_id,
            AutoRecapSendMode::Publicly.as_stored(),
            FIRST_ENABLE_RATES_PER_DAY,
            false,
        )
        .await?;
        return Ok(());
    };
    if !existing.pin_auto_recap_message {
        return Ok(());
    }
    set_pin(db, chat_id, false).await
}

async fn set_pin(db: &Database, chat_id: i64, pin: bool) -> Result<()> {
    sqlx::query(
        "UPDATE telegram_chat_recaps_options SET pin_auto_recap_message = $1 WHERE chat_id = $2",
    )
    .bind(pin)
    .bind(chat_id)
    .execute(&db.pool)
    .await?;
    Ok(())
}

/// The manual recap interval in seconds.
///
/// The stored per-chat value overrides the configured fallback only when it is
/// strictly greater, so a chat can loosen the limit but never tighten it below
/// the deployment-wide floor.
pub fn manual_rate_per_seconds(
    option: Option<&TelegramChatRecapsOptions>,
    configured_seconds: i64,
) -> i64 {
    match option {
        Some(option) if option.manual_recap_rate_per_seconds > configured_seconds => {
            option.manual_recap_rate_per_seconds
        }
        _ => configured_seconds,
    }
}

/// Remove every options row for a chat.
pub async fn delete_by_chat_id(db: &Database, chat_id: i64) -> Result<()> {
    sqlx::query("DELETE FROM telegram_chat_recaps_options WHERE chat_id = $1")
        .bind(chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

/// Move every options row onto the new chat identifier.
pub async fn migrate_chat_id(db: &Database, from_chat_id: i64, to_chat_id: i64) -> Result<()> {
    sqlx::query("UPDATE telegram_chat_recaps_options SET chat_id = $1 WHERE chat_id = $2")
        .bind(to_chat_id)
        .bind(from_chat_id)
        .execute(&db.pool)
        .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{FIRST_ENABLE_RATES_PER_DAY, INSERT_POSTGRES, INSERT_SQLITE, insert_statement};
    use crate::db::{DbBackend, models::AutoRecapSendMode};

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
        const COLUMNS: &str = "(id, chat_id, auto_recap_send_mode, manual_recap_rate_per_seconds,\n         auto_recap_rates_per_day, pin_auto_recap_message, created_at, updated_at)";
        for sql in [INSERT_POSTGRES, INSERT_SQLITE] {
            assert!(sql.contains(COLUMNS), "column order drifted: {sql}");
        }
    }

    #[test]
    fn the_stored_send_modes_are_the_go_integers() {
        assert_eq!(AutoRecapSendMode::Publicly.as_stored(), 0_i64);
        assert_eq!(
            AutoRecapSendMode::OnlyPrivateSubscriptions.as_stored(),
            1_i64
        );
        assert_eq!(FIRST_ENABLE_RATES_PER_DAY, 4_i64);
    }

    #[test]
    fn an_unknown_stored_send_mode_is_reported_rather_than_coerced() {
        for stored in [
            -1,
            2,
            i64::from(i32::MAX) + 1,
            i64::from(i32::MIN) - 1,
            i64::MAX,
            i64::MIN,
        ] {
            assert_eq!(
                AutoRecapSendMode::from_stored(stored),
                None,
                "{stored} must not be folded onto a known mode"
            );
        }
    }
}
