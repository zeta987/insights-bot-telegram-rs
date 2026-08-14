use anyhow::{Context, Result};
use sqlx::{AnyPool, any::AnyPoolOptions};
use tracing::{debug, info, warn};
use url::Url;

use crate::config::DbConfig;

pub mod chat_history;
pub mod codec;
pub mod feature_flags;
pub mod feedback;
pub mod logs;
pub mod migration;
pub mod models;
pub mod recap_config;
pub mod recap_logs;
pub mod recap_options;
pub mod sent_messages;
pub mod subscribers;
pub mod usage_metrics;

#[derive(Debug, Clone, Copy)]
pub enum DbBackend {
    Postgres,
    Sqlite,
}

/// The one fixed sentence reported for a PostgreSQL database this service
/// cannot serve.
///
/// It quotes nothing observed in the database and nothing from the connection,
/// so it is safe to surface anywhere.
const INCOMPATIBLE_POSTGRES_SCHEMA_MESSAGE: &str = "unsupported PostgreSQL schema: chat_histories.id is not an integer column. \
     This service owns chat_histories with an integer primary key; a UUID-keyed \
     Go chat_histories table must be migrated before starting this service.";

/// A PostgreSQL database whose shape this service cannot serve.
///
/// This is a distinct type rather than a plain message so the SQLite fallback
/// can tell "the server is unreachable" apart from "the server is reachable and
/// holds a schema we must not touch". Falling back on the latter would silently
/// start the bot against an empty SQLite file while the real data sat untouched.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
struct IncompatiblePostgresSchema;

impl std::fmt::Display for IncompatiblePostgresSchema {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(INCOMPATIBLE_POSTGRES_SCHEMA_MESSAGE)
    }
}

impl std::error::Error for IncompatiblePostgresSchema {}

#[derive(Clone)]
pub struct Database {
    pub pool: AnyPool,
    pub backend: DbBackend,
}

impl Database {
    pub async fn connect_from_env(cfg: &DbConfig) -> Result<Self> {
        sqlx::any::install_default_drivers();

        if let Some(url) = cfg.postgres_url.as_ref() {
            match Self::connect_postgres(url).await {
                Ok(db) => return Ok(db),
                Err(err) => {
                    if !Self::should_fall_back_to_sqlite(&err, cfg.sqlite_file.is_some()) {
                        // Either no SQLite fallback is configured, or the server
                        // answered with a schema this service must not touch.
                        return Err(err);
                    }
                    warn!("Postgres connection failed; falling back to SQLite");
                }
            }
        }

        // Connect to SQLite if configured
        let Some(sqlite_path) = cfg.sqlite_file.clone() else {
            anyhow::bail!(
                "no database configured: set DATABASE_URL for PostgreSQL or SQLITE_PATH for SQLite"
            );
        };

        let sqlite_url = if sqlite_path.starts_with("sqlite://") {
            // Ensure create_if_missing is enabled.
            if sqlite_path.contains('?') {
                sqlite_path
            } else {
                format!("{sqlite_path}?mode=rwc")
            }
        } else {
            format!("sqlite://{sqlite_path}?mode=rwc")
        };

        let pool = AnyPoolOptions::new()
            .connect(&sqlite_url)
            .await
            .with_context(|| format!("SQLite connect failed ({sqlite_url})"))?;

        info!("connected to SQLite at {sqlite_url}");
        let db = Self {
            pool,
            backend: DbBackend::Sqlite,
        };
        db.run_migrations().await?;
        Ok(db)
    }

    /// Run database migrations based on the backend type.
    async fn run_migrations(&self) -> Result<()> {
        let migration_files: Vec<&str> = match self.backend {
            DbBackend::Postgres => vec![
                include_str!("../../migrations/postgres/0001_init.sql"),
                include_str!("../../migrations/postgres/0002_recap_config_extensions.sql"),
                include_str!("../../migrations/postgres/0003_rich_recap_parity.sql"),
            ],
            DbBackend::Sqlite => vec![
                include_str!("../../migrations/sqlite/0001_init.sql"),
                include_str!("../../migrations/sqlite/0002_recap_config_extensions.sql"),
                include_str!("../../migrations/sqlite/0003_rich_recap_parity.sql"),
            ],
        };

        for migration_sql in migration_files {
            // Execute each statement separately (SQLite doesn't support multiple statements in one query)
            for statement in migration_sql.split(';') {
                // Strip leading comment lines (lines starting with --)
                let stmt: String = statement
                    .lines()
                    .filter(|line| {
                        let trimmed = line.trim();
                        !trimmed.is_empty() && !trimmed.starts_with("--")
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                let stmt = stmt.trim();
                if stmt.is_empty() {
                    continue;
                }
                // SQLite ALTER TABLE ADD COLUMN fails if column already exists;
                // treat "duplicate column" as success for idempotency.
                if let Err(err) = sqlx::query(stmt).execute(&self.pool).await {
                    let msg = err.to_string();
                    if msg.contains("duplicate column") || msg.contains("already exists") {
                        debug!("migration statement skipped (already applied): {}", msg);
                    } else {
                        return Err(err.into());
                    }
                }
            }
        }

        info!("database migrations completed");
        Ok(())
    }

    /// Connect to PostgreSQL, creating the database if it doesn't exist.
    async fn connect_postgres(url: &str) -> Result<Self> {
        // First, try direct connection
        match AnyPoolOptions::new().connect(url).await {
            Ok(pool) => {
                info!("connected to Postgres");
                Self::validate_postgres_schema(&pool).await?;
                let db = Self {
                    pool,
                    backend: DbBackend::Postgres,
                };
                db.run_migrations().await?;
                Ok(db)
            }
            Err(err) => {
                let err_str = err.to_string();
                // Check if error is "database does not exist"
                if err_str.contains("does not exist") {
                    debug!("database does not exist, attempting to create it");
                    Self::create_postgres_database(url).await?;
                    // Retry connection after creating database
                    let pool = AnyPoolOptions::new()
                        .connect(url)
                        .await
                        .with_context(|| "failed to connect after creating database")?;
                    info!("connected to Postgres (database was auto-created)");
                    Self::validate_postgres_schema(&pool).await?;
                    let db = Self {
                        pool,
                        backend: DbBackend::Postgres,
                    };
                    db.run_migrations().await?;
                    Ok(db)
                } else {
                    Err(err.into())
                }
            }
        }
    }

    /// Create a PostgreSQL database by connecting to the default 'postgres' database.
    async fn create_postgres_database(url: &str) -> Result<()> {
        let mut parsed = Url::parse(url).with_context(|| "invalid DATABASE_URL")?;

        // Extract database name from path (e.g., "/mydb" -> "mydb")
        let db_name = parsed.path().trim_start_matches('/').to_string();

        if db_name.is_empty() {
            anyhow::bail!("DATABASE_URL must specify a database name");
        }

        // Change path to connect to default 'postgres' database
        parsed.set_path("/postgres");
        let admin_url = parsed.as_str();

        debug!("connecting to 'postgres' database to create '{db_name}'");
        let admin_pool = AnyPoolOptions::new()
            .connect(admin_url)
            .await
            .with_context(|| "failed to connect to 'postgres' database for db creation")?;

        // Use dynamic SQL to create database (identifiers can't be parameterized)
        // Validate db_name to prevent SQL injection
        if !db_name.chars().all(|c| c.is_alphanumeric() || c == '_') {
            anyhow::bail!("invalid database name: {db_name}");
        }

        let create_sql = format!("CREATE DATABASE \"{db_name}\"");
        sqlx::query(&create_sql)
            .execute(&admin_pool)
            .await
            .with_context(|| format!("failed to create database '{db_name}'"))?;

        info!("created PostgreSQL database '{db_name}'");
        admin_pool.close().await;
        Ok(())
    }

    /// `information_schema` data types this service accepts for
    /// `chat_histories.id`.
    ///
    /// The Rich recap parity migration keeps the Rust-owned integer key, so an
    /// integral type is the whole compatibility contract.
    const SUPPORTED_CHAT_HISTORIES_ID_TYPES: [&'static str; 3] = ["smallint", "integer", "bigint"];

    /// Look up `chat_histories.id` in the schema the session actually resolves.
    ///
    /// Migrations and runtime queries name `chat_histories` without a schema
    /// qualifier, so they follow `search_path`. Pinning this probe to `public`
    /// would inspect a different object than the one the migrator writes to
    /// whenever `search_path` puts another schema first.
    const CHAT_HISTORIES_ID_TYPE_QUERY: &'static str = "SELECT data_type
             FROM information_schema.columns
             WHERE table_schema = current_schema()
               AND table_name = 'chat_histories'
               AND column_name = 'id'
             LIMIT 1";

    async fn validate_postgres_schema(pool: &AnyPool) -> Result<()> {
        // The recap parity tables carry Go's names because this service now owns
        // them, so their presence is not a compatibility signal. The only shape
        // that genuinely cannot be served is a chat_histories table keyed by
        // something other than an integer.
        let chat_histories_id_type: Option<String> =
            sqlx::query_scalar(Self::CHAT_HISTORIES_ID_TYPE_QUERY)
                .fetch_optional(pool)
                .await?;

        Self::ensure_supported_chat_histories_id(chat_histories_id_type.as_deref())
    }

    /// Whether a failed PostgreSQL attempt may be answered with SQLite.
    ///
    /// A genuine connectivity or availability failure still falls back, so a
    /// developer without a running server keeps working. A schema
    /// incompatibility never does: the data exists, this service simply must not
    /// serve it, and quietly switching to an empty SQLite file would hide that.
    fn should_fall_back_to_sqlite(error: &anyhow::Error, sqlite_configured: bool) -> bool {
        sqlite_configured && !Self::is_incompatible_schema(error)
    }

    /// Whether `error` was caused by [`IncompatiblePostgresSchema`].
    ///
    /// The whole chain is inspected so an added `.context(..)` cannot disarm it.
    fn is_incompatible_schema(error: &anyhow::Error) -> bool {
        error
            .chain()
            .any(|cause| cause.is::<IncompatiblePostgresSchema>())
    }

    /// Reject a `chat_histories` table this service cannot read.
    ///
    /// `None` means the table does not exist yet, which a fresh database is
    /// allowed to be: the migrations create it. The error text is a fixed string
    /// so no column value, table content, or connection detail can escape.
    fn ensure_supported_chat_histories_id(chat_histories_id_type: Option<&str>) -> Result<()> {
        let Some(data_type) = chat_histories_id_type else {
            return Ok(());
        };

        if Self::SUPPORTED_CHAT_HISTORIES_ID_TYPES
            .iter()
            .any(|supported| supported.eq_ignore_ascii_case(data_type))
        {
            return Ok(());
        }

        Err(anyhow::Error::new(IncompatiblePostgresSchema))
    }
}

#[cfg(test)]
mod tests {
    use super::Database;

    /// Table names the Rich recap parity migration makes this service own.
    ///
    /// They used to be a rejection signal. The guard no longer takes table names
    /// as an input at all, so a database holding every one of them validates
    /// purely on its `chat_histories` key.
    const PARITY_TABLES: [&str; 8] = [
        "telegram_chat_feature_flags",
        "telegram_chat_recaps_options",
        "telegram_chat_auto_recaps_subscribers",
        "log_chat_histories_recaps",
        "feedback_chat_histories_recaps_reactions",
        "feedback_summarizations_reactions",
        "sent_messages",
        "metric_open_ai_chat_completion_token_usages",
    ];

    #[test]
    fn parity_tables_are_accepted_alongside_the_rust_owned_integer_key() {
        // Presence of the parity tables cannot influence the decision, because
        // the guard reads nothing but the chat_histories key type.
        assert_eq!(PARITY_TABLES.len(), 8);
        Database::ensure_supported_chat_histories_id(Some("bigint"))
            .expect("a parity database keyed by an integer must start");
    }

    #[test]
    fn every_integer_chat_histories_key_is_accepted() {
        for data_type in ["smallint", "integer", "bigint", "BIGINT", "Integer"] {
            Database::ensure_supported_chat_histories_id(Some(data_type))
                .unwrap_or_else(|error| panic!("{data_type} must be accepted: {error}"));
        }
    }

    #[test]
    fn a_database_without_chat_histories_is_accepted() {
        Database::ensure_supported_chat_histories_id(None)
            .expect("a fresh database gets chat_histories from the migrations");
    }

    #[test]
    fn a_uuid_chat_histories_key_is_still_rejected() {
        let error = Database::ensure_supported_chat_histories_id(Some("uuid"))
            .expect_err("the Go UUID key cannot be served");
        assert!(
            error
                .to_string()
                .contains("chat_histories.id is not an integer column")
        );
    }

    #[test]
    fn other_non_integer_chat_histories_keys_are_rejected() {
        for data_type in ["uuid", "text", "character varying", "numeric", "bytea"] {
            Database::ensure_supported_chat_histories_id(Some(data_type))
                .expect_err("only integral keys are supported");
        }
    }

    #[test]
    fn an_incompatible_schema_never_selects_sqlite_even_when_one_is_configured() {
        let incompatible = Database::ensure_supported_chat_histories_id(Some("uuid"))
            .expect_err("uuid is rejected");
        assert!(Database::is_incompatible_schema(&incompatible));
        assert!(!Database::should_fall_back_to_sqlite(&incompatible, true));
        assert!(!Database::should_fall_back_to_sqlite(&incompatible, false));
    }

    #[test]
    fn added_context_cannot_disarm_the_incompatibility_marker() {
        let wrapped = Database::ensure_supported_chat_histories_id(Some("uuid"))
            .expect_err("uuid is rejected")
            .context("connecting to Postgres")
            .context("bootstrapping the database");

        assert!(Database::is_incompatible_schema(&wrapped));
        assert!(!Database::should_fall_back_to_sqlite(&wrapped, true));
    }

    #[test]
    fn an_ordinary_connectivity_failure_still_selects_sqlite() {
        let unreachable = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "connection refused",
        ))
        .context("error communicating with database");

        assert!(!Database::is_incompatible_schema(&unreachable));
        assert!(Database::should_fall_back_to_sqlite(&unreachable, true));
        assert!(
            !Database::should_fall_back_to_sqlite(&unreachable, false),
            "with no SQLite configured there is nothing to fall back to"
        );
    }

    #[test]
    fn the_chat_histories_probe_follows_the_effective_schema() {
        let query = Database::CHAT_HISTORIES_ID_TYPE_QUERY;
        assert!(
            query.contains("table_schema = current_schema()"),
            "the probe must inspect the schema search_path resolves: {query}"
        );
        assert!(
            !query.contains("'public'"),
            "hard-coding public would inspect a different table than the migrator writes: {query}"
        );
        assert!(query.contains("table_name = 'chat_histories'"));
        assert!(query.contains("column_name = 'id'"));
    }

    #[test]
    fn the_rejection_message_is_fixed_and_quotes_nothing_from_the_database() {
        let first = Database::ensure_supported_chat_histories_id(Some("uuid"))
            .expect_err("uuid is rejected")
            .to_string();
        let second = Database::ensure_supported_chat_histories_id(Some("bytea"))
            .expect_err("bytea is rejected")
            .to_string();

        assert_eq!(first, second, "the message must not vary with the input");
        for leaked in ["uuid", "bytea", "password", "postgres://"] {
            assert!(
                !first.contains(leaked),
                "the rejection message leaked {leaked}: {first}"
            );
        }
    }
}
