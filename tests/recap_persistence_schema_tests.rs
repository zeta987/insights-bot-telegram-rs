//! Task 4A1 — additive recap persistence schema characterization.
//!
//! Every expectation is pinned against the Go v1.0.0 Ent schema files under
//! `insights-bot-go/ent/schema`. The migration is additive by construction: the
//! pre-existing Rust tables and columns must survive untouched, and the new
//! tables must carry Go's names, defaults, nullability, and CHECK semantics.
//!
//! `/smr` generation stays out of scope; `feedback_summarizations_reactions`
//! exists only so the `smr/summarization/feedback/react` compatibility callback
//! keeps working.
//!
//! Every fixture in this file gets its schema from `Database::connect_from_env`
//! alone. No test executes migration SQL of its own, so a table observed here is
//! a table the shipped migrator created.

mod support;

use sqlx::AnyPool;
use support::sqlite_fixture::{
    PARITY_MIGRATION_POSTGRES, PARITY_MIGRATION_SQLITE, SchemaFixture, column, foreign_keys,
    table_columns, table_exists, unique_constraint_columns,
};

/// Every new table, with the Go field order it must reproduce.
const PARITY_TABLES: [(&str, &[&str]); 8] = [
    (
        "telegram_chat_feature_flags",
        &[
            "id",
            "chat_id",
            "chat_type",
            "chat_title",
            "feature_chat_histories_recap",
            "feature_language",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "telegram_chat_recaps_options",
        &[
            "id",
            "chat_id",
            "auto_recap_send_mode",
            "manual_recap_rate_per_seconds",
            "auto_recap_rates_per_day",
            "pin_auto_recap_message",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "telegram_chat_auto_recaps_subscribers",
        &["id", "chat_id", "user_id", "created_at", "updated_at"],
    ),
    (
        "log_chat_histories_recaps",
        &[
            "id",
            "chat_id",
            "recap_inputs",
            "recap_outputs",
            "from_platform",
            "prompt_token_usage",
            "completion_token_usage",
            "total_token_usage",
            "recap_type",
            "model_name",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "feedback_chat_histories_recaps_reactions",
        &[
            "id",
            "chat_id",
            "log_id",
            "user_id",
            "type",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "feedback_summarizations_reactions",
        &[
            "id",
            "chat_id",
            "log_id",
            "user_id",
            "type",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "sent_messages",
        &[
            "id",
            "chat_id",
            "message_id",
            "text",
            "is_pinned",
            "from_platform",
            "message_type",
            "created_at",
            "updated_at",
        ],
    ),
    (
        "metric_open_ai_chat_completion_token_usages",
        &[
            "id",
            "prompt_operation",
            "prompt_character_length",
            "prompt_token_usage",
            "completion_character_length",
            "completion_token_usage",
            "total_token_usage",
            "model_name",
            "created_at",
        ],
    ),
];

/// Columns the pre-existing Rust `chat_histories` table must keep.
const PRESERVED_CHAT_HISTORY_COLUMNS: [&str; 10] = [
    "id",
    "chat_id",
    "message_id",
    "from_id",
    "from_full_name",
    "from_username",
    "kind",
    "text",
    "media_url",
    "created_at",
];

/// Go parity columns added to `chat_histories` without touching its key.
const ADDED_CHAT_HISTORY_COLUMNS: [&str; 15] = [
    "chat_title",
    "chat_type",
    "user_id",
    "username",
    "full_name",
    "replied_to_message_id",
    "replied_to_user_id",
    "replied_to_full_name",
    "replied_to_username",
    "replied_to_text",
    "replied_to_chat_type",
    "chatted_at",
    "embedded",
    "from_platform",
    "updated_at",
];

/// The exact reaction vocabulary shared by both feedback tables.
const REACTION_VALUES: [&str; 4] = ["none", "up_vote", "down_vote", "lmao"];

const REACTION_TABLES: [&str; 2] = [
    "feedback_chat_histories_recaps_reactions",
    "feedback_summarizations_reactions",
];

// ---------------------------------------------------------------------------
// Structure
// ---------------------------------------------------------------------------

#[tokio::test]
async fn the_application_migrator_creates_every_parity_table_on_a_fresh_database() {
    let fixture = SchemaFixture::new();
    // A single bootstrap and no test-owned SQL: the migration list must carry
    // 0003 for any of this to exist.
    let pool = fixture.bootstrap().await;

    for (table, _) in PARITY_TABLES {
        assert!(
            table_exists(&pool, table).await,
            "{table} must come from the shipped migrator, not from the fixture"
        );
    }

    // The pre-existing tables from 0001 and 0002 are still applied in order.
    assert!(table_exists(&pool, "chat_histories").await);
    assert!(table_exists(&pool, "recap_configs").await);
    assert!(
        column(&pool, "recap_configs", "auto_recap_rates_per_day")
            .await
            .is_some(),
        "0002 must still run before 0003"
    );
    assert!(
        column(&pool, "chat_histories", "chatted_at")
            .await
            .is_some(),
        "0003 must run after 0001 created chat_histories"
    );
}

#[tokio::test]
async fn every_parity_table_declares_its_go_columns_in_declaration_order() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for (table, expected) in PARITY_TABLES {
        assert!(
            table_exists(&pool, table).await,
            "{table} must be created by the parity migration"
        );
        let actual: Vec<String> = table_columns(&pool, table)
            .await
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(
            actual,
            expected.to_vec(),
            "column layout mismatch for {table}"
        );
    }
}

#[tokio::test]
async fn every_parity_table_uses_a_text_identifier_as_its_only_primary_key() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for (table, _) in PARITY_TABLES {
        let columns = table_columns(&pool, table).await;
        let keys: Vec<&str> = columns
            .iter()
            .filter(|column| column.primary_key_position > 0)
            .map(|column| column.name.as_str())
            .collect();
        assert_eq!(keys, vec!["id"], "{table} must key on id alone");

        let id = column(&pool, table, "id")
            .await
            .expect("every parity table declares id");
        assert_eq!(
            id.declared_type.to_ascii_uppercase(),
            "TEXT",
            "{table}.id is TEXT on SQLite; repositories generate the UUID"
        );
        assert!(id.not_null, "{table}.id must be NOT NULL");
    }
}

#[tokio::test]
async fn unix_millisecond_timestamps_are_non_null_and_default_to_zero() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for (table, expected) in PARITY_TABLES {
        for name in ["created_at", "updated_at"] {
            if !expected.contains(&name) {
                continue;
            }
            let timestamp = column(&pool, table, name)
                .await
                .unwrap_or_else(|| panic!("{table}.{name} must exist"));
            assert!(timestamp.not_null, "{table}.{name} must be NOT NULL");
            assert_eq!(
                timestamp.default_value.as_deref(),
                Some("0"),
                "{table}.{name} is a repository-populated Unix-millisecond column"
            );
        }
    }

    // Go's metric row has no update path, so it carries no updated_at.
    assert!(
        column(
            &pool,
            "metric_open_ai_chat_completion_token_usages",
            "updated_at",
        )
        .await
        .is_none(),
        "the token-usage metric must not gain an updated_at column"
    );
}

#[tokio::test]
async fn feature_flag_chat_type_is_the_only_new_column_without_a_default() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    let chat_type = column(&pool, "telegram_chat_feature_flags", "chat_type")
        .await
        .expect("chat_type must exist");
    assert!(chat_type.not_null);
    assert_eq!(
        chat_type.default_value, None,
        "Go declares chat_type without a default"
    );

    for table in REACTION_TABLES {
        let log_id = column(&pool, table, "log_id")
            .await
            .expect("log_id must exist");
        assert!(log_id.not_null, "{table}.log_id must be NOT NULL");
        assert_eq!(
            log_id.default_value, None,
            "{table}.log_id is generated by the repository"
        );
    }

    for table in REACTION_TABLES {
        let reaction = column(&pool, table, "type").await.expect("type must exist");
        assert!(reaction.not_null);
        assert_eq!(reaction.default_value.as_deref(), Some("'none'"));
    }
}

// ---------------------------------------------------------------------------
// Additivity
// ---------------------------------------------------------------------------

#[tokio::test]
async fn chat_histories_keeps_its_integer_primary_key_and_every_old_column() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    let id = column(&pool, "chat_histories", "id")
        .await
        .expect("chat_histories keeps its id");
    assert_eq!(id.declared_type.to_ascii_uppercase(), "INTEGER");
    assert_eq!(id.primary_key_position, 1);

    for name in PRESERVED_CHAT_HISTORY_COLUMNS {
        assert!(
            column(&pool, "chat_histories", name).await.is_some(),
            "the additive migration must preserve chat_histories.{name}"
        );
    }
}

#[tokio::test]
async fn chat_histories_gains_the_go_parity_columns_without_a_unique_constraint() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for name in ADDED_CHAT_HISTORY_COLUMNS {
        let added = column(&pool, "chat_histories", name)
            .await
            .unwrap_or_else(|| panic!("chat_histories.{name} must be added"));
        assert!(added.not_null, "chat_histories.{name} must be NOT NULL");
        assert!(
            added.default_value.is_some(),
            "chat_histories.{name} needs a default so the ALTER can run on existing rows"
        );
    }

    assert!(
        unique_constraint_columns(&pool, "chat_histories")
            .await
            .is_empty(),
        "chat histories stay deliberately non-unique"
    );
}

#[tokio::test]
async fn only_feature_flags_and_recaps_options_declare_a_unique_chat_id() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for (table, _) in PARITY_TABLES {
        let declared = unique_constraint_columns(&pool, table).await;
        let expected: Vec<Vec<String>> = if matches!(
            table,
            "telegram_chat_feature_flags" | "telegram_chat_recaps_options"
        ) {
            vec![vec!["chat_id".to_owned()]]
        } else {
            Vec::new()
        };
        assert_eq!(
            declared, expected,
            "{table} declares the wrong uniqueness rules"
        );
    }
}

#[tokio::test]
async fn no_parity_table_declares_a_foreign_key() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for (table, _) in PARITY_TABLES {
        assert!(
            foreign_keys(&pool, table).await.is_empty(),
            "{table} must stay free of foreign keys"
        );
    }
    assert!(foreign_keys(&pool, "chat_histories").await.is_empty());
}

// ---------------------------------------------------------------------------
// Enforced behaviour
// ---------------------------------------------------------------------------

async fn insert_feature_flag(pool: &AnyPool, id: &str, chat_id: i64) -> sqlx::Result<()> {
    sqlx::query(
        "INSERT INTO telegram_chat_feature_flags (id, chat_id, chat_type) VALUES ($1, $2, $3)",
    )
    .bind(id)
    .bind(chat_id)
    .bind("supergroup")
    .execute(pool)
    .await
    .map(|_| ())
}

async fn insert_recaps_option(pool: &AnyPool, id: &str, chat_id: i64) -> sqlx::Result<()> {
    sqlx::query("INSERT INTO telegram_chat_recaps_options (id, chat_id) VALUES ($1, $2)")
        .bind(id)
        .bind(chat_id)
        .execute(pool)
        .await
        .map(|_| ())
}

async fn insert_reaction(
    pool: &AnyPool,
    table: &str,
    id: &str,
    reaction: &str,
) -> sqlx::Result<()> {
    sqlx::query(&format!(
        "INSERT INTO {table} (id, chat_id, log_id, user_id, \"type\") VALUES ($1, $2, $3, $4, $5)"
    ))
    .bind(id)
    .bind(-1_001_234_567_890_i64)
    .bind("00000000-0000-4000-8000-000000000001")
    .bind(42_i64)
    .bind(reaction)
    .execute(pool)
    .await
    .map(|_| ())
}

#[tokio::test]
async fn a_duplicate_chat_id_is_rejected_by_both_unique_tables() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    insert_feature_flag(&pool, "flag-1", -100)
        .await
        .expect("first flag");
    let error = insert_feature_flag(&pool, "flag-2", -100)
        .await
        .expect_err("a second flag for the same chat must be rejected");
    assert!(
        error.to_string().to_ascii_uppercase().contains("UNIQUE"),
        "unexpected rejection: {error}"
    );

    insert_recaps_option(&pool, "option-1", -200)
        .await
        .expect("first option");
    let error = insert_recaps_option(&pool, "option-2", -200)
        .await
        .expect_err("a second option for the same chat must be rejected");
    assert!(
        error.to_string().to_ascii_uppercase().contains("UNIQUE"),
        "unexpected rejection: {error}"
    );

    // A different chat is unaffected.
    insert_feature_flag(&pool, "flag-3", -101)
        .await
        .expect("another chat keeps working");
}

#[tokio::test]
async fn duplicate_tuples_are_accepted_where_go_stays_non_unique() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for id in ["subscriber-1", "subscriber-2"] {
        sqlx::query(
            "INSERT INTO telegram_chat_auto_recaps_subscribers (id, chat_id, user_id)
             VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(-100_i64)
        .bind(7_i64)
        .execute(&pool)
        .await
        .expect("subscribers stay deliberately non-unique");
    }

    for table in REACTION_TABLES {
        for id in ["reaction-1", "reaction-2"] {
            insert_reaction(&pool, table, id, "up_vote")
                .await
                .unwrap_or_else(|error| panic!("{table} must accept a duplicate tuple: {error}"));
        }
    }

    for id in ["sent-1", "sent-2"] {
        sqlx::query("INSERT INTO sent_messages (id, chat_id, message_id) VALUES ($1, $2, $3)")
            .bind(id)
            .bind(-100_i64)
            .bind(4_242_i64)
            .execute(&pool)
            .await
            .expect("sent_messages(chat_id, message_id) stays non-unique");
    }

    for _ in 0..2 {
        sqlx::query(
            "INSERT INTO chat_histories (chat_id, message_id, kind, created_at)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(-100_i64)
        .bind(11_i64)
        .bind("text")
        .bind(1_700_000_000_000_i64)
        .execute(&pool)
        .await
        .expect("chat histories stay non-unique");
    }
}

#[tokio::test]
async fn both_reaction_tables_accept_only_the_four_go_values() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for table in REACTION_TABLES {
        for (index, value) in REACTION_VALUES.iter().enumerate() {
            insert_reaction(&pool, table, &format!("{table}-ok-{index}"), value)
                .await
                .unwrap_or_else(|error| panic!("{table} must accept {value}: {error}"));
        }

        for rejected in ["", "NONE", "up-vote", "upvote", "unknown", "lmao "] {
            insert_reaction(&pool, table, &format!("{table}-bad-{rejected}"), rejected)
                .await
                .err()
                .unwrap_or_else(|| panic!("{table} must reject the reaction value {rejected:?}"));
        }
    }
}

#[tokio::test]
async fn the_default_reaction_is_none_when_the_column_is_omitted() {
    let fixture = SchemaFixture::new();
    let pool = fixture.bootstrap().await;

    for table in REACTION_TABLES {
        sqlx::query(&format!(
            "INSERT INTO {table} (id, chat_id, log_id, user_id) VALUES ($1, $2, $3, $4)"
        ))
        .bind("defaulted")
        .bind(-100_i64)
        .bind("00000000-0000-4000-8000-000000000002")
        .bind(7_i64)
        .execute(&pool)
        .await
        .expect("an omitted reaction must fall back to the default");

        let stored: String =
            sqlx::query_scalar(&format!("SELECT \"type\" FROM {table} WHERE id = $1"))
                .bind("defaulted")
                .fetch_one(&pool)
                .await
                .expect("the stored reaction");
        assert_eq!(stored, "none", "{table} must default to none");
    }
}

// ---------------------------------------------------------------------------
// Re-runnability
// ---------------------------------------------------------------------------

#[tokio::test]
async fn bootstrapping_the_same_database_twice_keeps_the_schema_and_the_data() {
    let fixture = SchemaFixture::new();

    let first = fixture.bootstrap().await;
    insert_feature_flag(&first, "survivor", -900)
        .await
        .expect("a row written before the restart");
    sqlx::query(
        "INSERT INTO chat_histories (chat_id, message_id, kind, created_at, chatted_at)
         VALUES ($1, $2, $3, $4, $5)",
    )
    .bind(-900_i64)
    .bind(5_i64)
    .bind("text")
    .bind(1_700_000_000_000_i64)
    .bind(1_700_000_000_000_i64)
    .execute(&first)
    .await
    .expect("a history row written into a 0003 column before the restart");
    first.close().await;

    // A restarted process re-runs every migration, including the non-idempotent
    // SQLite ALTER TABLE statements, against the same file.
    let second = fixture.bootstrap().await;

    let surviving: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM telegram_chat_feature_flags WHERE chat_id = $1")
            .bind(-900_i64)
            .fetch_one(&second)
            .await
            .expect("the count survives the second bootstrap");
    assert_eq!(
        surviving, 1,
        "re-running must not recreate or clear a table"
    );

    let history_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM chat_histories WHERE chat_id = $1")
            .bind(-900_i64)
            .fetch_one(&second)
            .await
            .expect("chat histories survive the second bootstrap");
    assert_eq!(history_rows, 1, "a repeated ALTER must not drop rows");

    // Read the Unix-millisecond value as text: the sqlx `Any` driver narrows a
    // SQLite INTEGER to 32 bits on the scalar path, which would mask the value
    // rather than the storage.
    let chatted_at: String = sqlx::query_scalar(
        "SELECT CAST(chatted_at AS TEXT) FROM chat_histories WHERE chat_id = $1",
    )
    .bind(-900_i64)
    .fetch_one(&second)
    .await
    .expect("the 0003 column keeps its value");
    assert_eq!(
        chatted_at, "1700000000000",
        "a repeated ALTER must not reset an added column"
    );

    for (table, expected) in PARITY_TABLES {
        let actual: Vec<String> = table_columns(&second, table)
            .await
            .into_iter()
            .map(|column| column.name)
            .collect();
        assert_eq!(
            actual,
            expected.to_vec(),
            "{table} changed on the second run"
        );
    }

    let history_columns: Vec<String> = table_columns(&second, "chat_histories")
        .await
        .into_iter()
        .map(|column| column.name)
        .collect();
    for name in PRESERVED_CHAT_HISTORY_COLUMNS
        .iter()
        .chain(&ADDED_CHAT_HISTORY_COLUMNS)
    {
        assert!(
            history_columns.contains(&(*name).to_string()),
            "chat_histories.{name} must survive the second run"
        );
    }
}

// ---------------------------------------------------------------------------
// PostgreSQL source alignment
//
// No isolated PostgreSQL URL is supplied and no environment variable is read to
// find one, so the PostgreSQL half is verified by comparing its declared
// structure against the executed SQLite half.
// ---------------------------------------------------------------------------

/// Which migration a declaration came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Engine {
    Postgres,
    Sqlite,
}

/// A column declaration reduced to the properties both engines must agree on.
///
/// The declared type is normalised into the SQLite spelling, so a legitimate
/// backend difference collapses while a real drift survives.
#[derive(Clone, Debug, Eq, PartialEq)]
struct ColumnSpec {
    name: String,
    /// The type as SQLite must spell it.
    normalized_type: String,
    /// `PRIMARY KEY` implies `NOT NULL`, which PostgreSQL leaves implicit.
    not_null: bool,
    primary_key: bool,
    unique: bool,
    /// `TRUE`/`FALSE` are folded onto `1`/`0`.
    default_value: Option<String>,
    /// Whitespace-normalised `CHECK (...)`, or `None`.
    check: Option<String>,
}

/// The SQLite type a PostgreSQL type must be written as.
///
/// Unmapped types panic rather than pass, so a newly introduced PostgreSQL type
/// cannot slip through unverified.
fn normalize_type(engine: Engine, declared: &str) -> String {
    let declared = declared.to_ascii_uppercase();
    match engine {
        Engine::Sqlite => match declared.as_str() {
            "TEXT" | "INTEGER" => declared,
            other => panic!("unexpected SQLite type {other}"),
        },
        Engine::Postgres => match declared.as_str() {
            "UUID" | "TEXT" => "TEXT".to_owned(),
            "SMALLINT" | "INTEGER" | "BIGINT" | "BOOLEAN" => "INTEGER".to_owned(),
            other => panic!("unmapped PostgreSQL type {other}"),
        },
    }
}

/// Fold the boolean literal spellings onto the SQLite representation.
fn normalize_default(raw: &str) -> String {
    match raw.to_ascii_uppercase().as_str() {
        "TRUE" => "1".to_owned(),
        "FALSE" => "0".to_owned(),
        _ => raw.to_owned(),
    }
}

/// Parse one column declaration, without its trailing comma.
fn parse_column_spec(engine: Engine, declaration: &str) -> ColumnSpec {
    let declaration = declaration.trim().trim_end_matches(',').trim();
    let mut parts = declaration.splitn(3, ' ');
    let name = parts
        .next()
        .expect("a column declaration starts with a name")
        .trim_matches('"')
        .to_owned();
    let declared_type = parts.next().expect("a column declaration names a type");
    let constraints = parts.next().unwrap_or_default();

    let (before_check, check) = match constraints.find("CHECK") {
        Some(index) => (
            &constraints[..index],
            Some(
                constraints[index..]
                    .split_whitespace()
                    .collect::<Vec<_>>()
                    .join(" "),
            ),
        ),
        None => (constraints, None),
    };

    let default_value = before_check.find("DEFAULT ").map(|index| {
        let tail = before_check[index + "DEFAULT ".len()..].trim();
        // The default is one token, except for the empty string literal.
        let raw = tail.split_whitespace().next().unwrap_or_default();
        normalize_default(raw)
    });

    let primary_key = before_check.contains("PRIMARY KEY");
    ColumnSpec {
        name,
        normalized_type: normalize_type(engine, declared_type),
        not_null: before_check.contains("NOT NULL") || primary_key,
        primary_key,
        unique: before_check.contains("UNIQUE"),
        default_value,
        check,
    }
}

/// Every `CREATE TABLE IF NOT EXISTS` block, as normalised column specs.
fn created_tables(engine: Engine, sql: &str) -> Vec<(String, Vec<ColumnSpec>)> {
    const TABLE_CONSTRAINT_KEYWORDS: [&str; 5] =
        ["UNIQUE", "CHECK", "PRIMARY", "CONSTRAINT", "FOREIGN"];

    let lines: Vec<&str> = sql.lines().collect();
    let mut tables = Vec::new();
    let mut index = 0;

    while index < lines.len() {
        let trimmed = lines[index].trim();
        index += 1;
        let Some(rest) = trimmed.strip_prefix("CREATE TABLE IF NOT EXISTS ") else {
            continue;
        };
        let name = rest.trim().trim_end_matches('(').trim().to_owned();

        let mut columns = Vec::new();
        while index < lines.len() {
            let body = lines[index].trim();
            index += 1;
            if body.starts_with(')') {
                break;
            }
            if body.is_empty() || body.starts_with("--") {
                continue;
            }
            let token = body.split_whitespace().next().unwrap_or_default();
            // A table-level constraint is not a column; this slice declares none.
            assert!(
                !TABLE_CONSTRAINT_KEYWORDS.contains(&token.to_ascii_uppercase().as_str()),
                "table-level constraints are out of scope for this slice: {body}"
            );
            columns.push(parse_column_spec(engine, body));
        }
        tables.push((name, columns));
    }
    tables
}

/// Every `ALTER TABLE ... ADD COLUMN`, as a table plus a normalised column spec.
fn altered_columns(engine: Engine, sql: &str) -> Vec<(String, ColumnSpec)> {
    sql.lines()
        .filter_map(|line| {
            let rest = line
                .trim()
                .trim_end_matches(';')
                .strip_prefix("ALTER TABLE ")?;
            let (table, tail) = rest.split_once(' ')?;
            let tail = tail.trim();
            // PostgreSQL guards the ALTER; SQLite has no such syntax.
            let tail = tail
                .strip_prefix("ADD COLUMN IF NOT EXISTS ")
                .or_else(|| tail.strip_prefix("ADD COLUMN "))?;
            Some((table.to_owned(), parse_column_spec(engine, tail)))
        })
        .collect()
}

#[tokio::test]
async fn the_two_migrations_declare_the_same_normalized_column_specifications() {
    let postgres = created_tables(Engine::Postgres, PARITY_MIGRATION_POSTGRES);
    let sqlite = created_tables(Engine::Sqlite, PARITY_MIGRATION_SQLITE);

    assert_eq!(
        postgres.len(),
        PARITY_TABLES.len(),
        "the migrations must declare exactly the Go parity tables"
    );
    assert_eq!(
        postgres
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        PARITY_TABLES
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
    );

    // Type, nullability, default, uniqueness, key, and CHECK all compared.
    assert_eq!(
        postgres, sqlite,
        "the two migrations drifted in more than a legitimate backend spelling"
    );

    for ((table, columns), (_, expected)) in postgres.iter().zip(PARITY_TABLES.iter()) {
        assert_eq!(
            columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
            expected.to_vec(),
            "column layout mismatch for {table}"
        );
    }
}

#[tokio::test]
async fn the_two_migrations_extend_chat_histories_with_the_same_specifications() {
    let postgres = altered_columns(Engine::Postgres, PARITY_MIGRATION_POSTGRES);
    let sqlite = altered_columns(Engine::Sqlite, PARITY_MIGRATION_SQLITE);

    assert_eq!(
        postgres, sqlite,
        "the added chat_histories columns drifted between engines"
    );
    assert_eq!(
        postgres
            .iter()
            .map(|(table, column)| (table.as_str(), column.name.as_str()))
            .collect::<Vec<_>>(),
        ADDED_CHAT_HISTORY_COLUMNS
            .iter()
            .map(|column| ("chat_histories", *column))
            .collect::<Vec<_>>()
    );

    // Every added column must be safe to apply to existing rows.
    for (_, column) in &postgres {
        assert!(
            column.not_null,
            "chat_histories.{} must be NOT NULL",
            column.name
        );
        assert!(
            column.default_value.is_some(),
            "chat_histories.{} needs a default for the ALTER to succeed",
            column.name
        );
        assert!(!column.unique, "chat histories stay non-unique");
        assert!(!column.primary_key, "the integer key must not be touched");
    }
}

#[tokio::test]
async fn uniqueness_and_the_reaction_check_sit_exactly_where_go_puts_them() {
    for (engine, migration) in [
        (Engine::Postgres, PARITY_MIGRATION_POSTGRES),
        (Engine::Sqlite, PARITY_MIGRATION_SQLITE),
    ] {
        let tables = created_tables(engine, migration);

        let unique: Vec<(String, String)> = tables
            .iter()
            .flat_map(|(table, columns)| {
                columns
                    .iter()
                    .filter(|column| column.unique)
                    .map(move |column| (table.clone(), column.name.clone()))
            })
            .collect();
        assert_eq!(
            unique,
            vec![
                (
                    "telegram_chat_feature_flags".to_owned(),
                    "chat_id".to_owned()
                ),
                (
                    "telegram_chat_recaps_options".to_owned(),
                    "chat_id".to_owned()
                ),
            ],
            "{engine:?} declares the wrong UNIQUE columns"
        );

        let keys: Vec<(String, String)> = tables
            .iter()
            .flat_map(|(table, columns)| {
                columns
                    .iter()
                    .filter(|column| column.primary_key)
                    .map(move |column| (table.clone(), column.name.clone()))
            })
            .collect();
        assert_eq!(keys.len(), PARITY_TABLES.len());
        assert!(keys.iter().all(|(_, column)| column == "id"));

        let checked: Vec<(String, String, String)> = tables
            .iter()
            .flat_map(|(table, columns)| {
                columns.iter().filter_map(move |column| {
                    column
                        .check
                        .clone()
                        .map(|check| (table.clone(), column.name.clone(), check))
                })
            })
            .collect();
        let expected_check = format!(
            "CHECK (\"type\" IN ({}))",
            REACTION_VALUES
                .iter()
                .map(|value| format!("'{value}'"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        assert_eq!(
            checked,
            REACTION_TABLES
                .iter()
                .map(|table| (
                    (*table).to_owned(),
                    "type".to_owned(),
                    expected_check.clone()
                ))
                .collect::<Vec<_>>(),
            "{engine:?} misplaced or reworded the reaction CHECK"
        );
    }
}

#[tokio::test]
async fn a_drifted_declaration_is_detected_by_the_normalizing_parser() {
    // The comparison is only worth running if it can actually fail.
    let baseline = parse_column_spec(Engine::Postgres, "chat_id BIGINT NOT NULL UNIQUE");

    for drifted in [
        "chat_id BIGINT NOT NULL",
        "chat_id BIGINT UNIQUE",
        "chat_id TEXT NOT NULL UNIQUE",
        "chat_id BIGINT NOT NULL UNIQUE DEFAULT 0",
        "chat_id BIGINT NOT NULL UNIQUE PRIMARY KEY",
    ] {
        assert_ne!(
            parse_column_spec(Engine::Postgres, drifted),
            baseline,
            "the parser must notice {drifted}"
        );
    }

    // Legitimate backend spellings must still compare equal.
    assert_eq!(
        parse_column_spec(Engine::Postgres, "is_pinned BOOLEAN NOT NULL DEFAULT FALSE"),
        parse_column_spec(Engine::Sqlite, "is_pinned INTEGER NOT NULL DEFAULT 0")
    );
    assert_eq!(
        parse_column_spec(Engine::Postgres, "id UUID PRIMARY KEY"),
        parse_column_spec(Engine::Sqlite, "id TEXT NOT NULL PRIMARY KEY")
    );
    assert_eq!(
        parse_column_spec(Engine::Postgres, "created_at BIGINT NOT NULL DEFAULT 0"),
        parse_column_spec(Engine::Sqlite, "created_at INTEGER NOT NULL DEFAULT 0")
    );
    assert_ne!(
        parse_column_spec(Engine::Postgres, "embedded BOOLEAN NOT NULL DEFAULT TRUE"),
        parse_column_spec(Engine::Sqlite, "embedded INTEGER NOT NULL DEFAULT 0"),
        "a genuine default drift must not be folded away"
    );
}

#[tokio::test]
async fn the_postgres_migration_keeps_uuid_identifiers_and_the_shared_check_vocabulary() {
    for (table, _) in PARITY_TABLES {
        assert!(
            PARITY_MIGRATION_POSTGRES.contains(&format!("CREATE TABLE IF NOT EXISTS {table}")),
            "{table} is missing from the PostgreSQL migration"
        );
    }

    assert_eq!(
        PARITY_MIGRATION_POSTGRES
            .matches("id UUID PRIMARY KEY")
            .count(),
        PARITY_TABLES.len(),
        "every PostgreSQL identifier is a UUID primary key"
    );
    assert_eq!(
        PARITY_MIGRATION_SQLITE
            .matches("id TEXT NOT NULL PRIMARY KEY")
            .count(),
        PARITY_TABLES.len(),
        "every SQLite identifier is a TEXT primary key"
    );

    // Only prose may mention UUID on the SQLite side; no column may declare it.
    for line in PARITY_MIGRATION_SQLITE.lines() {
        let trimmed = line.trim();
        assert!(
            trimmed.starts_with("--") || !trimmed.to_ascii_uppercase().contains("UUID"),
            "SQLite must not declare a UUID column: {trimmed}"
        );
    }

    let check = REACTION_VALUES
        .iter()
        .map(|value| format!("'{value}'"))
        .collect::<Vec<_>>()
        .join(", ");
    for migration in [PARITY_MIGRATION_POSTGRES, PARITY_MIGRATION_SQLITE] {
        assert_eq!(
            migration.matches(&check).count(),
            REACTION_TABLES.len(),
            "both reaction tables must share one CHECK vocabulary"
        );
    }
}

#[tokio::test]
async fn neither_migration_drops_renames_or_overwrites_anything() {
    for migration in [PARITY_MIGRATION_POSTGRES, PARITY_MIGRATION_SQLITE] {
        let upper = migration.to_ascii_uppercase();
        for forbidden in ["DROP ", "RENAME ", "TRUNCATE", "DELETE ", "ALTER COLUMN"] {
            assert!(
                !upper.contains(forbidden),
                "the additive migration must not contain {forbidden}"
            );
        }

        // Every table creation must be guarded, so a re-run never overwrites.
        let guarded = upper.matches("CREATE TABLE IF NOT EXISTS ").count();
        assert_eq!(
            upper.matches("CREATE TABLE ").count(),
            guarded,
            "every CREATE TABLE must carry IF NOT EXISTS"
        );
        assert_eq!(guarded, PARITY_TABLES.len());

        // A semicolon inside a comment would split a statement in half, because
        // the migrator splits on `;` before dropping comment lines.
        for line in migration.lines() {
            let trimmed = line.trim();
            assert!(
                !(trimmed.starts_with("--") && trimmed.contains(';')),
                "a comment must not contain a semicolon: {trimmed}"
            );
        }
    }
}
